use crate::audio::PlayRequest as EnginePlayRequest;
use crate::db::DbPool;
use crate::file_issues::FileIssue;
use crate::library::import::{
    fix_duplicate_track_entries, import_paths as do_import, prune_library,
    rescan_source as do_rescan_source,
};
use crate::models::{
    AppBootstrap, AppConfig, ArtistDetail, ArtistFixStats, ArtistRow, CompoundArtistCheck,
    DbPoolDebugSnapshot, ExternalCommand, FixMergedRecordingsStats, Id3FrameDebugInfo,
    Id3FrameDebugRequest, ImportProgress, ImportStats, InitialSetupRequest, LibrarySummary,
    PlayHistoryInput, PlayRequest, PlayerState, PlaylistRow, QueueSettingsUpdate,
    RatingUpdateRequest, RecordingDetail, RecordingRatingUpdateResult, RecordingRow,
    ReleaseGroupDetail, ReleaseGroupRow, SaveSmartPlaylistRequest, SeekRequest,
    SmartPlaylistResult, VolumeRequest,
};
use crate::query;
use crate::storage::CatalogReader;
use crate::AppState;
use serde::Serialize;
use sqlx::{Acquire, Row, Sqlite, Transaction};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use tauri::Emitter;

#[derive(Clone, Serialize)]
struct JobUpdate {
    remaining: i64,
    job_type: String,
}

#[derive(Clone, Serialize)]
struct RecordingUpdatedEvent {
    recording_ids: Vec<String>,
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
        false,
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
///
/// Returns a map of recording_id → set of release_group_ids that the rescanned
/// sources' files currently assert. Callers that need to detect stale tracks
/// (e.g., `rescan_sources_for_release_group`) can use this to identify tracks
/// on release groups that no source still claims.
async fn rescan_path_list(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<HashMap<String, HashSet<String>>, String> {
    let count = paths.len() as i64;
    state.pending_jobs.fetch_add(count, Ordering::Relaxed);
    emit_job_update(app, state, "rescan");

    let mut failures = Vec::new();
    let mut assertions: HashMap<String, HashSet<String>> = HashMap::new();
    for path in paths {
        let source_path = std::path::Path::new(&path);
        if !source_path.is_file() {
            failures.push(format!("{}: Source file is missing", source_path.display()));
        } else {
            match do_rescan_source(
                &state.db,
                source_path,
                state.acoustid_api_key.as_deref(),
                &state.write_serializer,
                true,
                &state.file_issues,
            )
            .await
            {
                Ok((recording_id, release_group_id)) => {
                    assertions
                        .entry(recording_id)
                        .or_default()
                        .insert(release_group_id);
                }
                Err(error) => {
                    failures.push(format!("{}:\n{error:#}", source_path.display()));
                }
            }
        }
        state.pending_jobs.fetch_sub(1, Ordering::Relaxed);
        emit_job_update(app, state, "rescan");
    }

    reload_catalog(state).await;

    // Single prune pass after the batch, regardless of individual failures.
    if let Err(e) = prune_library(&state.db).await {
        tracing::warn!("Library pruning after batch rescan failed: {e}");
    }

    // Single duplicate-track cleanup pass after the batch.
    if let Err(e) = fix_duplicate_track_entries(&state.db).await {
        tracing::warn!("Duplicate track cleanup after batch rescan failed: {e}");
    }

    // Notify the frontend that affected recordings may have changed releases.
    let recording_ids: Vec<String> = assertions.keys().cloned().collect();
    if !recording_ids.is_empty() {
        let _ = app.emit(
            "recording-releases-updated",
            RecordingUpdatedEvent { recording_ids },
        );
    }

    if failures.is_empty() {
        Ok(assertions)
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
        let mut conn = state
            .db
            .acquire(format!(
                "command.rescan_sources_for_artist artist_id={artist_id}"
            ))
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query_scalar(
            "SELECT DISTINCT s.file_path
             FROM source s
             JOIN recording_artist ra ON ra.recording_id = s.recording_id
             WHERE ra.artist_id = ?
               AND s.source_type = 'local_file'
               AND s.file_path IS NOT NULL
             ORDER BY s.file_path",
        )
        .bind(&artist_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| e.to_string())?
    };

    if paths.is_empty() {
        return Err("No local sources found for this artist.".to_string());
    }

    rescan_path_list(&app, &state, paths).await?;
    Ok(())
}

#[tauri::command]
pub async fn rescan_sources_for_release_group(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    release_group_id: String,
) -> Result<(), String> {
    let paths: Vec<String> = {
        let mut conn = state
            .db
            .acquire(format!(
                "command.rescan_sources_for_release_group release_group_id={release_group_id}"
            ))
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query_scalar(
            "SELECT DISTINCT s.file_path
             FROM source s
             JOIN track t ON t.recording_id = s.recording_id
             JOIN medium m ON m.id = t.medium_id
             JOIN release rel ON rel.id = m.release_id
             WHERE rel.release_group_id = ?
               AND s.source_type = 'local_file'
               AND s.file_path IS NOT NULL
             ORDER BY s.file_path",
        )
        .bind(&release_group_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| e.to_string())?
    };

    if paths.is_empty() {
        return Err("No local sources found for this album.".to_string());
    }

    let assertions = rescan_path_list(&app, &state, paths).await?;

    // After all rescans, recordings on the target release group may have
    // stale tracks left behind from prior imports that used different ID3
    // metadata.  For each recording whose rescanned sources no longer
    // assert the target release group, remove its track from that group.
    // (The recordings themselves and their tracks on other, still-asserted
    // release groups are preserved.)
    let mut prune_again = false;
    for (recording_id, asserted_groups) in &assertions {
        if !asserted_groups.contains(&release_group_id) {
            let mut conn = state
                .db
                .acquire(format!(
                    "command.rescan_sources_for_release_group.cleanup recording_id={recording_id}"
                ))
                .await
                .map_err(|e| e.to_string())?;
            let deleted = sqlx::query(
                "DELETE FROM track
                 WHERE recording_id = ?
                   AND id IN (
                       SELECT t.id FROM track t
                       JOIN medium m ON m.id = t.medium_id
                       JOIN release rel ON rel.id = m.release_id
                       WHERE t.recording_id = ?
                         AND rel.release_group_id = ?
                   )",
            )
            .bind(recording_id)
            .bind(recording_id)
            .bind(&release_group_id)
            .execute(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
            if deleted.rows_affected() > 0 {
                prune_again = true;
            }
        }
    }

    if prune_again {
        if let Err(e) = prune_library(&state.db).await {
            tracing::warn!("Library pruning after stale-track cleanup failed: {e}");
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn rescan_sources_for_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    recording_id: String,
) -> Result<(), String> {
    let paths: Vec<String> = {
        let mut conn = state
            .db
            .acquire(format!(
                "command.rescan_sources_for_recording recording_id={recording_id}"
            ))
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query_scalar(
            "SELECT DISTINCT s.file_path
             FROM source s
             WHERE s.recording_id = ?
               AND s.source_type = 'local_file'
               AND s.file_path IS NOT NULL
             ORDER BY s.file_path",
        )
        .bind(&recording_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| e.to_string())?
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
            "command.record_play_history recording_id={}",
            input.recording_id
        ))
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO play_history (recording_id, source_id, duration_played_ms)
         VALUES (?, ?, ?)",
    )
    .bind(input.recording_id)
    .bind(input.source_id)
    .bind(input.duration_played_ms)
    .execute(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    reload_catalog(&state).await;
    Ok(())
}

#[tauri::command]
pub async fn set_recording_rating(
    state: tauri::State<'_, AppState>,
    request: RatingUpdateRequest,
) -> Result<RecordingRatingUpdateResult, String> {
    validate_rating(request.stars)?;

    let mut conn = state
        .db
        .acquire(format!(
            "command.set_recording_rating recording_id={}",
            request.id
        ))
        .await
        .map_err(|e| e.to_string())?;

    if let Some(stars) = request.stars {
        sqlx::query(
            "INSERT INTO user_rating (recording_id, stars, updated_at)
             VALUES (?, ?, datetime('now'))
             ON CONFLICT(recording_id) DO UPDATE SET
                stars = excluded.stars,
                updated_at = datetime('now')",
        )
        .bind(&request.id)
        .bind(stars)
        .execute(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;
    } else {
        sqlx::query("DELETE FROM user_rating WHERE recording_id = ?")
            .bind(&request.id)
            .execute(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
    }

    let release_groups = sqlx::query(
        "WITH affected_release_groups AS (
            SELECT DISTINCT rel.release_group_id AS id
            FROM track t
            JOIN medium m
                ON m.id = t.medium_id
            JOIN release rel
                ON rel.id = m.release_id
            WHERE t.recording_id = ?
         )
         SELECT
            affected_release_groups.id AS id,
            (
                SELECT AVG(track_ratings.stars)
                FROM (
                    SELECT DISTINCT t2.recording_id, ur2.stars
                    FROM release rel2
                    JOIN medium m2
                        ON m2.release_id = rel2.id
                    JOIN track t2
                        ON t2.medium_id = m2.id
                    JOIN user_rating ur2
                        ON ur2.recording_id = t2.recording_id
                    WHERE rel2.release_group_id = affected_release_groups.id
                ) AS track_ratings
            ) AS rating
         FROM affected_release_groups",
    )
    .bind(&request.id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?
    .into_iter()
    .map(|row| crate::models::EntityRatingUpdate {
        id: row.get("id"),
        rating: row.get("rating"),
    })
    .collect();

    let artists = sqlx::query(
        "WITH affected_artists AS (
            SELECT DISTINCT artist_id AS id
            FROM recording_artist
            WHERE recording_id = ?
         )
         SELECT
            affected_artists.id AS id,
            AVG(ur.stars)       AS rating
         FROM affected_artists
         LEFT JOIN recording_artist ra
             ON ra.artist_id = affected_artists.id
         LEFT JOIN user_rating ur
             ON ur.recording_id = ra.recording_id
         GROUP BY affected_artists.id",
    )
    .bind(&request.id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?
    .into_iter()
    .map(|row| crate::models::EntityRatingUpdate {
        id: row.get("id"),
        rating: row.get("rating"),
    })
    .collect();

    reload_catalog(&state).await;
    Ok(RecordingRatingUpdateResult {
        release_groups,
        artists,
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
pub fn get_file_issues(state: tauri::State<'_, AppState>) -> Result<Vec<FileIssue>, String> {
    Ok(state.file_issues.all())
}

/// Query the database for sources whose recording has no track entries and
/// push each one into the in-memory issue log.
#[tauri::command]
pub async fn find_orphan_sources(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<FileIssue>, String> {
    let pool = state.db.raw_pool();
    let rows = sqlx::query_as::<_, (String, String, String, Option<String>)>(
        "SELECT s.id, s.recording_id, s.file_path, r.title
         FROM source s
         JOIN recording r ON r.id = s.recording_id
         WHERE s.source_type = 'local_file'
           AND s.file_path IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM track t WHERE t.recording_id = s.recording_id)",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to query orphan sources: {e}"))?;

    let file_issues = state.file_issues.clone();
    let mut result = Vec::new();
    for (source_id, recording_id, file_path, title) in &rows {
        let msg = format!(
            "Recording \"{}\" has no album track — source is orphaned",
            title.as_deref().unwrap_or("unknown")
        );
        file_issues.push_orphan_source(file_path, &msg, source_id, recording_id);
        result.push(FileIssue {
            file_path: file_path.clone(),
            kind: crate::file_issues::FileIssueKind::OrphanSource,
            message: msg,
            source_id: Some(source_id.clone()),
            recording_id: Some(recording_id.clone()),
            frame_id: None,
            field_name: None,
            lofty_value: None,
            corrected_value: None,
        });
    }
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
        false,
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

/// Resolve a duplicate frame issue by applying the user's chosen value to the
/// database and removing the issue from the in-memory log.
///
/// The auto-correction during scanning already applied the first-tag value
/// (corrected_value). If the user chose that value, this is a no-op DB-wise.
/// If they chose the lofty (last-tag) value, we do a targeted database update.
#[tauri::command]
pub async fn resolve_duplicate_frame(
    state: tauri::State<'_, AppState>,
    file_path: String,
    frame_id: String,
    chosen_value: String,
) -> Result<(), String> {
    // Find the issue so we can compare chosen vs corrected vs lofty values.
    let issue = {
        let issues = state.file_issues.all();
        issues
            .iter()
            .find(|i| {
                i.kind == crate::file_issues::FileIssueKind::DuplicateFrame
                    && i.file_path == file_path
                    && i.frame_id.as_deref() == Some(&frame_id)
            })
            .cloned()
            .ok_or_else(|| format!("No duplicate frame issue for {frame_id} in {file_path}"))?
    };

    let corrected = issue.corrected_value.as_deref().unwrap_or("");
    let lofty = issue.lofty_value.as_deref().unwrap_or("");

    // Only do DB work if the user picked the lofty value over the auto-corrected one.
    if chosen_value == lofty && chosen_value != corrected {
        let pool = state.db.raw_pool();
        let recording_id: String = sqlx::query_scalar(
            "SELECT recording_id FROM source WHERE file_path = ? AND source_type = 'local_file'",
        )
        .bind(&file_path)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("DB error: {e}"))?
        .flatten()
        .ok_or_else(|| format!("Source not found: {file_path}"))?;

        match frame_id.as_str() {
            "TIT2" => {
                sqlx::query("UPDATE recording SET title = ? WHERE id = ?")
                    .bind(&chosen_value)
                    .bind(&recording_id)
                    .execute(pool)
                    .await
                    .map_err(|e| format!("Failed to update recording title: {e}"))?;
            }
            "TPE1" => {
                // Replace the primary artist for this recording.
                let mut conn = pool
                    .acquire()
                    .await
                    .map_err(|e| format!("DB acquire error: {e}"))?;
                let mut tx = conn
                    .begin()
                    .await
                    .map_err(|e| format!("DB begin error: {e}"))?;
                let artist_id =
                    crate::library::import::get_or_create_artist(&mut tx, &chosen_value)
                        .await
                        .map_err(|e| format!("Artist error: {e}"))?;
                sqlx::query("DELETE FROM recording_artist WHERE recording_id = ?")
                    .bind(&recording_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| format!("Failed to clear recording artists: {e}"))?;
                sqlx::query(
                    "INSERT INTO recording_artist (recording_id, artist_id, position, role) \
                     VALUES (?, ?, 0, 'main')",
                )
                .bind(&recording_id)
                .bind(&artist_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("Failed to insert recording artist: {e}"))?;
                tx.commit()
                    .await
                    .map_err(|e| format!("Failed to commit artist change: {e}"))?;
            }
            "TPE2" => {
                // Replace the album artist on all release groups that contain this recording.
                let mut conn = pool
                    .acquire()
                    .await
                    .map_err(|e| format!("DB acquire error: {e}"))?;
                let mut tx = conn
                    .begin()
                    .await
                    .map_err(|e| format!("DB begin error: {e}"))?;
                let artist_id =
                    crate::library::import::get_or_create_artist(&mut tx, &chosen_value)
                        .await
                        .map_err(|e| format!("Artist error: {e}"))?;
                let release_groups: Vec<String> = sqlx::query_scalar(
                    "SELECT DISTINCT rg.id
                     FROM track t
                     JOIN medium m  ON m.id = t.medium_id
                     JOIN release r ON r.id = m.release_id
                     JOIN release_group rg ON rg.id = r.release_group_id
                     WHERE t.recording_id = ?",
                )
                .bind(&recording_id)
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| format!("Failed to find release groups: {e}"))?;
                for rg_id in &release_groups {
                    // Delete existing and re-insert at position 0
                    sqlx::query("DELETE FROM release_group_artist WHERE release_group_id = ?")
                        .bind(rg_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| format!("Failed to clear release group artists: {e}"))?;
                    sqlx::query(
                        "INSERT INTO release_group_artist (release_group_id, artist_id, position, role) \
                         VALUES (?, ?, 0, 'main')",
                    )
                    .bind(rg_id)
                    .bind(&artist_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| format!("Failed to insert release group artist: {e}"))?;
                }
                tx.commit()
                    .await
                    .map_err(|e| format!("Failed to commit album artist change: {e}"))?;
            }
            "TALB" => {
                // Update the album title on all release groups + releases containing
                // this recording.
                let mut conn = pool
                    .acquire()
                    .await
                    .map_err(|e| format!("DB acquire error: {e}"))?;
                let mut tx = conn
                    .begin()
                    .await
                    .map_err(|e| format!("DB begin error: {e}"))?;
                let release_groups: Vec<String> = sqlx::query_scalar(
                    "SELECT DISTINCT rg.id
                     FROM track t
                     JOIN medium m  ON m.id = t.medium_id
                     JOIN release r ON r.id = m.release_id
                     JOIN release_group rg ON rg.id = r.release_group_id
                     WHERE t.recording_id = ?",
                )
                .bind(&recording_id)
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| format!("Failed to find release groups: {e}"))?;
                for rg_id in &release_groups {
                    sqlx::query("UPDATE release_group SET title = ? WHERE id = ?")
                        .bind(&chosen_value)
                        .bind(rg_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| format!("Failed to update release group title: {e}"))?;
                    sqlx::query("UPDATE release SET title = ? WHERE release_group_id = ?")
                        .bind(&chosen_value)
                        .bind(rg_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| format!("Failed to update release title: {e}"))?;
                }
                tx.commit()
                    .await
                    .map_err(|e| format!("Failed to commit album title change: {e}"))?;
            }
            other => return Err(format!("Unsupported frame ID: {other}")),
        }
    }

    // Remove the resolved issue from the in-memory log.
    state.file_issues.retain(|issue| {
        !(issue.kind == crate::file_issues::FileIssueKind::DuplicateFrame
            && issue.file_path == file_path
            && issue.frame_id.as_deref() == Some(&frame_id))
    });

    reload_catalog(&state).await;
    Ok(())
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
            r.id                  AS recording_id,
            r.title               AS title,
            COALESCE(ra.credited_as, a.name) AS artist,
            s.replay_gain_track_db,
            s.lufs
         FROM source s
         JOIN recording r ON r.id = s.recording_id
         LEFT JOIN recording_artist ra ON ra.recording_id = r.id AND ra.position = 0
         LEFT JOIN artist a ON a.id = ra.artist_id
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
    let recording_id: String = row.get("recording_id");
    let title: Option<String> = row.get("title");
    let artist: Option<String> = row.get("artist");
    let replay_gain_track_db: Option<f64> = row.get("replay_gain_track_db");
    let lufs: Option<f64> = row.get("lufs");

    // Compute the linear normalization gain and source label.
    let (mut normalization_gain, mut normalization_source) =
        compute_normalization_gain(replay_gain_track_db, lufs);

    // If the file is missing, try to fall back to another source for the same recording.
    if !std::path::Path::new(&file_path).exists() {
        tracing::warn!(
            source_id = %source_id,
            path = %file_path,
            "Source file missing; searching for alternatives"
        );

        let alts = sqlx::query(
            "SELECT s.id, s.file_path, s.replay_gain_track_db, s.lufs FROM source s
             WHERE s.recording_id = ?
               AND s.id != ?
               AND s.source_type = 'local_file'
               AND s.file_path IS NOT NULL
             ORDER BY s.file_path",
        )
        .bind(&recording_id)
        .bind(&source_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;

        let mut found_alt: Option<(String, String, Option<f64>, Option<f64>)> = None;
        for alt in &alts {
            let alt_id: String = alt.get("id");
            let alt_path: String = alt.get("file_path");
            if std::path::Path::new(&alt_path).exists() {
                let alt_rg: Option<f64> = alt.get("replay_gain_track_db");
                let alt_lufs: Option<f64> = alt.get("lufs");
                found_alt = Some((alt_id, alt_path, alt_rg, alt_lufs));
                break;
            }
        }

        if let Some((alt_id, alt_path, alt_rg, alt_lufs)) = found_alt {
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
            // Update normalization gain from the alternative source.
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
        recording_id = %recording_id,
        path = %file_path,
        "Resolved play request"
    );

    state
        .player
        .play(EnginePlayRequest {
            recording_id,
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
pub async fn prune_empty_library_entities_command(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut tx = state
        .db
        .raw_pool()
        .begin()
        .await
        .map_err(|e| e.to_string())?;

    prune_empty_library_entities(&mut tx)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_waveform(
    state: tauri::State<'_, AppState>,
    recording_id: String,
) -> Result<Vec<f32>, String> {
    let mut conn = state
        .db
        .acquire(format!("command.get_waveform recording_id={recording_id}"))
        .await
        .map_err(|e| e.to_string())?;
    let file_path: Option<String> = sqlx::query_scalar(
        "SELECT file_path FROM source
         WHERE recording_id = ? AND source_type = 'local_file' AND file_path IS NOT NULL
         ORDER BY file_path LIMIT 1",
    )
    .bind(&recording_id)
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
    recording_id: String,
) -> Result<Option<String>, String> {
    let mut conn = state
        .db
        .acquire(format!("command.get_cover_art recording_id={recording_id}"))
        .await
        .map_err(|e| e.to_string())?;
    let file_path: Option<String> = sqlx::query_scalar(
        "SELECT file_path FROM source
         WHERE recording_id = ? AND source_type = 'local_file' AND file_path IS NOT NULL
         ORDER BY file_path LIMIT 1",
    )
    .bind(&recording_id)
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

#[tauri::command]
pub async fn delete_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    state.pending_jobs.fetch_add(1, Ordering::Relaxed);
    emit_job_update(&app, &state, "delete");

    let _permit = state
        .write_serializer
        .acquire()
        .await
        .map_err(|e| format!("Failed to acquire write lock for delete: {e}"))?;

    let result = delete_recording_inner(&state.db, &id).await;
    drop(_permit);

    state.pending_jobs.fetch_sub(1, Ordering::Relaxed);
    emit_job_update(&app, &state, "delete");

    reload_catalog(&state).await;
    result
}

async fn delete_recording_inner(db: &DbPool, id: &str) -> Result<(), String> {
    let mut conn = db
        .acquire(format!("command.delete_recording_inner id={id}"))
        .await
        .map_err(|e| e.to_string())?;
    conn.set_busy_timeout(std::time::Duration::from_secs(30))
        .await
        .map_err(|e| e.to_string())?;
    let mut tx = conn.begin().await.map_err(|e| e.to_string())?;

    // track.recording_id has no ON DELETE CASCADE
    sqlx::query("DELETE FROM track WHERE recording_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    // CASCADE deletes source, recording_artist, recording_tag, user_rating, play_history
    sqlx::query("DELETE FROM recording WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    prune_empty_library_entities(&mut tx)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn compare_recordings(
    state: tauri::State<'_, AppState>,
    recording_id_a: String,
    recording_id_b: String,
) -> Result<f32, String> {
    let mut conn = state
        .db
        .acquire("command.compare_recordings")
        .await
        .map_err(|e| e.to_string())?;

    let path_a: String = sqlx::query_scalar(
        "SELECT file_path FROM source
         WHERE recording_id = ? AND source_type = 'local_file' AND file_path IS NOT NULL
         ORDER BY file_path LIMIT 1",
    )
    .bind(&recording_id_a)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| e.to_string())?
    .flatten()
    .ok_or_else(|| "Recording A has no local file".to_string())?;

    let path_b: String = sqlx::query_scalar(
        "SELECT file_path FROM source
         WHERE recording_id = ? AND source_type = 'local_file' AND file_path IS NOT NULL
         ORDER BY file_path LIMIT 1",
    )
    .bind(&recording_id_b)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| e.to_string())?
    .flatten()
    .ok_or_else(|| "Recording B has no local file".to_string())?;

    tokio::task::spawn_blocking(move || {
        let a = crate::fingerprint::raw_fingerprint(std::path::Path::new(&path_a))
            .map_err(|e| e.to_string())?;
        let b = crate::fingerprint::raw_fingerprint(std::path::Path::new(&path_b))
            .map_err(|e| e.to_string())?;
        Ok(crate::fingerprint::ber(&a, &b))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn merge_recordings(
    state: tauri::State<'_, AppState>,
    primary_id: String,
    duplicate_id: String,
    title: String,
    artist_choice: String,
    custom_artist_text: Option<String>,
    chosen_rating: Option<i64>,
) -> Result<Vec<RecordingRow>, String> {
    let mut tx = state
        .db
        .raw_pool()
        .begin()
        .await
        .map_err(|e| e.to_string())?;

    // Transfer unique tags from duplicate to primary
    sqlx::query(
        "INSERT OR IGNORE INTO recording_tag (recording_id, tag)
         SELECT ?, tag FROM recording_tag WHERE recording_id = ?",
    )
    .bind(&primary_id)
    .bind(&duplicate_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    // Rebind play history
    sqlx::query("UPDATE play_history SET recording_id = ? WHERE recording_id = ?")
        .bind(&primary_id)
        .bind(&duplicate_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    // Move sources to primary recording
    sqlx::query("UPDATE source SET recording_id = ? WHERE recording_id = ?")
        .bind(&primary_id)
        .bind(&duplicate_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    // Apply chosen artist
    match artist_choice.as_str() {
        "B" => {
            // Copy duplicate's artist links and credit text to primary
            sqlx::query("DELETE FROM recording_artist WHERE recording_id = ?")
                .bind(&primary_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            sqlx::query(
                "INSERT INTO recording_artist (recording_id, artist_id, position, role, credited_as)
                 SELECT ?, artist_id, position, role, credited_as
                 FROM recording_artist WHERE recording_id = ?",
            )
            .bind(&primary_id)
            .bind(&duplicate_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
            sqlx::query(
                "UPDATE recording SET artist_credit_text =
                   (SELECT artist_credit_text FROM recording WHERE id = ?)
                 WHERE id = ?",
            )
            .bind(&duplicate_id)
            .bind(&primary_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
        "custom" => {
            let text = custom_artist_text
                .as_deref()
                .unwrap_or("")
                .trim()
                .to_string();
            sqlx::query("DELETE FROM recording_artist WHERE recording_id = ?")
                .bind(&primary_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            sqlx::query("UPDATE recording SET artist_credit_text = ? WHERE id = ?")
                .bind(&text)
                .bind(&primary_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        _ => {} // "A": keep primary's existing artist
    }

    // Apply chosen title
    sqlx::query("UPDATE recording SET title = ? WHERE id = ?")
        .bind(&title)
        .bind(&primary_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    // Apply chosen rating to primary
    sqlx::query("DELETE FROM user_rating WHERE recording_id = ?")
        .bind(&primary_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(stars) = chosen_rating {
        sqlx::query(
            "INSERT INTO user_rating (recording_id, stars, updated_at) VALUES (?, ?, datetime('now'))",
        )
        .bind(&primary_id)
        .bind(stars)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    }

    // track.recording_id has no ON DELETE CASCADE, so remove duplicate's track rows first
    sqlx::query("DELETE FROM track WHERE recording_id = ?")
        .bind(&duplicate_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    // Delete duplicate (CASCADE removes remaining child rows)
    sqlx::query("DELETE FROM recording WHERE id = ?")
        .bind(&duplicate_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    prune_empty_library_entities(&mut tx)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;

    reload_catalog(&state).await;
    let result = state
        .catalog
        .read()
        .await
        .list_recordings()
        .await
        .map_err(|e| e.to_string())?;
    Ok(result)
}

#[tauri::command]
pub async fn merge_release_groups(
    state: tauri::State<'_, AppState>,
    primary_id: String,
    duplicate_id: String,
) -> Result<String, String> {
    if primary_id == duplicate_id {
        return Err("Cannot merge a release group with itself".to_string());
    }

    let mut tx = state
        .db
        .raw_pool()
        .begin()
        .await
        .map_err(|e| e.to_string())?;

    // ── 1. Transfer release_group_rating if primary has none ──────────────
    let primary_has_rating: bool = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM release_group_rating WHERE release_group_id = ?",
    )
    .bind(&primary_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?
        > 0;

    if !primary_has_rating {
        let dup_rating: Option<i64> =
            sqlx::query_scalar("SELECT stars FROM release_group_rating WHERE release_group_id = ?")
                .bind(&duplicate_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;

        if let Some(stars) = dup_rating {
            sqlx::query(
                "INSERT OR REPLACE INTO release_group_rating (release_group_id, stars, updated_at)
                 VALUES (?, ?, datetime('now'))",
            )
            .bind(&primary_id)
            .bind(stars)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
    }

    // ── 2. Get primary release to merge into ──────────────────────────────
    let primary_release_id: String =
        sqlx::query_scalar("SELECT id FROM release WHERE release_group_id = ? LIMIT 1")
            .bind(&primary_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "Primary release group has no releases".to_string())?;

    // Get all releases from the duplicate group (excluding the one that is the same as primary)
    let dup_release_ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM release WHERE release_group_id = ? AND id != ?")
            .bind(&duplicate_id)
            .bind(&primary_release_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;

    // ── 3. Merge mediums from duplicate releases into the primary release ──
    for dup_release_id in &dup_release_ids {
        let dup_mediums: Vec<(String, i64)> = sqlx::query_as(
            "SELECT id, position FROM medium WHERE release_id = ? ORDER BY position",
        )
        .bind(dup_release_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        for (dup_medium_id, position) in &dup_mediums {
            // Find or create a matching medium in the primary release
            let target_medium_id: String = match sqlx::query_scalar(
                "SELECT id FROM medium WHERE release_id = ? AND position = ?",
            )
            .bind(&primary_release_id)
            .bind(position)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
            {
                Some(id) => id,
                None => {
                    let new_id = uuid::Uuid::new_v4().to_string();
                    sqlx::query(
                        "INSERT INTO medium (id, release_id, position, format)
                         VALUES (?, ?, ?, (SELECT format FROM medium WHERE id = ?))",
                    )
                    .bind(&new_id)
                    .bind(&primary_release_id)
                    .bind(position)
                    .bind(dup_medium_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| e.to_string())?;
                    new_id
                }
            };

            // Copy tracks from duplicate medium that don't already exist in the target
            sqlx::query(
                "INSERT OR IGNORE INTO track (id, medium_id, recording_id, position, title, duration_ms)
                 SELECT ?, ?, recording_id, position, title, duration_ms
                 FROM track
                 WHERE medium_id = ?
                   AND NOT EXISTS (
                       SELECT 1 FROM track t2
                       WHERE t2.medium_id = ?
                         AND t2.recording_id = track.recording_id
                   )",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(&target_medium_id)
            .bind(dup_medium_id)
            .bind(&target_medium_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
    }

    // ── 4. Delete duplicate releases and their group ──────────────────────
    for dup_release_id in &dup_release_ids {
        sqlx::query("DELETE FROM release WHERE id = ?")
            .bind(dup_release_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }

    // Delete the duplicate release group (CASCADES to release_group_artist, release_group_rating)
    sqlx::query("DELETE FROM release_group WHERE id = ?")
        .bind(&duplicate_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    // ── 5. Prune orphaned entities ────────────────────────────────────────
    prune_empty_library_entities(&mut tx)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;

    Ok(primary_id)
}

async fn prune_empty_library_entities(tx: &mut Transaction<'_, Sqlite>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM medium
         WHERE NOT EXISTS (
             SELECT 1
             FROM track
             WHERE track.medium_id = medium.id
         )",
    )
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "DELETE FROM release
         WHERE NOT EXISTS (
             SELECT 1
             FROM medium
             WHERE medium.release_id = release.id
         )",
    )
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "DELETE FROM release_group
         WHERE NOT EXISTS (
             SELECT 1
             FROM release
             WHERE release.release_group_id = release_group.id
         )",
    )
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "DELETE FROM release_group_artist
         WHERE artist_id IN (
             SELECT id FROM artist
             WHERE NOT EXISTS (
                 SELECT 1 FROM recording_artist
                 WHERE recording_artist.artist_id = artist.id
             )
         )",
    )
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "DELETE FROM artist
         WHERE NOT EXISTS (
             SELECT 1
             FROM recording_artist
             WHERE recording_artist.artist_id = artist.id
         )",
    )
    .execute(&mut **tx)
    .await?;

    Ok(())
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
    crate::library::artist_fixes::check_artist_compound(&state.db, &artist_id).await
}

#[tauri::command]
pub async fn apply_artist_fix(
    state: tauri::State<'_, AppState>,
    artist_id: String,
    individual_artist_names: Vec<String>,
) -> Result<ArtistFixStats, String> {
    let _permit = state
        .write_serializer
        .acquire()
        .await
        .map_err(|e| format!("Failed to acquire write lock: {e}"))?;

    let result = crate::library::artist_fixes::apply_artist_fix(
        &state.db,
        &artist_id,
        &individual_artist_names,
    )
    .await;

    drop(_permit);

    reload_catalog(&state).await;

    result
}

async fn split_recording_inner(
    db: &DbPool,
    recording_id: &str,
    source_ids_to_move: &[String],
) -> Result<String, String> {
    let mut conn = db
        .acquire(format!("command.split_recording_inner id={recording_id}"))
        .await
        .map_err(|e| e.to_string())?;
    conn.set_busy_timeout(std::time::Duration::from_secs(30))
        .await
        .map_err(|e| e.to_string())?;
    let mut tx = conn.begin().await.map_err(|e| e.to_string())?;

    // Validate: at least one source must remain on the original
    let total_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source WHERE recording_id = ?")
        .bind(recording_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    if source_ids_to_move.len() as i64 >= total_count {
        return Err("At least one source must remain on the original recording".to_string());
    }

    // Validate: all source_ids belong to this recording
    for sid in source_ids_to_move {
        let exists: bool = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM source WHERE id = ? AND recording_id = ?",
        )
        .bind(sid)
        .bind(recording_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
            > 0;
        if !exists {
            return Err(format!(
                "Source {sid} does not belong to recording {recording_id}"
            ));
        }
    }

    // Read metadata from the first local_file source in the list
    let mut title = String::new();
    let mut artist_name: Option<String> = None;
    let mut genre: Option<String> = None;
    let mut bpm: Option<f64> = None;
    let mut comment: Option<String> = None;
    let mut duration_ms: Option<i64> = None;

    for sid in source_ids_to_move {
        let file_path: Option<String> = sqlx::query_scalar(
            "SELECT file_path FROM source WHERE id = ? AND source_type = 'local_file'",
        )
        .bind(sid)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        if let Some(ref path) = file_path {
            if let Ok(meta) = crate::library::scanner::read_metadata(std::path::Path::new(path)) {
                title = meta.meta.title.unwrap_or_default();
                artist_name = meta.meta.artist;
                genre = meta.meta.genre;
                bpm = meta.meta.bpm;
                comment = meta.meta.comment;
                duration_ms = Some(meta.meta.duration_ms as i64);
                break; // First source with readable tags wins
            }
        }
    }

    // If no tags could be read, fall back to the original recording's title
    if title.is_empty() {
        title = sqlx::query_scalar::<_, Option<String>>("SELECT title FROM recording WHERE id = ?")
            .bind(recording_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| "Split Recording".to_string());
    }

    // Create the new recording
    let new_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO recording (id, title, duration_ms, genre, bpm, comment)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&new_id)
    .bind(&title)
    .bind(duration_ms)
    .bind(&genre)
    .bind(bpm)
    .bind(&comment)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    // Link the primary artist if we found one in the tags
    if let Some(ref name) = artist_name {
        let name = name.trim();
        if !name.is_empty() {
            let artist_id = crate::library::import::get_or_create_artist(&mut tx, name)
                .await
                .map_err(|e| e.to_string())?;
            sqlx::query(
                "INSERT INTO recording_artist (recording_id, artist_id, position, role)
                 VALUES (?, ?, 0, 'main')",
            )
            .bind(&new_id)
            .bind(&artist_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
    }

    // Move selected sources to the new recording
    for sid in source_ids_to_move {
        sqlx::query("UPDATE source SET recording_id = ? WHERE id = ?")
            .bind(&new_id)
            .bind(sid)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(new_id)
}

#[tauri::command]
pub async fn split_recording(
    state: tauri::State<'_, AppState>,
    recording_id: String,
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

    let result = split_recording_inner(&state.db, &recording_id, &source_ids_to_move).await;
    drop(_permit);

    reload_catalog(&state).await;
    result
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

    /// Regression: multi-disc releases where each disc has unanimous track_total
    /// (but different values per disc) should NOT trigger "Sources disagree".
    /// Previously the unanimous check compared source-claimed track_total against
    /// DB track count per disc, falsely rejecting unanimous multi-disc layouts.
    #[tokio::test]
    async fn test_multi_disc_unanimous_completeness() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = crate::db::init_pool(&db_path).await.unwrap();
        let mut conn = pool.acquire("test".to_string()).await.unwrap();

        let rg_id = "rg-test-unanimous";
        let release_id = "rel-test-unanimous";

        sqlx::query("INSERT INTO release_group (id, title) VALUES (?, ?)")
            .bind(rg_id)
            .bind("Test RG")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO release (id, release_group_id, title) VALUES (?, ?, ?)")
            .bind(release_id)
            .bind(rg_id)
            .bind("Test Release")
            .execute(&mut *conn)
            .await
            .unwrap();

        // Disc 1: 3 tracks, all sources claim track_total=3
        sqlx::query("INSERT INTO medium (id, release_id, position) VALUES ('m1', ?, 1)")
            .bind(release_id)
            .execute(&mut *conn)
            .await
            .unwrap();
        // Disc 2: 2 tracks, all sources claim track_total=2
        sqlx::query("INSERT INTO medium (id, release_id, position) VALUES ('m2', ?, 2)")
            .bind(release_id)
            .execute(&mut *conn)
            .await
            .unwrap();

        for i in 1..=3 {
            let rec_id = format!("rec-d1-{i}");
            sqlx::query("INSERT INTO recording (id, title) VALUES (?, ?)")
                .bind(&rec_id)
                .bind(format!("D1 Track {i}"))
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO track (id, medium_id, recording_id, position) VALUES (?, 'm1', ?, ?)",
            )
            .bind(format!("trk-d1-{i}"))
            .bind(&rec_id)
            .bind(i)
            .execute(&mut *conn)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO source (id, recording_id, source_type, file_path, track_total) \
                 VALUES (?, ?, 'local_file', ?, 3)",
            )
            .bind(format!("src-d1-{i}"))
            .bind(&rec_id)
            .bind(format!("/d1/track{i}.mp3"))
            .execute(&mut *conn)
            .await
            .unwrap();
        }

        for i in 1..=2 {
            let rec_id = format!("rec-d2-{i}");
            sqlx::query("INSERT INTO recording (id, title) VALUES (?, ?)")
                .bind(&rec_id)
                .bind(format!("D2 Track {i}"))
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO track (id, medium_id, recording_id, position) VALUES (?, 'm2', ?, ?)",
            )
            .bind(format!("trk-d2-{i}"))
            .bind(&rec_id)
            .bind(i)
            .execute(&mut *conn)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO source (id, recording_id, source_type, file_path, track_total) \
                 VALUES (?, ?, 'local_file', ?, 2)",
            )
            .bind(format!("src-d2-{i}"))
            .bind(&rec_id)
            .bind(format!("/d2/track{i}.mp3"))
            .execute(&mut *conn)
            .await
            .unwrap();
        }

        // ── Step 1: Run the same completeness query the command handler uses ──
        let stats = sqlx::query(
            "WITH release_source_stats AS (
                SELECT
                    t.id AS track_id,
                    MAX(CASE WHEN s.id IS NOT NULL THEN 1 ELSE 0 END) AS has_source,
                    MAX(s.track_total) AS source_track_total
                FROM medium m
                JOIN track t ON t.medium_id = m.id
                LEFT JOIN source s ON s.recording_id = t.recording_id
                WHERE m.release_id = ?
                GROUP BY t.id
            )
            SELECT
                COUNT(DISTINCT source_track_total) AS distinct_track_totals,
                COUNT(*) AS total_tracks,
                COALESCE(SUM(has_source), 0) AS tracks_with_sources
            FROM release_source_stats",
        )
        .bind(release_id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();

        let distinct: i64 = stats.get("distinct_track_totals");
        let total_tracks: i64 = stats.get("total_tracks");
        let with_sources: i64 = stats.get("tracks_with_sources");

        // distinct=2 because discs report different track_total — this enters
        // the `(_, _, _)` wildcard arm (the disagreement / multi-disc check).
        assert_eq!(distinct, 2, "two distinct track_total values across discs");
        assert_eq!(total_tracks, 5, "5 tracks in DB");
        assert_eq!(with_sources, 5, "all tracks have sources");

        // ── Step 2: Run the source_data query the wildcard arm uses ──
        let source_data = sqlx::query(
            "SELECT s.file_path, s.track_total,
                    m.position AS disc_position,
                    (SELECT MAX(m3.position) FROM medium m3
                     WHERE m3.release_id = m.release_id) AS disc_total
             FROM medium m
             JOIN track t ON t.medium_id = m.id
             JOIN source s ON s.recording_id = t.recording_id
             WHERE m.release_id = ? AND s.track_total IS NOT NULL
             ORDER BY s.track_total, m.position, s.file_path",
        )
        .bind(release_id)
        .fetch_all(&mut *conn)
        .await
        .unwrap();

        let release_disc_total: Option<i64> = source_data.first().and_then(|r| r.get("disc_total"));
        assert_eq!(release_disc_total, Some(2));

        // ── Step 3: Replicate the unanimous per-disc check ──
        let mut disc_map: BTreeMap<i64, BTreeMap<i64, usize>> = BTreeMap::new();
        for row in &source_data {
            if let Some(dp) = row.get::<Option<i64>, _>("disc_position") {
                let tt: i64 = row.get("track_total");
                *disc_map.entry(dp).or_default().entry(tt).or_default() += 1;
            }
        }

        assert_eq!(disc_map.len(), 2, "both discs represented");
        // Every disc has only one track_total claim — sources agree per disc.
        assert!(
            disc_map.values().all(|tts| tts.len() == 1),
            "each disc should have unanimous track_total: {disc_map:?}"
        );

        // All DB tracks have sources → this should be Complete, not "Sources disagree".
        assert!(
            with_sources >= total_tracks,
            "all tracks have sources, should be Complete not disagreement"
        );
    }

    /// Verify that genuine intra-disc disagreement is still detected as disagreement.
    #[tokio::test]
    async fn test_multi_disc_intra_disc_disagreement() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = crate::db::init_pool(&db_path).await.unwrap();
        let mut conn = pool.acquire("test".to_string()).await.unwrap();

        let rg_id = "rg-test-disagree";
        let release_id = "rel-test-disagree";

        sqlx::query("INSERT INTO release_group (id, title) VALUES (?, ?)")
            .bind(rg_id)
            .bind("Test RG")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO release (id, release_group_id, title) VALUES (?, ?, ?)")
            .bind(release_id)
            .bind(rg_id)
            .bind("Test Release")
            .execute(&mut *conn)
            .await
            .unwrap();

        // Single disc, 2 tracks — one source says track_total=1, the other says 2
        sqlx::query("INSERT INTO medium (id, release_id, position) VALUES ('m3', ?, 1)")
            .bind(release_id)
            .execute(&mut *conn)
            .await
            .unwrap();

        for i in 1..=2 {
            let rec_id = format!("rec-{i}");
            sqlx::query("INSERT INTO recording (id, title) VALUES (?, ?)")
                .bind(&rec_id)
                .bind(format!("Track {i}"))
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO track (id, medium_id, recording_id, position) \
                 VALUES (?, 'm3', ?, ?)",
            )
            .bind(format!("trk-{i}"))
            .bind(&rec_id)
            .bind(i)
            .execute(&mut *conn)
            .await
            .unwrap();
            let tt = if i == 1 { 1 } else { 2 };
            sqlx::query(
                "INSERT INTO source (id, recording_id, source_type, file_path, track_total) \
                 VALUES (?, ?, 'local_file', ?, ?)",
            )
            .bind(format!("src-{i}"))
            .bind(&rec_id)
            .bind(format!("/track{i}.mp3"))
            .bind(tt)
            .execute(&mut *conn)
            .await
            .unwrap();
        }

        let stats = sqlx::query(
            "WITH release_source_stats AS (
                SELECT
                    t.id AS track_id,
                    MAX(CASE WHEN s.id IS NOT NULL THEN 1 ELSE 0 END) AS has_source,
                    MAX(s.track_total) AS source_track_total
                FROM medium m
                JOIN track t ON t.medium_id = m.id
                LEFT JOIN source s ON s.recording_id = t.recording_id
                WHERE m.release_id = ?
                GROUP BY t.id
            )
            SELECT
                COUNT(DISTINCT source_track_total) AS distinct_track_totals,
                COUNT(*) AS total_tracks,
                COALESCE(SUM(has_source), 0) AS tracks_with_sources
            FROM release_source_stats",
        )
        .bind(release_id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();

        let distinct: i64 = stats.get("distinct_track_totals");
        let total_tracks: i64 = stats.get("total_tracks");
        let with_sources: i64 = stats.get("tracks_with_sources");

        assert_eq!(
            distinct, 2,
            "two distinct track_total values = disagreement"
        );
        assert_eq!(total_tracks, 2);
        assert_eq!(with_sources, 2);

        // Single disc → release_disc_total = 1 → unanimous check bail out correctly
        let source_data = sqlx::query(
            "SELECT s.file_path, s.track_total,
                    m.position AS disc_position,
                    (SELECT MAX(m3.position) FROM medium m3
                     WHERE m3.release_id = m.release_id) AS disc_total
             FROM medium m
             JOIN track t ON t.medium_id = m.id
             JOIN source s ON s.recording_id = t.recording_id
             WHERE m.release_id = ? AND s.track_total IS NOT NULL
             ORDER BY s.track_total, m.position, s.file_path",
        )
        .bind(release_id)
        .fetch_all(&mut *conn)
        .await
        .unwrap();

        let release_disc_total: Option<i64> = source_data.first().and_then(|r| r.get("disc_total"));
        assert_eq!(
            release_disc_total,
            Some(1),
            "single disc, unanimous check bails out"
        );
    }

    /// Regression: multi-disc release where sources unanimously claim more tracks
    /// per disc than the DB has should produce Incomplete with phantom tracks,
    /// not "Complete" (which was the false result from `with_sources >= total_tracks`
    /// ignoring the phantom track).
    #[tokio::test]
    async fn test_multi_disc_phantom_tracks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = crate::db::init_pool(&db_path).await.unwrap();
        let mut conn = pool.acquire("test".to_string()).await.unwrap();

        let rg_id = "rg-phantom";
        let release_id = "rel-phantom";

        sqlx::query("INSERT INTO release_group (id, title) VALUES (?, ?)")
            .bind(rg_id)
            .bind("Test RG")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO release (id, release_group_id, title) VALUES (?, ?, ?)")
            .bind(release_id)
            .bind(rg_id)
            .bind("Test Release")
            .execute(&mut *conn)
            .await
            .unwrap();

        // Disc 1: 3 tracks in DB, sources claim track_total=4 (phantom Track 4)
        sqlx::query("INSERT INTO medium (id, release_id, position) VALUES ('mp1', ?, 1)")
            .bind(release_id)
            .execute(&mut *conn)
            .await
            .unwrap();
        // Disc 2: 2 tracks in DB, sources claim track_total=2 (matches)
        sqlx::query("INSERT INTO medium (id, release_id, position) VALUES ('mp2', ?, 2)")
            .bind(release_id)
            .execute(&mut *conn)
            .await
            .unwrap();

        for i in 1..=3 {
            let rec_id = format!("rec-p1-{i}");
            sqlx::query("INSERT INTO recording (id, title) VALUES (?, ?)")
                .bind(&rec_id)
                .bind(format!("P1 Track {i}"))
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO track (id, medium_id, recording_id, position) \
                 VALUES (?, 'mp1', ?, ?)",
            )
            .bind(format!("trk-p1-{i}"))
            .bind(&rec_id)
            .bind(i)
            .execute(&mut *conn)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO source (id, recording_id, source_type, file_path, track_total) \
                 VALUES (?, ?, 'local_file', ?, 4)",
            )
            .bind(format!("src-p1-{i}"))
            .bind(&rec_id)
            .bind(format!("/p1/track{i}.mp3"))
            .execute(&mut *conn)
            .await
            .unwrap();
        }

        for i in 1..=2 {
            let rec_id = format!("rec-p2-{i}");
            sqlx::query("INSERT INTO recording (id, title) VALUES (?, ?)")
                .bind(&rec_id)
                .bind(format!("P2 Track {i}"))
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO track (id, medium_id, recording_id, position) \
                 VALUES (?, 'mp2', ?, ?)",
            )
            .bind(format!("trk-p2-{i}"))
            .bind(&rec_id)
            .bind(i)
            .execute(&mut *conn)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO source (id, recording_id, source_type, file_path, track_total) \
                 VALUES (?, ?, 'local_file', ?, 2)",
            )
            .bind(format!("src-p2-{i}"))
            .bind(&rec_id)
            .bind(format!("/p2/track{i}.mp3"))
            .execute(&mut *conn)
            .await
            .unwrap();
        }

        // ── Step 1: Completeness query ──
        let stats = sqlx::query(
            "WITH release_source_stats AS (
                SELECT
                    t.id AS track_id,
                    MAX(CASE WHEN s.id IS NOT NULL THEN 1 ELSE 0 END) AS has_source,
                    MAX(s.track_total) AS source_track_total
                FROM medium m
                JOIN track t ON t.medium_id = m.id
                LEFT JOIN source s ON s.recording_id = t.recording_id
                WHERE m.release_id = ?
                GROUP BY t.id
            )
            SELECT
                COUNT(DISTINCT source_track_total) AS distinct_track_totals,
                COUNT(*) AS total_tracks,
                COALESCE(SUM(has_source), 0) AS tracks_with_sources
            FROM release_source_stats",
        )
        .bind(release_id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();

        let distinct: i64 = stats.get("distinct_track_totals");
        let total_tracks: i64 = stats.get("total_tracks");
        let with_sources: i64 = stats.get("tracks_with_sources");

        assert_eq!(distinct, 2);
        assert_eq!(total_tracks, 5, "5 DB tracks");
        assert_eq!(with_sources, 5, "all DB tracks have sources");

        // ── Step 2: Source_data query ──
        let source_data = sqlx::query(
            "SELECT s.file_path, s.track_total,
                    m.position AS disc_position,
                    (SELECT MAX(m3.position) FROM medium m3
                     WHERE m3.release_id = m.release_id) AS disc_total
             FROM medium m
             JOIN track t ON t.medium_id = m.id
             JOIN source s ON s.recording_id = t.recording_id
             WHERE m.release_id = ? AND s.track_total IS NOT NULL
             ORDER BY s.track_total, m.position, s.file_path",
        )
        .bind(release_id)
        .fetch_all(&mut *conn)
        .await
        .unwrap();

        let release_disc_total: Option<i64> = source_data.first().and_then(|r| r.get("disc_total"));
        assert_eq!(release_disc_total, Some(2));

        // ── Step 3: Replicate unanimous check ──
        let mut disc_map: BTreeMap<i64, BTreeMap<i64, usize>> = BTreeMap::new();
        for row in &source_data {
            if let Some(dp) = row.get::<Option<i64>, _>("disc_position") {
                let tt: i64 = row.get("track_total");
                *disc_map.entry(dp).or_default().entry(tt).or_default() += 1;
            }
        }

        assert_eq!(disc_map.len(), 2);
        assert!(
            disc_map.values().all(|tts| tts.len() == 1),
            "unanimous per disc"
        );

        // ── Step 4: Per-disc actuals comparison ──
        let disc_actuals: Vec<(i64, i64)> = sqlx::query(
            "SELECT m.position, COUNT(t.id) AS track_count
             FROM medium m
             JOIN track t ON t.medium_id = m.id
             WHERE m.release_id = ?
             GROUP BY m.position
             ORDER BY m.position",
        )
        .bind(release_id)
        .fetch_all(&mut *conn)
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get("position"), row.get("track_count")))
        .collect();

        // Disc 1: claimed=4, actual=3 → mismatch.  Disc 2: claimed=2, actual=2 → match.
        let all_match = disc_actuals.iter().all(|(pos, actual_count)| {
            disc_map
                .get(pos)
                .and_then(|tts| tts.keys().next())
                .map(|&claimed| claimed == *actual_count)
                .unwrap_or(false)
        });
        assert!(!all_match, "disc 1 claimed 4 but DB has 3 — mismatch");

        let has_deficit = disc_actuals.iter().any(|(pos, actual_count)| {
            disc_map
                .get(pos)
                .and_then(|tts| tts.keys().next())
                .map(|&claimed| claimed < *actual_count)
                .unwrap_or(false)
        });
        assert!(!has_deficit, "no disc with source claims below DB count");

        // ── Step 5: Phantom track detection ──
        let mut phantom_found: Vec<(i64, i64)> = Vec::new();
        for (pos, actual_count) in &disc_actuals {
            if let Some(claimed_count) = disc_map.get(pos).and_then(|tts| tts.keys().next()) {
                if *claimed_count > *actual_count {
                    let positions: Vec<i64> = sqlx::query(
                        "WITH RECURSIVE positions(n) AS (
                            SELECT 1
                            UNION ALL
                            SELECT n + 1 FROM positions WHERE n < ?
                        )
                        SELECT p.n AS track_position
                        FROM positions p
                        WHERE NOT EXISTS (
                            SELECT 1 FROM track t
                            JOIN medium m2 ON t.medium_id = m2.id
                            WHERE m2.release_id = ?
                              AND m2.position = ?
                              AND t.position = p.n
                        )
                        ORDER BY p.n",
                    )
                    .bind(claimed_count)
                    .bind(release_id)
                    .bind(pos)
                    .fetch_all(&mut *conn)
                    .await
                    .unwrap()
                    .iter()
                    .map(|row| row.get("track_position"))
                    .collect();

                    for tp in positions {
                        phantom_found.push((*pos, tp));
                    }
                }
            }
        }

        assert_eq!(
            phantom_found,
            vec![(1, 4)],
            "disc 1 track 4 should be phantom"
        );
    }
}
