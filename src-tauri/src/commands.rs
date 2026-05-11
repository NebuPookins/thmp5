use crate::audio::PlayRequest as EnginePlayRequest;
use crate::db::DbPool;
use crate::file_issues::FileIssue;
use crate::library::import::{
    import_paths as do_import, prune_library, rescan_source as do_rescan_source,
};
use crate::models::{
    AppBootstrap, AppConfig, ArtistDetail, ArtistFixStats, ArtistRow, CompoundArtistCheck,
    DbPoolDebugSnapshot, ExternalCommand, FixMergedRecordingsStats, GuestAppearanceReleaseGroup,
    GuestAppearanceTrack, Id3FrameDebugInfo, Id3FrameDebugRequest, ImportProgress, ImportStats,
    InitialSetupRequest, LibrarySummary, PlayHistoryInput, PlayRequest, PlayerState, PlaylistRow,
    QueueSettingsUpdate, RatingUpdateRequest, RecordingArtistInfo, RecordingDetail,
    RecordingRatingUpdateResult, RecordingRow, ReleaseGroupDetail, ReleaseGroupRow, ReleaseInfo,
    SaveSmartPlaylistRequest, SeekRequest, SmartPlaylistResult, SourceDetail, VolumeRequest,
};
use crate::query::{self, LimitUnit};
use crate::AppState;
use serde::Serialize;
use sqlx::{Acquire, Row, Sqlite, SqliteConnection, Transaction};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
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

/// CTEs shared by list_recordings and evaluate_smart_playlist.
/// Aggregating in CTEs replaces three correlated subqueries (one per row) with three
/// single-pass aggregations, eliminating the N×3 per-row execution cost.
const RECORDING_CTES: &str = "
  ph_agg AS (
    SELECT recording_id,
           COUNT(*)       AS play_count,
           MAX(played_at) AS last_played
    FROM play_history
    GROUP BY recording_id
  ),
  src_primary AS (
    SELECT recording_id, MIN(file_path) AS file_path
    FROM source
    WHERE source_type = 'local_file' AND file_path IS NOT NULL
    GROUP BY recording_id
  ),
  tags_agg AS (
    SELECT recording_id, GROUP_CONCAT(tag, char(0)) AS tags_raw
    FROM (SELECT recording_id, tag FROM recording_tag ORDER BY recording_id, tag)
    GROUP BY recording_id
  ),
  source_paths_agg AS (
    SELECT recording_id, GROUP_CONCAT(file_path, char(0)) AS source_paths_raw
    FROM (
      SELECT recording_id, file_path FROM source
      WHERE source_type = 'local_file' AND file_path IS NOT NULL
      ORDER BY recording_id, file_path
    )
    GROUP BY recording_id
  ),
  artists_agg AS (
    SELECT recording_id, GROUP_CONCAT(artist_id, char(0)) AS artist_ids_raw
    FROM (
      SELECT recording_id, artist_id
      FROM recording_artist
      ORDER BY recording_id, position
    )
    GROUP BY recording_id
  ),
  releases_agg AS (
    SELECT recording_id, GROUP_CONCAT(entry, char(0)) AS releases_raw
    FROM (
      SELECT t2.recording_id,
             rg2.id || char(1) ||
             rg2.title || char(1) ||
             COALESCE(CAST(t2.position AS TEXT), '') || char(1) ||
             COALESCE(CAST(m2.position AS TEXT), '') || char(1) ||
             COALESCE(CAST((SELECT MAX(m3.position) FROM medium m3 WHERE m3.release_id = rel2.id) AS TEXT), '') AS entry
      FROM track t2
      JOIN medium m2         ON m2.id = t2.medium_id
      JOIN release rel2      ON rel2.id = m2.release_id
      JOIN release_group rg2 ON rg2.id = rel2.release_group_id
      ORDER BY t2.recording_id, rg2.title, m2.position, t2.position
    )
    GROUP BY recording_id
  )
";

// ── Import ────────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn import_paths(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<ImportStats, String> {
    let result = do_import(&state.db, paths, state.acoustid_api_key.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    *state.recordings_cache.write().await = None;
    Ok(result)
}

#[tauri::command]
pub async fn get_app_bootstrap(state: tauri::State<'_, AppState>) -> Result<AppBootstrap, String> {
    let config = load_app_config(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let library_summary = load_library_summary(&state.db)
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
    )
    .await
    .map_err(|e| e.to_string())?;
    *state.recordings_cache.write().await = None;
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
    )
    .await
    .map_err(|e| format!("Failed to rescan {}:\n{e:#}", source_path.display()));

    state.pending_jobs.fetch_sub(1, Ordering::Relaxed);
    emit_job_update(&app, &state, "rescan");

    *state.recordings_cache.write().await = None;
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
    rescan_path_list(&app, &state, trimmed).await
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
        } else if let Err(error) = do_rescan_source(
            &state.db,
            source_path,
            state.acoustid_api_key.as_deref(),
            &state.write_serializer,
            true,
        )
        .await
        {
            failures.push(format!("{}:\n{error:#}", source_path.display()));
        }
        state.pending_jobs.fetch_sub(1, Ordering::Relaxed);
        emit_job_update(app, state, "rescan");
    }

    *state.recordings_cache.write().await = None;

    // Single prune pass after the batch, regardless of individual failures.
    if let Err(e) = prune_library(&state.db).await {
        tracing::warn!("Library pruning after batch rescan failed: {e}");
    }

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

    rescan_path_list(&app, &state, paths).await
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

    rescan_path_list(&app, &state, paths).await
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

    rescan_path_list(&app, &state, paths).await
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

    *state.recordings_cache.write().await = None;
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

    *state.recordings_cache.write().await = None;
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
    )
    .await
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

    *state.recordings_cache.write().await = None;
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
    load_library_summary(&state.db)
        .await
        .map_err(|e| e.to_string())
}

fn parse_recording_row(row: &sqlx::sqlite::SqliteRow) -> RecordingRow {
    RecordingRow {
        id: row.get("id"),
        title: row.get("title"),
        duration_ms: row.get("duration_ms"),
        primary_artist_id: row.get("primary_artist_id"),
        artist_credit_name: row.get("artist_credit_name"),
        genre: row.get("genre"),
        rating: row.get("rating"),
        play_count: row.get::<Option<i64>, _>("play_count").unwrap_or(0),
        last_played: row.get("last_played"),
        primary_source_id: row.get("primary_source_id"),
        primary_source_path: row.get("primary_source_path"),
        tags: {
            let raw: Option<String> = row.get("tags_raw");
            raw.map(|r| {
                r.split('\0')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
        },
        artist_ids: {
            let raw: Option<String> = row.get("artist_ids_raw");
            raw.map(|r| {
                r.split('\0')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
        },
        source_paths: {
            let raw: Option<String> = row.get("source_paths_raw");
            raw.map(|r| {
                r.split('\0')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
        },
        releases: {
            let raw: Option<String> = row.get("releases_raw");
            raw.map(|r| {
                r.split('\0')
                    .filter(|s| !s.is_empty())
                    .map(|entry| {
                        let mut parts = entry.splitn(5, '\x01');
                        let release_group_id = parts.next().unwrap_or("").to_string();
                        let release_group_title = parts.next().unwrap_or("").to_string();
                        let track_position =
                            parts.next().and_then(
                                |s| {
                                    if s.is_empty() {
                                        None
                                    } else {
                                        s.parse().ok()
                                    }
                                },
                            );
                        let disc_position =
                            parts.next().and_then(
                                |s| {
                                    if s.is_empty() {
                                        None
                                    } else {
                                        s.parse().ok()
                                    }
                                },
                            );
                        let disc_total =
                            parts.next().and_then(
                                |s| {
                                    if s.is_empty() {
                                        None
                                    } else {
                                        s.parse().ok()
                                    }
                                },
                            );
                        ReleaseInfo {
                            release_group_id,
                            release_group_title,
                            track_position,
                            disc_position,
                            disc_total,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
        },
    }
}

async fn fetch_all_recordings_from_db(db: &crate::db::DbPool) -> Result<Vec<RecordingRow>, String> {
    let mut conn = db
        .acquire("command.list_recordings populate_cache")
        .await
        .map_err(|e| e.to_string())?;

    let rows = sqlx::query(&format!(
        "WITH {RECORDING_CTES}
         SELECT
            r.id,
            r.title,
            r.duration_ms,
            a.id                             AS primary_artist_id,
            COALESCE(ra.credited_as, a.name) AS artist_credit_name,
            r.genre,
            ur.stars                         AS rating,
            COALESCE(ph.play_count, 0)       AS play_count,
            ph.last_played,
            ps.id                            AS primary_source_id,
            sp.file_path                     AS primary_source_path,
            ta.tags_raw,
            spa.source_paths_raw,
            aa.artist_ids_raw,
            ragg.releases_raw
         FROM recording r
         LEFT JOIN recording_artist ra  ON ra.recording_id = r.id AND ra.position = 0
         LEFT JOIN artist a              ON a.id = ra.artist_id
         LEFT JOIN user_rating ur        ON ur.recording_id = r.id
         LEFT JOIN ph_agg ph             ON ph.recording_id = r.id
         LEFT JOIN src_primary sp        ON sp.recording_id = r.id
         LEFT JOIN source ps             ON ps.file_path = sp.file_path
         LEFT JOIN tags_agg ta           ON ta.recording_id = r.id
         LEFT JOIN source_paths_agg spa  ON spa.recording_id = r.id
         LEFT JOIN artists_agg aa        ON aa.recording_id = r.id
         LEFT JOIN releases_agg ragg     ON ragg.recording_id = r.id
         ORDER BY lower(a.sort_name), lower(r.title)"
    ))
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    Ok(rows.iter().map(parse_recording_row).collect())
}

#[tauri::command]
pub async fn list_recordings(
    state: tauri::State<'_, AppState>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<RecordingRow>, String> {
    let limit = limit.unwrap_or(200) as usize;
    let offset = offset.unwrap_or(0) as usize;

    // Fast path: serve from in-memory cache.
    {
        let guard = state.recordings_cache.read().await;
        if let Some(all) = guard.as_ref() {
            return Ok(all.iter().skip(offset).take(limit).cloned().collect());
        }
    }

    // Cache miss: fetch all recordings once and populate the cache.
    let all = Arc::new(fetch_all_recordings_from_db(&state.db).await?);
    let result = all.iter().skip(offset).take(limit).cloned().collect();
    *state.recordings_cache.write().await = Some(Arc::clone(&all));
    Ok(result)
}

#[tauri::command]
pub async fn list_artists(state: tauri::State<'_, AppState>) -> Result<Vec<ArtistRow>, String> {
    let mut conn = state
        .db
        .acquire("command.list_artists")
        .await
        .map_err(|e| e.to_string())?;

    let rows = sqlx::query(
        "SELECT
            a.id,
            a.name,
            a.sort_name,
            COUNT(DISTINCT rga.release_group_id) AS release_group_count,
            COUNT(DISTINCT ra.recording_id)      AS recording_count,
            AVG(ur.stars)                        AS rating,
            MAX(ph.played_at)                    AS last_played
         FROM artist a
         LEFT JOIN recording_artist ra      ON ra.artist_id = a.id
         LEFT JOIN release_group_artist rga ON rga.artist_id = a.id
         LEFT JOIN user_rating ur           ON ur.recording_id = ra.recording_id
         LEFT JOIN play_history ph          ON ph.recording_id = ra.recording_id
         GROUP BY a.id
         ORDER BY lower(a.sort_name)",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let artists = rows
        .into_iter()
        .map(|row| ArtistRow {
            id: row.get("id"),
            name: row.get("name"),
            sort_name: row.get("sort_name"),
            release_group_count: row
                .get::<Option<i64>, _>("release_group_count")
                .unwrap_or(0),
            recording_count: row.get::<Option<i64>, _>("recording_count").unwrap_or(0),
            rating: row.get("rating"),
            last_played: row.get("last_played"),
        })
        .collect();

    Ok(artists)
}

#[tauri::command]
pub async fn list_release_groups(
    state: tauri::State<'_, AppState>,
    artist_id: Option<String>,
    search: Option<String>,
) -> Result<Vec<ReleaseGroupRow>, String> {
    let artist_filter = artist_id.as_deref();
    let search_filter = search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let mut conn = state
        .db
        .acquire(format!(
            "command.list_release_groups artist_id={} search={}",
            artist_filter.unwrap_or("<all>"),
            search_filter.unwrap_or("<none>")
        ))
        .await
        .map_err(|e| e.to_string())?;

    let rows = sqlx::query(
        "SELECT
            rg.id,
            rg.title,
            COALESCE(rg.artist_credit_text, rga.credited_as, a.name) AS artist_credit_name,
            a.id                                                     AS primary_artist_id,
            COUNT(DISTINCT rel.id)                                   AS release_count,
            COUNT(DISTINCT t.recording_id)                           AS recording_count,
            MIN(rel.release_date)                                    AS release_date,
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
                    WHERE rel2.release_group_id = rg.id
                ) AS track_ratings
            )                                                        AS rating,
            (
                SELECT MAX(ph2.played_at)
                FROM release rel2
                JOIN medium m2
                    ON m2.release_id = rel2.id
                JOIN track t2
                    ON t2.medium_id = m2.id
                JOIN play_history ph2
                    ON ph2.recording_id = t2.recording_id
                WHERE rel2.release_group_id = rg.id
            )                                                        AS last_played
         FROM release_group rg
         LEFT JOIN release_group_artist rga
             ON rga.release_group_id = rg.id AND rga.position = 0
         LEFT JOIN artist a
             ON a.id = rga.artist_id
         LEFT JOIN release rel
             ON rel.release_group_id = rg.id
         LEFT JOIN medium m
             ON m.release_id = rel.id
         LEFT JOIN track t
             ON t.medium_id = m.id
         WHERE (
               ? IS NULL
               OR a.id = ?
               OR EXISTS (
                   SELECT 1 FROM recording_artist ra2
                   JOIN track t2 ON t2.recording_id = ra2.recording_id
                   JOIN medium m2 ON m2.id = t2.medium_id
                   JOIN release rel2 ON rel2.id = m2.release_id
                   WHERE rel2.release_group_id = rg.id AND ra2.artist_id = ?
               )
           )
           AND (
               ? IS NULL
               OR lower(rg.title) LIKE '%' || lower(?) || '%'
               OR lower(COALESCE(rg.artist_credit_text, rga.credited_as, a.name, '')) LIKE '%' || lower(?) || '%'
           )
         GROUP BY rg.id
         ORDER BY lower(COALESCE(a.sort_name, a.name, '')), lower(rg.title)",
    )
    .bind(artist_filter)
    .bind(artist_filter)
    .bind(artist_filter)
    .bind(search_filter)
    .bind(search_filter)
    .bind(search_filter)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let release_groups = rows
        .into_iter()
        .map(|row| ReleaseGroupRow {
            id: row.get("id"),
            title: row.get("title"),
            artist_credit_name: row.get("artist_credit_name"),
            primary_artist_id: row.get("primary_artist_id"),
            release_count: row.get::<Option<i64>, _>("release_count").unwrap_or(0),
            recording_count: row.get::<Option<i64>, _>("recording_count").unwrap_or(0),
            release_date: row.get("release_date"),
            rating: row.get("rating"),
            last_played: row.get("last_played"),
        })
        .collect();

    Ok(release_groups)
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
    let mut conn = state
        .db
        .acquire("command.list_all_tags")
        .await
        .map_err(|e| e.to_string())?;
    let tags =
        sqlx::query_scalar::<_, String>("SELECT DISTINCT tag FROM recording_tag ORDER BY tag")
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
    Ok(tags)
}

// ── Smart playlists ───────────────────────────────────────────────────────────

#[tauri::command]
pub async fn evaluate_smart_playlist(
    state: tauri::State<'_, AppState>,
    query: String,
) -> Result<SmartPlaylistResult, String> {
    let parsed = query::parse(&query).map_err(|e| e.to_string())?;
    let (where_sql, limit) = query::compile(&parsed);

    // Build the full query: fetch matching recording IDs from smart_playlist_view,
    // then JOIN to get the full RecordingRow data.
    let full_sql = format!(
        "WITH {RECORDING_CTES}
         SELECT
            r.id,
            r.title,
            r.duration_ms,
            a.id                             AS primary_artist_id,
            COALESCE(ra.credited_as, a.name) AS artist_credit_name,
            r.genre,
            ur.stars                         AS rating,
            COALESCE(ph.play_count, 0)       AS play_count,
            ph.last_played,
            ps.id                            AS primary_source_id,
            sp.file_path                     AS primary_source_path,
            ta.tags_raw,
            spa.source_paths_raw,
            aa.artist_ids_raw,
            ragg.releases_raw
         FROM recording r
         LEFT JOIN recording_artist ra  ON ra.recording_id = r.id AND ra.position = 0
         LEFT JOIN artist a              ON a.id = ra.artist_id
         LEFT JOIN user_rating ur        ON ur.recording_id = r.id
         LEFT JOIN ph_agg ph             ON ph.recording_id = r.id
         LEFT JOIN src_primary sp        ON sp.recording_id = r.id
         LEFT JOIN source ps             ON ps.file_path = sp.file_path
         LEFT JOIN tags_agg ta           ON ta.recording_id = r.id
         LEFT JOIN source_paths_agg spa  ON spa.recording_id = r.id
         LEFT JOIN artists_agg aa        ON aa.recording_id = r.id
         LEFT JOIN releases_agg ragg     ON ragg.recording_id = r.id
         WHERE r.id IN (
             SELECT spv.id FROM smart_playlist_view spv WHERE {}
         )
         ORDER BY RANDOM()",
        where_sql
    );

    tracing::debug!(sql = %full_sql, "Evaluating smart playlist");

    let mut conn = state
        .db
        .acquire(format!("command.evaluate_smart_playlist query={query}"))
        .await
        .map_err(|e| e.to_string())?;
    let rows = sqlx::query(&full_sql)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| format!("Query error: {e}"))?;

    let mut recordings: Vec<_> = rows
        .into_iter()
        .map(|row| RecordingRow {
            id: row.get("id"),
            title: row.get("title"),
            duration_ms: row.get("duration_ms"),
            primary_artist_id: row.get("primary_artist_id"),
            artist_credit_name: row.get("artist_credit_name"),
            genre: row.get("genre"),
            rating: row.get("rating"),
            play_count: row.get::<Option<i64>, _>("play_count").unwrap_or(0),
            last_played: row.get("last_played"),
            primary_source_id: row.get("primary_source_id"),
            primary_source_path: row.get("primary_source_path"),
            tags: {
                let raw: Option<String> = row.get("tags_raw");
                raw.map(|r| {
                    r.split('\0')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default()
            },
            artist_ids: {
                let raw: Option<String> = row.get("artist_ids_raw");
                raw.map(|r| {
                    r.split('\0')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default()
            },
            source_paths: {
                let raw: Option<String> = row.get("source_paths_raw");
                raw.map(|r| {
                    r.split('\0')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default()
            },
            releases: {
                let raw: Option<String> = row.get("releases_raw");
                raw.map(|r| {
                    r.split('\0')
                        .filter(|s| !s.is_empty())
                        .map(|entry| {
                            let mut parts = entry.splitn(5, '\x01');
                            let release_group_id = parts.next().unwrap_or("").to_string();
                            let release_group_title = parts.next().unwrap_or("").to_string();
                            let track_position = parts.next().and_then(|s| {
                                if s.is_empty() {
                                    None
                                } else {
                                    s.parse().ok()
                                }
                            });
                            let disc_position = parts.next().and_then(|s| {
                                if s.is_empty() {
                                    None
                                } else {
                                    s.parse().ok()
                                }
                            });
                            let disc_total = parts.next().and_then(|s| {
                                if s.is_empty() {
                                    None
                                } else {
                                    s.parse().ok()
                                }
                            });
                            ReleaseInfo {
                                release_group_id,
                                release_group_title,
                                track_position,
                                disc_position,
                                disc_total,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
            },
        })
        .collect();

    // Apply LIMIT post-filter in Rust (track count or duration)
    if let Some(lim) = limit {
        match lim.unit {
            LimitUnit::Tracks => {
                recordings.truncate(lim.value as usize);
            }
            LimitUnit::Minutes | LimitUnit::Hours => {
                let target_ms = match lim.unit {
                    LimitUnit::Minutes => lim.value * 60_000,
                    LimitUnit::Hours => lim.value * 3_600_000,
                    LimitUnit::Tracks => unreachable!(),
                };
                let mut accumulated: i64 = 0;
                recordings.retain(|r| {
                    if accumulated >= target_ms {
                        return false;
                    }
                    accumulated += r.duration_ms.unwrap_or(0);
                    true
                });
            }
        }
    }

    let total_duration_ms = recordings.iter().map(|r| r.duration_ms.unwrap_or(0)).sum();

    Ok(SmartPlaylistResult {
        recordings,
        total_duration_ms,
        sql: full_sql,
    })
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

    *state.recordings_cache.write().await = None;
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

    // Invalidate cache and return fresh list
    *state.recordings_cache.write().await = None;
    let all = std::sync::Arc::new(fetch_all_recordings_from_db(&state.db).await?);
    let result = (*all).clone();
    *state.recordings_cache.write().await = Some(all);
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

async fn load_library_summary(db: &crate::db::DbPool) -> anyhow::Result<LibrarySummary> {
    let mut conn = db.acquire("summary.load_library_summary").await?;
    let recording_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recording")
        .fetch_one(&mut *conn)
        .await?;

    let artist_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artist")
        .fetch_one(&mut *conn)
        .await?;

    let release_group_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_group")
        .fetch_one(&mut *conn)
        .await?;

    let source_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source")
        .fetch_one(&mut *conn)
        .await?;

    Ok(LibrarySummary {
        recording_count,
        artist_count,
        release_group_count,
        source_count,
    })
}

#[tauri::command]
pub async fn get_artist_detail(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<ArtistDetail, String> {
    let mut conn = state
        .db
        .acquire(format!("command.get_artist_detail id={id}"))
        .await
        .map_err(|e| e.to_string())?;

    // Artist base info
    let artist_row = sqlx::query(
        "SELECT a.id, a.name, a.sort_name, a.mbid,
                AVG(ur.stars) AS rating,
                MAX(ph.played_at) AS last_played,
                COUNT(DISTINCT ra.recording_id) AS recording_count
         FROM artist a
         LEFT JOIN recording_artist ra ON ra.artist_id = a.id
         LEFT JOIN user_rating ur ON ur.recording_id = ra.recording_id
         LEFT JOIN play_history ph ON ph.recording_id = ra.recording_id
         WHERE a.id = ?
         GROUP BY a.id",
    )
    .bind(&id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| format!("Artist {id} not found"))?;

    // Release groups for this artist
    let rg_rows = sqlx::query(
        "SELECT rg.id, rg.title, rg.rg_type,
                MIN(rel.release_date) AS release_date,
                COUNT(DISTINCT t.recording_id) AS recording_count,
                (SELECT AVG(ur2.stars)
                 FROM release rel2
                 JOIN medium m2 ON m2.release_id = rel2.id
                 JOIN track t2 ON t2.medium_id = m2.id
                 JOIN user_rating ur2 ON ur2.recording_id = t2.recording_id
                 WHERE rel2.release_group_id = rg.id) AS rating,
                COALESCE(rg.artist_credit_text, rga.credited_as, a2.name) AS artist_credit_name,
                a2.id AS primary_artist_id
         FROM release_group rg
         JOIN release_group_artist rga ON rga.release_group_id = rg.id AND rga.position = 0
         JOIN artist a2 ON a2.id = rga.artist_id
         LEFT JOIN release rel ON rel.release_group_id = rg.id
         LEFT JOIN medium m ON m.release_id = rel.id
         LEFT JOIN track t ON t.medium_id = m.id
         WHERE rga.artist_id = ?
         GROUP BY rg.id
         ORDER BY rg.title",
    )
    .bind(&id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let release_groups: Vec<crate::models::ArtistReleaseGroup> = rg_rows
        .into_iter()
        .map(|row| crate::models::ArtistReleaseGroup {
            id: row.get("id"),
            title: row.get("title"),
            rg_type: row.get("rg_type"),
            release_date: row.get("release_date"),
            recording_count: row.get::<Option<i64>, _>("recording_count").unwrap_or(0),
            rating: row.get("rating"),
            primary_artist_id: row.get("primary_artist_id"),
            artist_credit_name: row.get("artist_credit_name"),
        })
        .collect();

    // Guest appearances — release groups where this artist appears as a
    // secondary artist (e.g. via TXXX=ARTISTS).
    let ga_rg_rows = sqlx::query(
        "SELECT rg.id, rg.title, rg.rg_type,
                MIN(rel.release_date) AS release_date,
                COALESCE(rg.artist_credit_text, rga.credited_as, a2.name) AS artist_credit_name,
                a2.id AS primary_artist_id
         FROM recording_artist ra
         JOIN track t ON t.recording_id = ra.recording_id
         JOIN medium m ON m.id = t.medium_id
         JOIN release rel ON rel.id = m.release_id
         JOIN release_group rg ON rg.id = rel.release_group_id
         LEFT JOIN release_group_artist rga ON rga.release_group_id = rg.id AND rga.position = 0
         LEFT JOIN artist a2 ON a2.id = rga.artist_id
         WHERE ra.artist_id = ?
           AND ra.position > 0
           AND NOT EXISTS (
               SELECT 1 FROM release_group_artist rga2
               WHERE rga2.release_group_id = rg.id
                 AND rga2.artist_id = ra.artist_id
                 AND rga2.position = 0
           )
         GROUP BY rg.id
         ORDER BY rg.title",
    )
    .bind(&id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let mut guest_appearances = Vec::new();
    for rg_row in ga_rg_rows {
        let rg_id: String = rg_row.get("id");

        let track_rows = sqlx::query(
            "SELECT t.recording_id, r.title AS recording_title,
                    t.position AS track_position,
                    m.position AS disc_position
             FROM recording_artist ra
             JOIN track t ON t.recording_id = ra.recording_id
             JOIN medium m ON m.id = t.medium_id
             JOIN release rel ON rel.id = m.release_id
             JOIN release_group rg ON rg.id = rel.release_group_id
             JOIN recording r ON r.id = t.recording_id
             WHERE ra.artist_id = ?
               AND ra.position > 0
               AND rg.id = ?
               AND NOT EXISTS (
                   SELECT 1 FROM release_group_artist rga2
                   WHERE rga2.release_group_id = rg.id
                     AND rga2.artist_id = ra.artist_id
                     AND rga2.position = 0
               )
             ORDER BY m.position, t.position",
        )
        .bind(&id)
        .bind(&rg_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;

        let tracks = track_rows
            .into_iter()
            .map(|trow| GuestAppearanceTrack {
                recording_id: trow.get("recording_id"),
                recording_title: trow.get("recording_title"),
                track_position: trow.get("track_position"),
                disc_position: trow.get("disc_position"),
            })
            .collect();

        guest_appearances.push(GuestAppearanceReleaseGroup {
            id: rg_id,
            title: rg_row.get("title"),
            rg_type: rg_row.get("rg_type"),
            release_date: rg_row.get("release_date"),
            primary_artist_id: rg_row.get("primary_artist_id"),
            artist_credit_name: rg_row.get("artist_credit_name"),
            tracks,
        });
    }

    Ok(ArtistDetail {
        id: artist_row.get("id"),
        name: artist_row.get("name"),
        sort_name: artist_row.get("sort_name"),
        mbid: artist_row.get("mbid"),
        rating: artist_row.get("rating"),
        last_played: artist_row.get("last_played"),
        recording_count: artist_row
            .get::<Option<i64>, _>("recording_count")
            .unwrap_or(0),
        release_group_count: release_groups.len() as i64,
        release_groups,
        guest_appearances,
    })
}

/// For a release, read `track_total` from file tags for sources that have a NULL
/// value in the database (e.g. imported before the column existed).  Returns true
/// if any rows were updated so the caller can re-run its completeness query.
async fn backfill_track_totals_for_release(
    conn: &mut SqliteConnection,
    release_id: &str,
) -> Result<bool, String> {
    let candidates = sqlx::query(
        "SELECT s.id, s.file_path
         FROM source s
         JOIN track t ON t.recording_id = s.recording_id
         JOIN medium m ON m.id = t.medium_id
         WHERE m.release_id = ?
           AND s.source_type = 'local_file'
           AND s.file_path IS NOT NULL
           AND s.track_total IS NULL",
    )
    .bind(release_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    if candidates.is_empty() {
        return Ok(false);
    }

    let mut updated = 0u32;
    for row in &candidates {
        let path: String = row.get("file_path");
        let sid: String = row.get("id");
        match crate::library::scanner::read_metadata(std::path::Path::new(&path)) {
            Ok(result) => {
                if let Some(tt) = result.meta.track_total {
                    let n = sqlx::query("UPDATE source SET track_total = ? WHERE id = ?")
                        .bind(tt as i64)
                        .bind(&sid)
                        .execute(&mut *conn)
                        .await
                        .map_err(|e| e.to_string())?
                        .rows_affected();
                    if n > 0 {
                        updated += 1;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to read tags for track_total backfill ({}): {e:#}",
                    path
                );
            }
        }
    }

    Ok(updated > 0)
}

#[tauri::command]
pub async fn get_release_group_detail(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<ReleaseGroupDetail, String> {
    let mut conn = state
        .db
        .acquire(format!("command.get_release_group_detail id={id}"))
        .await
        .map_err(|e| e.to_string())?;

    // Release group base info
    let rg_row = sqlx::query(
        "SELECT rg.id, rg.title, rg.rg_type,
                COALESCE(rg.artist_credit_text, rga.credited_as, a.name) AS artist_credit_name,
                a.id AS primary_artist_id,
                MIN(rel.release_date) AS release_date,
                (SELECT AVG(ur2.stars)
                 FROM release rel2
                 JOIN medium m2 ON m2.release_id = rel2.id
                 JOIN track t2 ON t2.medium_id = m2.id
                 JOIN user_rating ur2 ON ur2.recording_id = t2.recording_id
                 WHERE rel2.release_group_id = rg.id) AS rating,
                (SELECT MAX(ph2.played_at)
                 FROM release rel2
                 JOIN medium m2 ON m2.release_id = rel2.id
                 JOIN track t2 ON t2.medium_id = m2.id
                 JOIN play_history ph2 ON ph2.recording_id = t2.recording_id
                 WHERE rel2.release_group_id = rg.id) AS last_played
         FROM release_group rg
         LEFT JOIN release_group_artist rga ON rga.release_group_id = rg.id AND rga.position = 0
         LEFT JOIN artist a ON a.id = rga.artist_id
         LEFT JOIN release rel ON rel.release_group_id = rg.id
         WHERE rg.id = ?
         GROUP BY rg.id",
    )
    .bind(&id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| format!("Release group {id} not found"))?;

    // Releases with mediums and tracks
    let release_rows = sqlx::query(
        "SELECT rel.id AS release_id, rel.title AS release_title,
                rel.release_date, rel.country, rel.label, rel.catalog_number
         FROM release rel
         WHERE rel.release_group_id = ?
         ORDER BY rel.release_date, rel.title",
    )
    .bind(&id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let mut releases = Vec::new();
    for rel_row in release_rows {
        let release_id: String = rel_row.get("release_id");

        let medium_rows = sqlx::query(
            "SELECT m.id AS medium_id, m.position, m.format
             FROM medium m
             WHERE m.release_id = ?
             ORDER BY m.position",
        )
        .bind(&release_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;

        let mut mediums = Vec::new();
        for med_row in medium_rows {
            let medium_id: String = med_row.get("medium_id");

            let track_rows = sqlx::query(
                "SELECT t.id, t.position, t.title, t.duration_ms,
                        r.id AS recording_id, r.title AS recording_title,
                        COALESCE(ra.credited_as, a.name) AS artist_credit_name,
                        a.id AS primary_artist_id,
                        CASE WHEN s.id IS NOT NULL THEN 1 ELSE 0 END AS has_source,
                        s.id AS primary_source_id
                 FROM track t
                 JOIN recording r ON r.id = t.recording_id
                 LEFT JOIN recording_artist ra ON ra.recording_id = r.id AND ra.position = 0
                 LEFT JOIN artist a ON a.id = ra.artist_id
                 LEFT JOIN source s ON s.id = (
                     SELECT s2.id FROM source s2
                     WHERE s2.recording_id = r.id
                     ORDER BY CASE s2.source_type WHEN 'local_file' THEN 0 ELSE 1 END, s2.file_path
                     LIMIT 1
                 )
                 WHERE t.medium_id = ?
                 ORDER BY t.position",
            )
            .bind(&medium_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;

            let tracks = track_rows
                .into_iter()
                .map(|trow| {
                    let has_source: i64 = trow.get("has_source");
                    crate::models::TrackDetail {
                        id: trow.get("id"),
                        position: trow.get("position"),
                        title: trow.get("title"),
                        duration_ms: trow.get("duration_ms"),
                        recording_id: trow.get("recording_id"),
                        recording_title: trow.get("recording_title"),
                        artist_credit_name: trow.get("artist_credit_name"),
                        primary_artist_id: trow.get("primary_artist_id"),
                        has_source: has_source != 0,
                        primary_source_id: trow.get("primary_source_id"),
                    }
                })
                .collect();

            mediums.push(crate::models::MediumDetail {
                id: medium_id,
                position: med_row.get("position"),
                format: med_row.get("format"),
                tracks,
            });
        }

        // Determine release completeness.
        let completeness: crate::models::ReleaseCompleteness = {
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
                    MAX(source_track_total) AS consensus_track_total,
                    COUNT(*) AS total_tracks,
                    COALESCE(SUM(has_source), 0) AS tracks_with_sources
                FROM release_source_stats",
            )
            .bind(&release_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;

            let mut distinct: i64 = stats.get("distinct_track_totals");
            let mut consensus: Option<i64> = stats.get("consensus_track_total");
            let total_tracks: i64 = stats.get("total_tracks");
            let with_sources: i64 = stats.get("tracks_with_sources");

            // If no sources have track_total stored but the release has tracks,
            // try to backfill from file tags (supports sources imported before
            // the track_total column existed).
            if distinct == 0 && total_tracks > 0 {
                if backfill_track_totals_for_release(&mut *conn, &release_id).await? {
                    let stats2 = sqlx::query(
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
                            MAX(source_track_total) AS consensus_track_total,
                            COALESCE(SUM(has_source), 0) AS tracks_with_sources
                        FROM release_source_stats",
                    )
                    .bind(&release_id)
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(|e| e.to_string())?;

                    distinct = stats2.get("distinct_track_totals");
                    consensus = stats2.get("consensus_track_total");
                }
            }

            match (total_tracks, distinct, consensus) {
                (0, _, _) => crate::models::ReleaseCompleteness::Unknown {
                    reason: "No tracks on this release.".to_string(),
                    disagreement_groups: Vec::new(),
                },
                (_, 0, _) => crate::models::ReleaseCompleteness::Unknown {
                    reason: "No source files provide track_total information.".to_string(),
                    disagreement_groups: Vec::new(),
                },
                (_, 1, Some(0)) => crate::models::ReleaseCompleteness::Unknown {
                    reason: "Track total is zero.".to_string(),
                    disagreement_groups: Vec::new(),
                },
                (_, 1, _) if with_sources >= consensus.unwrap_or(0) => {
                    crate::models::ReleaseCompleteness::Complete
                }
                (_, 1, _) if with_sources < total_tracks => {
                    use nonempty::NonEmpty;

                    let missing_vec: Vec<crate::models::MissingTrackDetail> = sqlx::query(
                        "SELECT t.position AS track_position,
                                m.position AS disc_position,
                                COALESCE(t.title, r.title) AS title,
                                r.id AS recording_id
                         FROM medium m
                         JOIN track t ON t.medium_id = m.id
                         JOIN recording r ON r.id = t.recording_id
                         WHERE m.release_id = ?
                           AND NOT EXISTS (SELECT 1 FROM source s WHERE s.recording_id = r.id)
                         ORDER BY m.position, t.position",
                    )
                    .bind(&release_id)
                    .fetch_all(&mut *conn)
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|row| crate::models::MissingTrackDetail {
                        disc_position: row.get("disc_position"),
                        track_position: row.get("track_position"),
                        title: row.get("title"),
                        recording_id: Some(row.get("recording_id")),
                    })
                    .collect();

                    let missing = NonEmpty::from_vec(missing_vec).unwrap_or_else(|| {
                        panic!(
                            "incomplete arm reached but no missing tracks found. \
                             release_id={release_id}, total_tracks={total_tracks}, \
                             with_sources={with_sources}, consensus={consensus:?}, distinct={distinct}"
                        )
                    });

                    crate::models::ReleaseCompleteness::Incomplete {
                        missing_tracks: missing,
                    }
                }
                (_, 1, _) => {
                    use nonempty::NonEmpty;

                    let consensus_val = consensus.unwrap_or(0);

                    let missing_vec: Vec<crate::models::MissingTrackDetail> = sqlx::query(
                        "WITH RECURSIVE positions(n) AS (
                            SELECT 1
                            UNION ALL
                            SELECT n + 1 FROM positions WHERE n < ?
                        )
                        SELECT m.position AS disc_position,
                               p.n AS track_position
                        FROM medium m
                        CROSS JOIN positions p
                        WHERE m.release_id = ? AND p.n <= ?
                          AND NOT EXISTS (
                              SELECT 1 FROM track t
                              WHERE t.medium_id = m.id AND t.position = p.n
                          )
                        ORDER BY m.position, p.n",
                    )
                    .bind(consensus_val)
                    .bind(&release_id)
                    .bind(consensus_val)
                    .fetch_all(&mut *conn)
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|row| crate::models::MissingTrackDetail {
                        disc_position: row.get("disc_position"),
                        track_position: row.get("track_position"),
                        title: String::new(),
                        recording_id: None,
                    })
                    .collect();

                    let missing = NonEmpty::from_vec(missing_vec).unwrap_or_else(|| {
                        panic!(
                            "phantom-track arm reached but no missing positions found. \
                             release_id={release_id}, total_tracks={total_tracks}, \
                             with_sources={with_sources}, consensus={consensus:?}, distinct={distinct}"
                        )
                    });

                    crate::models::ReleaseCompleteness::Incomplete {
                        missing_tracks: missing,
                    }
                }
                (_, _, _) => {
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
                    .bind(&release_id)
                    .fetch_all(&mut *conn)
                    .await
                    .map_err(|e| e.to_string())?;

                    let release_disc_total: Option<i64> =
                        source_data.first().and_then(|r| r.get("disc_total"));

                    struct SourceEntry {
                        file_path: Option<String>,
                        track_total: i64,
                        disc_position: Option<i64>,
                    }

                    let entries: Vec<SourceEntry> = source_data
                        .iter()
                        .map(|row| SourceEntry {
                            file_path: row.get("file_path"),
                            track_total: row.get("track_total"),
                            disc_position: row.get("disc_position"),
                        })
                        .collect();

                    // Group by track_total
                    let mut by_tt: BTreeMap<i64, Vec<String>> = BTreeMap::new();
                    for e in &entries {
                        if let Some(ref fp) = e.file_path {
                            by_tt.entry(e.track_total).or_default().push(fp.clone());
                        }
                    }

                    // Check for multi-disc unanimous per-disc agreement.
                    // Sources on different discs naturally report different track_total values
                    // (one per disc), which would make COUNT(DISTINCT) > 1 and enter this
                    // disagreement arm. If all sources on each disc agree, there's no real
                    // disagreement — just per-disc differences.
                    let unanimous_completeness: Option<crate::models::ReleaseCompleteness> = 'check: {
                        match release_disc_total {
                            Some(dt) if dt > 1 => dt,
                            _ => break 'check None,
                        };

                        let mut disc_map: BTreeMap<i64, BTreeMap<i64, usize>> = BTreeMap::new();
                        for e in &entries {
                            if let Some(dp) = e.disc_position {
                                *disc_map
                                    .entry(dp)
                                    .or_default()
                                    .entry(e.track_total)
                                    .or_default() += 1;
                            }
                        }

                        if disc_map.is_empty() {
                            break 'check None;
                        }

                        // Compare per-disc source claims against actual DB track counts.
                        // If claims match DB per disc, use standard completeness logic.
                        // If sources claim more tracks per disc than the DB has, there
                        // are phantom tracks (claimed by sources but absent from MusicBrainz).
                        let disc_actuals: Vec<(i64, i64)> = sqlx::query(
                            "SELECT m.position, COUNT(t.id) AS track_count
                             FROM medium m
                             JOIN track t ON t.medium_id = m.id
                             WHERE m.release_id = ?
                             GROUP BY m.position
                             ORDER BY m.position",
                        )
                        .bind(&release_id)
                        .fetch_all(&mut *conn)
                        .await
                        .map_err(|e| e.to_string())?
                        .iter()
                        .map(|row| (row.get("position"), row.get("track_count")))
                        .collect();

                        // If any disc has sources reporting multiple different track_total
                        // values, try to find a consensus via per-disc mode. If the mode-
                        // based layout matches the DB, proceed with standard completeness.
                        if !disc_map.values().all(|tts| tts.len() == 1) {
                            tracing::warn!(
                                release_id,
                                ?disc_map,
                                "multi-disc unanimity: intra-disc disagreement",
                            );

                            let mode_claims: BTreeMap<i64, i64> = disc_map
                                .iter()
                                .map(|(&disc, tts)| {
                                    let mode = tts
                                        .iter()
                                        .max_by_key(|&(_, &c)| c)
                                        .map(|(&v, _)| v)
                                        .unwrap_or(0);
                                    (disc, mode)
                                })
                                .collect();

                            let modes_match = disc_actuals.iter().all(|(pos, actual_count)| {
                                mode_claims
                                    .get(pos)
                                    .map(|&claimed| claimed == *actual_count)
                                    .unwrap_or(false)
                            });

                            if modes_match {
                                // Replace disc_map with only the mode values so the existing
                                // completeness logic can proceed with the consensus layout.
                                disc_map = disc_map
                                    .into_iter()
                                    .map(|(disc, tts)| {
                                        let mode = mode_claims.get(&disc).copied().unwrap_or(0);
                                        let mut new_tts = BTreeMap::new();
                                        new_tts.insert(mode, tts.get(&mode).copied().unwrap_or(0));
                                        (disc, new_tts)
                                    })
                                    .collect();
                            } else {
                                break 'check None;
                            }
                        }

                        let all_discs_match = disc_actuals.iter().all(|(pos, actual_count)| {
                            disc_map
                                .get(pos)
                                .and_then(|tts| tts.keys().next())
                                .map(|&claimed| claimed == *actual_count)
                                .unwrap_or(false)
                        });

                        if all_discs_match {
                            // Source claims match DB — standard completeness logic.
                            if with_sources >= total_tracks {
                                break 'check Some(crate::models::ReleaseCompleteness::Complete);
                            }

                            // Some tracks have no source files — report which ones.
                            use nonempty::NonEmpty;
                            let missing_vec: Vec<crate::models::MissingTrackDetail> = sqlx::query(
                                "SELECT t.position AS track_position,
                                            m.position AS disc_position,
                                            COALESCE(t.title, r.title) AS title,
                                            r.id AS recording_id
                                     FROM medium m
                                     JOIN track t ON t.medium_id = m.id
                                     JOIN recording r ON r.id = t.recording_id
                                     WHERE m.release_id = ?
                                       AND NOT EXISTS (
                                           SELECT 1 FROM source s
                                           WHERE s.recording_id = r.id
                                       )
                                     ORDER BY m.position, t.position",
                            )
                            .bind(&release_id)
                            .fetch_all(&mut *conn)
                            .await
                            .map_err(|e| e.to_string())?
                            .into_iter()
                            .map(|row| crate::models::MissingTrackDetail {
                                disc_position: row.get("disc_position"),
                                track_position: row.get("track_position"),
                                title: row.get("title"),
                                recording_id: Some(row.get("recording_id")),
                            })
                            .collect();

                            let missing = NonEmpty::from_vec(missing_vec).unwrap_or_else(|| {
                                panic!(
                                    "unanimous multi-disc incomplete reached but no missing \
                                     tracks. release_id={release_id}, total_tracks={total_tracks}, \
                                     with_sources={with_sources}"
                                )
                            });

                            break 'check Some(crate::models::ReleaseCompleteness::Incomplete {
                                missing_tracks: missing,
                            });
                        }

                        // Source claims differ from DB. If any disc has sources claiming
                        // *fewer* tracks than the DB, that's an anomalous deficit — bail
                        // to the disagreement display for investigation.
                        let has_deficit = disc_actuals.iter().any(|(pos, actual_count)| {
                            disc_map
                                .get(pos)
                                .and_then(|tts| tts.keys().next())
                                .map(|&claimed| claimed < *actual_count)
                                .unwrap_or(false)
                        });

                        if has_deficit {
                            tracing::warn!(
                                release_id,
                                ?disc_map,
                                ?disc_actuals,
                                "multi-disc unanimity: source claims below DB count",
                            );
                            break 'check None;
                        }

                        // Phantom tracks: sources claim more tracks per disc than the DB has.
                        // Build a combined missing list from:
                        //   1. DB tracks that lack source files (Type A)
                        //   2. Track positions claimed by sources but absent from DB (Type B)
                        use nonempty::NonEmpty;
                        let mut all_missing: Vec<crate::models::MissingTrackDetail> = Vec::new();

                        // Type A: DB tracks without sources
                        let type_a: Vec<crate::models::MissingTrackDetail> = sqlx::query(
                            "SELECT t.position AS track_position,
                                    m.position AS disc_position,
                                    COALESCE(t.title, r.title) AS title,
                                    r.id AS recording_id
                             FROM medium m
                             JOIN track t ON t.medium_id = m.id
                             JOIN recording r ON r.id = t.recording_id
                             WHERE m.release_id = ?
                               AND NOT EXISTS (SELECT 1 FROM source s WHERE s.recording_id = r.id)
                             ORDER BY m.position, t.position",
                        )
                        .bind(&release_id)
                        .fetch_all(&mut *conn)
                        .await
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .map(|row| crate::models::MissingTrackDetail {
                            disc_position: row.get("disc_position"),
                            track_position: row.get("track_position"),
                            title: row.get("title"),
                            recording_id: Some(row.get("recording_id")),
                        })
                        .collect();

                        all_missing.extend(type_a);

                        // Type B: phantom tracks on discs where claimed > actual
                        for (pos, actual_count) in &disc_actuals {
                            if let Some(claimed_count) =
                                disc_map.get(pos).and_then(|tts| tts.keys().next())
                            {
                                if *claimed_count > *actual_count {
                                    let phantom_positions: Vec<i64> = sqlx::query(
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
                                    .bind(&release_id)
                                    .bind(pos)
                                    .fetch_all(&mut *conn)
                                    .await
                                    .map_err(|e| e.to_string())?
                                    .iter()
                                    .map(|row| row.get("track_position"))
                                    .collect();

                                    for tp in phantom_positions {
                                        all_missing.push(crate::models::MissingTrackDetail {
                                            disc_position: *pos,
                                            track_position: tp,
                                            title: String::new(),
                                            recording_id: None,
                                        });
                                    }
                                }
                            }
                        }

                        let missing_count = all_missing.len();
                        let missing = NonEmpty::from_vec(all_missing).unwrap_or_else(|| {
                            panic!(
                                "phantom path reached but no missing tracks. \
                                 release_id={release_id}, disc_actuals={disc_actuals:?}, \
                                 disc_map={disc_map:?}"
                            )
                        });

                        tracing::warn!(
                            release_id,
                            ?disc_map,
                            ?disc_actuals,
                            missing_count,
                            "multi-disc unanimity: phantom tracks detected",
                        );

                        break 'check Some(crate::models::ReleaseCompleteness::Incomplete {
                            missing_tracks: missing,
                        });
                    };

                    if let Some(c) = unanimous_completeness {
                        c
                    } else {
                        // Genuine disagreement — build the Unknown display with
                        // grouped source paths so the user can inspect the claims.
                        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
                        let mut parts: Vec<String> = Vec::new();

                        if let Some(dt) = release_disc_total {
                            if dt > 1 {
                                // Multi-disc: merge all track_total groups into one
                                let mut disc_map: BTreeMap<i64, BTreeMap<i64, usize>> =
                                    BTreeMap::new();
                                let mut all_paths: Vec<String> = Vec::new();
                                for e in &entries {
                                    if let Some(ref fp) = e.file_path {
                                        all_paths.push(fp.clone());
                                    }
                                    if let Some(dp) = e.disc_position {
                                        *disc_map
                                            .entry(dp)
                                            .or_default()
                                            .entry(e.track_total)
                                            .or_default() += 1;
                                    }
                                }
                                all_paths.sort();
                                all_paths.dedup();

                                if !disc_map.is_empty() {
                                    tracing::warn!(
                                        release_id,
                                        ?disc_map,
                                        "multi-disc disagreement display: producing single layout from merged claims",
                                    );

                                    let per_disc: Vec<String> = disc_map
                                        .iter()
                                        .map(|(_, tts)| {
                                            let best = tts
                                                .iter()
                                                .max_by_key(|&(_, &c)| c)
                                                .map(|(v, _)| v.to_string())
                                                .unwrap_or_default();
                                            best
                                        })
                                        .collect();

                                    let cnt = all_paths.len();
                                    let desc = format!(
                                        "{} disc{} with {} track{}",
                                        dt,
                                        if dt == 1 { "" } else { "s" },
                                        per_disc.join("+"),
                                        if cnt == 1 { "" } else { "s" }
                                    );
                                    let claim_word = if cnt == 1 { "claims" } else { "claim" };
                                    parts.push(format!(
                                        "{} from {} source{}",
                                        desc,
                                        cnt,
                                        if cnt == 1 { "" } else { "s" }
                                    ));
                                    groups.push((
                                        format!("{} source {} {}", cnt, claim_word, desc),
                                        all_paths,
                                    ));
                                } else {
                                    // Multi-disc release but no disc_position info:
                                    // keep per-track_total groups
                                    for (tt, paths) in &by_tt {
                                        let cnt = paths.len();
                                        let claim_word = if cnt == 1 { "claims" } else { "claim" };
                                        let desc = format!(
                                            "{} disc with {} track{}",
                                            dt,
                                            tt,
                                            if *tt == 1 { "" } else { "s" }
                                        );
                                        parts.push(format!(
                                            "{} from {} source{}",
                                            tt,
                                            cnt,
                                            if cnt == 1 { "" } else { "s" }
                                        ));
                                        groups.push((
                                            format!("{} source {} {}", cnt, claim_word, desc),
                                            paths.clone(),
                                        ));
                                    }
                                }
                            } else {
                                // Single disc: group by track_total
                                for (tt, paths) in &by_tt {
                                    let cnt = paths.len();
                                    let claim_word = if cnt == 1 { "claims" } else { "claim" };
                                    let desc = format!(
                                        "1 disc with {} track{}",
                                        tt,
                                        if *tt == 1 { "" } else { "s" }
                                    );
                                    parts.push(format!(
                                        "{} from {} source{}",
                                        tt,
                                        cnt,
                                        if cnt == 1 { "" } else { "s" }
                                    ));
                                    groups.push((
                                        format!("{} source {} {}", cnt, claim_word, desc),
                                        paths.clone(),
                                    ));
                                }
                            }
                        } else {
                            // No disc info: group by track_total
                            for (tt, paths) in &by_tt {
                                let cnt = paths.len();
                                let claim_word = if cnt == 1 { "claims" } else { "claim" };
                                let desc = format!(
                                    "(no total disc info) {} track{}",
                                    tt,
                                    if *tt == 1 { "" } else { "s" }
                                );
                                parts.push(format!(
                                    "{} from {} source{} (no disc info)",
                                    tt,
                                    cnt,
                                    if cnt == 1 { "" } else { "s" }
                                ));
                                groups.push((
                                    format!("{} source {} {}", cnt, claim_word, desc),
                                    paths.clone(),
                                ));
                            }
                        }

                        crate::models::ReleaseCompleteness::Unknown {
                            reason: format!(
                                "Sources disagree on total track count: {}",
                                parts.join(", ")
                            ),
                            disagreement_groups: groups
                                .into_iter()
                                .map(|(description, source_paths)| {
                                    crate::models::SourceDisagreementGroup {
                                        description,
                                        source_paths,
                                    }
                                })
                                .collect(),
                        }
                    }
                }
            }
        };

        releases.push(crate::models::ReleaseDetail {
            id: release_id,
            title: rel_row.get("release_title"),
            release_date: rel_row.get("release_date"),
            country: rel_row.get("country"),
            label: rel_row.get("label"),
            catalog_number: rel_row.get("catalog_number"),
            mediums,
            completeness,
        });
    }

    Ok(ReleaseGroupDetail {
        id: rg_row.get("id"),
        title: rg_row.get("title"),
        rg_type: rg_row.get("rg_type"),
        artist_credit_name: rg_row.get("artist_credit_name"),
        primary_artist_id: rg_row.get("primary_artist_id"),
        rating: rg_row.get("rating"),
        last_played: rg_row.get("last_played"),
        release_date: rg_row.get("release_date"),
        releases,
    })
}

#[tauri::command]
pub async fn get_recording_detail(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<RecordingDetail, String> {
    let mut conn = state
        .db
        .acquire(format!("command.get_recording_detail id={id}"))
        .await
        .map_err(|e| e.to_string())?;

    let row = sqlx::query(
        "SELECT r.id, r.title, r.duration_ms, r.genre, r.bpm, r.comment,
                r.artist_credit_text, r.mbid, r.acoustid,
                COALESCE(ra.credited_as, a.name) AS artist_credit_name,
                a.id AS primary_artist_id,
                ur.stars AS rating,
                COALESCE(ph.play_count, 0) AS play_count,
                ph.last_played
         FROM recording r
         LEFT JOIN recording_artist ra ON ra.recording_id = r.id AND ra.position = 0
         LEFT JOIN artist a ON a.id = ra.artist_id
         LEFT JOIN user_rating ur ON ur.recording_id = r.id
         LEFT JOIN (
             SELECT recording_id, COUNT(*) AS play_count, MAX(played_at) AS last_played
             FROM play_history GROUP BY recording_id
         ) ph ON ph.recording_id = r.id
         WHERE r.id = ?",
    )
    .bind(&id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| format!("Recording {id} not found"))?;

    // Releases for this recording
    let release_rows = sqlx::query(
        "SELECT rg.id AS release_group_id, rg.title AS release_group_title,
                t.position AS track_position, m.position AS disc_position,
                (SELECT MAX(m3.position) FROM medium m3 WHERE m3.release_id = rel.id) AS disc_total
         FROM track t
         JOIN medium m ON m.id = t.medium_id
         JOIN release rel ON rel.id = m.release_id
         JOIN release_group rg ON rg.id = rel.release_group_id
         WHERE t.recording_id = ?
         ORDER BY rg.title, m.position, t.position",
    )
    .bind(&id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let releases = release_rows
        .into_iter()
        .map(|rrow| ReleaseInfo {
            release_group_id: rrow.get("release_group_id"),
            release_group_title: rrow.get("release_group_title"),
            track_position: rrow.get("track_position"),
            disc_position: rrow.get("disc_position"),
            disc_total: rrow.get("disc_total"),
        })
        .collect();

    // All artists for this recording
    let artist_rows = sqlx::query(
        "SELECT a.id AS artist_id, a.name,
                ra.position, ra.role, ra.credited_as
         FROM recording_artist ra
         JOIN artist a ON a.id = ra.artist_id
         WHERE ra.recording_id = ?
         ORDER BY ra.position",
    )
    .bind(&id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let artists = artist_rows
        .into_iter()
        .map(|rrow| RecordingArtistInfo {
            artist_id: rrow.get("artist_id"),
            name: rrow.get("name"),
            position: rrow.get("position"),
            role: rrow.get("role"),
            credited_as: rrow.get("credited_as"),
        })
        .collect();

    // Sources for this recording
    let source_rows = sqlx::query(
        "SELECT s.id, s.source_type, s.file_path, s.format, s.duration_ms,
                s.replay_gain_track_db, s.replay_gain_track_peak
         FROM source s
         WHERE s.recording_id = ?
         ORDER BY s.source_type, s.file_path",
    )
    .bind(&id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let mut sources = Vec::new();
    for srow in source_rows {
        let file_path: Option<String> = srow.get("file_path");
        let source_type: String = srow.get("source_type");

        let tags = if let Some(ref path_str) = file_path {
            if source_type == "local_file" {
                crate::library::scanner::list_all_tags(std::path::Path::new(path_str))
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        sources.push(SourceDetail {
            id: srow.get("id"),
            source_type,
            file_path,
            format: srow.get("format"),
            duration_ms: srow.get("duration_ms"),
            replay_gain_track_db: srow.get("replay_gain_track_db"),
            replay_gain_track_peak: srow.get("replay_gain_track_peak"),
            tags,
        });
    }

    Ok(RecordingDetail {
        id: row.get("id"),
        title: row.get("title"),
        duration_ms: row.get("duration_ms"),
        genre: row.get("genre"),
        bpm: row.get("bpm"),
        comment: row.get("comment"),
        artist_credit_name: row.get("artist_credit_name"),
        primary_artist_id: row.get("primary_artist_id"),
        artist_credit_text: row.get("artist_credit_text"),
        mbid: row.get("mbid"),
        acoustid: row.get("acoustid"),
        rating: row.get("rating"),
        play_count: row.get::<Option<i64>, _>("play_count").unwrap_or(0),
        last_played: row.get("last_played"),
        artists,
        releases,
        sources,
    })
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

    *state.recordings_cache.write().await = None;

    result
}

#[cfg(test)]
mod tests {
    use super::*;

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
