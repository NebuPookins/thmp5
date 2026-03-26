use crate::audio::PlayRequest as EnginePlayRequest;
use crate::library::import::import_paths as do_import;
use crate::models::{
    AppBootstrap, AppConfig, ArtistRow, ImportProgress, ImportStats, InitialSetupRequest,
    LibrarySummary, PlayHistoryInput, PlaylistRow, PlayRequest, PlayerState, QueueSettingsUpdate,
    RatingUpdateRequest, RecordingRow, ReleaseGroupRow, SaveSmartPlaylistRequest, SeekRequest,
    SmartPlaylistResult, VolumeRequest,
};
use crate::query::{self, LimitUnit};
use crate::AppState;
use sqlx::Row;

// ── Import ────────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn import_paths(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<ImportStats, String> {
    do_import(&state.db, paths, state.acoustid_api_key.as_deref())
        .await
        .map_err(|e| e.to_string())
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
pub async fn record_play_history(
    state: tauri::State<'_, AppState>,
    input: PlayHistoryInput,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO play_history (recording_id, source_id, duration_played_ms)
         VALUES (?, ?, ?)",
    )
    .bind(input.recording_id)
    .bind(input.source_id)
    .bind(input.duration_played_ms)
    .execute(&state.db)
    .await
    .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
pub async fn set_recording_rating(
    state: tauri::State<'_, AppState>,
    request: RatingUpdateRequest,
) -> Result<(), String> {
    validate_rating(request.stars)?;

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
        .execute(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    } else {
        sqlx::query("DELETE FROM user_rating WHERE recording_id = ?")
            .bind(&request.id)
            .execute(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    }

    Ok(())
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
pub async fn play(
    state: tauri::State<'_, AppState>,
    request: PlayRequest,
) -> Result<PlayerState, String> {
    tracing::info!(source_id = %request.source_id, "Play command received");
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
    .fetch_optional(&state.db)
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "Playable local source not found.".to_string())?;

    let source_id: String = row.get("source_id");
    let file_path: String = row.get("file_path");
    let recording_id: String = row.get("recording_id");
    let title: Option<String> = row.get("title");
    let artist: Option<String> = row.get("artist");

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

#[tauri::command]
pub async fn list_recordings(
    state: tauri::State<'_, AppState>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<RecordingRow>, String> {
    let db = &state.db;
    let limit = limit.unwrap_or(200);
    let offset = offset.unwrap_or(0);

    let rows = sqlx::query(
        "SELECT
            r.id,
            r.title,
            r.duration_ms,
            a.id                  AS primary_artist_id,
            rg.id                 AS release_group_id,
            COALESCE(ra.credited_as, a.name) AS artist_credit_name,
            rg.title             AS release_group_title,
            r.genre,
            rel.release_date,
            t.position           AS track_position,
            m.position           AS disc_position,
            ur.stars             AS rating,
            COUNT(ph.id)         AS play_count,
            MAX(ph.played_at)    AS last_played,
            (
                SELECT s.id
                FROM source s
                WHERE s.recording_id = r.id
                  AND s.source_type = 'local_file'
                  AND s.file_path IS NOT NULL
                ORDER BY s.file_path
                LIMIT 1
            ) AS primary_source_id,
            (
                SELECT s.file_path
                FROM source s
                WHERE s.recording_id = r.id
                  AND s.source_type = 'local_file'
                  AND s.file_path IS NOT NULL
                ORDER BY s.file_path
                LIMIT 1
            ) AS primary_source_path,
            (
                SELECT GROUP_CONCAT(rt.tag, char(0))
                FROM recording_tag rt
                WHERE rt.recording_id = r.id
                ORDER BY rt.tag
            ) AS tags_raw
         FROM recording r
         LEFT JOIN recording_artist ra         ON ra.recording_id = r.id AND ra.position = 0
         LEFT JOIN artist a                    ON a.id = ra.artist_id
         LEFT JOIN track t                     ON t.recording_id = r.id
         LEFT JOIN medium m                    ON m.id = t.medium_id
         LEFT JOIN release rel                 ON rel.id = m.release_id
         LEFT JOIN release_group rg            ON rg.id = rel.release_group_id
         LEFT JOIN user_rating ur              ON ur.recording_id = r.id
         LEFT JOIN play_history ph             ON ph.recording_id = r.id
         GROUP BY r.id
         ORDER BY lower(a.name), lower(rg.title), m.position, t.position
         LIMIT ? OFFSET ?",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;

    let recordings = rows
        .into_iter()
        .map(|row| RecordingRow {
            id: row.get("id"),
            title: row.get("title"),
            duration_ms: row.get("duration_ms"),
            primary_artist_id: row.get("primary_artist_id"),
            release_group_id: row.get("release_group_id"),
            artist_credit_name: row.get("artist_credit_name"),
            release_group_title: row.get("release_group_title"),
            genre: row.get("genre"),
            release_date: row.get("release_date"),
            track_position: row.get("track_position"),
            disc_position: row.get("disc_position"),
            rating: row.get("rating"),
            play_count: row.get::<Option<i64>, _>("play_count").unwrap_or(0),
            last_played: row.get("last_played"),
            primary_source_id: row.get("primary_source_id"),
            primary_source_path: row.get("primary_source_path"),
            tags: {
                let raw: Option<String> = row.get("tags_raw");
                raw.map(|r| r.split('\0').filter(|s| !s.is_empty()).map(String::from).collect())
                   .unwrap_or_default()
            },
        })
        .collect();

    Ok(recordings)
}

#[tauri::command]
pub async fn list_artists(state: tauri::State<'_, AppState>) -> Result<Vec<ArtistRow>, String> {
    let db = &state.db;

    let rows = sqlx::query(
        "SELECT
            a.id,
            a.name,
            a.sort_name,
            COUNT(DISTINCT rga.release_group_id) AS release_group_count,
            COUNT(DISTINCT ra.recording_id)      AS recording_count,
            AVG(ur.stars)                        AS rating
         FROM artist a
         LEFT JOIN recording_artist ra      ON ra.artist_id = a.id
         LEFT JOIN release_group_artist rga ON rga.artist_id = a.id
         LEFT JOIN user_rating ur           ON ur.recording_id = ra.recording_id
         GROUP BY a.id
         ORDER BY lower(a.sort_name)",
    )
    .fetch_all(db)
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
    let db = &state.db;
    let artist_filter = artist_id.as_deref();
    let search_filter = search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

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
            )                                                        AS rating
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
    .fetch_all(db)
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
        })
        .collect();

    Ok(release_groups)
}

#[tauri::command]
pub async fn get_cover_art(
    state: tauri::State<'_, AppState>,
    recording_id: String,
) -> Result<Option<String>, String> {
    let file_path: Option<String> = sqlx::query_scalar(
        "SELECT file_path FROM source
         WHERE recording_id = ? AND source_type = 'local_file' AND file_path IS NOT NULL
         ORDER BY file_path LIMIT 1",
    )
    .bind(&recording_id)
    .fetch_optional(&state.db)
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
    let tags = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT tag FROM recording_tag ORDER BY tag",
    )
    .fetch_all(&state.db)
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
        "SELECT
            r.id,
            r.title,
            r.duration_ms,
            a.id                  AS primary_artist_id,
            rg.id                 AS release_group_id,
            COALESCE(ra.credited_as, a.name) AS artist_credit_name,
            rg.title             AS release_group_title,
            r.genre,
            rel.release_date,
            t.position           AS track_position,
            m.position           AS disc_position,
            ur.stars             AS rating,
            COUNT(ph.id)         AS play_count,
            MAX(ph.played_at)    AS last_played,
            (
                SELECT s.id FROM source s
                WHERE s.recording_id = r.id AND s.source_type = 'local_file'
                  AND s.file_path IS NOT NULL
                ORDER BY s.file_path LIMIT 1
            ) AS primary_source_id,
            (
                SELECT s.file_path FROM source s
                WHERE s.recording_id = r.id AND s.source_type = 'local_file'
                  AND s.file_path IS NOT NULL
                ORDER BY s.file_path LIMIT 1
            ) AS primary_source_path,
            (
                SELECT GROUP_CONCAT(rt.tag, char(0))
                FROM recording_tag rt WHERE rt.recording_id = r.id ORDER BY rt.tag
            ) AS tags_raw
         FROM recording r
         LEFT JOIN recording_artist ra ON ra.recording_id = r.id AND ra.position = 0
         LEFT JOIN artist a             ON a.id = ra.artist_id
         LEFT JOIN track t              ON t.recording_id = r.id
         LEFT JOIN medium m             ON m.id = t.medium_id
         LEFT JOIN release rel          ON rel.id = m.release_id
         LEFT JOIN release_group rg     ON rg.id = rel.release_group_id
         LEFT JOIN user_rating ur       ON ur.recording_id = r.id
         LEFT JOIN play_history ph      ON ph.recording_id = r.id
         WHERE r.id IN (
             SELECT spv.id FROM smart_playlist_view spv WHERE {}
         )
         GROUP BY r.id
         ORDER BY RANDOM()",
        where_sql
    );

    tracing::debug!(sql = %full_sql, "Evaluating smart playlist");

    let rows = sqlx::query(&full_sql)
        .fetch_all(&state.db)
        .await
        .map_err(|e| format!("Query error: {e}"))?;

    let mut recordings: Vec<_> = rows
        .into_iter()
        .map(|row| RecordingRow {
            id: row.get("id"),
            title: row.get("title"),
            duration_ms: row.get("duration_ms"),
            primary_artist_id: row.get("primary_artist_id"),
            release_group_id: row.get("release_group_id"),
            artist_credit_name: row.get("artist_credit_name"),
            release_group_title: row.get("release_group_title"),
            genre: row.get("genre"),
            release_date: row.get("release_date"),
            track_position: row.get("track_position"),
            disc_position: row.get("disc_position"),
            rating: row.get("rating"),
            play_count: row.get::<Option<i64>, _>("play_count").unwrap_or(0),
            last_played: row.get("last_played"),
            primary_source_id: row.get("primary_source_id"),
            primary_source_path: row.get("primary_source_path"),
            tags: {
                let raw: Option<String> = row.get("tags_raw");
                raw.map(|r| r.split('\0').filter(|s| !s.is_empty()).map(String::from).collect())
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
                    LimitUnit::Hours   => lim.value * 3_600_000,
                    LimitUnit::Tracks  => unreachable!(),
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
pub async fn list_playlists(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<PlaylistRow>, String> {
    let rows = sqlx::query(
        "SELECT id, name, kind, query FROM playlist ORDER BY name",
    )
    .fetch_all(&state.db)
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

    sqlx::query(
        "INSERT INTO playlist (name, kind, query)
         VALUES (?, 'smart', ?)
         ON CONFLICT(name) DO UPDATE SET query = excluded.query",
    )
    .bind(name)
    .bind(&request.query)
    .execute(&state.db)
    .await
    .map_err(|e| e.to_string())?;

    let row = sqlx::query(
        "SELECT id, name, kind, query FROM playlist WHERE name = ?",
    )
    .bind(name)
    .fetch_one(&state.db)
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
pub async fn delete_playlist(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<(), String> {
    sqlx::query("DELETE FROM playlist WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(|e| e.to_string())?;
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

pub async fn load_music_root(db: &sqlx::SqlitePool) -> anyhow::Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT value FROM app_config WHERE key = 'music_root'")
            .fetch_optional(db)
            .await?,
    )
}

async fn load_app_config(db: &sqlx::SqlitePool) -> anyhow::Result<AppConfig> {
    let music_root = load_music_root(db).await?;
    let queue_history_limit = sqlx::query_scalar::<_, String>(
        "SELECT value FROM app_config WHERE key = 'queue_history_limit'",
    )
    .fetch_optional(db)
    .await?
    .and_then(|value| value.parse::<i64>().ok())
    .unwrap_or(5);

    Ok(AppConfig {
        music_root,
        queue_history_limit,
    })
}

async fn set_config_value(db: &sqlx::SqlitePool, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO app_config (key, value)
         VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(db)
    .await?;

    Ok(())
}

async fn load_library_summary(db: &sqlx::SqlitePool) -> anyhow::Result<LibrarySummary> {
    let recording_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recording")
        .fetch_one(db)
        .await?;

    let artist_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artist")
        .fetch_one(db)
        .await?;

    let release_group_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_group")
        .fetch_one(db)
        .await?;

    let source_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source")
        .fetch_one(db)
        .await?;

    Ok(LibrarySummary {
        recording_count,
        artist_count,
        release_group_count,
        source_count,
    })
}
