use crate::audio::PlayRequest as EnginePlayRequest;
use crate::library::import::import_paths as do_import;
use crate::models::{
    AppBootstrap, AppConfig, ArtistRow, ImportProgress, ImportStats, InitialSetupRequest,
    LibrarySummary, PlayHistoryInput, PlayRequest, PlayerState, QueueSettingsUpdate, RecordingRow,
    SeekRequest, VolumeRequest,
};
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
            ) AS primary_source_path
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
            COUNT(DISTINCT ra.recording_id)      AS recording_count
         FROM artist a
         LEFT JOIN recording_artist ra      ON ra.artist_id = a.id
         LEFT JOIN release_group_artist rga ON rga.artist_id = a.id
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
        })
        .collect();

    Ok(artists)
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
