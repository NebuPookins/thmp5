use crate::audio::PlayRequest as EnginePlayRequest;
use crate::command_error::CommandError;
use crate::file_issues::{FileIssue, FileIssueKind};
use crate::library::import::{import_paths as do_import, rescan_source as do_rescan_source};
use crate::models::{
    AppBootstrap, AppConfig, ArtistDetail, ArtistRow, CompoundArtistCheck, DbPoolDebugSnapshot,
    EntityRatingUpdate, ExternalCommand, FixMergedRecordingsStats, Id3FrameDebugInfo,
    Id3FrameDebugRequest, ImportProgress, ImportStats, InitialSetupRequest, LastFmAuthUrl,
    LastFmGetTrackLovedRequest, LastFmLoveTrackRequest, LastFmNowPlayingRequest,
    LastFmScrobbleRequest, LastFmStatus, LibrarySummary, PlayHistoryInput, PlayRequest,
    PlayerState, PlaylistRow, QueueSettingsUpdate, RecordingDetail, RecordingRatingUpdateResult,
    RecordingRow, ReleaseGroupDetail, ReleaseGroupRow, SaveSmartPlaylistRequest, SeekRequest,
    SmartPlaylistResult, SourceRatingUpdateRequest, VolumeRequest,
};
use crate::query;
use crate::storage::{Catalog, CatalogReader};
use crate::AppState;
use anyhow::Context;
use serde::Serialize;
use sqlx::Acquire;
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

/// Re-scan a source's metadata into the DB, then reload the in-memory catalog.
/// Used by the single-frame tag-edit commands, which propagate a re-scan
/// failure as a command error and skip the catalog reload. `apply_duplicate_merge`
/// does not use this helper: it always reloads the catalog even if the
/// re-scan fails, since the merge has already been applied to the file.
async fn rescan_and_reload(state: &AppState, path: &std::path::Path) -> anyhow::Result<()> {
    do_rescan_source(
        &state.db,
        path,
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
        &state.file_issues,
    )
    .await
    .map(|_| ())?;

    reload_catalog(state).await;
    Ok(())
}

// ── Import ────────────────────────────────────────────────────────────────────

#[tauri::command]
#[tracing::instrument(skip_all, fields(paths = ?paths))]
pub async fn import_paths(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<ImportStats, CommandError> {
    let result = do_import(
        &state.db,
        paths,
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
    )
    .await?;
    reload_catalog(&state).await;
    Ok(result)
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn get_app_bootstrap(
    state: tauri::State<'_, AppState>,
) -> Result<AppBootstrap, CommandError> {
    let config = load_app_config(&state.db)
        .await
        .context("Failed to load app config")?;
    let library_summary = state
        .catalog
        .read()
        .await
        .get_library_summary()
        .await
        .context("Failed to load library summary")?;
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
#[tracing::instrument(skip_all, fields(music_root = %request.music_root))]
pub async fn complete_initial_setup(
    state: tauri::State<'_, AppState>,
    request: InitialSetupRequest,
) -> Result<AppConfig, CommandError> {
    let root = request.music_root.trim();
    if root.is_empty() {
        return Err(CommandError::rejected("Music root cannot be empty."));
    }

    let path = std::path::Path::new(root);
    if !path.is_dir() {
        return Err(CommandError::rejected(format!(
            "Music root is not a directory: {}",
            path.display()
        )));
    }

    set_config_value(&state.db, "music_root", root).await?;

    let config = load_app_config(&state.db)
        .await
        .context("Failed to reload app config")?;
    state.importer.spawn_scan(
        state.db.clone(),
        root.to_string(),
        state.acoustid_api_key.clone(),
    );
    Ok(config)
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn get_import_progress(
    state: tauri::State<'_, AppState>,
) -> Result<ImportProgress, CommandError> {
    Ok(state.importer.snapshot())
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn fix_merged_recordings(
    state: tauri::State<'_, AppState>,
) -> Result<FixMergedRecordingsStats, CommandError> {
    let result = crate::library::fix_merges::fix_merged_recordings(
        &state.db,
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
    )
    .await?;
    reload_catalog(&state).await;
    Ok(result)
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn trigger_library_scan(
    state: tauri::State<'_, AppState>,
) -> Result<ImportProgress, CommandError> {
    let root = load_music_root(&state.db)
        .await
        .context("Failed to load music root")?
        .ok_or_else(|| CommandError::rejected("Music root is not configured."))?;

    state
        .importer
        .spawn_scan(state.db.clone(), root, state.acoustid_api_key.clone());

    Ok(state.importer.snapshot())
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(path = %path))]
pub async fn rescan_source(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), CommandError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(CommandError::rejected("Source path cannot be empty."));
    }

    let source_path = std::path::Path::new(trimmed);
    if !source_path.is_file() {
        return Err(CommandError::rejected(format!(
            "Source file is missing: {}",
            source_path.display()
        )));
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
    .with_context(|| format!("Failed to rescan {}", source_path.display()));

    state.pending_jobs.fetch_sub(1, Ordering::Relaxed);
    emit_job_update(&app, &state, "rescan");

    reload_catalog(&state).await;
    Ok(result?)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(count = paths.len()))]
pub async fn rescan_sources(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<(), CommandError> {
    if paths.is_empty() {
        return Err(CommandError::rejected(
            "At least one source path is required.",
        ));
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
) -> Result<(), CommandError> {
    let count = paths.len() as i64;
    state.pending_jobs.fetch_add(count, Ordering::Relaxed);
    emit_job_update(app, state, "rescan");

    let total = paths.len();
    let mut failed_paths: Vec<String> = Vec::new();
    for path in paths {
        let source_path = std::path::Path::new(&path);
        if !source_path.is_file() {
            tracing::error!(path = %source_path.display(), "Rescan skipped: source file is missing");
            failed_paths.push(source_path.display().to_string());
        } else if let Err(error) = do_rescan_source(
            &state.db,
            source_path,
            state.acoustid_api_key.as_deref(),
            &state.write_serializer,
            &state.file_issues,
        )
        .await
        {
            tracing::error!(path = %source_path.display(), "Rescan failed: {error:#}");
            failed_paths.push(source_path.display().to_string());
        }
        state.pending_jobs.fetch_sub(1, Ordering::Relaxed);
        emit_job_update(app, state, "rescan");
    }

    reload_catalog(state).await;

    // Each failure was already logged above with full context; the returned
    // error is only a summary so it is not logged a second time in full.
    match failed_paths.first() {
        None => Ok(()),
        Some(first) => Err(anyhow::anyhow!(
            "Failed to rescan {} of {} files (see log for details); first failure: {}",
            failed_paths.len(),
            total,
            first
        )
        .into()),
    }
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(artist_id = %artist_id))]
pub async fn rescan_sources_for_artist(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    artist_id: String,
) -> Result<(), CommandError> {
    let paths: Vec<String> = {
        let cat = state.catalog.read().await;
        cat.source_paths_for_artist(&artist_id).1
    };

    if paths.is_empty() {
        return Err(CommandError::rejected(
            "No local sources found for this artist.",
        ));
    }

    rescan_path_list(&app, &state, paths).await
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(release_group_id = %release_group_id))]
pub async fn rescan_sources_for_release_group(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    release_group_id: String,
) -> Result<(), CommandError> {
    let paths: Vec<String> = {
        let cat = state.catalog.read().await;
        cat.source_paths_for_release_group(&release_group_id)
    };

    if paths.is_empty() {
        return Err(CommandError::rejected(
            "No local sources found for this album.",
        ));
    }

    rescan_path_list(&app, &state, paths).await
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(recording_id = %recording_id))]
pub async fn rescan_sources_for_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    recording_id: String,
) -> Result<(), CommandError> {
    // Resolve recording_id to source paths via the in-memory catalog.
    let paths: Vec<String> = {
        let catalog = state.catalog.read().await;
        let recs = catalog
            .list_recordings()
            .await
            .context("Failed to list recordings")?;
        recs.iter()
            .find(|r| r.id == recording_id)
            .ok_or_else(|| CommandError::rejected("Recording not found in catalog."))?
            .source_paths
            .iter()
            .filter(|p| !p.is_empty())
            .cloned()
            .collect()
    };

    if paths.is_empty() {
        return Err(CommandError::rejected(
            "No local sources found for this recording.",
        ));
    }

    rescan_path_list(&app, &state, paths).await
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn rescan_all_sources(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), CommandError> {
    let paths: Vec<String> = {
        let mut conn = state
            .db
            .acquire("command.rescan_all_sources".to_string())
            .await
            .context("Failed to acquire connection")?;
        sqlx::query_scalar!(
            r#"SELECT DISTINCT s.file_path AS "file_path!"
             FROM source s
             WHERE s.source_type = 'local_file'
               AND s.file_path IS NOT NULL
             ORDER BY s.file_path"#
        )
        .fetch_all(&mut *conn)
        .await
        .context("Failed to list local sources")?
    };

    if paths.is_empty() {
        return Err(CommandError::rejected(
            "No local sources found in the library.",
        ));
    }

    rescan_path_list(&app, &state, paths).await
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(path = %path))]
pub async fn set_music_root(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<AppConfig, CommandError> {
    let root = path.trim();
    if root.is_empty() {
        return Err(CommandError::rejected("Music root cannot be empty."));
    }
    let p = std::path::Path::new(root);
    if !p.is_dir() {
        return Err(CommandError::rejected(format!(
            "Not a directory: {}",
            p.display()
        )));
    }
    set_config_value(&state.db, "music_root", root).await?;
    state.importer.spawn_scan(
        state.db.clone(),
        root.to_string(),
        state.acoustid_api_key.clone(),
    );
    Ok(load_app_config(&state.db)
        .await
        .context("Failed to reload app config")?)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(queue_history_limit = update.queue_history_limit))]
pub async fn update_queue_settings(
    state: tauri::State<'_, AppState>,
    update: QueueSettingsUpdate,
) -> Result<AppConfig, CommandError> {
    let limit = update.queue_history_limit.clamp(1, 100);
    set_config_value(&state.db, "queue_history_limit", &limit.to_string()).await?;
    Ok(load_app_config(&state.db)
        .await
        .context("Failed to reload app config")?)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(count = commands.len()))]
pub async fn save_external_commands(
    state: tauri::State<'_, AppState>,
    commands: Vec<ExternalCommand>,
) -> Result<AppConfig, CommandError> {
    let json = serde_json::to_string(&commands).context("Failed to serialize external commands")?;
    set_config_value(&state.db, "external_commands", &json).await?;
    Ok(load_app_config(&state.db)
        .await
        .context("Failed to reload app config")?)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(template = %template, file_path = %file_path))]
pub fn spawn_external_command(template: String, file_path: String) -> Result<(), CommandError> {
    let parts: Vec<String> = shell_words(&template)
        .into_iter()
        .map(|w| w.replace("%%", &file_path))
        .collect();
    let Some((program, args)) = parts.split_first() else {
        return Err(CommandError::rejected("Empty command"));
    };
    std::process::Command::new(program)
        .args(args)
        .spawn()
        .with_context(|| format!("Failed to spawn '{program}' with args {args:?}"))?;
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
#[tracing::instrument(
    skip_all,
    fields(source_id = %input.source_id, duration_played_ms = ?input.duration_played_ms)
)]
pub async fn record_play_history(
    state: tauri::State<'_, AppState>,
    input: PlayHistoryInput,
) -> Result<(), CommandError> {
    let mut conn = state
        .db
        .acquire(format!(
            "command.record_play_history source_id={}",
            input.source_id
        ))
        .await
        .context("Failed to acquire connection")?;
    sqlx::query!(
        "INSERT INTO play_history (source_id, duration_played_ms)
         VALUES (?, ?)",
        input.source_id,
        input.duration_played_ms,
    )
    .execute(&mut *conn)
    .await
    .context("Failed to insert play history")?;

    reload_catalog(&state).await;
    Ok(())
}

/// The result sent to the frontend when the rated source belongs to no known recording.
fn no_rating_update() -> RecordingRatingUpdateResult {
    RecordingRatingUpdateResult {
        recording: EntityRatingUpdate {
            id: String::new(),
            rating: None,
        },
        release_groups: Vec::new(),
        artists: Vec::new(),
        affected_recordings: Vec::new(),
    }
}

/// Ratings of every artist in `artist_ids` that still exists. A failed lookup
/// is logged and skipped so one bad entity does not hide the others.
async fn artist_rating_updates(
    catalog: &Catalog,
    artist_ids: &[String],
) -> Vec<EntityRatingUpdate> {
    let mut updates = Vec::new();
    for artist_id in artist_ids {
        match catalog.get_artist_detail(artist_id).await {
            Ok(Some(detail)) => updates.push(EntityRatingUpdate {
                id: artist_id.clone(),
                rating: detail.rating,
            }),
            Ok(None) => {}
            Err(e) => tracing::error!(%artist_id, "Failed to load artist detail: {e:#}"),
        }
    }
    updates
}

/// Ratings of every release group in `release_group_ids` that still exists.
/// A failed lookup is logged and skipped, as in [`artist_rating_updates`].
async fn release_group_rating_updates(
    catalog: &Catalog,
    release_group_ids: &[&str],
) -> Vec<EntityRatingUpdate> {
    let mut updates = Vec::new();
    for &release_group_id in release_group_ids {
        match catalog.get_release_group_detail(release_group_id).await {
            Ok(Some(detail)) => updates.push(EntityRatingUpdate {
                id: release_group_id.to_string(),
                rating: detail.rating,
            }),
            Ok(None) => {}
            Err(e) => {
                tracing::error!(%release_group_id, "Failed to load release group detail: {e:#}")
            }
        }
    }
    updates
}

/// Everything the frontend needs to refresh after `rec` was re-rated: the
/// recording, its artists and release groups, and the recordings whose
/// predicted rating depends on them.
async fn rating_update_for(
    catalog: &Catalog,
    recs: &[RecordingRow],
    rec: &RecordingRow,
) -> Result<RecordingRatingUpdateResult, CommandError> {
    let Some(rec_detail) = catalog
        .get_recording_detail(&rec.id)
        .await
        .with_context(|| format!("Failed to load recording {}", rec.id))?
    else {
        return Ok(no_rating_update());
    };

    let rg_ids: Vec<&str> = {
        let mut seen = std::collections::HashSet::new();
        rec_detail
            .releases
            .iter()
            .map(|r| r.release_group_id.as_str())
            .filter(|id| seen.insert(*id))
            .collect()
    };
    let artist_ids: Vec<String> = rec_detail
        .artists
        .iter()
        .map(|a| a.artist_id.clone())
        .collect();

    // Recordings that share an artist or release group with the rated
    // recording, so the frontend can refresh their predicted-rating stars.
    let artist_set: std::collections::HashSet<&str> =
        rec.artist_ids.iter().map(String::as_str).collect();
    let rg_id_set: std::collections::HashSet<&str> = rg_ids.iter().copied().collect();
    let affected_recordings = recs
        .iter()
        .filter(|sibling| sibling.id != rec.id)
        .filter(|sibling| {
            sibling
                .artist_ids
                .iter()
                .any(|aid| artist_set.contains(aid.as_str()))
                || sibling
                    .releases
                    .iter()
                    .any(|r| rg_id_set.contains(r.release_group_id.as_str()))
        })
        .map(|sibling| EntityRatingUpdate {
            id: sibling.id.clone(),
            rating: sibling.predicted_rating,
        })
        .collect();

    Ok(RecordingRatingUpdateResult {
        recording: EntityRatingUpdate {
            id: rec.id.clone(),
            rating: rec_detail.rating,
        },
        release_groups: release_group_rating_updates(catalog, &rg_ids).await,
        artists: artist_rating_updates(catalog, &artist_ids).await,
        affected_recordings,
    })
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(source_id = %request.source_id, stars = ?request.stars))]
pub async fn set_source_rating(
    state: tauri::State<'_, AppState>,
    request: SourceRatingUpdateRequest,
) -> Result<RecordingRatingUpdateResult, CommandError> {
    validate_rating(request.stars)?;

    {
        let mut conn = state
            .db
            .acquire(format!(
                "command.set_source_rating source_id={}",
                request.source_id
            ))
            .await
            .context("Failed to acquire connection")?;

        match request.stars {
            Some(stars) => sqlx::query!(
                "INSERT INTO source_rating (source_id, stars, updated_at)
                 VALUES (?, ?, datetime('now'))
                 ON CONFLICT(source_id) DO UPDATE SET
                    stars = excluded.stars,
                    updated_at = datetime('now')",
                request.source_id,
                stars,
            )
            .execute(&mut *conn)
            .await
            .context("Failed to save rating")?,
            None => sqlx::query!(
                "DELETE FROM source_rating WHERE source_id = ?",
                request.source_id
            )
            .execute(&mut *conn)
            .await
            .context("Failed to clear rating")?,
        };
    }

    reload_catalog(&state).await;

    // Find which recording this source belongs to via the in-memory catalog.
    let catalog = state.catalog.read().await;
    let recs = catalog
        .list_recordings()
        .await
        .context("Failed to list recordings")?;
    match catalog
        .recording_id_for_source(&request.source_id)
        .and_then(|id| recs.iter().find(|r| r.id == id))
    {
        Some(rec) => rating_update_for(&catalog, &recs, rec).await,
        None => Ok(no_rating_update()),
    }
}

// ── Library queries ───────────────────────────────────────────────────────────

#[tauri::command]
#[tracing::instrument(skip_all)]
pub fn get_player_state(state: tauri::State<'_, AppState>) -> Result<PlayerState, CommandError> {
    Ok(state.player.snapshot())
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub fn get_log_file_path(state: tauri::State<'_, AppState>) -> Result<String, CommandError> {
    Ok(state.log_file_path.clone())
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub fn get_db_pool_debug_snapshot(
    state: tauri::State<'_, AppState>,
) -> Result<DbPoolDebugSnapshot, CommandError> {
    Ok(state.db.snapshot())
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn get_file_issues(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<FileIssue>, CommandError> {
    let manual_rows = {
        let mut conn = state
            .db
            .acquire("command.get_file_issues")
            .await
            .context("Failed to acquire connection")?;
        sqlx::query!(
            r#"SELECT s.file_path, s.id AS "source_id!", i.note
             FROM source_file_issue i JOIN source s ON s.id = i.source_id"#
        )
        .fetch_all(&mut *conn)
        .await
        .context("Failed to load manual file issues")?
    };

    let catalog = state.catalog.read().await;
    // Recordings are derived in memory, so resolve each source's recording via the catalog.
    let manual_issues = manual_rows.into_iter().filter_map(|row| {
        let file_path = row.file_path?;
        let source_id = row.source_id;
        Some(FileIssue {
            file_path,
            kind: FileIssueKind::Manual,
            message: row.note,
            recording_id: catalog.recording_id_for_source(&source_id),
            source_id: Some(source_id),
            frame_id: None,
            field_name: None,
            lofty_value: None,
            corrected_value: None,
            backup_path: None,
        })
    });
    Ok(state
        .file_issues
        .all()
        .into_iter()
        .chain(catalog.file_issues())
        .chain(manual_issues)
        .collect())
}

/// Store the single user-authored issue note for a local source file.
#[tauri::command]
#[tracing::instrument(skip_all, fields(file_path = %file_path))]
pub async fn set_source_file_issue(
    state: tauri::State<'_, AppState>,
    file_path: String,
    note: String,
) -> Result<(), CommandError> {
    let note = note.trim();
    if note.is_empty() {
        return Err(CommandError::rejected("A file issue note cannot be empty"));
    }
    let mut conn = state
        .db
        .acquire("command.set_source_file_issue")
        .await
        .context("Failed to acquire connection")?;
    let result = sqlx::query!(
        "INSERT INTO source_file_issue (source_id, note, updated_at)
         SELECT id, ?, datetime('now') FROM source WHERE file_path = ?
         ON CONFLICT(source_id) DO UPDATE SET note = excluded.note, updated_at = excluded.updated_at",
        note,
        file_path,
    )
    .execute(&mut *conn)
    .await
    .context("Failed to save file issue note")?;
    if result.rows_affected() == 0 {
        return Err(CommandError::rejected(format!(
            "Local source not found: {file_path}"
        )));
    }
    Ok(())
}

/// Remove the user-authored issue note for a local source file. Detected issues
/// remain in the session log and are deliberately not affected.
#[tauri::command]
#[tracing::instrument(skip_all, fields(file_path = %file_path))]
pub async fn clear_source_file_issue(
    state: tauri::State<'_, AppState>,
    file_path: String,
) -> Result<(), CommandError> {
    let mut conn = state
        .db
        .acquire("command.clear_source_file_issue")
        .await
        .context("Failed to acquire connection")?;
    sqlx::query!(
        "DELETE FROM source_file_issue WHERE source_id = (SELECT id FROM source WHERE file_path = ?)",
        file_path,
    )
    .execute(&mut *conn)
    .await
    .context("Failed to clear file issue note")?;
    Ok(())
}

/// Delete a `.thmp5bak` backup file created by a previous tag edit.
///
/// Only deletes paths that match a currently-tracked `BackupFileExists`
/// issue — `backup_path` is caller-supplied over the Tauri IPC bridge, so
/// without this check the command would be an arbitrary-file-delete
/// primitive for anything the app process can write to.
#[tauri::command]
#[tracing::instrument(skip_all, fields(backup_path = %backup_path))]
pub async fn delete_backup_file(
    state: tauri::State<'_, AppState>,
    backup_path: String,
) -> Result<(), CommandError> {
    let is_tracked = state.file_issues.all().iter().any(|issue| {
        issue.kind == FileIssueKind::BackupFileExists
            && issue.backup_path.as_deref() == Some(backup_path.as_str())
    });
    if !is_tracked {
        return Err(CommandError::rejected("Not a tracked backup file"));
    }

    let p = std::path::Path::new(&backup_path);
    if p.exists() {
        std::fs::remove_file(p).context("Failed to delete backup file")?;
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
#[tracing::instrument(skip_all, fields(file_path = %file_path, frame_id = %frame_id))]
pub async fn write_tag_frame(
    _app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    file_path: String,
    frame_id: String,
    new_value: String,
) -> Result<crate::library::tag_writer::TagWriteResult, CommandError> {
    let path_buf = std::path::Path::new(&file_path).to_path_buf();
    let fid = frame_id.clone();

    let result = tokio::task::spawn_blocking(move || {
        crate::library::tag_writer::write_single_frame(&path_buf, &fid, &new_value)
    })
    .await
    .context("Tag write task panicked")?
    .context("Failed to write tag")?;

    // Re-read metadata and update the DB via rescan.
    rescan_and_reload(&state, std::path::Path::new(&file_path))
        .await
        .context("Failed to re-scan after tag edit")?;
    Ok(result)
}

/// Delete all frames matching `frame_id` from a file's tag section.
#[tauri::command]
#[tracing::instrument(skip_all, fields(file_path = %file_path, frame_id = %frame_id))]
pub async fn delete_tag_frame(
    _app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    file_path: String,
    frame_id: String,
) -> Result<crate::library::tag_writer::TagWriteResult, CommandError> {
    let path_buf = std::path::Path::new(&file_path).to_path_buf();
    let fid = frame_id.clone();

    let result = tokio::task::spawn_blocking(move || {
        crate::library::tag_writer::delete_frame(&path_buf, &fid)
    })
    .await
    .context("Tag delete task panicked")?
    .context("Failed to delete tag frame")?;

    rescan_and_reload(&state, std::path::Path::new(&file_path))
        .await
        .context("Failed to re-scan after tag delete")?;
    Ok(result)
}

/// Fix an orphan source by re-scanning its file metadata and re-creating the
/// album structure (release-group / release / medium / track) so the recording
/// becomes visible in the library.
#[tauri::command]
#[tracing::instrument(skip_all, fields(source_id = %source_id))]
pub async fn fix_orphan_source(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    source_id: String,
) -> Result<(), CommandError> {
    let file_path = sqlx::query_scalar!("SELECT file_path FROM source WHERE id = ?", source_id)
        .fetch_optional(state.db.raw_pool())
        .await
        .context("Failed to look up source")?
        .flatten()
        .ok_or_else(|| CommandError::rejected(format!("Source {source_id} not found")))?;
    let source_path = std::path::Path::new(&file_path);
    if !source_path.is_file() {
        return Err(CommandError::rejected(format!(
            "Source file is missing: {}",
            source_path.display()
        )));
    }

    state.pending_jobs.fetch_add(1, Ordering::Relaxed);
    emit_job_update(&app, &state, "rescan");

    let rescan = do_rescan_source(
        &state.db,
        source_path,
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
        &state.file_issues,
    )
    .await
    .map(|_| ())
    .with_context(|| format!("Failed to fix orphan source {}", source_path.display()));

    state.pending_jobs.fetch_sub(1, Ordering::Relaxed);
    emit_job_update(&app, &state, "rescan");
    rescan?;

    reload_catalog(&state).await;
    Ok(())
}

/// Enumerate conflicting frames across all consecutive ID3v2 tags in a file,
/// so the frontend can ask the user to pick a value for each before writing.
#[tauri::command]
#[tracing::instrument(skip_all, fields(file_path = %file_path))]
pub async fn preview_duplicate_merge(
    file_path: String,
) -> Result<Vec<crate::library::tag_writer::MergeConflict>, CommandError> {
    let path_buf = std::path::Path::new(&file_path).to_path_buf();
    Ok(
        tokio::task::spawn_blocking(move || crate::library::tag_writer::preview_merge(&path_buf))
            .await
            .context("Merge preview task panicked")?
            .context("Failed to preview merge")?,
    )
}

/// Merge all consecutive ID3v2 tags into one, applying the user's choices for
/// each conflicting frame, then re-scan the source and reload the catalog.
#[tauri::command]
#[tracing::instrument(skip_all, fields(file_path = %file_path))]
pub async fn apply_duplicate_merge(
    state: tauri::State<'_, AppState>,
    file_path: String,
    decisions: Vec<crate::library::tag_writer::MergeDecision>,
) -> Result<crate::library::tag_writer::TagWriteResult, CommandError> {
    let path_buf = std::path::Path::new(&file_path).to_path_buf();
    if !path_buf.is_file() {
        return Err(CommandError::rejected(format!(
            "Source file is missing: {file_path}"
        )));
    }

    let write_path = path_buf.clone();
    let write_result = tokio::task::spawn_blocking(move || {
        crate::library::tag_writer::apply_merge(&write_path, &decisions)
    })
    .await
    .context("Tag merge task panicked")?
    .context("Failed to merge tags")?;

    // The merge has already succeeded on disk: the file now carries a single
    // tag, so any duplicate-frame conflicts it once had are gone. Clear them
    // regardless of whether the follow-up re-scan (below) succeeds.
    state.file_issues.clear_duplicate_frames(&file_path);

    // Re-scan the source so `raw_tags_json` (which feeds both the catalog's
    // derived issues and metadata display) reflects a single, coherent value.
    // A failure here must not fail the command — the merge already applied —
    // so log it and reload the catalog regardless (unlike the tag-edit
    // commands, which propagate the rescan error and skip the reload).
    if let Err(e) = do_rescan_source(
        &state.db,
        &path_buf,
        state.acoustid_api_key.as_deref(),
        &state.write_serializer,
        &state.file_issues,
    )
    .await
    {
        tracing::warn!(
            error = %format!("{e:#}"),
            path = %file_path,
            "Re-scan after tag merge failed; merge already applied"
        );
    }
    reload_catalog(&state).await;

    Ok(write_result)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(path = %request.path, frame_id = %request.frame_id))]
pub fn debug_id3_text_frame(
    request: Id3FrameDebugRequest,
) -> Result<Id3FrameDebugInfo, CommandError> {
    Ok(crate::library::scanner::debug_id3_text_frame(
        std::path::Path::new(&request.path),
        &request.frame_id,
    )?)
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
#[tracing::instrument(skip_all, fields(source_id = %request.source_id))]
pub async fn play(
    state: tauri::State<'_, AppState>,
    request: PlayRequest,
) -> Result<PlayerState, CommandError> {
    tracing::info!("Play command received");
    let mut conn = state
        .db
        .acquire(format!(
            "command.play resolve_source source_id={}",
            request.source_id
        ))
        .await
        .context("Failed to acquire connection")?;
    let row = sqlx::query!(
        r#"SELECT
            s.id                  AS "source_id!",
            s.file_path           AS "file_path!",
            s.raw_tags_json,
            s.fingerprint,
            s.replay_gain_track_db,
            s.lufs
         FROM source s
         WHERE s.id = ?
           AND s.source_type = 'local_file'
           AND s.file_path IS NOT NULL"#,
        request.source_id,
    )
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to resolve source")?
    .ok_or_else(|| CommandError::rejected("Playable local source not found."))?;

    let mut source_id = row.source_id;
    let mut file_path = row.file_path;
    let raw_tags_json = row.raw_tags_json;
    let fingerprint = row.fingerprint;

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
    let replay_gain_track_db = row.replay_gain_track_db;
    let lufs = row.lufs;

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
            let alts = sqlx::query!(
                r#"SELECT s.id AS "id!", s.file_path AS "file_path!", s.replay_gain_track_db, s.lufs
                 FROM source s
                 WHERE s.fingerprint = (SELECT fingerprint FROM source WHERE id = ?)
                   AND s.id != ?
                   AND s.source_type = 'local_file'
                   AND s.file_path IS NOT NULL
                 ORDER BY s.file_path"#,
                source_id,
                source_id,
            )
            .fetch_all(&mut *conn)
            .await
            .context("Failed to find alternative sources by fingerprint")?;

            alts.into_iter()
                .find(|alt| std::path::Path::new(&alt.file_path).exists())
                .map(|alt| (alt.id, alt.file_path, alt.replay_gain_track_db, alt.lufs))
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
                let alts = sqlx::query!(
                    r#"SELECT s.id AS "id!", s.file_path AS "file_path!", s.replay_gain_track_db, s.lufs, s.raw_tags_json
                     FROM source s
                     WHERE s.id != ?
                       AND s.source_type = 'local_file'
                       AND s.file_path IS NOT NULL
                     ORDER BY s.file_path"#,
                    source_id,
                )
                .fetch_all(&mut *conn)
                .await
                .context("Failed to find alternative sources by MBID")?;

                for alt_row in alts {
                    let alt_raw_tags = alt_row.raw_tags_json;
                    let has_mbid = alt_raw_tags
                        .as_deref()
                        .and_then(|j| serde_json::from_str::<Vec<(String, String)>>(j).ok())
                        .map(|t| {
                            t.iter()
                                .any(|(k, v)| k == "UFID:http://musicbrainz.org" && v == &mbid)
                        })
                        .unwrap_or(false);
                    if has_mbid && std::path::Path::new(&alt_row.file_path).exists() {
                        alt = Some((
                            alt_row.id,
                            alt_row.file_path,
                            alt_row.replay_gain_track_db,
                            alt_row.lufs,
                        ));
                        break;
                    }
                }
            }
        }

        if let Some((alt_id, alt_path, alt_rg, alt_lufs)) = alt {
            // Remove the stale source row silently.
            sqlx::query!("DELETE FROM source WHERE id = ?", source_id)
                .execute(&mut *conn)
                .await
                .context("Failed to remove missing source")?;
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
            return Err(CommandError::rejected(format!(
                "File not found and no working alternative source exists: {file_path}"
            )));
        }
    }

    tracing::info!(
        source_id = %source_id,
        path = %file_path,
        "Resolved play request"
    );

    let duration_ms: Option<i64> =
        sqlx::query_scalar!("SELECT duration_ms FROM source WHERE id = ?", source_id)
            .fetch_one(&mut *conn)
            .await
            .context("Failed to load source duration")?;

    state
        .player
        .play(EnginePlayRequest {
            source_id: source_id.clone(),
            file_path: file_path.clone(),
            duration_ms: duration_ms
                .and_then(|d| u64::try_from(d).ok())
                .filter(|&d| d > 0),
            title,
            artist,
            normalization_gain,
            normalization_source: normalization_source.to_string(),
        })
        .with_context(|| format!("Failed to start playback of {file_path}"))?;

    Ok(state.player.snapshot())
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(enabled = enabled))]
pub fn set_normalization_enabled(
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<PlayerState, CommandError> {
    tracing::info!("Set normalization enabled command received");
    state.player.set_normalization_enabled(enabled)?;
    Ok(state.player.snapshot())
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub fn pause(state: tauri::State<'_, AppState>) -> Result<PlayerState, CommandError> {
    tracing::info!("Pause command received");
    state.player.pause()?;
    Ok(state.player.snapshot())
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub fn resume(state: tauri::State<'_, AppState>) -> Result<PlayerState, CommandError> {
    tracing::info!("Resume command received");
    state.player.resume()?;
    Ok(state.player.snapshot())
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(position_ms = request.position_ms))]
pub fn seek(
    state: tauri::State<'_, AppState>,
    request: SeekRequest,
) -> Result<PlayerState, CommandError> {
    tracing::info!("Seek command received");
    state.player.seek(request.position_ms)?;
    Ok(state.player.snapshot())
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(volume = request.volume))]
pub fn set_volume(
    state: tauri::State<'_, AppState>,
    request: VolumeRequest,
) -> Result<PlayerState, CommandError> {
    tracing::info!("Set volume command received");
    state.player.set_volume(request.volume)?;
    Ok(state.player.snapshot())
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub fn stop(state: tauri::State<'_, AppState>) -> Result<PlayerState, CommandError> {
    tracing::info!("Stop command received");
    state.player.stop()?;
    Ok(state.player.snapshot())
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn get_library_summary(
    state: tauri::State<'_, AppState>,
) -> Result<LibrarySummary, CommandError> {
    Ok(state.catalog.read().await.get_library_summary().await?)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(limit = ?limit, offset = ?offset))]
pub async fn list_recordings(
    state: tauri::State<'_, AppState>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<RecordingRow>, CommandError> {
    let limit = limit.unwrap_or(200) as usize;
    let offset = offset.unwrap_or(0) as usize;
    let all = state.catalog.read().await.list_recordings().await?;
    Ok(all.into_iter().skip(offset).take(limit).collect())
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn list_artists(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ArtistRow>, CommandError> {
    Ok(state.catalog.read().await.list_artists().await?)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(artist_id = ?artist_id, search = ?search))]
pub async fn list_release_groups(
    state: tauri::State<'_, AppState>,
    artist_id: Option<String>,
    search: Option<String>,
) -> Result<Vec<ReleaseGroupRow>, CommandError> {
    let search = search.as_deref().map(str::trim).filter(|v| !v.is_empty());
    Ok(state
        .catalog
        .read()
        .await
        .list_release_groups(artist_id.as_deref(), search)
        .await?)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(source_id = %source_id))]
pub async fn get_waveform(
    state: tauri::State<'_, AppState>,
    source_id: String,
) -> Result<Vec<f32>, CommandError> {
    let path = {
        let mut conn = state
            .db
            .acquire(format!("command.get_waveform source_id={source_id}"))
            .await
            .context("Failed to acquire connection")?;
        sqlx::query_scalar!(
            "SELECT file_path FROM source
             WHERE id = ? AND source_type = 'local_file' AND file_path IS NOT NULL",
            source_id,
        )
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to look up source file")?
        .flatten()
        .ok_or_else(|| CommandError::rejected("No file found for this recording"))?
    };
    let waveform_error = format!("Failed to compute waveform for {path}");

    let data = tokio::task::spawn_blocking(move || {
        crate::waveform::compute_waveform(
            std::path::Path::new(&path),
            crate::waveform::WAVEFORM_RESOLUTION,
        )
    })
    .await
    .context("Waveform task panicked")?
    .context(waveform_error)?;

    Ok(data)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(source_id = %source_id))]
pub async fn get_cover_art(
    state: tauri::State<'_, AppState>,
    source_id: String,
) -> Result<Option<String>, CommandError> {
    let file_path = {
        let mut conn = state
            .db
            .acquire(format!("command.get_cover_art source_id={source_id}"))
            .await
            .context("Failed to acquire connection")?;
        sqlx::query_scalar!(
            "SELECT file_path FROM source
             WHERE id = ? AND source_type = 'local_file' AND file_path IS NOT NULL",
            source_id,
        )
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to look up source file")?
        .flatten()
    };

    let Some(path) = file_path else {
        return Ok(None);
    };

    Ok(
        crate::library::scanner::extract_cover_art(std::path::Path::new(&path))
            .with_context(|| format!("Failed to extract cover art from {path}"))?,
    )
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn list_all_tags(state: tauri::State<'_, AppState>) -> Result<Vec<String>, CommandError> {
    Ok(state.catalog.read().await.list_all_tags().await?)
}

// ── Smart playlists ───────────────────────────────────────────────────────────

#[tauri::command]
#[tracing::instrument(skip_all, fields(query = %query))]
pub async fn evaluate_smart_playlist(
    state: tauri::State<'_, AppState>,
    query: String,
) -> Result<SmartPlaylistResult, CommandError> {
    // The query is user input: a syntax error is a rejection, not a failure.
    // Parse up front so only genuine execution failures reach `Failed`.
    query::parse(&query).map_err(|e| CommandError::rejected(format!("{e:#}")))?;
    Ok(state
        .catalog
        .read()
        .await
        .evaluate_smart_playlist(&query)
        .await?)
}

#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn list_playlists(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<PlaylistRow>, CommandError> {
    let mut conn = state
        .db
        .acquire("command.list_playlists")
        .await
        .context("Failed to acquire connection")?;
    Ok(sqlx::query_as!(
        PlaylistRow,
        r#"SELECT id AS "id!", name, kind, query FROM playlist ORDER BY name"#
    )
    .fetch_all(&mut *conn)
    .await
    .context("Failed to list playlists")?)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(name = %request.name, query = %request.query))]
pub async fn save_smart_playlist(
    state: tauri::State<'_, AppState>,
    request: SaveSmartPlaylistRequest,
) -> Result<PlaylistRow, CommandError> {
    // Validate the query parses before saving
    query::parse(&request.query)
        .map_err(|e| CommandError::rejected(format!("Invalid query: {e:#}")))?;

    let name = request.name.trim();
    if name.is_empty() {
        return Err(CommandError::rejected("Playlist name cannot be empty."));
    }

    let mut conn = state
        .db
        .acquire(format!("command.save_smart_playlist name={name}"))
        .await
        .context("Failed to acquire connection")?;
    sqlx::query!(
        "INSERT INTO playlist (name, kind, query)
         VALUES (?, 'smart', ?)
         ON CONFLICT(name) DO UPDATE SET query = excluded.query",
        name,
        request.query,
    )
    .execute(&mut *conn)
    .await
    .context("Failed to save playlist")?;

    Ok(sqlx::query_as!(
        PlaylistRow,
        r#"SELECT id AS "id!", name, kind, query FROM playlist WHERE name = ?"#,
        name
    )
    .fetch_one(&mut *conn)
    .await
    .context("Failed to reload saved playlist")?)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(id = id))]
pub async fn delete_playlist(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<(), CommandError> {
    let mut conn = state
        .db
        .acquire(format!("command.delete_playlist id={id}"))
        .await
        .context("Failed to acquire connection")?;
    sqlx::query!("DELETE FROM playlist WHERE id = ?", id)
        .execute(&mut *conn)
        .await
        .context("Failed to delete playlist")?;
    Ok(())
}

// ── Last.fm scrobbling ─────────────────────────────────────────────────────────

const LASTFM_NOT_CONFIGURED: &str =
    "Last.fm not configured. Enter your API key and shared secret in Settings.";
const LASTFM_NOT_LOGGED_IN: &str = "Not logged in to Last.fm. Connect in Settings.";

/// Load the Last.fm API credentials; `None` means they have not been entered
/// yet, which is a normal state rather than an error.
pub(crate) async fn load_lastfm_config(
    db: &crate::db::DbPool,
) -> anyhow::Result<Option<crate::lastfm::LastFmConfig>> {
    let mut conn = db
        .acquire("lastfm.load_config")
        .await
        .context("DB error loading Last.fm config")?;

    let api_key: Option<String> =
        sqlx::query_scalar!("SELECT value FROM app_config WHERE key = 'lastfm_api_key'")
            .fetch_optional(&mut *conn)
            .await
            .context("DB query error for lastfm_api_key")?;

    let shared_secret: Option<String> =
        sqlx::query_scalar!("SELECT value FROM app_config WHERE key = 'lastfm_shared_secret'")
            .fetch_optional(&mut *conn)
            .await
            .context("DB query error for lastfm_shared_secret")?;

    match (api_key, shared_secret) {
        (Some(api_key), Some(shared_secret)) => {
            tracing::debug!("Loaded Last.fm config from app_config");
            Ok(Some(crate::lastfm::LastFmConfig {
                api_key,
                shared_secret,
            }))
        }
        _ => Ok(None),
    }
}

/// Config for a user-invoked command; "not configured" is a rejection.
async fn require_lastfm_config(
    db: &crate::db::DbPool,
) -> Result<crate::lastfm::LastFmConfig, CommandError> {
    load_lastfm_config(db)
        .await?
        .ok_or_else(|| CommandError::rejected(LASTFM_NOT_CONFIGURED))
}

/// Config and session key for a user-invoked command.
async fn require_lastfm_session(
    db: &crate::db::DbPool,
) -> Result<(crate::lastfm::LastFmConfig, String), CommandError> {
    let config = require_lastfm_config(db).await?;
    let session_key = load_lastfm_session_key(db)
        .await?
        .ok_or_else(|| CommandError::rejected(LASTFM_NOT_LOGGED_IN))?;
    Ok((config, session_key))
}

/// Config and session key for background use; `None` when Last.fm is not set
/// up (config missing or not logged in), which is a normal state.
pub(crate) async fn load_lastfm_credentials(
    db: &crate::db::DbPool,
) -> anyhow::Result<Option<(crate::lastfm::LastFmConfig, String)>> {
    let Some(config) = load_lastfm_config(db).await? else {
        return Ok(None);
    };
    Ok(load_lastfm_session_key(db)
        .await?
        .map(|session_key| (config, session_key)))
}

/// Save the Last.fm API key and shared secret to app_config.
///
/// Credentials are secrets: they are deliberately not span fields.
#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn save_lastfm_credentials(
    state: tauri::State<'_, AppState>,
    api_key: String,
    shared_secret: String,
) -> Result<(), CommandError> {
    tracing::info!("Saving Last.fm credentials");
    if api_key.trim().is_empty() || shared_secret.trim().is_empty() {
        return Err(CommandError::rejected(
            "API key and shared secret must not be empty.",
        ));
    }

    let mut conn = state
        .db
        .acquire("lastfm.save_credentials")
        .await
        .context("DB error saving Last.fm credentials")?;

    let mut tx = conn.begin().await.context("Failed to start transaction")?;

    let api_key = api_key.trim();
    sqlx::query!(
        "INSERT INTO app_config (key, value) VALUES ('lastfm_api_key', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        api_key,
    )
    .execute(&mut *tx)
    .await
    .context("Failed to save lastfm_api_key")?;

    let shared_secret = shared_secret.trim();
    sqlx::query!(
        "INSERT INTO app_config (key, value) VALUES ('lastfm_shared_secret', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        shared_secret,
    )
    .execute(&mut *tx)
    .await
    .context("Failed to save lastfm_shared_secret")?;

    tx.commit().await.context("Failed to commit transaction")?;
    tracing::info!("Last.fm credentials saved successfully");
    Ok(())
}

/// Returns the current Last.fm integration status.
#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn get_lastfm_status(
    state: tauri::State<'_, AppState>,
) -> Result<LastFmStatus, CommandError> {
    tracing::debug!("get_lastfm_status called");
    let mut conn = state
        .db
        .acquire("lastfm.get_status")
        .await
        .context("DB error getting Last.fm status")?;

    let api_key: Option<String> =
        sqlx::query_scalar!("SELECT value FROM app_config WHERE key = 'lastfm_api_key'")
            .fetch_optional(&mut *conn)
            .await
            .context("DB query error for lastfm_api_key")?;
    let configured = api_key.is_some();

    let session_key: Option<String> =
        sqlx::query_scalar!("SELECT value FROM app_config WHERE key = 'lastfm_session_key'")
            .fetch_optional(&mut *conn)
            .await
            .context("DB query error for lastfm_session_key")?;

    let username: Option<String> =
        sqlx::query_scalar!("SELECT value FROM app_config WHERE key = 'lastfm_username'")
            .fetch_optional(&mut *conn)
            .await
            .context("DB query error for lastfm_username")?;

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
#[tracing::instrument(skip_all)]
pub async fn lastfm_get_auth_url(
    state: tauri::State<'_, AppState>,
) -> Result<LastFmAuthUrl, CommandError> {
    tracing::info!("lastfm_get_auth_url called");
    let config = require_lastfm_config(&state.db).await?;

    let token = crate::lastfm::get_token(&config)
        .await
        .map_err(anyhow::Error::msg)
        .context("Failed to request Last.fm auth token")?;
    tracing::info!(token_prefix = %token.chars().take(8).collect::<String>(), "Obtained Last.fm auth token");

    // Store the token in memory for the completion step
    *state
        .lastfm_auth_token
        .lock()
        .map_err(|e| anyhow::anyhow!("Failed to lock auth token mutex: {e}"))? =
        Some(token.clone());

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
#[tracing::instrument(skip_all)]
pub async fn lastfm_complete_auth(
    state: tauri::State<'_, AppState>,
) -> Result<String, CommandError> {
    tracing::info!("lastfm_complete_auth called");
    let config = require_lastfm_config(&state.db).await?;

    let token = state
        .lastfm_auth_token
        .lock()
        .map_err(|e| anyhow::anyhow!("Failed to lock auth token mutex: {e}"))?
        .take()
        .ok_or_else(|| {
            CommandError::rejected("No pending auth request. Call lastfm_get_auth_url first.")
        })?;

    tracing::info!(token_prefix = %token.chars().take(8).collect::<String>(), "Exchanging token for session");
    let (session_key, username) = crate::lastfm::get_session(&config, &token)
        .await
        .map_err(anyhow::Error::msg)
        .context("Failed to exchange token for Last.fm session")?;
    tracing::info!(%username, "Session obtained, persisting to app_config");

    let mut conn = state
        .db
        .acquire("lastfm.complete_auth")
        .await
        .context("DB error on complete_auth")?;

    let mut tx = conn.begin().await.context("Failed to start transaction")?;

    // The session key is a secret: keep it out of the log context.
    sqlx::query!(
        "INSERT INTO app_config (key, value) VALUES ('lastfm_session_key', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        session_key,
    )
    .execute(&mut *tx)
    .await
    .context("Failed to save session_key")?;

    sqlx::query!(
        "INSERT INTO app_config (key, value) VALUES ('lastfm_username', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        username,
    )
    .execute(&mut *tx)
    .await
    .with_context(|| format!("Failed to save username {username}"))?;

    tx.commit().await.context("Failed to commit transaction")?;

    tracing::info!(%username, "Last.fm auth complete — user is now logged in");
    Ok(username)
}

/// Disconnect from Last.fm by removing the stored session key and username.
#[tauri::command]
#[tracing::instrument(skip_all)]
pub async fn lastfm_disconnect(state: tauri::State<'_, AppState>) -> Result<(), CommandError> {
    tracing::info!("lastfm_disconnect called");
    let mut conn = state
        .db
        .acquire("lastfm.disconnect")
        .await
        .context("DB error disconnecting")?;

    sqlx::query!("DELETE FROM app_config WHERE key IN ('lastfm_session_key', 'lastfm_username')")
        .execute(&mut *conn)
        .await
        .context("Failed to remove session data")?;

    tracing::info!("Last.fm session cleared");
    Ok(())
}

/// Love (heart) a track on Last.fm.
#[tauri::command]
#[tracing::instrument(skip_all, fields(artist = %request.artist, track = %request.track))]
pub async fn lastfm_love_track(
    state: tauri::State<'_, AppState>,
    request: LastFmLoveTrackRequest,
) -> Result<(), CommandError> {
    tracing::info!("lastfm_love_track called");
    let (config, session_key) = require_lastfm_session(&state.db).await?;

    Ok(
        crate::lastfm::love_track(&config, &session_key, &request.artist, &request.track)
            .await
            .map_err(anyhow::Error::msg)?,
    )
}

/// Unlove a track on Last.fm.
#[tauri::command]
#[tracing::instrument(skip_all, fields(artist = %request.artist, track = %request.track))]
pub async fn lastfm_unlove_track(
    state: tauri::State<'_, AppState>,
    request: LastFmLoveTrackRequest,
) -> Result<(), CommandError> {
    tracing::info!("lastfm_unlove_track called");
    let (config, session_key) = require_lastfm_session(&state.db).await?;

    Ok(
        crate::lastfm::unlove_track(&config, &session_key, &request.artist, &request.track)
            .await
            .map_err(anyhow::Error::msg)?,
    )
}

/// Check whether the current user has loved a track on Last.fm.
#[tauri::command]
#[tracing::instrument(skip_all, fields(artist = %request.artist, track = %request.track))]
pub async fn lastfm_get_track_loved(
    state: tauri::State<'_, AppState>,
    request: LastFmGetTrackLovedRequest,
) -> Result<bool, CommandError> {
    tracing::info!("lastfm_get_track_loved called");
    let (config, session_key) = require_lastfm_session(&state.db).await?;
    Ok(
        crate::lastfm::get_track_loved(&config, &session_key, &request.artist, &request.track)
            .await
            .map_err(anyhow::Error::msg)?,
    )
}

/// Update the "now playing" status on Last.fm for the currently playing track.
#[tauri::command]
#[tracing::instrument(
    skip_all,
    fields(artist = %request.artist, track = %request.track, album = ?request.album)
)]
pub async fn lastfm_now_playing(
    state: tauri::State<'_, AppState>,
    request: LastFmNowPlayingRequest,
) -> Result<(), CommandError> {
    tracing::info!("lastfm_now_playing called");
    let (config, session_key) = require_lastfm_session(&state.db).await?;

    Ok(crate::lastfm::now_playing(
        &config,
        &session_key,
        &request.artist,
        &request.track,
        request.album.as_deref(),
    )
    .await
    .map_err(anyhow::Error::msg)?)
}

/// Scrobble a track to Last.fm. `timestamp` is the UNIX second when the track started playing.
#[tauri::command]
#[tracing::instrument(
    skip_all,
    fields(
        artist = %request.artist,
        track = %request.track,
        album = ?request.album,
        timestamp = %request.timestamp
    )
)]
pub async fn lastfm_scrobble(
    state: tauri::State<'_, AppState>,
    request: LastFmScrobbleRequest,
) -> Result<(), CommandError> {
    tracing::info!("lastfm_scrobble called");
    let (config, session_key) = require_lastfm_session(&state.db).await?;

    Ok(crate::lastfm::scrobble(
        &config,
        &session_key,
        &request.artist,
        &request.track,
        request.album.as_deref(),
        request.timestamp,
    )
    .await
    .map_err(anyhow::Error::msg)?)
}

pub(crate) async fn load_lastfm_session_key(
    db: &crate::db::DbPool,
) -> anyhow::Result<Option<String>> {
    let mut conn = db
        .acquire("lastfm.load_session_key")
        .await
        .context("DB error loading session key")?;
    sqlx::query_scalar!("SELECT value FROM app_config WHERE key = 'lastfm_session_key'")
        .fetch_optional(&mut *conn)
        .await
        .context("DB query error for session key")
}

fn validate_rating(stars: Option<i64>) -> Result<(), CommandError> {
    match stars {
        Some(stars) if !(1..=5).contains(&stars) => {
            Err(CommandError::rejected("Rating must be between 1 and 5."))
        }
        _ => Ok(()),
    }
}

pub async fn load_music_root(db: &crate::db::DbPool) -> anyhow::Result<Option<String>> {
    let mut conn = db.acquire("config.load_music_root").await?;
    sqlx::query_scalar!("SELECT value FROM app_config WHERE key = 'music_root'")
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read music_root from app_config")
}

async fn load_app_config(db: &crate::db::DbPool) -> anyhow::Result<AppConfig> {
    let music_root = load_music_root(db).await?;
    let mut conn = db
        .acquire("config.load_app_config.queue_history_limit")
        .await?;
    let queue_history_limit =
        sqlx::query_scalar!("SELECT value FROM app_config WHERE key = 'queue_history_limit'")
            .fetch_optional(&mut *conn)
            .await
            .context("Failed to read queue_history_limit from app_config")?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(5);

    let external_commands_json: Option<String> =
        sqlx::query_scalar!("SELECT value FROM app_config WHERE key = 'external_commands'")
            .fetch_optional(&mut *conn)
            .await
            .context("Failed to read external_commands from app_config")?;
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
    sqlx::query!(
        "INSERT INTO app_config (key, value)
         VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        key,
        value,
    )
    .execute(&mut *conn)
    .await
    .with_context(|| format!("Failed to write app_config key={key}"))?;

    Ok(())
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(id = %id))]
pub async fn get_artist_detail(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<ArtistDetail, CommandError> {
    state
        .catalog
        .read()
        .await
        .get_artist_detail(&id)
        .await?
        .ok_or_else(|| CommandError::rejected(format!("Artist {id} not found")))
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(id = %id))]
pub async fn get_release_group_detail(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<ReleaseGroupDetail, CommandError> {
    state
        .catalog
        .read()
        .await
        .get_release_group_detail(&id)
        .await?
        .ok_or_else(|| CommandError::rejected(format!("Release group {id} not found")))
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(id = %id))]
pub async fn get_recording_detail(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<RecordingDetail, CommandError> {
    state
        .catalog
        .read()
        .await
        .get_recording_detail(&id)
        .await?
        .ok_or_else(|| CommandError::rejected(format!("Recording {id} not found")))
}

// ── Artist fix commands ──────────────────────────────────────────────────────

#[tauri::command]
#[tracing::instrument(skip_all, fields(artist_id = %artist_id))]
pub async fn check_artist_compound(
    state: tauri::State<'_, AppState>,
    artist_id: String,
) -> Result<CompoundArtistCheck, CommandError> {
    Ok(
        crate::library::artist_fixes::check_artist_compound(&state.catalog, &artist_id)
            .await
            .map_err(anyhow::Error::msg)?,
    )
}

/// Drop the MusicBrainz recording-ID frame from a `raw_tags_json` value so the
/// catalog stops grouping the source with its former siblings.
///
/// Fails if the JSON is not a list of `[frame_id, value]` pairs.
fn strip_mbid_ufid(raw_tags_json: &str) -> serde_json::Result<String> {
    let tags = serde_json::from_str::<Vec<(String, String)>>(raw_tags_json)?
        .into_iter()
        .filter(|(frame_id, _)| frame_id != "UFID:http://musicbrainz.org")
        .collect::<Vec<_>>();
    serde_json::to_string(&tags)
}

#[tauri::command]
#[tracing::instrument(skip_all, fields(source_ids_to_move = ?source_ids_to_move))]
pub async fn split_recording(
    state: tauri::State<'_, AppState>,
    _recording_id: String,
    source_ids_to_move: Vec<String>,
) -> Result<String, CommandError> {
    if source_ids_to_move.is_empty() {
        return Err(CommandError::rejected("No sources selected to move"));
    }

    // Remove the MBID from each moved source's raw_tags_json so the catalog
    // will group them independently on the next load.
    {
        let _permit = state
            .write_serializer
            .acquire()
            .await
            .context("Failed to acquire write lock for split")?;
        let mut conn = state
            .db
            .acquire("command.split_recording")
            .await
            .context("Failed to acquire connection")?;
        let mut tx = conn.begin().await.context("Failed to begin transaction")?;

        for sid in &source_ids_to_move {
            let raw_json =
                sqlx::query_scalar!("SELECT raw_tags_json FROM source WHERE id = ?", sid)
                    .fetch_optional(&mut *tx)
                    .await
                    .with_context(|| format!("Failed to read raw_tags_json of source {sid}"))?
                    .flatten();

            let Some(raw_json) = raw_json else { continue };
            let new_json = match strip_mbid_ufid(&raw_json) {
                Ok(json) => json,
                Err(error) => {
                    let file_path =
                        sqlx::query_scalar!("SELECT file_path FROM source WHERE id = ?", sid)
                            .fetch_optional(&mut *tx)
                            .await
                            .unwrap_or_else(|lookup_error| {
                                tracing::warn!(source_id = %sid, "Could not look up file path for malformed source: {lookup_error:#}");
                                None
                            })
                            .flatten()
                            .unwrap_or_else(|| "unknown path".to_string());
                    return Err(anyhow::Error::new(error)
                        .context(format!(
                            "Malformed raw_tags_json for source {sid} ({file_path}); nothing was split"
                        ))
                        .into());
                }
            };
            sqlx::query!(
                "UPDATE source SET raw_tags_json = ? WHERE id = ?",
                new_json,
                sid,
            )
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to update raw_tags_json of source {sid}"))?;
        }

        tx.commit().await.context("Failed to commit transaction")?;
    }

    reload_catalog(&state).await;

    // Return the recording ID that the catalog assigned to the first moved source.
    let first_sid = source_ids_to_move
        .first()
        .context("No sources selected to move")?;
    let recording_id = state
        .catalog
        .read()
        .await
        .recording_id_for_source(first_sid)
        .with_context(|| {
            format!("Split succeeded but source {first_sid} is not in any recording after reload")
        })?;
    Ok(recording_id)
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

    #[test]
    fn strip_mbid_ufid_removes_only_the_musicbrainz_frame() {
        let stripped = strip_mbid_ufid(
            r#"[["TIT2","Song"],["UFID:http://musicbrainz.org","abc"],["TPE1","Band"]]"#,
        )
        .unwrap();
        let tags: Vec<(String, String)> = serde_json::from_str(&stripped).unwrap();
        assert_eq!(
            tags,
            vec![
                ("TIT2".to_string(), "Song".to_string()),
                ("TPE1".to_string(), "Band".to_string()),
            ]
        );
    }

    #[test]
    fn strip_mbid_ufid_rejects_malformed_json() {
        assert!(strip_mbid_ufid("not json").is_err());
    }
}
