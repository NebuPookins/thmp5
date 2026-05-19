mod audio;
mod audio_probe;
mod commands;
mod db;
pub mod file_issues;
pub mod fingerprint;
mod importer;
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
use models::RecordingRow;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use tauri::Manager;
use tokio::sync::{RwLock, Semaphore};

pub struct AppState {
    pub db: DbPool,
    pub catalog: storage::Catalog,
    /// AcoustID client API key, read from the `ACOUSTID_API_KEY` environment variable.
    /// AcoustID lookups are skipped when this is `None`.
    pub acoustid_api_key: Option<String>,
    pub importer: ImportManager,
    pub player: AudioEngineHandle,
    pub log_file_path: String,
    pub file_issues: FileIssueLog,
    /// In-memory cache of all recordings, sorted by (lower(artist.sort_name), lower(title)).
    /// Populated on first `list_recordings` call; invalidated by any write that changes recording data.
    pub recordings_cache: RwLock<Option<Arc<Vec<RecordingRow>>>>,
    /// Total number of background jobs (rescans + deletes) queued or in-progress.
    /// Used for UI progress display and to decide when to invalidate cache.
    pub pending_jobs: AtomicI64,
    /// Serializes write operations (rescan, delete) so only one DB writer is active at a time,
    /// preventing SQLITE_BUSY / "database is locked" errors under concurrent access.
    pub write_serializer: Arc<Semaphore>,
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
            let importer = ImportManager::new(file_issues.clone(), write_serializer.clone());
            let player = AudioEngineHandle::new(app.handle().clone(), file_issues.clone())
                .map_err(|e| format!("Failed to initialize audio engine: {e}"))?;
            let catalog =
                storage::Catalog::Sql(storage::sql::SqlCatalog::new(pool.clone()));
            let state = AppState {
                db: pool.clone(),
                catalog,
                acoustid_api_key: acoustid_api_key.clone(),
                importer,
                player,
                log_file_path: log_path.display().to_string(),
                file_issues,
                recordings_cache: RwLock::new(None),
                pending_jobs: AtomicI64::new(0),
                write_serializer,
            };

            if let Ok(Some(root_path)) =
                tauri::async_runtime::block_on(commands::load_music_root(&pool))
            {
                state
                    .importer
                    .spawn_scan(pool.clone(), root_path, acoustid_api_key.clone());
            }

            // Background worker: detect orphan sources on startup.
            let orphan_pool = pool.clone();
            let orphan_issues = state.file_issues.clone();
            tauri::async_runtime::spawn(async move {
                let raw_pool = orphan_pool.raw_pool().clone();
                let rows = match sqlx::query_as::<_, (String, String, String, Option<String>)>(
                    "SELECT s.id, s.recording_id, s.file_path, r.title
                     FROM source s
                     JOIN recording r ON r.id = s.recording_id
                     WHERE s.source_type = 'local_file'
                       AND s.file_path IS NOT NULL
                       AND NOT EXISTS (SELECT 1 FROM track t WHERE t.recording_id = s.recording_id)",
                )
                .fetch_all(&raw_pool)
                .await
                {
                    Ok(rows) => rows,
                    Err(e) => {
                        tracing::warn!("Orphan source check failed: {e}");
                        return;
                    }
                };
                for (source_id, recording_id, file_path, title) in &rows {
                    tracing::info!(
                        source_id = %source_id,
                        recording_id = %recording_id,
                        path = %file_path,
                        "Orphan source detected during startup scan"
                    );
                    orphan_issues.push_orphan_source(
                        file_path,
                        format!(
                            "Recording \"{}\" has no album track — source is orphaned",
                            title.as_deref().unwrap_or("unknown")
                        ),
                        source_id,
                        recording_id,
                    );
                }
                let count = rows.len();
                if count > 0 {
                    tracing::info!("Found {count} orphan source(s) on startup");
                }
            });

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
            commands::prune_empty_library_entities_command,
            commands::record_play_history,
            commands::set_recording_rating,
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
            commands::find_orphan_sources,
            commands::fix_orphan_source,
            commands::resolve_duplicate_frame,
            commands::compare_recordings,
            commands::merge_recordings,
            commands::split_recording,
            commands::merge_release_groups,
            commands::get_artist_detail,
            commands::get_release_group_detail,
            commands::get_recording_detail,
            commands::check_artist_compound,
            commands::apply_artist_fix,
        ])
        .run(tauri::generate_context!())
        .expect("error while running thmp5");
}
