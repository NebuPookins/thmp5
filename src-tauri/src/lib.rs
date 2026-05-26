mod audio;
mod audio_probe;
mod commands;
mod db;
pub mod file_issues;
pub mod fingerprint;
mod importer;
mod lastfm;
mod library;
mod logging;
mod models;
pub mod query;
mod sleep_inhibitor;
pub mod storage;
mod waveform;

use audio::AudioEngineHandle;
use db::DbPool;
use file_issues::FileIssueLog;
use importer::ImportManager;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{Emitter, Manager};
use tokio::sync::{RwLock, Semaphore};

pub struct AppState {
    pub db: DbPool,
    /// In-memory catalog rebuilt from `source.raw_tags_json` on startup and after every write.
    pub catalog: Arc<RwLock<storage::Catalog>>,
    /// AcoustID client API key, read from the `ACOUSTID_API_KEY` environment variable.
    /// AcoustID lookups are skipped when this is `None`.
    pub acoustid_api_key: Option<String>,
    pub importer: ImportManager,
    pub player: AudioEngineHandle,
    pub log_file_path: String,
    pub file_issues: FileIssueLog,
    /// Total number of background jobs (rescans + deletes) queued or in-progress.
    /// Used for UI progress display and to decide when to invalidate cache.
    pub pending_jobs: AtomicI64,
    /// Serializes write operations (rescan, delete) so only one DB writer is active at a time,
    /// preventing SQLITE_BUSY / "database is locked" errors under concurrent access.
    pub write_serializer: Arc<Semaphore>,
    /// Temporary auth token stored between `lastfm_get_auth_url` and `lastfm_complete_auth`.
    pub lastfm_auth_token: Mutex<Option<String>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let app_data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&app_data_dir)?;
            let db_path = app_data_dir.join("library.db");
            let log_path = app_data_dir.join("thmp5.log");

            logging::init(&log_path).map_err(|e| format!("Failed to initialize logging: {e}"))?;

            tracing::info!("Database path: {}", db_path.display());
            tracing::info!("Log path: {}", log_path.display());

            let pool = tauri::async_runtime::block_on(db::init_pool(&db_path))
                .map_err(|e| format!("Failed to initialize database: {e}"))?;

            let acoustid_api_key = std::env::var("ACOUSTID_API_KEY").ok();
            if acoustid_api_key.is_some() {
                tracing::info!("AcoustID API key loaded from environment");
            } else {
                tracing::info!("ACOUSTID_API_KEY not set — fingerprint lookups disabled");
            }
            let file_issues = FileIssueLog::new();
            let write_serializer = Arc::new(Semaphore::new(1));

            // Channel for the audio engine to send Last.fm events to a background listener.
            let (lastfm_tx, mut lastfm_rx) = tokio::sync::mpsc::channel::<audio::LastFmAction>(256);
            let db_for_lastfm = pool.clone();
            let lastfm_app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Some(action) = lastfm_rx.recv().await {
                    match action {
                        audio::LastFmAction::NowPlaying { artist, track } => {
                            let config =
                                match crate::commands::load_lastfm_config(&db_for_lastfm).await {
                                    Ok(c) => c,
                                    Err(_) => continue,
                                };
                            let session_key = match crate::commands::load_lastfm_session_key(
                                &db_for_lastfm,
                            )
                            .await
                            {
                                Ok(k) => k,
                                Err(_) => continue,
                            };
                            let _ =
                                lastfm::now_playing(&config, &session_key, &artist, &track, None)
                                    .await;
                            // Query loved status and push to frontend so the heart button updates.
                            match lastfm::get_track_loved(&config, &session_key, &artist, &track)
                                .await
                            {
                                Ok(loved) => {
                                    let _ = lastfm_app_handle.emit("lastfm-loved-status", loved);
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        artist,
                                        track,
                                        "Last.fm: loved status query failed: {e}"
                                    );
                                    let _ = lastfm_app_handle.emit("lastfm-loved-status", false);
                                }
                            }
                        }
                        audio::LastFmAction::Scrobble {
                            artist,
                            track,
                            started_at_secs,
                            played_ms,
                            duration_ms,
                        } => {
                            tracing::info!(
                                artist,
                                track,
                                started_at_secs,
                                played_ms,
                                duration_ms,
                                "Last.fm scrobble received by listener"
                            );
                            // Only scrobble if track > 30s and played >= min(50%, 4 min).
                            // If duration is unknown (0), use played_ms alone — scrobble
                            // if at least 30s were played.
                            if duration_ms > 0 && duration_ms < 30_000 {
                                tracing::info!(
                                    "Skipping scrobble: duration {}ms < 30s",
                                    duration_ms
                                );
                                continue;
                            }
                            let min_play_ms = if duration_ms > 0 {
                                std::cmp::min(duration_ms / 2, 240_000)
                            } else {
                                30_000
                            };
                            if played_ms < min_play_ms {
                                tracing::info!(
                                    "Skipping scrobble: played {}ms < min {}ms",
                                    played_ms,
                                    min_play_ms
                                );
                                continue;
                            }
                            let config =
                                match crate::commands::load_lastfm_config(&db_for_lastfm).await {
                                    Ok(c) => c,
                                    Err(_) => continue,
                                };
                            let session_key = match crate::commands::load_lastfm_session_key(
                                &db_for_lastfm,
                            )
                            .await
                            {
                                Ok(k) => k,
                                Err(_) => continue,
                            };
                            let _ = lastfm::scrobble(
                                &config,
                                &session_key,
                                &artist,
                                &track,
                                None,
                                started_at_secs,
                            )
                            .await;
                        }
                    }
                }
            });

            let player =
                AudioEngineHandle::new(app.handle().clone(), file_issues.clone(), Some(lastfm_tx))
                    .map_err(|e| format!("Failed to initialize audio engine: {e}"))?;
            let mem_catalog =
                tauri::async_runtime::block_on(storage::memory::MemoryCatalog::load_from_db(&pool))
                    .map_err(|e| format!("Failed to build in-memory catalog: {e}"))?;
            let catalog = Arc::new(RwLock::new(storage::Catalog::Memory(Box::new(mem_catalog))));
            let importer = ImportManager::new(
                file_issues.clone(),
                write_serializer.clone(),
                Arc::clone(&catalog),
            );
            let state = AppState {
                db: pool.clone(),
                catalog,
                acoustid_api_key: acoustid_api_key.clone(),
                importer,
                player,
                log_file_path: log_path.display().to_string(),
                file_issues,
                pending_jobs: AtomicI64::new(0),
                write_serializer,
                lastfm_auth_token: Mutex::new(None),
            };

            if let Ok(Some(root_path)) =
                tauri::async_runtime::block_on(commands::load_music_root(&pool))
            {
                state
                    .importer
                    .spawn_scan(pool.clone(), root_path, acoustid_api_key.clone());
            }

            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::import_paths,
            commands::fix_merged_recordings,
            commands::get_app_bootstrap,
            commands::complete_initial_setup,
            commands::get_import_progress,
            commands::trigger_library_scan,
            commands::rescan_source,
            commands::rescan_sources,
            commands::rescan_sources_for_artist,
            commands::rescan_sources_for_recording,
            commands::rescan_sources_for_release_group,
            commands::rescan_all_sources,
            commands::set_music_root,
            commands::update_queue_settings,
            commands::save_external_commands,
            commands::spawn_external_command,
            commands::get_library_summary,
            commands::list_recordings,
            commands::list_artists,
            commands::list_release_groups,
            commands::record_play_history,
            commands::set_source_rating,
            commands::get_player_state,
            commands::get_log_file_path,
            commands::get_db_pool_debug_snapshot,
            commands::debug_id3_text_frame,
            commands::play,
            commands::pause,
            commands::resume,
            commands::seek,
            commands::set_volume,
            commands::set_normalization_enabled,
            commands::stop,
            commands::get_cover_art,
            commands::get_waveform,
            commands::list_all_tags,
            commands::evaluate_smart_playlist,
            commands::list_playlists,
            commands::save_smart_playlist,
            commands::delete_playlist,
            commands::delete_recording,
            commands::get_file_issues,
            commands::delete_backup_file,
            commands::write_tag_frame,
            commands::delete_tag_frame,
            commands::fix_orphan_source,
            commands::resolve_duplicate_frame,
            commands::compare_recordings,
            commands::merge_recordings,
            commands::split_recording,
            commands::get_artist_detail,
            commands::get_release_group_detail,
            commands::get_recording_detail,
            commands::check_artist_compound,
            commands::get_lastfm_status,
            commands::save_lastfm_credentials,
            commands::lastfm_get_auth_url,
            commands::lastfm_complete_auth,
            commands::lastfm_disconnect,
            commands::lastfm_love_track,
            commands::lastfm_get_track_loved,
            commands::lastfm_now_playing,
            commands::lastfm_scrobble,
        ])
        .run(tauri::generate_context!())
        .expect("error while running thmp5");
}
