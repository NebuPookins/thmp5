use crate::audio::PlayRequest as EnginePlayRequest;
use crate::file_issues::FileIssue;
use crate::library::import::{import_paths as do_import, rescan_source as do_rescan_source};
use crate::models::{
    AppBootstrap, AppConfig, ArtistRow, DbPoolDebugSnapshot, ExternalCommand,
    FixMergedRecordingsStats, Id3FrameDebugInfo, Id3FrameDebugRequest, ImportProgress, ImportStats,
    InitialSetupRequest, LibrarySummary, PlayHistoryInput, PlayRequest, PlayerState, PlaylistRow,
    QueueSettingsUpdate, RatingUpdateRequest, RecordingRatingUpdateResult, RecordingRow,
    ReleaseGroupRow, ReleaseInfo, SaveSmartPlaylistRequest, SeekRequest, SmartPlaylistResult,
    VolumeRequest,
};
use crate::query::{self, LimitUnit};
use crate::AppState;
use sqlx::{Row, Sqlite, Transaction};
use std::sync::Arc;

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
  releases_agg AS (
    SELECT recording_id, GROUP_CONCAT(entry, char(0)) AS releases_raw
    FROM (
      SELECT t2.recording_id,
             rg2.id || char(1) ||
             rg2.title || char(1) ||
             COALESCE(CAST(t2.position AS TEXT), '') || char(1) ||
             COALESCE(CAST(m2.position AS TEXT), '') AS entry
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
pub async fn rescan_source(state: tauri::State<'_, AppState>, path: String) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("Source path cannot be empty.".to_string());
    }

    let source_path = std::path::Path::new(trimmed);
    if !source_path.is_file() {
        return Err(format!("Source file is missing: {}", source_path.display()));
    }

    do_rescan_source(&state.db, source_path, state.acoustid_api_key.as_deref())
        .await
        .map_err(|e| format!("Failed to rescan {}:\n{e:#}", source_path.display()))?;
    *state.recordings_cache.write().await = None;
    Ok(())
}

#[tauri::command]
pub async fn rescan_sources(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<(), String> {
    if paths.is_empty() {
        return Err("At least one source path is required.".to_string());
    }

    let mut failures = Vec::new();

    for path in paths {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err("Source path cannot be empty.".to_string());
        }

        let source_path = std::path::Path::new(trimmed);
        if !source_path.is_file() {
            failures.push(format!("{}: Source file is missing", source_path.display()));
            continue;
        }

        if let Err(error) =
            do_rescan_source(&state.db, source_path, state.acoustid_api_key.as_deref()).await
        {
            failures.push(format!("{}:\n{error:#}", source_path.display()));
        }
    }

    *state.recordings_cache.write().await = None;
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

#[tauri::command]
pub fn debug_id3_text_frame(request: Id3FrameDebugRequest) -> Result<Id3FrameDebugInfo, String> {
    crate::library::scanner::debug_id3_text_frame(
        std::path::Path::new(&request.path),
        &request.frame_id,
    )
    .map_err(|e| e.to_string())
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
            COALESCE(ra.credited_as, a.name) AS artist
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

    // If the file is missing, try to fall back to another source for the same recording.
    if !std::path::Path::new(&file_path).exists() {
        tracing::warn!(
            source_id = %source_id,
            path = %file_path,
            "Source file missing; searching for alternatives"
        );

        let alts = sqlx::query(
            "SELECT s.id, s.file_path FROM source s
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

        let mut found_alt: Option<(String, String)> = None;
        for alt in &alts {
            let alt_id: String = alt.get("id");
            let alt_path: String = alt.get("file_path");
            if std::path::Path::new(&alt_path).exists() {
                found_alt = Some((alt_id, alt_path));
                break;
            }
        }

        if let Some((alt_id, alt_path)) = found_alt {
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
        })
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
                        let mut parts = entry.splitn(4, '\x01');
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
                        ReleaseInfo {
                            release_group_id,
                            release_group_title,
                            track_position,
                            disc_position,
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
         WHERE (? IS NULL OR a.id = ?)
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
                            let mut parts = entry.splitn(4, '\x01');
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
                            ReleaseInfo {
                                release_group_id,
                                release_group_title,
                                track_position,
                                disc_position,
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
        "DELETE FROM artist
         WHERE NOT EXISTS (
             SELECT 1
             FROM recording_artist
             WHERE recording_artist.artist_id = artist.id
         )
           AND NOT EXISTS (
             SELECT 1
             FROM release_group_artist
             WHERE release_group_artist.artist_id = artist.id
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
