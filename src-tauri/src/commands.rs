use crate::audio::PlayRequest as EnginePlayRequest;
use crate::file_issues::{FileIssue, FileIssueKind};
use crate::library::import::{import_paths as do_import, rescan_source as do_rescan_source};
use crate::models::{
    AppBootstrap, AppConfig, ArtistDetail, ArtistRow, CompoundArtistCheck, DbPoolDebugSnapshot,
    ExternalCommand, FixMergedRecordingsStats, Id3FrameDebugInfo, Id3FrameDebugRequest,
    ImportProgress, ImportStats, InitialSetupRequest, LastFmAuthUrl, LastFmGetTrackLovedRequest,
    LastFmLoveTrackRequest, LastFmNowPlayingRequest, LastFmScrobbleRequest, LastFmStatus,
    LibrarySummary, PlayHistoryInput, PlayRequest, PlayerState, PlaylistRow, QueueSettingsUpdate,
    RecordingDetail, RecordingRatingUpdateResult, RecordingRow, ReleaseGroupDetail,
    ReleaseGroupRow, SaveSmartPlaylistRequest, SeekRequest, SmartPlaylistResult,
    SourceRatingUpdateRequest, VolumeRequest,
};
use crate::query;
use crate::storage::CatalogReader;
use crate::AppState;
use serde::Serialize;
use sqlx::{Acquire, Row};
use std::sync::atomic::Ordering;
use tauri::Emitter;

#[derive(Clone, Serialize)]
struct JobUpdate {
    remaining: i64,
    job_type: String,
}

/// Emit a `job-update` event so the frontend can show the queue status.
fn emit_job_update(app: &tauri::AppHandle, state: &AppState, job_type: &str) {
    let remaining = state.pending_jobs.load(Ordering::Relaxed);
    let _ = app.emit(
        "job-update",
        JobUpdate {
            remaining,
            job_type: job_type.to_string(),
        },
    );
}

/// Rebuild the in-memory catalog from the current database state.
/// Called after any write that changes source or user data.
async fn reload_catalog(state: &AppState) {
    match crate::storage::memory::MemoryCatalog::load_from_db(&state.db).await {
        Ok(mem) => {
            *state.catalog.write().await = crate::storage::Catalog::Memory(Box::new(mem));
        }
        Err(e) => {
            tracing::error!("Failed to reload memory catalog: {e:#}");
        }
    }
}

// ── Import ────────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn import_paths(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<ImportStats, String> {
    let result = do_import(
        &state.db,
        paths,
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
    )
    .await
    .map_err(|e| e.to_string())?;
    reload_catalog(&state).await;
    Ok(result)
}

#[tauri::command]
pub async fn get_app_bootstrap(state: tauri::State<'_, AppState>) -> Result<AppBootstrap, String> {
    let config = load_app_config(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let library_summary = state
        .catalog
        .read()
        .await
        .get_library_summary()
        .await
        .map_err(|e| e.to_string())?;
    let import_progress = state.importer.snapshot();
    let needs_setup = config.music_root.is_none();

    Ok(AppBootstrap {
        needs_setup,
        config,
        import_progress,
        library_summary,
    })
}

#[tauri::command]
pub async fn complete_initial_setup(
    state: tauri::State<'_, AppState>,
    request: InitialSetupRequest,
) -> Result<AppConfig, String> {
    let root = request.music_root.trim();
    if root.is_empty() {
        return Err("Music root cannot be empty.".to_string());
    }

    let path = std::path::Path::new(root);
    if !path.is_dir() {
        return Err(format!("Music root is not a directory: {}", path.display()));
    }

    set_config_value(&state.db, "music_root", root)
        .await
        .map_err(|e| e.to_string())?;

    let config = load_app_config(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    state.importer.spawn_scan(
        state.db.clone(),
        root.to_string(),
        state.acoustid_api_key.clone(),
    );
    Ok(config)
}

#[tauri::command]
pub async fn get_import_progress(
    state: tauri::State<'_, AppState>,
) -> Result<ImportProgress, String> {
    Ok(state.importer.snapshot())
}

#[tauri::command]
pub async fn fix_merged_recordings(
    state: tauri::State<'_, AppState>,
) -> Result<FixMergedRecordingsStats, String> {
    let result = crate::library::fix_merges::fix_merged_recordings(
        &state.db,
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
    )
    .await
    .map_err(|e| e.to_string())?;
    reload_catalog(&state).await;
    Ok(result)
}

#[tauri::command]
pub async fn trigger_library_scan(
    state: tauri::State<'_, AppState>,
) -> Result<ImportProgress, String> {
    let root = load_music_root(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Music root is not configured.".to_string())?;

    state
        .importer
        .spawn_scan(state.db.clone(), root, state.acoustid_api_key.clone());

    Ok(state.importer.snapshot())
}

#[tauri::command]
pub async fn rescan_source(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("Source path cannot be empty.".to_string());
    }

    let source_path = std::path::Path::new(trimmed);
    if !source_path.is_file() {
        return Err(format!("Source file is missing: {}", source_path.display()));
    }

    state.pending_jobs.fetch_add(1, Ordering::Relaxed);
    emit_job_update(&app, &state, "rescan");

    let result = do_rescan_source(
        &state.db,
        source_path,
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
        &state.file_issues,
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("Failed to rescan {}:\n{e:#}", source_path.display()));

    state.pending_jobs.fetch_sub(1, Ordering::Relaxed);
    emit_job_update(&app, &state, "rescan");

    reload_catalog(&state).await;
    result
}

#[tauri::command]
pub async fn rescan_sources(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<(), String> {
    if paths.is_empty() {
        return Err("At least one source path is required.".to_string());
    }
    let trimmed: Vec<String> = paths
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    rescan_path_list(&app, &state, trimmed).await?;
    Ok(())
}

/// Shared loop: track per-file counter in AppState, emit `job-update` after each file.
async fn rescan_path_list(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<(), String> {
    let count = paths.len() as i64;
    state.pending_jobs.fetch_add(count, Ordering::Relaxed);
    emit_job_update(app, state, "rescan");

    let mut failures = Vec::new();
    for path in paths {
        let source_path = std::path::Path::new(&path);
        if !source_path.is_file() {
            failures.push(format!("{}: Source file is missing", source_path.display()));
        } else {
            if let Err(error) = do_rescan_source(
                &state.db,
                source_path,
                state.acoustid_api_key.as_deref(),
                &state.write_serializer,
                &state.file_issues,
            )
            .await
            {
                failures.push(format!("{}:\n{error:#}", source_path.display()));
            }
        }
        state.pending_jobs.fetch_sub(1, Ordering::Relaxed);
        emit_job_update(app, state, "rescan");
    }

    reload_catalog(state).await;

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Failed to rescan {} source(s):\n{}",
            failures.len(),
            failures.join("\n\n")
        ))
    }
}

#[tauri::command]
pub async fn rescan_sources_for_artist(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    artist_id: String,
) -> Result<(), String> {
    let paths: Vec<String> = {
        let cat = state.catalog.read().await;
        cat.source_paths_for_artist(&artist_id).1
    };

    if paths.is_empty() {
        return Err("No local sources found for this artist.".to_string());
    }

    rescan_path_list(&app, &state, paths).await
}

#[tauri::command]
pub async fn rescan_sources_for_release_group(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    release_group_id: String,
) -> Result<(), String> {
    let paths: Vec<String> = {
        let cat = state.catalog.read().await;
        cat.source_paths_for_release_group(&release_group_id)
    };

    if paths.is_empty() {
        return Err("No local sources found for this album.".to_string());
    }

    rescan_path_list(&app, &state, paths).await
}

#[tauri::command]
pub async fn rescan_sources_for_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    recording_id: String,
) -> Result<(), String> {
    // Resolve recording_id to source paths via the in-memory catalog.
    let paths: Vec<String> = {
        let catalog = state.catalog.read().await;
        let recs = catalog
            .list_recordings()
            .await
            .map_err(|e| format!("Failed to list recordings: {e}"))?;
        let matching = recs.iter().find(|r| r.id == recording_id);
        match matching {
            Some(rec) => rec
                .source_paths
                .iter()
                .filter(|p| !p.is_empty())
                .cloned()
                .collect(),
            None => return Err("Recording not found in catalog.".to_string()),
        }
    };

    if paths.is_empty() {
        return Err("No local sources found for this recording.".to_string());
    }

    rescan_path_list(&app, &state, paths).await?;
    Ok(())
}

#[tauri::command]
pub async fn rescan_all_sources(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let paths: Vec<String> = {
        let mut conn = state
            .db
            .acquire("command.rescan_all_sources".to_string())
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query_scalar(
            "SELECT DISTINCT s.file_path
             FROM source s
             WHERE s.source_type = 'local_file'
               AND s.file_path IS NOT NULL
             ORDER BY s.file_path",
        )
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| e.to_string())?
    };

    if paths.is_empty() {
        return Err("No local sources found in the library.".to_string());
    }

    rescan_path_list(&app, &state, paths).await?;
    Ok(())
}

#[tauri::command]
pub async fn set_music_root(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<AppConfig, String> {
    let root = path.trim();
    if root.is_empty() {
        return Err("Music root cannot be empty.".to_string());
    }
    let p = std::path::Path::new(root);
    if !p.is_dir() {
        return Err(format!("Not a directory: {}", p.display()));
    }
    set_config_value(&state.db, "music_root", root)
        .await
        .map_err(|e| e.to_string())?;
    state.importer.spawn_scan(
        state.db.clone(),
        root.to_string(),
        state.acoustid_api_key.clone(),
    );
    load_app_config(&state.db).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_queue_settings(
    state: tauri::State<'_, AppState>,
    update: QueueSettingsUpdate,
) -> Result<AppConfig, String> {
    let limit = update.queue_history_limit.clamp(1, 100);
    set_config_value(&state.db, "queue_history_limit", &limit.to_string())
        .await
        .map_err(|e| e.to_string())?;
    load_app_config(&state.db).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn save_external_commands(
    state: tauri::State<'_, AppState>,
    commands: Vec<ExternalCommand>,
) -> Result<AppConfig, String> {
    let json = serde_json::to_string(&commands).map_err(|e| e.to_string())?;
    set_config_value(&state.db, "external_commands", &json)
        .await
        .map_err(|e| e.to_string())?;
    load_app_config(&state.db).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub fn spawn_external_command(template: String, file_path: String) -> Result<(), String> {
    let mut parts: Vec<String> = shell_words(&template)
        .into_iter()
        .map(|w| w.replace("%%", &file_path))
        .collect();
    if parts.is_empty() {
        return Err("Empty command".to_string());
    }
    let program = parts.remove(0);
    std::process::Command::new(&program)
        .args(&parts)
        .spawn()
        .map_err(|e| format!("Failed to spawn '{}': {}", program, e))?;
    Ok(())
}

fn shell_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = ' ';
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == quote_char {
                in_quotes = false;
            } else {
                current.push(c);
            }
        } else if c == '"' || c == '\'' {
            in_quotes = true;
            quote_char = c;
        } else if c == ' ' || c == '\t' {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[tauri::command]
pub async fn record_play_history(
    state: tauri::State<'_, AppState>,
    input: PlayHistoryInput,
) -> Result<(), String> {
    let mut conn = state
        .db
        .acquire(format!(
            "command.record_play_history source_id={}",
            input.source_id
        ))
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO play_history (source_id, duration_played_ms)
         VALUES (?, ?)",
    )
    .bind(input.source_id)
    .bind(input.duration_played_ms)
    .execute(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    reload_catalog(&state).await;
    Ok(())
}

#[tauri::command]
pub async fn set_source_rating(
    state: tauri::State<'_, AppState>,
    request: SourceRatingUpdateRequest,
) -> Result<RecordingRatingUpdateResult, String> {
    validate_rating(request.stars)?;

    let mut conn = state
        .db
        .acquire(format!(
            "command.set_source_rating source_id={}",
            request.source_id
        ))
        .await
        .map_err(|e| e.to_string())?;

    if let Some(stars) = request.stars {
        sqlx::query(
            "INSERT INTO source_rating (source_id, stars, updated_at)
             VALUES (?, ?, datetime('now'))
             ON CONFLICT(source_id) DO UPDATE SET
                stars = excluded.stars,
                updated_at = datetime('now')",
        )
        .bind(&request.source_id)
        .bind(stars)
        .execute(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;
    } else {
        sqlx::query("DELETE FROM source_rating WHERE source_id = ?")
            .bind(&request.source_id)
            .execute(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
    }

    // Look up the source's file path to find the recording in the catalog.
    let source_path: Option<String> =
        sqlx::query_scalar("SELECT file_path FROM source WHERE id = ?")
            .bind(&request.source_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| e.to_string())?
            .flatten();

    drop(conn);
    reload_catalog(&state).await;

    // Find which recording this source belongs to via the in-memory catalog.
    let catalog = state.catalog.read().await;
    let recs = catalog.list_recordings().await.map_err(|e| e.to_string())?;
    let rec = source_path.as_ref().and_then(|path| {
        recs.iter()
            .find(|r| r.source_paths.iter().any(|p| p == path))
    });

    let mut recording = crate::models::EntityRatingUpdate {
        id: String::new(),
        rating: None,
    };
    let mut artists: Vec<crate::models::EntityRatingUpdate> = Vec::new();
    let mut release_groups: Vec<crate::models::EntityRatingUpdate> = Vec::new();
    let mut affected_recordings: Vec<crate::models::EntityRatingUpdate> = Vec::new();

    if let Some(rec) = rec {
        let rec_detail = catalog
            .get_recording_detail(&rec.id)
            .await
            .map_err(|e| e.to_string())?;

        if let Some(rec_detail) = rec_detail {
            recording = crate::models::EntityRatingUpdate {
                id: rec.id.clone(),
                rating: rec_detail.rating,
            };
            let rg_ids: Vec<String> = {
                let mut seen = std::collections::HashSet::new();
                rec_detail
                    .releases
                    .iter()
                    .filter(|r| seen.insert(r.release_group_id.clone()))
                    .map(|r| r.release_group_id.clone())
                    .collect()
            };
            for a in &rec_detail.artists {
                if let Ok(Some(detail)) = catalog.get_artist_detail(&a.artist_id).await {
                    artists.push(crate::models::EntityRatingUpdate {
                        id: a.artist_id.clone(),
                        rating: detail.rating,
                    });
                }
            }
            for rg_id in &rg_ids {
                if let Ok(Some(detail)) = catalog.get_release_group_detail(rg_id).await {
                    release_groups.push(crate::models::EntityRatingUpdate {
                        id: rg_id.clone(),
                        rating: detail.rating,
                    });
                }
            }

            // Collect predicted ratings for all recordings that share an artist
            // or release group with the rated recording, so the frontend can
            // refresh their predicted-rating stars.
            let artist_set: std::collections::HashSet<&str> =
                rec.artist_ids.iter().map(|s| s.as_str()).collect();
            let rg_id_set: std::collections::HashSet<&str> =
                rg_ids.iter().map(|s| s.as_str()).collect();

            for sibling in &recs {
                if sibling.id == rec.id {
                    continue;
                }
                let shares_artist = sibling
                    .artist_ids
                    .iter()
                    .any(|aid| artist_set.contains(aid.as_str()));
                let shares_album = sibling
                    .releases
                    .iter()
                    .any(|r| rg_id_set.contains(r.release_group_id.as_str()));
                if shares_artist || shares_album {
                    affected_recordings.push(crate::models::EntityRatingUpdate {
                        id: sibling.id.clone(),
                        rating: sibling.predicted_rating,
                    });
                }
            }
        }
    }
    drop(catalog);

    Ok(RecordingRatingUpdateResult {
        recording,
        release_groups,
        artists,
        affected_recordings,
    })
}

// ── Library queries ───────────────────────────────────────────────────────────

#[tauri::command]
pub fn get_player_state(state: tauri::State<'_, AppState>) -> Result<PlayerState, String> {
    Ok(state.player.snapshot())
}

#[tauri::command]
pub fn get_log_file_path(state: tauri::State<'_, AppState>) -> Result<String, String> {
    Ok(state.log_file_path.clone())
}

#[tauri::command]
pub fn get_db_pool_debug_snapshot(
    state: tauri::State<'_, AppState>,
) -> Result<DbPoolDebugSnapshot, String> {
    Ok(state.db.snapshot())
}

#[tauri::command]
pub async fn get_file_issues(state: tauri::State<'_, AppState>) -> Result<Vec<FileIssue>, String> {
    let mut issues = state.file_issues.all();
    let catalog = state.catalog.read().await;
    issues.extend(catalog.file_issues());
    Ok(issues)
}

/// Delete a `.thmp5bak` backup file created by a previous tag edit.
#[tauri::command]
pub async fn delete_backup_file(
    state: tauri::State<'_, AppState>,
    backup_path: String,
) -> Result<(), String> {
    let p = std::path::Path::new(&backup_path);
    if p.exists() {
        std::fs::remove_file(p).map_err(|e| format!("Failed to delete backup file: {e}"))?;
    }
    // Remove the corresponding issue from the log.
    state.file_issues.retain(|issue| {
        !(issue.kind == FileIssueKind::BackupFileExists
            && issue.backup_path.as_deref() == Some(&backup_path))
    });
    Ok(())
}

/// Write (upsert) a single ID3v2 text frame in a file's tag section.
#[tauri::command]
pub async fn write_tag_frame(
    _app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    file_path: String,
    frame_id: String,
    new_value: String,
) -> Result<crate::library::tag_writer::TagWriteResult, String> {
    let path_buf = std::path::Path::new(&file_path).to_path_buf();
    let fid = frame_id.clone();
    let nv = new_value.clone();

    let result = tokio::task::spawn_blocking(move || {
        crate::library::tag_writer::write_single_frame(&path_buf, &fid, &nv)
    })
    .await
    .map_err(|e| format!("Tag write task panicked: {e}"))?
    .map_err(|e| format!("Failed to write tag: {e}"))?;

    // Re-read metadata and update DB via rescan (lightweight — skips
    // AcoustID by passing None for the API key).
    do_rescan_source(
        &state.db,
        std::path::Path::new(&file_path),
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
        &state.file_issues,
    )
    .await
    .map_err(|e| format!("Failed to re-scan after tag edit: {e}"))?;

    reload_catalog(&state).await;
    Ok(result)
}

/// Delete all frames matching `frame_id` from a file's tag section.
#[tauri::command]
pub async fn delete_tag_frame(
    _app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    file_path: String,
    frame_id: String,
) -> Result<crate::library::tag_writer::TagWriteResult, String> {
    let path_buf = std::path::Path::new(&file_path).to_path_buf();
    let fid = frame_id.clone();

    let result = tokio::task::spawn_blocking(move || {
        crate::library::tag_writer::delete_frame(&path_buf, &fid)
    })
    .await
    .map_err(|e| format!("Tag delete task panicked: {e}"))?
    .map_err(|e| format!("Failed to delete tag frame: {e}"))?;

    do_rescan_source(
        &state.db,
        std::path::Path::new(&file_path),
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
        &state.file_issues,
    )
    .await
    .map_err(|e| format!("Failed to re-scan after tag delete: {e}"))?;

    reload_catalog(&state).await;
    Ok(result)
}

/// Fix an orphan source by re-scanning its file metadata and re-creating the
/// album structure (release-group / release / medium / track) so the recording
/// becomes visible in the library.
#[tauri::command]
pub async fn fix_orphan_source(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    source_id: String,
) -> Result<(), String> {
    let pool = state.db.raw_pool();
    let file_path: Option<String> = sqlx::query_scalar("SELECT file_path FROM source WHERE id = ?")
        .bind(&source_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("Failed to look up source: {e}"))?
        .flatten();

    let file_path = file_path.ok_or_else(|| format!("Source {source_id} not found"))?;
    let source_path = std::path::Path::new(&file_path);
    if !source_path.is_file() {
        return Err(format!("Source file is missing: {}", source_path.display()));
    }

    state
        .pending_jobs
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    emit_job_update(&app, &state, "rescan");

    do_rescan_source(
        &state.db,
        source_path,
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
        &state.file_issues,
    )
    .await
    .map(|_| ())
    .map_err(|e| {
        format!(
            "Failed to fix orphan source {}:\n{e:#}",
            source_path.display()
        )
    })?;

    state
        .pending_jobs
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    emit_job_update(&app, &state, "rescan");

    reload_catalog(&state).await;
    Ok(())
}

/// Enumerate conflicting frames across all consecutive ID3v2 tags in a file,
/// so the frontend can ask the user to pick a value for each before writing.
#[tauri::command]
pub async fn preview_duplicate_merge(
    file_path: String,
) -> Result<Vec<crate::library::tag_writer::MergeConflict>, String> {
    let path_buf = std::path::Path::new(&file_path).to_path_buf();
    tokio::task::spawn_blocking(move || crate::library::tag_writer::preview_merge(&path_buf))
        .await
        .map_err(|e| format!("Merge preview task panicked: {e}"))?
        .map_err(|e| format!("Failed to preview merge: {e}"))
}

/// Merge all consecutive ID3v2 tags into one, applying the user's choices for
/// each conflicting frame, then re-scan the source and reload the catalog.
#[tauri::command]
pub async fn apply_duplicate_merge(
    state: tauri::State<'_, AppState>,
    file_path: String,
    decisions: Vec<crate::library::tag_writer::MergeDecision>,
) -> Result<crate::library::tag_writer::TagWriteResult, String> {
    let path_buf = std::path::Path::new(&file_path).to_path_buf();
    if !path_buf.is_file() {
        return Err(format!("Source file is missing: {}", file_path));
    }

    let write_path = path_buf.clone();
    let _write = tokio::task::spawn_blocking(move || {
        crate::library::tag_writer::apply_merge(&write_path, &decisions)
    })
    .await
    .map_err(|e| format!("Tag merge task panicked: {e}"))?
    .map_err(|e| format!("Failed to merge tags: {e}"))?;

    // Re-scan the source so `raw_tags_json` (which feeds both the catalog's
    // derived issues and metadata display) reflects a single, coherent value.
    do_rescan_source(
        &state.db,
        &path_buf,
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
        &state.file_issues,
    )
    .await
    .map_err(|e| format!("Failed to re-scan after tag merge: {e}"))?;

    reload_catalog(&state).await;
    Ok(_write)
}

#[tauri::command]
pub fn debug_id3_text_frame(request: Id3FrameDebugRequest) -> Result<Id3FrameDebugInfo, String> {
    crate::library::scanner::debug_id3_text_frame(
        std::path::Path::new(&request.path),
        &request.frame_id,
    )
    .map_err(|e| e.to_string())
}

/// Compute a linear gain factor from ReplayGain track gain (preferred) or
/// RMS-based loudness (lufs) for loudness normalization.
/// Returns 1.0 and `"None"` if neither measurement is available.
/// The result is clamped to [0.1, 10.0] to prevent extreme values.
fn compute_normalization_gain(
    replay_gain_db: Option<f64>,
    lufs: Option<f64>,
) -> (f32, &'static str) {
    let (gain, source) = if let Some(db) = replay_gain_db {
        // ReplayGain is already the desired gain adjustment in dB.
        (10.0_f64.powf(db / 20.0), "ReplayGain")
    } else if let Some(loudness) = lufs {
        // Target loudness in dB FS (RMS-based measurement).
        const TARGET_LOUDNESS_DB: f64 = -16.0;
        let gain_db = TARGET_LOUDNESS_DB - loudness;
        (10.0_f64.powf(gain_db / 20.0), "LUFS")
    } else {
        (1.0, "None")
    };
    (gain.clamp(0.1, 10.0) as f32, source)
}

#[tauri::command]
pub async fn play(
    state: tauri::State<'_, AppState>,
    request: PlayRequest,
) -> Result<PlayerState, String> {
    tracing::info!(source_id = %request.source_id, "Play command received");
    let mut conn = state
        .db
        .acquire(format!(
            "command.play resolve_source source_id={}",
            request.source_id
        ))
        .await
        .map_err(|e| e.to_string())?;
    let row = sqlx::query(
        "SELECT
            s.id                  AS source_id,
            s.file_path           AS file_path,
            s.raw_tags_json,
            s.fingerprint,
            s.replay_gain_track_db,
            s.lufs
         FROM source s
         WHERE s.id = ?
           AND s.source_type = 'local_file'
           AND s.file_path IS NOT NULL",
    )
    .bind(&request.source_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "Playable local source not found.".to_string())?;

    let mut source_id: String = row.get("source_id");
    let mut file_path: String = row.get("file_path");
    let raw_tags_json: Option<String> = row.get("raw_tags_json");
    let fingerprint: Option<String> = row.get("fingerprint");

    let tags: Vec<(String, String)> = raw_tags_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    let title: Option<String> = tags
        .iter()
        .find(|(k, _)| k == "TIT2")
        .map(|(_, v)| v.clone());
    let artist: Option<String> = tags
        .iter()
        .find(|(k, _)| k == "TPE1")
        .map(|(_, v)| v.clone());
    let replay_gain_track_db: Option<f64> = row.get("replay_gain_track_db");
    let lufs: Option<f64> = row.get("lufs");

    // Compute the linear normalization gain and source label.
    let (mut normalization_gain, mut normalization_source) =
        compute_normalization_gain(replay_gain_track_db, lufs);

    // If the file is missing, try to fall back to another source for the same
    // recording using fingerprint (Chromaprint) or MBID from raw_tags.
    if !std::path::Path::new(&file_path).exists() {
        tracing::warn!(
            source_id = %source_id,
            path = %file_path,
            "Source file missing; searching for alternatives"
        );

        // Try fingerprint-based fallback first, then MBID-based if no fingerprint.
        let mut alt = if fingerprint.is_some() {
            let alts = sqlx::query(
                "SELECT s.id, s.file_path, s.replay_gain_track_db, s.lufs
                 FROM source s
                 WHERE s.fingerprint = (SELECT fingerprint FROM source WHERE id = ?)
                   AND s.id != ?
                   AND s.source_type = 'local_file'
                   AND s.file_path IS NOT NULL
                 ORDER BY s.file_path",
            )
            .bind(&source_id)
            .bind(&source_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;

            let mut found: Option<(String, String, Option<f64>, Option<f64>)> = None;
            for alt in &alts {
                let alt_id: String = alt.get("id");
                let alt_path: String = alt.get("file_path");
                if std::path::Path::new(&alt_path).exists() {
                    found = Some((
                        alt_id,
                        alt_path,
                        alt.get("replay_gain_track_db"),
                        alt.get("lufs"),
                    ));
                    break;
                }
            }
            found
        } else {
            None
        };

        // Fall back to MBID-based lookup if fingerprint produced no result.
        if alt.is_none() {
            let mbid = tags
                .iter()
                .find(|(k, _)| k == "UFID:http://musicbrainz.org")
                .map(|(_, v)| v.clone());
            if let Some(mbid) = mbid {
                let alts = sqlx::query(
                    "SELECT s.id, s.file_path, s.replay_gain_track_db, s.lufs, s.raw_tags_json
                     FROM source s
                     WHERE s.id != ?
                       AND s.source_type = 'local_file'
                       AND s.file_path IS NOT NULL
                     ORDER BY s.file_path",
                )
                .bind(&source_id)
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| e.to_string())?;

                for alt_row in alts {
                    let alt_raw_tags: Option<String> = alt_row.get("raw_tags_json");
                    let has_mbid = alt_raw_tags
                        .as_deref()
                        .and_then(|j| serde_json::from_str::<Vec<(String, String)>>(j).ok())
                        .map(|t| {
                            t.iter()
                                .any(|(k, v)| k == "UFID:http://musicbrainz.org" && v == &mbid)
                        })
                        .unwrap_or(false);
                    if has_mbid {
                        let alt_id: String = alt_row.get("id");
                        let alt_path: String = alt_row.get("file_path");
                        if std::path::Path::new(&alt_path).exists() {
                            alt = Some((
                                alt_id,
                                alt_path,
                                alt_row.get("replay_gain_track_db"),
                                alt_row.get("lufs"),
                            ));
                            break;
                        }
                    }
                }
            }
        }

        if let Some((alt_id, alt_path, alt_rg, alt_lufs)) = alt {
            // Remove the stale source row silently.
            sqlx::query("DELETE FROM source WHERE id = ?")
                .bind(&source_id)
                .execute(&mut *conn)
                .await
                .map_err(|e| e.to_string())?;
            tracing::info!(
                removed_source_id = %source_id,
                removed_path = %file_path,
                using_source_id = %alt_id,
                using_path = %alt_path,
                "Removed missing source, using alternative"
            );
            source_id = alt_id;
            file_path = alt_path;
            let (alt_gain, alt_src) = compute_normalization_gain(alt_rg, alt_lufs);
            normalization_gain = alt_gain;
            normalization_source = alt_src;
        } else {
            return Err(format!(
                "File not found and no working alternative source exists: {}",
                file_path
            ));
        }
    }

    tracing::info!(
        source_id = %source_id,
        path = %file_path,
        "Resolved play request"
    );

    state
        .player
        .play(EnginePlayRequest {
            source_id,
            file_path,
            title,
            artist,
            normalization_gain,
            normalization_source: normalization_source.to_string(),
        })
        .map_err(|e| e.to_string())?;

    Ok(state.player.snapshot())
}

#[tauri::command]
pub fn set_normalization_enabled(
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<PlayerState, String> {
    tracing::info!(enabled, "Set normalization enabled command received");
    state
        .player
        .set_normalization_enabled(enabled)
        .map_err(|e| e.to_string())?;
    Ok(state.player.snapshot())
}

#[tauri::command]
pub fn pause(state: tauri::State<'_, AppState>) -> Result<PlayerState, String> {
    tracing::info!("Pause command received");
    state.player.pause().map_err(|e| e.to_string())?;
    Ok(state.player.snapshot())
}

#[tauri::command]
pub fn resume(state: tauri::State<'_, AppState>) -> Result<PlayerState, String> {
    tracing::info!("Resume command received");
    state.player.resume().map_err(|e| e.to_string())?;
    Ok(state.player.snapshot())
}

#[tauri::command]
pub fn seek(
    state: tauri::State<'_, AppState>,
    request: SeekRequest,
) -> Result<PlayerState, String> {
    tracing::info!(position_ms = request.position_ms, "Seek command received");
    state
        .player
        .seek(request.position_ms)
        .map_err(|e| e.to_string())?;
    Ok(state.player.snapshot())
}

#[tauri::command]
pub fn set_volume(
    state: tauri::State<'_, AppState>,
    request: VolumeRequest,
) -> Result<PlayerState, String> {
    tracing::info!(volume = request.volume, "Set volume command received");
    state
        .player
        .set_volume(request.volume)
        .map_err(|e| e.to_string())?;
    Ok(state.player.snapshot())
}

#[tauri::command]
pub fn stop(state: tauri::State<'_, AppState>) -> Result<PlayerState, String> {
    tracing::info!("Stop command received");
    state.player.stop().map_err(|e| e.to_string())?;
    Ok(state.player.snapshot())
}

#[tauri::command]
pub async fn get_library_summary(
    state: tauri::State<'_, AppState>,
) -> Result<LibrarySummary, String> {
    state
        .catalog
        .read()
        .await
        .get_library_summary()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_recordings(
    state: tauri::State<'_, AppState>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<RecordingRow>, String> {
    let limit = limit.unwrap_or(200) as usize;
    let offset = offset.unwrap_or(0) as usize;
    let all = state
        .catalog
        .read()
        .await
        .list_recordings()
        .await
        .map_err(|e| e.to_string())?;
    Ok(all.into_iter().skip(offset).take(limit).collect())
}

#[tauri::command]
pub async fn list_artists(state: tauri::State<'_, AppState>) -> Result<Vec<ArtistRow>, String> {
    state
        .catalog
        .read()
        .await
        .list_artists()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_release_groups(
    state: tauri::State<'_, AppState>,
    artist_id: Option<String>,
    search: Option<String>,
) -> Result<Vec<ReleaseGroupRow>, String> {
    let search = search.as_deref().map(str::trim).filter(|v| !v.is_empty());
    state
        .catalog
        .read()
        .await
        .list_release_groups(artist_id.as_deref(), search)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_waveform(
    state: tauri::State<'_, AppState>,
    source_id: String,
) -> Result<Vec<f32>, String> {
    let mut conn = state
        .db
        .acquire(format!("command.get_waveform source_id={source_id}"))
        .await
        .map_err(|e| e.to_string())?;
    let file_path: Option<String> = sqlx::query_scalar(
        "SELECT file_path FROM source
         WHERE id = ? AND source_type = 'local_file' AND file_path IS NOT NULL",
    )
    .bind(&source_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| e.to_string())?
    .flatten();

    let Some(path) = file_path else {
        return Err("No file found for this recording".to_string());
    };

    let data = tokio::task::spawn_blocking(move || {
        crate::waveform::compute_waveform(
            std::path::Path::new(&path),
            crate::waveform::WAVEFORM_RESOLUTION,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    Ok(data)
}

#[tauri::command]
pub async fn get_cover_art(
    state: tauri::State<'_, AppState>,
    source_id: String,
) -> Result<Option<String>, String> {
    let mut conn = state
        .db
        .acquire(format!("command.get_cover_art source_id={source_id}"))
        .await
        .map_err(|e| e.to_string())?;
    let file_path: Option<String> = sqlx::query_scalar(
        "SELECT file_path FROM source
         WHERE id = ? AND source_type = 'local_file' AND file_path IS NOT NULL",
    )
    .bind(&source_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| e.to_string())?
    .flatten();

    let Some(path) = file_path else {
        return Ok(None);
    };

    crate::library::scanner::extract_cover_art(std::path::Path::new(&path))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_all_tags(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    state
        .catalog
        .read()
        .await
        .list_all_tags()
        .await
        .map_err(|e| e.to_string())
}

// ── Smart playlists ───────────────────────────────────────────────────────────

#[tauri::command]
pub async fn evaluate_smart_playlist(
    state: tauri::State<'_, AppState>,
    query: String,
) -> Result<SmartPlaylistResult, String> {
    state
        .catalog
        .read()
        .await
        .evaluate_smart_playlist(&query)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_playlists(state: tauri::State<'_, AppState>) -> Result<Vec<PlaylistRow>, String> {
    let mut conn = state
        .db
        .acquire("command.list_playlists")
        .await
        .map_err(|e| e.to_string())?;
    let rows = sqlx::query("SELECT id, name, kind, query FROM playlist ORDER BY name")
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;

    Ok(rows
        .into_iter()
        .map(|row| PlaylistRow {
            id: row.get("id"),
            name: row.get("name"),
            kind: row.get("kind"),
            query: row.get("query"),
        })
        .collect())
}

#[tauri::command]
pub async fn save_smart_playlist(
    state: tauri::State<'_, AppState>,
    request: SaveSmartPlaylistRequest,
) -> Result<PlaylistRow, String> {
    // Validate the query parses before saving
    query::parse(&request.query).map_err(|e| format!("Invalid query: {e}"))?;

    let name = request.name.trim();
    if name.is_empty() {
        return Err("Playlist name cannot be empty.".to_string());
    }

    let mut conn = state
        .db
        .acquire(format!("command.save_smart_playlist name={name}"))
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO playlist (name, kind, query)
         VALUES (?, 'smart', ?)
         ON CONFLICT(name) DO UPDATE SET query = excluded.query",
    )
    .bind(name)
    .bind(&request.query)
    .execute(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let row = sqlx::query("SELECT id, name, kind, query FROM playlist WHERE name = ?")
        .bind(name)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;

    Ok(PlaylistRow {
        id: row.get("id"),
        name: row.get("name"),
        kind: row.get("kind"),
        query: row.get("query"),
    })
}

#[tauri::command]
pub async fn delete_playlist(state: tauri::State<'_, AppState>, id: i64) -> Result<(), String> {
    let mut conn = state
        .db
        .acquire(format!("command.delete_playlist id={id}"))
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("DELETE FROM playlist WHERE id = ?")
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ── Last.fm scrobbling ─────────────────────────────────────────────────────────

pub(crate) async fn load_lastfm_config(
    db: &crate::db::DbPool,
) -> Result<crate::lastfm::LastFmConfig, String> {
    let mut conn = db
        .acquire("lastfm.load_config")
        .await
        .map_err(|e| format!("DB error loading Last.fm config: {e}"))?;

    let api_key: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_config WHERE key = 'lastfm_api_key'")
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| format!("DB query error for lastfm_api_key: {e}"))?;

    let shared_secret: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_config WHERE key = 'lastfm_shared_secret'")
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| format!("DB query error for lastfm_shared_secret: {e}"))?;

    match (api_key, shared_secret) {
        (Some(api_key), Some(shared_secret)) => {
            tracing::debug!("Loaded Last.fm config from app_config");
            Ok(crate::lastfm::LastFmConfig {
                api_key,
                shared_secret,
            })
        }
        _ => {
            tracing::warn!(
                "Last.fm not configured — missing api_key or shared_secret in app_config"
            );
            Err(
                "Last.fm not configured. Enter your API key and shared secret in Settings."
                    .to_string(),
            )
        }
    }
}

/// Save the Last.fm API key and shared secret to app_config.
#[tauri::command]
pub async fn save_lastfm_credentials(
    state: tauri::State<'_, AppState>,
    api_key: String,
    shared_secret: String,
) -> Result<(), String> {
    tracing::info!("Saving Last.fm credentials");
    if api_key.trim().is_empty() || shared_secret.trim().is_empty() {
        tracing::warn!("Attempted to save empty Last.fm credentials");
        return Err("API key and shared secret must not be empty.".to_string());
    }

    let mut conn = state
        .db
        .acquire("lastfm.save_credentials")
        .await
        .map_err(|e| format!("DB error saving Last.fm credentials: {e}"))?;

    let mut tx = conn
        .begin()
        .await
        .map_err(|e| format!("Failed to start transaction: {e}"))?;

    sqlx::query(
        "INSERT INTO app_config (key, value) VALUES ('lastfm_api_key', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(api_key.trim())
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("Failed to save lastfm_api_key: {e}"))?;

    sqlx::query(
        "INSERT INTO app_config (key, value) VALUES ('lastfm_shared_secret', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(shared_secret.trim())
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("Failed to save lastfm_shared_secret: {e}"))?;

    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit transaction: {e}"))?;
    tracing::info!("Last.fm credentials saved successfully");
    Ok(())
}

/// Returns the current Last.fm integration status.
#[tauri::command]
pub async fn get_lastfm_status(state: tauri::State<'_, AppState>) -> Result<LastFmStatus, String> {
    tracing::debug!("get_lastfm_status called");
    let mut conn = state
        .db
        .acquire("lastfm.get_status")
        .await
        .map_err(|e| format!("DB error getting Last.fm status: {e}"))?;

    let api_key: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_config WHERE key = 'lastfm_api_key'")
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| format!("DB query error for lastfm_api_key: {e}"))?;
    let configured = api_key.is_some();

    let session_key: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_config WHERE key = 'lastfm_session_key'")
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| format!("DB query error for lastfm_session_key: {e}"))?;

    let username: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_config WHERE key = 'lastfm_username'")
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| format!("DB query error for lastfm_username: {e}"))?;

    tracing::debug!(
        configured,
        logged_in = session_key.is_some(),
        "Last.fm status"
    );
    Ok(LastFmStatus {
        configured,
        logged_in: session_key.is_some(),
        username,
    })
}

/// Generate a Last.fm auth token and return the URL the user must visit to authorize the app.
/// Stores the token in `state.lastfm_auth_token` for the subsequent `lastfm_complete_auth` call.
#[tauri::command]
pub async fn lastfm_get_auth_url(
    state: tauri::State<'_, AppState>,
) -> Result<LastFmAuthUrl, String> {
    tracing::info!("lastfm_get_auth_url called");
    let config = load_lastfm_config(&state.db).await?;

    let token = crate::lastfm::get_token(&config).await?;
    tracing::info!(token_prefix = %token.chars().take(8).collect::<String>(), "Obtained Last.fm auth token");

    // Store the token in memory for the completion step
    *state.lastfm_auth_token.lock().map_err(|e| e.to_string())? = Some(token.clone());

    let url = format!(
        "https://www.last.fm/api/auth/?api_key={}&token={}",
        config.api_key, token
    );
    tracing::info!("Returning Last.fm auth URL");
    Ok(LastFmAuthUrl { url })
}

/// Complete the Last.fm authentication by exchanging the previously-obtained token for a session key.
/// Persists the session key and username to app_config.
#[tauri::command]
pub async fn lastfm_complete_auth(state: tauri::State<'_, AppState>) -> Result<String, String> {
    tracing::info!("lastfm_complete_auth called");
    let config = load_lastfm_config(&state.db).await?;

    let token = state
        .lastfm_auth_token
        .lock()
        .map_err(|e| {
            tracing::error!("Failed to lock lastfm_auth_token mutex: {e}");
            e.to_string()
        })?
        .take()
        .ok_or_else(|| {
            tracing::warn!(
                "No pending auth token found — lastfm_get_auth_url must be called first"
            );
            "No pending auth request. Call lastfm_get_auth_url first.".to_string()
        })?;

    tracing::info!(token_prefix = %token.chars().take(8).collect::<String>(), "Exchanging token for session");
    let (session_key, username) = crate::lastfm::get_session(&config, &token).await?;
    tracing::info!(%username, "Session obtained, persisting to app_config");

    let mut conn = state
        .db
        .acquire("lastfm.complete_auth")
        .await
        .map_err(|e| format!("DB error on complete_auth: {e}"))?;

    let mut tx = conn
        .begin()
        .await
        .map_err(|e| format!("Failed to start transaction: {e}"))?;

    sqlx::query(
        "INSERT INTO app_config (key, value) VALUES ('lastfm_session_key', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(&session_key)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("Failed to save session_key: {e}"))?;

    sqlx::query(
        "INSERT INTO app_config (key, value) VALUES ('lastfm_username', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(&username)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("Failed to save username: {e}"))?;

    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit transaction: {e}"))?;

    tracing::info!(%username, "Last.fm auth complete — user is now logged in");
    Ok(username)
}

/// Disconnect from Last.fm by removing the stored session key and username.
#[tauri::command]
pub async fn lastfm_disconnect(state: tauri::State<'_, AppState>) -> Result<(), String> {
    tracing::info!("lastfm_disconnect called");
    let mut conn = state
        .db
        .acquire("lastfm.disconnect")
        .await
        .map_err(|e| format!("DB error disconnecting: {e}"))?;

    sqlx::query("DELETE FROM app_config WHERE key IN ('lastfm_session_key', 'lastfm_username')")
        .execute(&mut *conn)
        .await
        .map_err(|e| format!("Failed to remove session data: {e}"))?;

    tracing::info!("Last.fm session cleared");
    Ok(())
}

/// Love (heart) a track on Last.fm.
#[tauri::command]
pub async fn lastfm_love_track(
    state: tauri::State<'_, AppState>,
    request: LastFmLoveTrackRequest,
) -> Result<(), String> {
    tracing::info!(artist = %request.artist, track = %request.track, "lastfm_love_track called");
    let config = load_lastfm_config(&state.db).await?;
    let session_key = load_lastfm_session_key(&state.db).await?;

    let result =
        crate::lastfm::love_track(&config, &session_key, &request.artist, &request.track).await;
    if let Err(ref e) = result {
        tracing::warn!("Love track failed: {e}");
    }
    result
}

/// Check whether the current user has loved a track on Last.fm.
#[tauri::command]
pub async fn lastfm_get_track_loved(
    state: tauri::State<'_, AppState>,
    request: LastFmGetTrackLovedRequest,
) -> Result<bool, String> {
    tracing::info!(artist = %request.artist, track = %request.track, "lastfm_get_track_loved called");
    let config = load_lastfm_config(&state.db).await?;
    let session_key = load_lastfm_session_key(&state.db).await?;
    crate::lastfm::get_track_loved(&config, &session_key, &request.artist, &request.track).await
}

/// Update the "now playing" status on Last.fm for the currently playing track.
#[tauri::command]
pub async fn lastfm_now_playing(
    state: tauri::State<'_, AppState>,
    request: LastFmNowPlayingRequest,
) -> Result<(), String> {
    tracing::info!(artist = %request.artist, track = %request.track, album = ?request.album, "lastfm_now_playing called");
    let config = load_lastfm_config(&state.db).await?;
    let session_key = load_lastfm_session_key(&state.db).await?;

    let result = crate::lastfm::now_playing(
        &config,
        &session_key,
        &request.artist,
        &request.track,
        request.album.as_deref(),
    )
    .await;
    if let Err(ref e) = result {
        tracing::warn!("Now playing update failed: {e}");
    }
    result
}

/// Scrobble a track to Last.fm. `timestamp` is the UNIX second when the track started playing.
#[tauri::command]
pub async fn lastfm_scrobble(
    state: tauri::State<'_, AppState>,
    request: LastFmScrobbleRequest,
) -> Result<(), String> {
    tracing::info!(artist = %request.artist, track = %request.track, timestamp = %request.timestamp, "lastfm_scrobble called");
    let config = load_lastfm_config(&state.db).await?;
    let session_key = load_lastfm_session_key(&state.db).await?;

    let result = crate::lastfm::scrobble(
        &config,
        &session_key,
        &request.artist,
        &request.track,
        request.album.as_deref(),
        request.timestamp,
    )
    .await;
    if let Err(ref e) = result {
        tracing::warn!("Scrobble failed: {e}");
    }
    result
}

pub(crate) async fn load_lastfm_session_key(db: &crate::db::DbPool) -> Result<String, String> {
    let mut conn = db
        .acquire("lastfm.load_session_key")
        .await
        .map_err(|e| format!("DB error loading session key: {e}"))?;
    sqlx::query_scalar("SELECT value FROM app_config WHERE key = 'lastfm_session_key'")
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| format!("DB query error for session key: {e}"))?
        .ok_or_else(|| {
            tracing::warn!("No Last.fm session key in app_config");
            "Not logged in to Last.fm. Connect in Settings.".to_string()
        })
}

fn validate_rating(stars: Option<i64>) -> Result<(), String> {
    if let Some(stars) = stars {
        if !(1..=5).contains(&stars) {
            return Err("Rating must be between 1 and 5.".to_string());
        }
    }

    Ok(())
}

pub async fn load_music_root(db: &crate::db::DbPool) -> anyhow::Result<Option<String>> {
    let mut conn = db.acquire("config.load_music_root").await?;
    Ok(
        sqlx::query_scalar("SELECT value FROM app_config WHERE key = 'music_root'")
            .fetch_optional(&mut *conn)
            .await?,
    )
}

async fn load_app_config(db: &crate::db::DbPool) -> anyhow::Result<AppConfig> {
    let music_root = load_music_root(db).await?;
    let mut conn = db
        .acquire("config.load_app_config.queue_history_limit")
        .await?;
    let queue_history_limit = sqlx::query_scalar::<_, String>(
        "SELECT value FROM app_config WHERE key = 'queue_history_limit'",
    )
    .fetch_optional(&mut *conn)
    .await?
    .and_then(|value| value.parse::<i64>().ok())
    .unwrap_or(5);

    let external_commands_json: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_config WHERE key = 'external_commands'")
            .fetch_optional(&mut *conn)
            .await?;
    let external_commands = external_commands_json
        .and_then(|json| serde_json::from_str::<Vec<ExternalCommand>>(&json).ok())
        .unwrap_or_default();

    Ok(AppConfig {
        music_root,
        queue_history_limit,
        external_commands,
    })
}

async fn set_config_value(db: &crate::db::DbPool, key: &str, value: &str) -> anyhow::Result<()> {
    let mut conn = db.acquire(format!("config.set key={key}")).await?;
    sqlx::query(
        "INSERT INTO app_config (key, value)
         VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

#[tauri::command]
pub async fn get_artist_detail(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<ArtistDetail, String> {
    state
        .catalog
        .read()
        .await
        .get_artist_detail(&id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Artist {id} not found"))
}

#[tauri::command]
pub async fn get_release_group_detail(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<ReleaseGroupDetail, String> {
    state
        .catalog
        .read()
        .await
        .get_release_group_detail(&id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Release group {id} not found"))
}

#[tauri::command]
pub async fn get_recording_detail(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<RecordingDetail, String> {
    state
        .catalog
        .read()
        .await
        .get_recording_detail(&id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Recording {id} not found"))
}

// ── Artist fix commands ──────────────────────────────────────────────────────

#[tauri::command]
pub async fn check_artist_compound(
    state: tauri::State<'_, AppState>,
    artist_id: String,
) -> Result<CompoundArtistCheck, String> {
    crate::library::artist_fixes::check_artist_compound(&state.catalog, &artist_id).await
}

#[tauri::command]
pub async fn split_recording(
    state: tauri::State<'_, AppState>,
    _recording_id: String,
    source_ids_to_move: Vec<String>,
) -> Result<String, String> {
    if source_ids_to_move.is_empty() {
        return Err("No sources selected to move".to_string());
    }

    let _permit = state
        .write_serializer
        .acquire()
        .await
        .map_err(|e| format!("Failed to acquire write lock for split: {e}"))?;

    // Remove the MBID from each moved source's raw_tags_json so the catalog
    // will group them independently on the next load.
    {
        let mut conn = state
            .db
            .acquire("command.split_recording")
            .await
            .map_err(|e| e.to_string())?;
        let mut tx = conn.begin().await.map_err(|e| e.to_string())?;

        for sid in &source_ids_to_move {
            let raw_json: Option<String> =
                sqlx::query_scalar("SELECT raw_tags_json FROM source WHERE id = ?")
                    .bind(sid)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| e.to_string())?
                    .flatten();

            if let Some(json) = raw_json {
                if let Ok(mut tags) = serde_json::from_str::<Vec<(String, String)>>(&json) {
                    tags.retain(|(k, _)| k != "UFID:http://musicbrainz.org");
                    let new_json = serde_json::to_string(&tags).unwrap_or(json);
                    sqlx::query("UPDATE source SET raw_tags_json = ? WHERE id = ?")
                        .bind(&new_json)
                        .bind(sid)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
        }

        tx.commit().await.map_err(|e| e.to_string())?;
    }

    drop(_permit);
    reload_catalog(&state).await;

    // Return the recording ID that the catalog assigns to one of the moved sources.
    let catalog = state.catalog.read().await;
    let recs = catalog.list_recordings().await.map_err(|e| e.to_string())?;
    drop(catalog);

    if let Some(first_sid) = source_ids_to_move.first() {
        for rec in &recs {
            if rec.primary_source_id.as_deref() == Some(first_sid) {
                return Ok(rec.id.clone());
            }
        }
    }

    // Fallback: return a placeholder; the frontend will refresh from the catalog.
    Ok(uuid::Uuid::new_v4().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn test_compute_normalization_gain_replaygain() {
        let (gain, source) = compute_normalization_gain(Some(-6.0), None);
        let expected = 10.0_f64.powf(-6.0 / 20.0) as f32;
        assert!((gain - expected).abs() < f32::EPSILON);
        assert_eq!(source, "ReplayGain");
    }

    #[test]
    fn test_compute_normalization_gain_lufs() {
        // -16 LUFS = reference level, should give 0 dB gain → 1.0x
        let (gain, source) = compute_normalization_gain(None, Some(-16.0));
        assert!((gain - 1.0).abs() < f32::EPSILON);
        assert_eq!(source, "LUFS");
    }

    #[test]
    fn test_compute_normalization_gain_lufs_offset() {
        // -20 LUFS is quieter than reference, should get +4 dB → ~1.58x
        let (gain, source) = compute_normalization_gain(None, Some(-20.0));
        let expected = 10.0_f64.powf(4.0 / 20.0) as f32;
        assert!((gain - expected).abs() < 0.001);
        assert_eq!(source, "LUFS");
    }

    #[test]
    fn test_compute_normalization_gain_none() {
        let (gain, source) = compute_normalization_gain(None, None);
        assert!((gain - 1.0).abs() < f32::EPSILON);
        assert_eq!(source, "None");
    }

    #[test]
    fn test_compute_normalization_gain_replaygain_over_lufs() {
        // When both are present, ReplayGain takes priority
        let (gain, source) = compute_normalization_gain(Some(-6.0), Some(-20.0));
        let expected = 10.0_f64.powf(-6.0 / 20.0) as f32;
        assert!((gain - expected).abs() < f32::EPSILON);
        assert_eq!(source, "ReplayGain");
    }

    #[test]
    fn test_compute_normalization_gain_clamp_low() {
        // +20 dB → 10.0x, but let's test negative: -40 dB → 0.01x, clamped to 0.1
        let (gain, _) = compute_normalization_gain(Some(-120.0), None);
        assert!((gain - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn test_compute_normalization_gain_clamp_high() {
        // +40 dB → 100x, clamped to 10.0
        let (gain, _) = compute_normalization_gain(Some(40.0), None);
        assert!((gain - 10.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_compute_normalization_gain_zero_db() {
        // 0 dB → 1.0x
        let (gain, source) = compute_normalization_gain(Some(0.0), None);
        assert!((gain - 1.0).abs() < f32::EPSILON);
        assert_eq!(source, "ReplayGain");
    }
}
