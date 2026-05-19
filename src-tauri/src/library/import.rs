use super::scanner::{build_raw_tags_json, is_audio_file, read_metadata};
use crate::db::DbPool;
use crate::file_issues::{FileIssueKind, FileIssueLog};
use crate::fingerprint::{self, AcoustIdMatch};
use crate::models::{DuplicateFrameInfo, ImportStats, TrackMetadata};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Sqlite, Transaction};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;
use tokio::sync::Semaphore;
use uuid::Uuid;
use walkdir::WalkDir;

/// Tracks which release group each successfully rescanned source asserts
/// (source_file_path → release_group_id).
pub type SourceAssertions = Arc<Mutex<HashMap<String, String>>>;

/// Set the calling thread's I/O priority to idle class (Linux only).
fn set_io_priority_idle() {
    #[cfg(target_os = "linux")]
    unsafe {
        // ioprio_set(IOPRIO_WHO_PROCESS=1, pid=0 (self), IOPRIO_PRIO_VALUE(IOPRIO_CLASS_IDLE=3, 0))
        libc::syscall(libc::SYS_ioprio_set, 1i64, 0i64, (3i64 << 13) | 0i64);
    }
}

pub(crate) struct PreparedImport {
    path: PathBuf,
    path_str: String,
    existing_source_id: Option<String>,
    hash: String,
    meta: TrackMetadata,
    warnings: Vec<String>,
    file_size: i64,
    file_mtime_ms: i64,
    fp: Option<fingerprint::FingerprintResult>,
    acoustid_match: Option<AcoustIdMatch>,
    raw_tags_json: String,
}

pub async fn import_paths(
    db: &DbPool,
    paths: Vec<String>,
    acoustid_key: Option<&str>,
) -> Result<ImportStats> {
    let mut stats = ImportStats {
        scanned: 0,
        imported: 0,
        skipped: 0,
        errors: 0,
        error_messages: Vec::new(),
    };

    for path_str in paths {
        let path = Path::new(&path_str);
        if path.is_dir() {
            for entry in WalkDir::new(path).follow_links(true) {
                match entry {
                    Ok(e) if e.file_type().is_file() && is_audio_file(e.path()) => {
                        stats.scanned += 1;
                        match import_file(db, e.path(), acoustid_key).await {
                            Ok(true) => stats.imported += 1,
                            Ok(false) => stats.skipped += 1,
                            Err(e) => {
                                println!("[importer] import error: {:#}", e);
                                stats.errors += 1;
                                stats.error_messages.push(format!("{:#}", e));
                            }
                        }
                    }
                    Err(e) => {
                        let msg = match e.path() {
                            Some(p) => format!("{}: {}", p.display(), e),
                            None => e.to_string(),
                        };
                        println!("[importer] import error: {msg}");
                        stats.errors += 1;
                        stats.error_messages.push(msg);
                    }
                    _ => {}
                }
            }
        } else if path.is_file() && is_audio_file(path) {
            stats.scanned += 1;
            match import_file(db, path, acoustid_key).await {
                Ok(true) => stats.imported += 1,
                Ok(false) => stats.skipped += 1,
                Err(e) => {
                    println!("[importer] import error: {:#}", e);
                    stats.errors += 1;
                    stats.error_messages.push(format!("{:#}", e));
                }
            }
        }
    }

    Ok(stats)
}

pub async fn rescan_source(
    db: &DbPool,
    path: &Path,
    acoustid_key: Option<&str>,
    serializer: &Semaphore,
    skip_prune: bool,
    file_issues: &FileIssueLog,
) -> Result<(String, String)> {
    let path_str = path.to_string_lossy().to_string();
    let mut conn = db
        .acquire(format!("source_rescan.lookup path={path_str}"))
        .await
        .context("Failed to acquire DB connection for source rescan lookup")?;
    let existing_source = sqlx::query_as::<_, (String, String)>(
        "SELECT id, recording_id
         FROM source
         WHERE file_path = ? AND source_type = 'local_file'",
    )
    .bind(&path_str)
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to load source for rescan")?
    .ok_or_else(|| anyhow::anyhow!("Source not found for path: {}", path.display()))?;

    let (source_id, recording_id) = existing_source;
    drop(conn);
    let (file_size, file_mtime_ms) = file_identity(path).context("Failed to read file metadata")?;

    struct BlockingResult {
        hash: String,
        meta: TrackMetadata,
        warnings: Vec<String>,
        fp: Option<fingerprint::FingerprintResult>,
        duplicate_frames: Vec<DuplicateFrameInfo>,
        raw_tags_json: String,
    }

    let p = path.to_path_buf();
    let blocking = tokio::task::spawn_blocking(move || {
        let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Min);
        set_io_priority_idle();
        let hash = file_sha256(&p).context("Failed to hash file")?;
        let metadata_read = read_metadata(&p).context("Failed to read metadata")?;
        let fp = match fingerprint::generate_fingerprint(&p) {
            Ok(fp) => Some(fp),
            Err(e) => {
                tracing::warn!(path = %p.display(), "Fingerprint generation failed during rescan: {e}");
                None
            }
        };
        let raw_tags_json = build_raw_tags_json(
            &p,
            &metadata_read.meta,
            &metadata_read.all_tags,
        );
        Ok::<_, anyhow::Error>(BlockingResult {
            hash,
            meta: metadata_read.meta,
            warnings: metadata_read.warning.into_iter().collect(),
            fp,
            duplicate_frames: metadata_read.duplicate_frames,
            raw_tags_json,
        })
    })
    .await;

    let blocking = match blocking {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e),
        Err(e) => anyhow::bail!("Blocking rescan task panicked: {e}"),
    };

    let acoustid_match: Option<AcoustIdMatch> = match (acoustid_key, blocking.fp.as_ref()) {
        (Some(key), Some(fp_result)) => match fingerprint::lookup_acoustid(key, fp_result).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(path = %path.display(), "AcoustID lookup failed during rescan: {e}");
                None
            }
        },
        _ => None,
    };

    let _permit = serializer
        .acquire()
        .await
        .context("Failed to acquire write serializer for source rescan")?;

    let mut db_conn = db
        .acquire(format!("source_rescan.transaction path={path_str}"))
        .await
        .context("Failed to acquire DB connection for source rescan transaction")?;
    db_conn
        .set_busy_timeout(std::time::Duration::from_secs(30))
        .await
        .context("Failed to set busy timeout for source rescan transaction")?;
    let mut tx = db_conn
        .begin()
        .await
        .context("Failed to start source rescan transaction")?;

    tracing::info!(
        path = %path.display(),
        artist = %blocking.meta.artist.as_deref().unwrap_or("(none)"),
        album_artist = %blocking.meta.album_artist.as_deref().unwrap_or("(none)"),
        "Rescan: read metadata"
    );

    let artist_name = blocking
        .meta
        .artist
        .as_deref()
        .or(blocking.meta.album_artist.as_deref())
        .unwrap_or("Unknown Artist")
        .to_string();
    let artist_id = get_or_create_artist(&mut tx, &artist_name).await?;

    let album_artist_name = blocking
        .meta
        .album_artist
        .as_deref()
        .or(blocking.meta.artist.as_deref())
        .unwrap_or("Unknown Artist")
        .to_string();
    let album_artist_id = get_or_create_artist(&mut tx, &album_artist_name).await?;

    let album_title = blocking
        .meta
        .album
        .as_deref()
        .unwrap_or("Unknown Album")
        .to_string();
    tracing::info!(
        path = %path.display(),
        recording_id = %recording_id,
        album_title = %album_title,
        album_artist_name = %album_artist_name,
        "Rescan: resolved metadata"
    );
    let release_group_id =
        get_or_create_release_group(&mut tx, &album_title, &album_artist_id).await?;
    tracing::info!(
        path = %path.display(),
        release_group_id = %release_group_id,
        album_title = %album_title,
        "Rescan: resolved release group"
    );
    let release_date = blocking.meta.year.map(|y| y.to_string());
    let release_id = get_or_create_release(
        &mut tx,
        &release_group_id,
        &album_title,
        release_date.as_deref(),
    )
    .await?;
    let disc = blocking.meta.disc_number.unwrap_or(1) as i64;
    let medium_id = get_or_create_medium(&mut tx, &release_id, disc).await?;
    let track_position = blocking.meta.track_number.unwrap_or(0) as i64;

    let title = blocking.meta.title.as_deref().unwrap_or("Unknown Title");
    let fingerprint_str = blocking.fp.as_ref().map(|f| f.fingerprint.as_str());

    let acoustid_value = acoustid_match.as_ref().map(|a| a.acoustid.as_str());
    let recording_mbid_value = acoustid_match
        .as_ref()
        .and_then(|a| a.recording_mbid.as_deref());

    sqlx::query(
        "UPDATE recording
         SET title = ?,
             duration_ms = ?,
             genre = ?,
             bpm = ?,
             comment = ?,
             acoustid = COALESCE(?, acoustid),
             mbid = COALESCE(?, mbid)
         WHERE id = ?",
    )
    .bind(title)
    .bind(blocking.meta.duration_ms as i64)
    .bind(&blocking.meta.genre)
    .bind(blocking.meta.bpm)
    .bind(&blocking.meta.comment)
    .bind(acoustid_value)
    .bind(recording_mbid_value)
    .bind(&recording_id)
    .execute(&mut *tx)
    .await
    .context("Failed to update recording during source rescan")?;

    sync_tags_from_comment(&mut tx, &recording_id, blocking.meta.comment.as_deref()).await?;

    // Replace recording artist (handles artist metadata changes and TXXX=ARTISTS).
    sqlx::query("DELETE FROM recording_artist WHERE recording_id = ?")
        .bind(&recording_id)
        .execute(&mut *tx)
        .await
        .context("Failed to replace recording artist during source rescan")?;
    insert_recording_artist(&mut tx, &recording_id, &artist_id, 0, "main").await?;

    // Additional artists from TXXX=ARTISTS
    if let Some(artists_str) = &blocking.meta.artists {
        let primary_artist_lower = artist_name.to_lowercase();
        let next_pos = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT MAX(position) FROM recording_artist WHERE recording_id = ?",
        )
        .bind(&recording_id)
        .fetch_optional(&mut *tx)
        .await?
        .flatten()
        .unwrap_or(0)
            + 1;

        for (i, name) in artists_str
            .split(';')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .enumerate()
        {
            if name.to_lowercase() == primary_artist_lower {
                continue;
            }
            let aid = get_or_create_artist(&mut tx, name).await?;
            insert_recording_artist(&mut tx, &recording_id, &aid, next_pos + i as i64, "main")
                .await?;
        }
    }

    // Find or create a track for this recording on the newly-asserted release
    // group.  Prefer an existing track that is already on this release group
    // rather than picking the first track by ID order — the latter can select
    // a track that belongs to a different release group and move it, corrupting
    // multi-release recordings whose multiple sources assert different albums.
    let track_id: String = if let Some(id) = sqlx::query_scalar::<_, String>(
        "SELECT t.id FROM track t
         JOIN medium m ON m.id = t.medium_id
         JOIN release rel ON rel.id = m.release_id
         WHERE t.recording_id = ? AND rel.release_group_id = ?
         LIMIT 1",
    )
    .bind(&recording_id)
    .bind(&release_group_id)
    .fetch_optional(&mut *tx)
    .await
    .context("Failed to find existing track on target release group")?
    {
        id
    } else {
        let id = get_or_create_track(&mut tx, &medium_id, &recording_id, track_position).await?;
        tracing::info!(
            recording_id = %recording_id,
            track_id = %id,
            release_group_id = %release_group_id,
            "Rescan: created new track on target release group"
        );
        id
    };

    tracing::info!(
        track_id = %track_id,
        medium_id = %medium_id,
        position = track_position,
        "Rescan: updating track position"
    );
    sqlx::query("UPDATE track SET position = ?, title = NULL, duration_ms = NULL WHERE id = ?")
        .bind(track_position)
        .bind(&track_id)
        .execute(&mut *tx)
        .await
        .context("Failed to update track position during source rescan")?;

    // If the recording has only one source, the track we just found/created
    // is the only one that matters — any other track on a different release
    // group must be stale (left behind by a prior import with different ID3
    // metadata).  Multi-source recordings are intentionally skipped because
    // other source files may legitimately assert other release groups.
    let source_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM source WHERE recording_id = ?")
            .bind(&recording_id)
            .fetch_one(&mut *tx)
            .await
            .context("Failed to count sources for stale-track check")?;

    if source_count == 1 {
        let deleted = sqlx::query(
            "DELETE FROM track
             WHERE id != ?
               AND recording_id = ?
               AND id IN (
                   SELECT t.id FROM track t
                   JOIN medium m ON m.id = t.medium_id
                   JOIN release rel ON rel.id = m.release_id
                   WHERE t.recording_id = ?
                     AND rel.release_group_id != ?
               )",
        )
        .bind(&track_id)
        .bind(&recording_id)
        .bind(&recording_id)
        .bind(&release_group_id)
        .execute(&mut *tx)
        .await
        .context("Failed to clean up stale tracks during source rescan")?;
        if deleted.rows_affected() > 0 {
            tracing::info!(
                recording_id = %recording_id,
                "Rescan: removed stale track(s) from other release groups"
            );
        }
    }

    sqlx::query(
        "UPDATE source
         SET file_hash = ?,
             format = ?,
             duration_ms = ?,
             fingerprint = ?,
             file_size = ?,
             file_mtime_ms = ?,
             track_total = ?,
             replay_gain_track_db = ?,
             replay_gain_track_peak = ?,
             replay_gain_album_db = ?,
             replay_gain_album_peak = ?,
             raw_tags_json = ?,
             last_verified = datetime('now')
         WHERE id = ?",
    )
    .bind(&blocking.hash)
    .bind(&blocking.meta.format)
    .bind(blocking.meta.duration_ms as i64)
    .bind(fingerprint_str)
    .bind(file_size)
    .bind(file_mtime_ms)
    .bind(blocking.meta.track_total.map(|v| v as i64))
    .bind(blocking.meta.replay_gain_track_db)
    .bind(blocking.meta.replay_gain_track_peak)
    .bind(blocking.meta.replay_gain_album_db)
    .bind(blocking.meta.replay_gain_album_peak)
    .bind(&blocking.raw_tags_json)
    .bind(&source_id)
    .execute(&mut *tx)
    .await
    .context("Failed to update source during rescan")?;

    tx.commit()
        .await
        .context("Failed to commit source rescan transaction")?;

    if !skip_prune {
        let mut prune_conn = db
            .acquire(format!("source_rescan.prune path={path_str}"))
            .await
            .context("Failed to acquire DB connection for source rescan cleanup")?;
        prune_conn
            .set_busy_timeout(std::time::Duration::from_secs(30))
            .await
            .context("Failed to set busy timeout for source rescan cleanup")?;
        let mut prune_tx = prune_conn
            .begin()
            .await
            .context("Failed to start source rescan cleanup transaction")?;
        prune_empty_library_entities(&mut prune_tx).await?;
        prune_tx
            .commit()
            .await
            .context("Failed to commit source rescan cleanup transaction")?;
    }

    tracing::info!(path = %path.display(), recording_id = %recording_id, source_id = %source_id, "Rescanned source");
    for warning in blocking.warnings {
        println!("[importer] rescan warning: {}: {}", path.display(), warning);
    }

    // Clear stale DuplicateFrame issues for this file, then push new ones.
    file_issues.retain(|issue| {
        !(issue.kind == FileIssueKind::DuplicateFrame
            && issue.file_path == path.display().to_string())
    });
    for df in &blocking.duplicate_frames {
        file_issues.push_duplicate_frame(
            path.display().to_string(),
            &df.frame_id,
            &df.field_name,
            &df.lofty_value,
            &df.corrected_value,
        );
    }

    Ok((recording_id, release_group_id))
}

/// Returns `Ok(true)` if imported, `Ok(false)` if skipped (already exists).
pub(crate) async fn import_file(
    db: &DbPool,
    path: &Path,
    acoustid_key: Option<&str>,
) -> Result<bool> {
    let Some(prepared) = prepare_import(db, path, acoustid_key).await? else {
        return Ok(false);
    };

    store_prepared_import(db, prepared).await
}

pub(crate) async fn prepare_import(
    db: &DbPool,
    path: &Path,
    acoustid_key: Option<&str>,
) -> Result<Option<PreparedImport>> {
    let (file_size, file_mtime_ms) = file_identity(path).context("Failed to read file metadata")?;
    let path_str = path.to_string_lossy().to_string();
    let mut conn = db
        .acquire(format!(
            "import.prepare.check_existing_source path={path_str}"
        ))
        .await
        .context("Failed to acquire DB connection for import path check")?;
    let existing_source = sqlx::query_as::<_, (String, Option<i64>, Option<i64>)>(
        "SELECT id, file_size, file_mtime_ms FROM source WHERE file_path = ?",
    )
    .bind(&path_str)
    .fetch_optional(&mut *conn)
    .await
    .context("DB error checking existing path")?;

    if let Some((_, existing_size, existing_mtime_ms)) = &existing_source {
        if existing_size == &Some(file_size) && existing_mtime_ms == &Some(file_mtime_ms) {
            return Ok(None);
        }
    }

    let existing_source_id = existing_source.as_ref().map(|(id, _, _)| id.as_str());
    drop(conn);

    struct BlockingResult {
        hash: String,
        meta: TrackMetadata,
        warnings: Vec<String>,
        fp: Option<fingerprint::FingerprintResult>,
        raw_tags_json: String,
    }

    let p = path.to_path_buf();
    let blocking = tokio::task::spawn_blocking(move || {
        let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Min);
        set_io_priority_idle();
        let hash = file_sha256(&p).context("Failed to hash file")?;
        let metadata_read = read_metadata(&p).context("Failed to read metadata")?;
        let fp = match fingerprint::generate_fingerprint(&p) {
            Ok(fp) => Some(fp),
            Err(e) => {
                tracing::warn!(path = %p.display(), "Fingerprint generation failed: {e}");
                None
            }
        };
        let raw_tags_json = build_raw_tags_json(&p, &metadata_read.meta, &metadata_read.all_tags);
        Ok::<_, anyhow::Error>(BlockingResult {
            hash,
            meta: metadata_read.meta,
            warnings: metadata_read.warning.into_iter().collect(),
            fp,
            raw_tags_json,
        })
    })
    .await;

    let blocking = match blocking {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e),
        Err(e) => anyhow::bail!("Blocking import task panicked: {e}"),
    };

    let (hash, meta, warnings, fp, raw_tags_json) = (
        blocking.hash,
        blocking.meta,
        blocking.warnings,
        blocking.fp,
        blocking.raw_tags_json,
    );

    let acoustid_match: Option<AcoustIdMatch> = match (acoustid_key, fp.as_ref()) {
        (Some(key), Some(fp_result)) => match fingerprint::lookup_acoustid(key, fp_result).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(path = %path.display(), "AcoustID lookup failed: {e}");
                None
            }
        },
        _ => None,
    };

    Ok(Some(PreparedImport {
        path: path.to_path_buf(),
        path_str,
        existing_source_id: existing_source_id.map(ToOwned::to_owned),
        hash,
        meta,
        warnings,
        file_size,
        file_mtime_ms,
        fp,
        acoustid_match,
        raw_tags_json,
    }))
}

pub(crate) async fn store_prepared_import(db: &DbPool, prepared: PreparedImport) -> Result<bool> {
    let PreparedImport {
        path,
        path_str,
        existing_source_id,
        hash,
        meta,
        warnings,
        file_size,
        file_mtime_ms,
        fp,
        acoustid_match,
        raw_tags_json,
    } = prepared;

    let mut dup_conn = db
        .acquire(format!("import.store.check_duplicate_hash path={path_str}"))
        .await
        .context("Failed to acquire DB connection for duplicate hash check")?;
    let existing_with_hash = sqlx::query_as::<_, (String, String)>(
        "SELECT id, recording_id FROM source
         WHERE file_hash = ?
           AND (? IS NULL OR id != ?)
         LIMIT 1",
    )
    .bind(&hash)
    .bind(existing_source_id.as_deref())
    .bind(existing_source_id.as_deref())
    .fetch_optional(&mut *dup_conn)
    .await
    .context("DB error checking hash")?;
    drop(dup_conn);

    if let Some((_matched_source_id, matched_recording_id)) = existing_with_hash {
        // This file's content already exists at a different path (e.g. the file was moved or
        // copied). Register the current path as an alternate source for the same recording
        // rather than creating a duplicate recording.
        let source_id = existing_source_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let fingerprint_str = fp.as_ref().map(|f| f.fingerprint.as_str());
        let mut alt_conn = db
            .acquire(format!("import.store.add_alternate_source path={path_str}"))
            .await
            .context("Failed to acquire DB connection for alternate source")?;
        sqlx::query(
            "INSERT INTO source (
                id, recording_id, source_type, file_path, file_hash, format, duration_ms,
                fingerprint, file_size, file_mtime_ms,
                replay_gain_track_db, replay_gain_track_peak,
                replay_gain_album_db, replay_gain_album_peak,
                raw_tags_json
             ) VALUES (?, ?, 'local_file', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(file_path) DO UPDATE SET
                recording_id = excluded.recording_id,
                file_hash = excluded.file_hash,
                format = excluded.format,
                duration_ms = excluded.duration_ms,
                fingerprint = excluded.fingerprint,
                file_size = excluded.file_size,
                file_mtime_ms = excluded.file_mtime_ms,
                replay_gain_track_db = excluded.replay_gain_track_db,
                replay_gain_track_peak = excluded.replay_gain_track_peak,
                replay_gain_album_db = excluded.replay_gain_album_db,
                replay_gain_album_peak = excluded.replay_gain_album_peak,
                raw_tags_json = excluded.raw_tags_json",
        )
        .bind(&source_id)
        .bind(&matched_recording_id)
        .bind(&path_str)
        .bind(&hash)
        .bind(&meta.format)
        .bind(meta.duration_ms as i64)
        .bind(fingerprint_str)
        .bind(file_size)
        .bind(file_mtime_ms)
        .bind(meta.replay_gain_track_db)
        .bind(meta.replay_gain_track_peak)
        .bind(meta.replay_gain_album_db)
        .bind(meta.replay_gain_album_peak)
        .bind(&raw_tags_json)
        .execute(&mut *alt_conn)
        .await
        .context("Failed to insert alternate source")?;

        tracing::debug!(
            path = %path.display(),
            recording_id = %matched_recording_id,
            "Added as alternate source for existing recording (same file hash)"
        );
        for warning in warnings {
            println!("[importer] import warning: {}: {}", path.display(), warning);
        }
        return Ok(true);
    }

    // ── 6. If re-importing (existing source found), look up the recording
    //    to reuse so that we don't orphan the original recording. ─────────────
    let existing_recording_id: Option<String> = if existing_source_id.is_some() {
        let mut conn = db
            .acquire(format!(
                "import.store.lookup_recording_for_reimport path={path_str}"
            ))
            .await
            .context("Failed to acquire DB connection for existing recording lookup")?;
        sqlx::query_scalar::<_, String>("SELECT recording_id FROM source WHERE id = ?")
            .bind(existing_source_id.as_deref().unwrap())
            .fetch_optional(&mut *conn)
            .await
            .context("Failed to look up existing recording for source")?
    } else {
        None
    };

    let mut conn = db
        .acquire(format!("import.store.transaction path={path_str}"))
        .await
        .context("Failed to acquire DB connection for import transaction")?;
    let mut tx = conn
        .begin()
        .await
        .context("Failed to start import transaction")?;

    // ── 7. Artist ─────────────────────────────────────────────────────────────
    let artist_name = meta
        .artist
        .as_deref()
        .or(meta.album_artist.as_deref())
        .unwrap_or("Unknown Artist")
        .to_string();
    let artist_id = get_or_create_artist(&mut tx, &artist_name).await?;

    // The release group artist uses album_artist if available — tracks with
    // different per-track artists but the same album artist belong to one album.
    let album_artist_name = meta
        .album_artist
        .as_deref()
        .or(meta.artist.as_deref())
        .unwrap_or("Unknown Artist")
        .to_string();
    let album_artist_id = get_or_create_artist(&mut tx, &album_artist_name).await?;

    // ── 8. Find or create Recording ───────────────────────────────────────────
    let recording_id = if let Some(ref rec_id) = existing_recording_id {
        // Re-import of a file that already existed — update the existing recording
        // in place rather than creating a new one (which would leave the original
        // recording orphaned with no sources).
        let title = meta.title.as_deref().unwrap_or("Unknown Title");
        let acoustid_value = acoustid_match.as_ref().map(|a| a.acoustid.as_str());
        let recording_mbid_value = acoustid_match
            .as_ref()
            .and_then(|a| a.recording_mbid.as_deref());
        sqlx::query(
            "UPDATE recording
             SET title = ?,
                 duration_ms = ?,
                 genre = ?,
                 bpm = ?,
                 comment = ?,
                 acoustid = COALESCE(?, acoustid),
                 mbid = COALESCE(?, mbid)
             WHERE id = ?",
        )
        .bind(title)
        .bind(meta.duration_ms as i64)
        .bind(&meta.genre)
        .bind(meta.bpm)
        .bind(&meta.comment)
        .bind(acoustid_value)
        .bind(recording_mbid_value)
        .bind(rec_id)
        .execute(&mut *tx)
        .await
        .context("Failed to update recording during re-import")?;

        // Replace recording artist (handles artist metadata changes).
        sqlx::query("DELETE FROM recording_artist WHERE recording_id = ?")
            .bind(rec_id)
            .execute(&mut *tx)
            .await
            .context("Failed to replace recording artist during re-import")?;
        insert_recording_artist(&mut tx, rec_id, &artist_id, 0, "main").await?;

        rec_id.clone()
    } else {
        find_or_create_recording(&mut tx, &meta, &artist_id, acoustid_match.as_ref()).await?
    };
    sync_tags_from_comment(&mut tx, &recording_id, meta.comment.as_deref()).await?;

    // ── 8b. Additional artists from TXXX=ARTISTS ──────────────────────────
    if let Some(artists_str) = &meta.artists {
        let primary_artist_lower = artist_name.to_lowercase();
        let next_pos = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT MAX(position) FROM recording_artist WHERE recording_id = ?",
        )
        .bind(&recording_id)
        .fetch_optional(&mut *tx)
        .await?
        .flatten()
        .unwrap_or(0)
            + 1;

        for (i, name) in artists_str
            .split(';')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .enumerate()
        {
            if name.to_lowercase() == primary_artist_lower {
                continue;
            }
            let aid = get_or_create_artist(&mut tx, name).await?;
            insert_recording_artist(&mut tx, &recording_id, &aid, next_pos + i as i64, "main")
                .await?;
        }
    }

    // ── 9. ReleaseGroup / Release / Medium ────────────────────────────────────
    let album_title = meta.album.as_deref().unwrap_or("Unknown Album").to_string();
    let release_group_id =
        get_or_create_release_group(&mut tx, &album_title, &album_artist_id).await?;

    let release_date = meta.year.map(|y| y.to_string());
    let release_id = get_or_create_release(
        &mut tx,
        &release_group_id,
        &album_title,
        release_date.as_deref(),
    )
    .await?;

    let disc = meta.disc_number.unwrap_or(1) as i64;
    let medium_id = get_or_create_medium(&mut tx, &release_id, disc).await?;

    // ── 10. Track ─────────────────────────────────────────────────────────────
    let track_position = meta.track_number.unwrap_or(0) as i64;
    get_or_create_track(&mut tx, &medium_id, &recording_id, track_position).await?;

    let source_id = existing_source_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let fingerprint_str = fp.as_ref().map(|f| f.fingerprint.as_str());
    sqlx::query(
        "INSERT INTO source (
            id, recording_id, source_type, file_path, file_hash, format, duration_ms,
            fingerprint, file_size, file_mtime_ms, track_total,
            replay_gain_track_db, replay_gain_track_peak,
            replay_gain_album_db, replay_gain_album_peak,
            raw_tags_json
         ) VALUES (?, ?, 'local_file', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(file_path) DO UPDATE SET
            recording_id = excluded.recording_id,
            file_hash = excluded.file_hash,
            format = excluded.format,
            duration_ms = excluded.duration_ms,
            fingerprint = excluded.fingerprint,
            file_size = excluded.file_size,
            file_mtime_ms = excluded.file_mtime_ms,
            track_total = excluded.track_total,
            replay_gain_track_db = excluded.replay_gain_track_db,
            replay_gain_track_peak = excluded.replay_gain_track_peak,
            replay_gain_album_db = excluded.replay_gain_album_db,
            replay_gain_album_peak = excluded.replay_gain_album_peak,
            raw_tags_json = excluded.raw_tags_json",
    )
    .bind(&source_id)
    .bind(&recording_id)
    .bind(&path_str)
    .bind(&hash)
    .bind(&meta.format)
    .bind(meta.duration_ms as i64)
    .bind(fingerprint_str)
    .bind(file_size)
    .bind(file_mtime_ms)
    .bind(meta.track_total.map(|v| v as i64))
    .bind(meta.replay_gain_track_db)
    .bind(meta.replay_gain_track_peak)
    .bind(meta.replay_gain_album_db)
    .bind(meta.replay_gain_album_peak)
    .bind(&raw_tags_json)
    .execute(&mut *tx)
    .await
    .context("Failed to insert source")?;

    tx.commit()
        .await
        .context("Failed to commit import transaction")?;

    tracing::debug!(
        path = %path.display(),
        recording_id = %recording_id,
        acoustid = acoustid_match.as_ref().map(|a| a.acoustid.as_str()).unwrap_or("none"),
        "Imported file"
    );
    for warning in warnings {
        println!("[importer] import warning: {}: {}", path.display(), warning);
    }
    Ok(true)
}

// ─────────────────────────────────────────────────────────────────────────────
// Recording deduplication (3 levels)
// ─────────────────────────────────────────────────────────────────────────────

/// Find an existing recording or create a new one.
///
/// Deduplication order:
/// 1. AcoustID match — same acoustic fingerprint → same recording.
/// 2. Create new.
///
/// SHA-256 deduplication (byte-for-byte identical files) is handled upstream
/// in `store_prepared_import` before this function is called.
async fn find_or_create_recording(
    tx: &mut Transaction<'_, Sqlite>,
    meta: &crate::models::TrackMetadata,
    artist_id: &str,
    acoustid_match: Option<&AcoustIdMatch>,
) -> Result<String> {
    let title = meta.title.as_deref().unwrap_or("Unknown Title");
    let duration = meta.duration_ms as i64;

    // ── Level 1: AcoustID ────────────────────────────────────────────────────
    if let Some(aid) = acoustid_match {
        if let Some(id) =
            sqlx::query_scalar::<_, String>("SELECT id FROM recording WHERE acoustid = ?")
                .bind(&aid.acoustid)
                .fetch_optional(&mut **tx)
                .await?
        {
            tracing::debug!(acoustid = %aid.acoustid, recording_id = %id, "AcoustID hit");

            // Update the recording's primary artist from this source's TPE1.
            // When a new source file matches an existing recording via AcoustID,
            // the recording may have been created from a different source whose
            // TPE1 was missing or set to a generic album-artist value (e.g.
            // "Various Artists").  We replace position 0 (the primary artist)
            // while preserving any additional positions (featuring artists etc).
            sqlx::query(
                "INSERT INTO recording_artist (recording_id, artist_id, position, role, credited_as)
                 VALUES (?, ?, 0, 'main', NULL)
                 ON CONFLICT(recording_id, position) DO UPDATE
                 SET artist_id = excluded.artist_id,
                     role      = excluded.role,
                     credited_as = excluded.credited_as",
            )
                .bind(&id)
                .bind(artist_id)
                .execute(&mut **tx)
                .await
                .context("Failed to update primary recording artist on AcoustID match")?;

            return Ok(id);
        }
    }

    // ── Level 2: create new ──────────────────────────────────────────────────
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO recording (id, title, duration_ms, genre, bpm, comment, acoustid, mbid)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(title)
    .bind(duration)
    .bind(&meta.genre)
    .bind(meta.bpm)
    .bind(&meta.comment)
    .bind(acoustid_match.as_ref().map(|a| a.acoustid.as_str()))
    .bind(acoustid_match.and_then(|a| a.recording_mbid.as_deref()))
    .execute(&mut **tx)
    .await?;

    insert_recording_artist(tx, &id, artist_id, 0, "main").await?;

    Ok(id)
}

// ─────────────────────────────────────────────────────────────────────────────
// "Get or create" helpers
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) async fn get_or_create_artist(
    tx: &mut Transaction<'_, Sqlite>,
    name: &str,
) -> Result<String> {
    let name_lower = name.to_lowercase();
    if let Some(id) = sqlx::query_scalar::<_, String>("SELECT id FROM artist WHERE lower(name) = ?")
        .bind(&name_lower)
        .fetch_optional(&mut **tx)
        .await?
    {
        return Ok(id);
    }

    let id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO artist (id, name, sort_name) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(name)
        .bind(name)
        .execute(&mut **tx)
        .await?;
    Ok(id)
}

async fn get_or_create_release_group(
    tx: &mut Transaction<'_, Sqlite>,
    title: &str,
    artist_id: &str,
) -> Result<String> {
    let title_lower = title.to_lowercase();
    if let Some(id) = sqlx::query_scalar::<_, String>(
        "SELECT rg.id FROM release_group rg
         JOIN release_group_artist rga ON rga.release_group_id = rg.id
         WHERE lower(rg.title) = ? AND rga.artist_id = ?
         LIMIT 1",
    )
    .bind(&title_lower)
    .bind(artist_id)
    .fetch_optional(&mut **tx)
    .await?
    {
        tracing::info!(release_group_id = %id, title = %title, "get_or_create_release_group: found existing");
        return Ok(id);
    }

    let id = Uuid::new_v4().to_string();
    tracing::info!(release_group_id = %id, title = %title, "get_or_create_release_group: creating new");
    sqlx::query("INSERT INTO release_group (id, title, rg_type) VALUES (?, ?, 'album')")
        .bind(&id)
        .bind(title)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO release_group_artist (release_group_id, artist_id, position, role)
         VALUES (?, ?, 0, 'main')",
    )
    .bind(&id)
    .bind(artist_id)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

async fn get_or_create_release(
    tx: &mut Transaction<'_, Sqlite>,
    release_group_id: &str,
    title: &str,
    release_date: Option<&str>,
) -> Result<String> {
    if let Some(id) =
        sqlx::query_scalar::<_, String>("SELECT id FROM release WHERE release_group_id = ? LIMIT 1")
            .bind(release_group_id)
            .fetch_optional(&mut **tx)
            .await?
    {
        tracing::info!(release_id = %id, release_group_id = %release_group_id, "get_or_create_release: found existing");
        return Ok(id);
    }

    let id = Uuid::new_v4().to_string();
    tracing::info!(release_id = %id, release_group_id = %release_group_id, "get_or_create_release: creating new");
    sqlx::query(
        "INSERT INTO release (id, release_group_id, title, release_date) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(release_group_id)
    .bind(title)
    .bind(release_date)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

async fn get_or_create_medium(
    tx: &mut Transaction<'_, Sqlite>,
    release_id: &str,
    disc: i64,
) -> Result<String> {
    if let Some(id) = sqlx::query_scalar::<_, String>(
        "SELECT id FROM medium WHERE release_id = ? AND position = ?",
    )
    .bind(release_id)
    .bind(disc)
    .fetch_optional(&mut **tx)
    .await?
    {
        tracing::info!(medium_id = %id, release_id = %release_id, disc = %disc, "get_or_create_medium: found existing");
        return Ok(id);
    }

    let id = Uuid::new_v4().to_string();
    tracing::info!(medium_id = %id, release_id = %release_id, disc = %disc, "get_or_create_medium: creating new");
    sqlx::query(
        "INSERT INTO medium (id, release_id, position, format) VALUES (?, ?, ?, 'Digital')",
    )
    .bind(&id)
    .bind(release_id)
    .bind(disc)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

async fn get_or_create_track(
    tx: &mut Transaction<'_, Sqlite>,
    medium_id: &str,
    recording_id: &str,
    position: i64,
) -> Result<String> {
    if let Some(id) = sqlx::query_scalar::<_, String>(
        "SELECT id FROM track WHERE medium_id = ? AND recording_id = ?",
    )
    .bind(medium_id)
    .bind(recording_id)
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(id);
    }

    let id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO track (id, medium_id, recording_id, position) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(medium_id)
        .bind(recording_id)
        .bind(position)
        .execute(&mut **tx)
        .await?;
    Ok(id)
}

async fn insert_recording_artist(
    tx: &mut Transaction<'_, Sqlite>,
    recording_id: &str,
    artist_id: &str,
    position: i64,
    role: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO recording_artist (recording_id, artist_id, position, role)
         VALUES (?, ?, ?, ?)",
    )
    .bind(recording_id)
    .bind(artist_id)
    .bind(position)
    .bind(role)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Sync `recording_tag` rows from the recording's comment field.
///
/// Always updates `recording.comment` so rescans pick up changes, then
/// replaces all tag rows with ones freshly parsed from the comment.
async fn sync_tags_from_comment(
    tx: &mut Transaction<'_, Sqlite>,
    recording_id: &str,
    comment: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE recording SET comment = ? WHERE id = ?")
        .bind(comment)
        .bind(recording_id)
        .execute(&mut **tx)
        .await?;

    sqlx::query("DELETE FROM recording_tag WHERE recording_id = ?")
        .bind(recording_id)
        .execute(&mut **tx)
        .await?;

    let tags = comment
        .map(super::scanner::parse_comment_tags)
        .unwrap_or_default();

    for tag in tags {
        sqlx::query("INSERT OR IGNORE INTO recording_tag (recording_id, tag) VALUES (?, ?)")
            .bind(recording_id)
            .bind(&tag)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Run after a batch of rescans to clean up any orphaned library entities.
pub async fn prune_library(db: &DbPool) -> Result<()> {
    let mut conn = db
        .acquire("prune_library".to_string())
        .await
        .context("Failed to acquire DB connection for library pruning")?;
    conn.set_busy_timeout(std::time::Duration::from_secs(30))
        .await
        .context("Failed to set busy timeout for library pruning")?;
    let mut tx = conn
        .begin()
        .await
        .context("Failed to start library pruning transaction")?;
    prune_empty_library_entities(&mut tx).await?;
    tx.commit()
        .await
        .context("Failed to commit library pruning transaction")?;
    Ok(())
}

async fn prune_empty_library_entities(tx: &mut Transaction<'_, Sqlite>) -> Result<()> {
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

/// Detect and delete duplicate track entries.
///
/// A "duplicate track entry" is when the same recording has more than one
/// track on the same release group at the same track position.  When such
/// duplicates exist, all but one are kept (the one with the smallest ID).
///
/// Returns the number of rows deleted.
pub async fn fix_duplicate_track_entries(db: &DbPool) -> Result<u64> {
    let mut conn = db
        .acquire("fix_duplicate_track_entries".to_string())
        .await
        .context("Failed to acquire DB connection for duplicate track cleanup")?;
    conn.set_busy_timeout(std::time::Duration::from_secs(30))
        .await
        .context("Failed to set busy timeout for duplicate track cleanup")?;
    let mut tx = conn
        .begin()
        .await
        .context("Failed to start duplicate track cleanup transaction")?;
    let deleted = fix_duplicate_track_entries_inner(&mut tx).await?;
    tx.commit()
        .await
        .context("Failed to commit duplicate track cleanup transaction")?;
    Ok(deleted)
}

async fn fix_duplicate_track_entries_inner(tx: &mut Transaction<'_, Sqlite>) -> Result<u64> {
    let result = sqlx::query(
        "DELETE FROM track WHERE id IN (
            SELECT t.id FROM track t
            JOIN medium m ON m.id = t.medium_id
            JOIN release rel ON rel.id = m.release_id
            WHERE (t.recording_id, rel.release_group_id, t.position) IN (
                SELECT t2.recording_id, rel2.release_group_id, t2.position
                FROM track t2
                JOIN medium m2 ON m2.id = t2.medium_id
                JOIN release rel2 ON rel2.id = m2.release_id
                GROUP BY t2.recording_id, rel2.release_group_id, t2.position
                HAVING COUNT(*) > 1
            )
            AND t.id NOT IN (
                SELECT MIN(t3.id)
                FROM track t3
                JOIN medium m3 ON m3.id = t3.medium_id
                JOIN release rel3 ON rel3.id = m3.release_id
                WHERE (t3.recording_id, rel3.release_group_id, t3.position) IN (
                    SELECT t4.recording_id, rel4.release_group_id, t4.position
                    FROM track t4
                    JOIN medium m4 ON m4.id = t4.medium_id
                    JOIN release rel4 ON rel4.id = m4.release_id
                    GROUP BY t4.recording_id, rel4.release_group_id, t4.position
                    HAVING COUNT(*) > 1
                )
                GROUP BY t3.recording_id, rel3.release_group_id, t3.position
            )
        )",
    )
    .execute(&mut **tx)
    .await
    .context("Failed to delete duplicate track entries")?;
    Ok(result.rows_affected())
}

// ─────────────────────────────────────────────────────────────────────────────
// Utilities
// ─────────────────────────────────────────────────────────────────────────────

fn file_sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn file_identity(path: &Path) -> Result<(i64, i64)> {
    let metadata = std::fs::metadata(path)?;
    let file_size = i64::try_from(metadata.len()).context("File too large to index")?;
    let modified = metadata.modified()?;
    let file_mtime_ms = i64::try_from(modified.duration_since(UNIX_EPOCH)?.as_millis())
        .context("File modification time out of range")?;
    Ok((file_size, file_mtime_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_pool;
    use crate::file_issues::FileIssueKind;
    use tempfile::TempDir;

    fn tagged_meta(
        title: &str,
        artist: &str,
        album_artist: &str,
        album: &str,
        track_number: u32,
    ) -> TrackMetadata {
        TrackMetadata {
            title: Some(title.into()),
            artist: Some(artist.into()),
            album_artist: Some(album_artist.into()),
            album: Some(album.into()),
            year: Some(2024),
            track_number: Some(track_number),
            track_total: Some(1),
            disc_number: Some(1),
            duration_ms: 200000,
            format: "mp3".into(),
            ..Default::default()
        }
    }

    fn utf8_text_frame(value: &str) -> Vec<u8> {
        let mut data = vec![0x03u8];
        data.extend_from_slice(value.as_bytes());
        data
    }

    fn write_tagged_mp3(path: &Path, title: &str, artist: &str, album_artist: &str, album: &str) {
        let title_data = utf8_text_frame(title);
        let artist_data = utf8_text_frame(artist);
        let album_artist_data = utf8_text_frame(album_artist);
        let album_data = utf8_text_frame(album);
        let track_data = utf8_text_frame("1/1");
        let disc_data = utf8_text_frame("1/1");

        let frames = vec![
            crate::library::scanner::id3_frame(b"TIT2", &title_data),
            crate::library::scanner::id3_frame(b"TPE1", &artist_data),
            crate::library::scanner::id3_frame(b"TPE2", &album_artist_data),
            crate::library::scanner::id3_frame(b"TALB", &album_data),
            crate::library::scanner::id3_frame(b"TRCK", &track_data),
            crate::library::scanner::id3_frame(b"TPOS", &disc_data),
        ];
        let mp3_data = crate::library::scanner::synth_mp3_with_id3(&frames);
        std::fs::write(path, mp3_data).unwrap();
    }

    fn prepared_import_for_path(
        path: &Path,
        meta: TrackMetadata,
        acoustid: Option<&str>,
    ) -> PreparedImport {
        let (file_size, file_mtime_ms) = file_identity(path).unwrap();
        PreparedImport {
            path: path.to_path_buf(),
            path_str: path.to_string_lossy().to_string(),
            existing_source_id: None,
            hash: file_sha256(path).unwrap(),
            meta,
            warnings: vec![],
            file_size,
            file_mtime_ms,
            fp: None,
            acoustid_match: acoustid.map(|acoustid| AcoustIdMatch {
                acoustid: acoustid.to_string(),
                score: 1.0,
                recording_mbid: None,
            }),
            raw_tags_json: "[]".into(),
        }
    }

    async fn release_group_id_by_title(pool: &crate::db::DbPool, title: &str) -> Option<String> {
        let mut conn = pool
            .acquire("test.release_group_id_by_title")
            .await
            .unwrap();
        sqlx::query_scalar("SELECT id FROM release_group WHERE title = ? ORDER BY id LIMIT 1")
            .bind(title)
            .fetch_optional(&mut *conn)
            .await
            .unwrap()
    }

    async fn recording_id_for_path(pool: &crate::db::DbPool, path: &Path) -> String {
        let path_str = path.to_string_lossy().to_string();
        let mut conn = pool.acquire("test.recording_id_for_path").await.unwrap();
        sqlx::query_scalar(
            "SELECT recording_id FROM source WHERE file_path = ? AND source_type = 'local_file'",
        )
        .bind(path_str)
        .fetch_one(&mut *conn)
        .await
        .unwrap()
    }

    async fn release_titles_for_recording(
        pool: &crate::db::DbPool,
        recording_id: &str,
    ) -> Vec<String> {
        let mut conn = pool
            .acquire("test.release_titles_for_recording")
            .await
            .unwrap();
        sqlx::query_scalar(
            "SELECT DISTINCT rg.title
             FROM track t
             JOIN medium m ON m.id = t.medium_id
             JOIN release rel ON rel.id = m.release_id
             JOIN release_group rg ON rg.id = rel.release_group_id
             WHERE t.recording_id = ?
             ORDER BY rg.title",
        )
        .bind(recording_id)
        .fetch_all(&mut *conn)
        .await
        .unwrap()
    }

    async fn source_paths_for_release_group(
        pool: &crate::db::DbPool,
        release_group_id: &str,
    ) -> Vec<String> {
        let mut conn = pool
            .acquire("test.source_paths_for_release_group")
            .await
            .unwrap();
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
        .bind(release_group_id)
        .fetch_all(&mut *conn)
        .await
        .unwrap()
    }

    async fn force_track_id_for_release(
        pool: &crate::db::DbPool,
        recording_id: &str,
        release_group_title: &str,
        track_id: &str,
    ) {
        let mut conn = pool
            .acquire("test.force_track_id_for_release")
            .await
            .unwrap();
        let result = sqlx::query(
            "UPDATE track
             SET id = ?
             WHERE id = (
                 SELECT t.id
                 FROM track t
                 JOIN medium m ON m.id = t.medium_id
                 JOIN release rel ON rel.id = m.release_id
                 JOIN release_group rg ON rg.id = rel.release_group_id
                 WHERE t.recording_id = ? AND rg.title = ?
                 LIMIT 1
             )",
        )
        .bind(track_id)
        .bind(recording_id)
        .bind(release_group_title)
        .execute(&mut *conn)
        .await
        .unwrap();
        assert_eq!(
            result.rows_affected(),
            1,
            "expected one track for recording {recording_id} on release group {release_group_title}"
        );
    }

    async fn add_release_appearance(
        pool: &crate::db::DbPool,
        recording_id: &str,
        album_artist_name: &str,
        release_group_title: &str,
        track_id: &str,
    ) {
        let mut conn = pool.acquire("test.add_release_appearance").await.unwrap();
        let mut tx = conn.begin().await.unwrap();
        let album_artist_id = get_or_create_artist(&mut tx, album_artist_name)
            .await
            .unwrap();
        let release_group_id =
            get_or_create_release_group(&mut tx, release_group_title, &album_artist_id)
                .await
                .unwrap();
        let release_id =
            get_or_create_release(&mut tx, &release_group_id, release_group_title, None)
                .await
                .unwrap();
        let medium_id = get_or_create_medium(&mut tx, &release_id, 1).await.unwrap();
        sqlx::query(
            "INSERT INTO track (id, medium_id, recording_id, position) VALUES (?, ?, ?, 1)",
        )
        .bind(track_id)
        .bind(&medium_id)
        .bind(recording_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn test_album_artist_groups_tracks_together() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();

        let meta1 = TrackMetadata {
            title: Some("Song A".into()),
            artist: Some("Track Artist A".into()),
            album_artist: Some("Album Artist".into()),
            album: Some("Same Album".into()),
            year: Some(2024),
            track_number: Some(1),
            track_total: Some(2),
            disc_number: Some(1),
            duration_ms: 200000,
            format: "mp3".into(),
            genre: None,
            bpm: None,
            comment: None,
            replay_gain_track_db: None,
            replay_gain_track_peak: None,
            replay_gain_album_db: None,
            replay_gain_album_peak: None,
            artists: None,
        };

        let meta2 = TrackMetadata {
            title: Some("Song B".into()),
            artist: Some("Track Artist B".into()),
            track_number: Some(2),
            ..meta1.clone()
        };

        let prepared1 = PreparedImport {
            path: PathBuf::from("/tmp/test1.mp3"),
            path_str: "/tmp/test1.mp3".to_string(),
            existing_source_id: None,
            hash: "a".repeat(64),
            meta: meta1,
            warnings: vec![],
            file_size: 5000000,
            file_mtime_ms: 1000000,
            fp: None,
            acoustid_match: None,
            raw_tags_json: "[]".into(),
        };

        let prepared2 = PreparedImport {
            path: PathBuf::from("/tmp/test2.mp3"),
            path_str: "/tmp/test2.mp3".to_string(),
            existing_source_id: None,
            hash: "b".repeat(64),
            meta: meta2,
            warnings: vec![],
            file_size: 6000000,
            file_mtime_ms: 2000000,
            fp: None,
            acoustid_match: None,
            raw_tags_json: "[]".into(),
        };

        store_prepared_import(&pool, prepared1).await.unwrap();
        store_prepared_import(&pool, prepared2).await.unwrap();

        // Both tracks should share exactly one release_group.
        let mut conn = pool.acquire("test_query".to_string()).await.unwrap();
        let rg_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT rg.id)
             FROM release_group rg
             JOIN release_group_artist rga ON rga.release_group_id = rg.id
             JOIN artist a ON a.id = rga.artist_id
             WHERE a.name = 'Album Artist'",
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert_eq!(
            rg_count, 1,
            "Both tracks should be grouped into one release group"
        );
    }

    #[tokio::test]
    async fn test_acoustid_match_updates_primary_artist() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();

        // Two sources with the same AcoustID but different TPE1:
        //  - Source 1: TPE1 = "Various Artists" (generic album-artist value)
        //  - Source 2: TPE1 = "Track Artist" (the real track performer)
        //
        // After import, the recording's primary artist should come from
        // source 2's TPE1 (updated via the AcoustID-match path).

        let meta1 = TrackMetadata {
            title: Some("Song".into()),
            artist: Some("Various Artists".into()),
            album_artist: Some("Various Artists".into()),
            album: Some("Compilation".into()),
            year: Some(2024),
            track_number: Some(1),
            track_total: Some(2),
            disc_number: Some(1),
            duration_ms: 200000,
            format: "mp3".into(),
            ..Default::default()
        };

        let meta2 = TrackMetadata {
            title: Some("Song".into()),
            artist: Some("Track Artist".into()),
            album_artist: Some("Various Artists".into()),
            album: Some("Compilation".into()),
            track_number: Some(2),
            ..Default::default()
        };

        let prepared1 = PreparedImport {
            path: PathBuf::from("/tmp/acoustid_test_1.mp3"),
            path_str: "/tmp/acoustid_test_1.mp3".to_string(),
            existing_source_id: None,
            hash: "aaa".repeat(64),
            meta: meta1,
            warnings: vec![],
            file_size: 5000000,
            file_mtime_ms: 1000000,
            fp: None,
            acoustid_match: Some(AcoustIdMatch {
                acoustid: "test_fp_for_artist_update".to_string(),
                score: 1.0,
                recording_mbid: None,
            }),
            raw_tags_json: "[]".into(),
        };

        let prepared2 = PreparedImport {
            path: PathBuf::from("/tmp/acoustid_test_2.mp3"),
            path_str: "/tmp/acoustid_test_2.mp3".to_string(),
            existing_source_id: None,
            hash: "bbb".repeat(64),
            meta: meta2,
            warnings: vec![],
            file_size: 6000000,
            file_mtime_ms: 2000000,
            fp: None,
            acoustid_match: Some(AcoustIdMatch {
                acoustid: "test_fp_for_artist_update".to_string(),
                score: 1.0,
                recording_mbid: None,
            }),
            raw_tags_json: "[]".into(),
        };

        store_prepared_import(&pool, prepared1).await.unwrap();
        store_prepared_import(&pool, prepared2).await.unwrap();

        // Both sources should share one recording (AcoustID match).
        let mut conn = pool.acquire("test".to_string()).await.unwrap();
        let recording_count: i64 =
            sqlx::query_scalar("SELECT COUNT(DISTINCT recording_id) FROM source")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(
            recording_count, 1,
            "Both sources should share one recording via AcoustID match"
        );

        // The recording's primary artist should be source 2's TPE1,
        // updated via the AcoustID-match path. It should NOT remain
        // "Various Artists" from source 1.
        let artist_name: String = sqlx::query_scalar(
            "SELECT a.name
             FROM recording_artist ra
             JOIN artist a ON a.id = ra.artist_id
             WHERE ra.position = 0
               AND ra.recording_id = (SELECT recording_id FROM source LIMIT 1)",
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert_eq!(
            artist_name, "Track Artist",
            "AcoustID match should update primary artist from new source's TPE1"
        );

        // The album artist should remain the original album artist ("Various Artists"),
        // since release-group-level metadata is not affected by recording-level changes.
        let album_artist_name: String = sqlx::query_scalar(
            "SELECT a.name
             FROM release_group_artist rga
             JOIN artist a ON a.id = rga.artist_id
             WHERE rga.position = 0",
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert_eq!(
            album_artist_name, "Various Artists",
            "Album artist should remain unchanged"
        );
    }

    #[tokio::test]
    async fn test_album_rescan_prunes_stale_ddr_extreme_release() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();
        let serializer = tokio::sync::Semaphore::new(1);
        let file_issues = FileIssueLog::new();

        // This mirrors a stale app state seen in the wild: the recording has
        // one source, and that source's TALB frame asserts the corrected album,
        // but the recording still appears on both the corrected album and an
        // older stale album.
        let stale_path = tmp.path().join("stale-album.mp3");
        write_tagged_mp3(
            &stale_path,
            "Stale Track",
            "DDR Artist",
            "DDR Album Artist",
            "Dance Dance Revolution EXTREME ORIGINAL SOUNDTRACK",
        );
        store_prepared_import(
            &pool,
            prepared_import_for_path(
                &stale_path,
                tagged_meta(
                    "Stale Track",
                    "DDR Artist",
                    "DDR Album Artist",
                    "Dance Dance Revolution Extreme",
                    1,
                ),
                None,
            ),
        )
        .await
        .unwrap();

        let stale_recording_id = recording_id_for_path(&pool, &stale_path).await;
        force_track_id_for_release(
            &pool,
            &stale_recording_id,
            "Dance Dance Revolution Extreme",
            "z-stale-ddr-extreme-track",
        )
        .await;
        add_release_appearance(
            &pool,
            &stale_recording_id,
            "DDR Album Artist",
            "Dance Dance Revolution EXTREME ORIGINAL SOUNDTRACK",
            "a-correct-ddr-extreme-track",
        )
        .await;

        assert_eq!(
            release_titles_for_recording(&pool, &stale_recording_id).await,
            vec![
                "Dance Dance Revolution EXTREME ORIGINAL SOUNDTRACK",
                "Dance Dance Revolution Extreme"
            ],
            "test setup should mirror a recording that appears on both old and corrected releases"
        );
        assert!(
            release_group_id_by_title(&pool, "Dance Dance Revolution Extreme")
                .await
                .is_some(),
            "test setup should create the old release group"
        );

        let stale_release_group_id =
            release_group_id_by_title(&pool, "Dance Dance Revolution Extreme")
                .await
                .unwrap();
        let stale_rescan_paths =
            source_paths_for_release_group(&pool, &stale_release_group_id).await;
        assert_eq!(
            stale_rescan_paths,
            vec![stale_path.to_string_lossy().to_string()],
            "album rescan should find the recording's single source"
        );

        for path in stale_rescan_paths {
            rescan_source(
                &pool,
                Path::new(&path),
                None,
                &serializer,
                true,
                &file_issues,
            )
            .await
            .unwrap();
        }
        prune_library(&pool).await.unwrap();

        assert_eq!(
            release_titles_for_recording(&pool, &stale_recording_id).await,
            vec!["Dance Dance Revolution EXTREME ORIGINAL SOUNDTRACK"],
            "the recording should only remain associated with the album asserted by its rescanned source"
        );
        assert!(
            release_group_id_by_title(&pool, "Dance Dance Revolution Extreme")
                .await
                .is_none(),
            "the old release group should be deleted once no recordings remain"
        );
    }

    #[tokio::test]
    async fn test_album_rescan_preserves_multi_release_recording_when_one_source_asserts_target() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();
        let serializer = tokio::sync::Semaphore::new(1);
        let file_issues = FileIssueLog::new();

        // Edge case from the feature note: one recording can legitimately
        // appear on multiple releases, with different source files asserting
        // each appearance. A target album rescan must inspect every source for
        // the recording before deciding whether to remove the target release.
        let best_path = tmp.path().join("10-best-of-cool-band.mp3");
        let cool_path = tmp.path().join("20-cool-album.mp3");
        write_tagged_mp3(
            &best_path,
            "Cool Track",
            "Cool Band",
            "Cool Band",
            "Best of Cool Band",
        );
        write_tagged_mp3(
            &cool_path,
            "Cool Track",
            "Cool Band",
            "Cool Band",
            "Cool Album",
        );

        store_prepared_import(
            &pool,
            prepared_import_for_path(
                &best_path,
                tagged_meta(
                    "Cool Track",
                    "Cool Band",
                    "Cool Band",
                    "Best of Cool Band",
                    1,
                ),
                Some("cool-track-shared-acoustid"),
            ),
        )
        .await
        .unwrap();
        store_prepared_import(
            &pool,
            prepared_import_for_path(
                &cool_path,
                tagged_meta("Cool Track", "Cool Band", "Cool Band", "Cool Album", 1),
                Some("cool-track-shared-acoustid"),
            ),
        )
        .await
        .unwrap();

        let cool_recording_id = recording_id_for_path(&pool, &best_path).await;
        assert_eq!(
            recording_id_for_path(&pool, &cool_path).await,
            cool_recording_id,
            "test setup should attach both source files to one recording"
        );
        force_track_id_for_release(
            &pool,
            &cool_recording_id,
            "Best of Cool Band",
            "a-best-of-track",
        )
        .await;
        force_track_id_for_release(
            &pool,
            &cool_recording_id,
            "Cool Album",
            "z-cool-album-track",
        )
        .await;

        let best_of_release_group_id = release_group_id_by_title(&pool, "Best of Cool Band")
            .await
            .unwrap();
        let best_of_rescan_paths =
            source_paths_for_release_group(&pool, &best_of_release_group_id).await;
        assert_eq!(
            best_of_rescan_paths,
            vec![
                best_path.to_string_lossy().to_string(),
                cool_path.to_string_lossy().to_string()
            ],
            "album rescan should rescan all local sources for recordings on the target release"
        );

        for path in best_of_rescan_paths {
            rescan_source(
                &pool,
                Path::new(&path),
                None,
                &serializer,
                true,
                &file_issues,
            )
            .await
            .unwrap();
        }
        prune_library(&pool).await.unwrap();

        assert_eq!(
            release_titles_for_recording(&pool, &cool_recording_id).await,
            vec!["Best of Cool Band", "Cool Album"],
            "the target release must remain because one of the recording's sources still asserts it"
        );
    }

    #[tokio::test]
    async fn test_diagnose_ddr_extreme_rescan() {
        let path = std::path::Path::new("/home/nebu/Music/!Full Albums/Game - 2003 - Dance Dance Revolution Extreme/DDR Extreme Disc 1 (02) Houseboyz - We Will Rock You.mp3");
        let db_path =
            std::path::Path::new("/home/nebu/.local/share/net.nebupookins.thmp5/library.db");
        let pool = crate::db::init_pool(db_path).await.unwrap();

        // Read metadata first
        let meta = crate::library::scanner::read_metadata(path).unwrap();
        println!(
            "METADATA: album={:?} album_artist={:?} artist={:?}",
            meta.meta.album, meta.meta.album_artist, meta.meta.artist
        );

        let lower_title = meta
            .meta
            .album
            .as_deref()
            .unwrap_or("Unknown Album")
            .to_lowercase();
        let album_artist_name = meta
            .meta
            .album_artist
            .as_deref()
            .or(meta.meta.artist.as_deref())
            .unwrap_or("Unknown Artist");
        let mut conn = pool.acquire("diag.artist_id").await.unwrap();
        let artist_id: String = sqlx::query_scalar("SELECT id FROM artist WHERE lower(name) = ?")
            .bind(&album_artist_name.to_lowercase())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        drop(conn);
        println!("RESOLVED: album_artist_name={album_artist_name} artist_id={artist_id} lower_title={lower_title}");

        // Check what get_or_create_release_group would match
        let mut cc = pool.acquire("diag.match").await.unwrap();
        let matched: Vec<(String, String)> = sqlx::query_as(
            "SELECT rg.id, rg.title FROM release_group rg
             JOIN release_group_artist rga ON rga.release_group_id = rg.id
             WHERE lower(rg.title) = ? AND rga.artist_id = ?",
        )
        .bind(&lower_title)
        .bind(&artist_id)
        .fetch_all(&mut *cc)
        .await
        .unwrap();
        println!("SQL MATCH would find: {:?}", matched);

        // Check what the old release_group_artist is
        let old_artist: Vec<(String, String)> = sqlx::query_as(
            "SELECT rg.id, a.name FROM release_group rg
             JOIN release_group_artist rga ON rga.release_group_id = rg.id
             JOIN artist a ON a.id = rga.artist_id
             WHERE rg.id = (SELECT rel.release_group_id
                 FROM track t
                 JOIN medium m ON m.id = t.medium_id
                 JOIN release rel ON rel.id = m.release_id
                 WHERE t.recording_id = (
                     SELECT recording_id FROM source
                     WHERE file_path = ? AND source_type = 'local_file'
                 )
             )",
        )
        .bind(&path.to_string_lossy().to_string())
        .fetch_all(&mut *cc)
        .await
        .unwrap();
        println!("OLD album artist: {:?}", old_artist);
        drop(cc);

        // Snapshot before rescan
        let mut conn = pool.acquire("diag.before").await.unwrap();
        let before: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT t.id, t.medium_id, rg.title
             FROM track t
             JOIN medium m ON m.id = t.medium_id
             JOIN release rel ON rel.id = m.release_id
             JOIN release_group rg ON rg.id = rel.release_group_id
             WHERE t.recording_id = (
                 SELECT recording_id FROM source
                 WHERE file_path = ? AND source_type = 'local_file'
             )",
        )
        .bind(&path.to_string_lossy().to_string())
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        println!(
            "BEFORE: track_id={:?} medium_id={:?} album={:?}",
            before[0].0, before[0].1, before[0].2
        );
        drop(conn);

        // Run rescan
        let serializer = tokio::sync::Semaphore::new(1);
        let file_issues = FileIssueLog::new();
        let result = rescan_source(&pool, path, None, &serializer, false, &file_issues).await;
        match &result {
            Ok(_) => println!("RESCAN SUCCEEDED"),
            Err(e) => println!("RESCAN FAILED: {e:?}"),
        }

        // Snapshot after rescan
        let mut conn = pool.acquire("diag.after").await.unwrap();
        let after: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT t.id, t.medium_id, rg.title
             FROM track t
             JOIN medium m ON m.id = t.medium_id
             JOIN release rel ON rel.id = m.release_id
             JOIN release_group rg ON rg.id = rel.release_group_id
             WHERE t.recording_id = (
                 SELECT recording_id FROM source
                 WHERE file_path = ? AND source_type = 'local_file'
             )",
        )
        .bind(&path.to_string_lossy().to_string())
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        println!(
            "AFTER: track_id={:?} medium_id={:?} album={:?}",
            after[0].0, after[0].1, after[0].2
        );
        println!("CHANGED: {}", before[0].1 != after[0].1);
        drop(conn);

        assert!(result.is_ok(), "Rescan should succeed, got: {:?}", result);
    }

    #[tokio::test]
    async fn test_rescan_source_picks_up_txxx_artists() {
        let tmp = TempDir::new().unwrap();

        // Build a synthetic MP3 with TXXX=ARTISTS
        let mut artists_frame_data = vec![0x03u8]; // UTF-8 encoding
        artists_frame_data.extend_from_slice(b"ARTISTS\x00");
        artists_frame_data.extend_from_slice(b"Extra Artist 1; Extra Artist 2");

        let frames = vec![
            crate::library::scanner::id3_frame(b"TIT2", b"\x03Test Song"),
            crate::library::scanner::id3_frame(b"TPE1", b"\x03Main Artist"),
            crate::library::scanner::id3_frame(b"TXXX", &artists_frame_data),
        ];
        let mp3_data = crate::library::scanner::synth_mp3_with_id3(&frames);
        let file_path = tmp.path().join("test.mp3");
        std::fs::write(&file_path, &mp3_data).unwrap();

        // Create test DB
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();

        let path_str = file_path.to_string_lossy().to_string();
        let (file_size, file_mtime_ms) = file_identity(&file_path).unwrap();
        let hash = file_sha256(&file_path).unwrap();

        // Step 1: Import with artists: None (simulating old code that didn't
        // parse TXXX=ARTISTS, so only the primary artist gets stored).
        let meta = TrackMetadata {
            title: Some("Test Song".into()),
            artist: Some("Main Artist".into()),
            album: Some("Test Album".into()),
            duration_ms: 200000,
            format: "mp3".into(),
            track_number: Some(1),
            track_total: Some(1),
            disc_number: Some(1),
            artists: None,
            ..Default::default()
        };

        let prepared = PreparedImport {
            path: file_path.clone(),
            path_str: path_str.clone(),
            existing_source_id: None,
            hash: hash.clone(),
            meta,
            warnings: vec![],
            file_size,
            file_mtime_ms,
            fp: None,
            acoustid_match: None,
            raw_tags_json: "[]".into(),
        };

        store_prepared_import(&pool, prepared).await.unwrap();

        // Verify only the primary artist exists after import
        let mut conn = pool.acquire("test.verify.initial").await.unwrap();
        let initial_artists: Vec<(String, i64)> = sqlx::query_as(
            "SELECT a.name, ra.position
             FROM recording_artist ra
             JOIN artist a ON a.id = ra.artist_id
             WHERE ra.recording_id = (
                 SELECT recording_id FROM source WHERE file_path = ? AND source_type = 'local_file'
             )
             ORDER BY ra.position",
        )
        .bind(&path_str)
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        assert_eq!(
            initial_artists.len(),
            1,
            "Should have only the primary artist before rescan"
        );
        assert_eq!(initial_artists[0].0, "Main Artist");
        assert_eq!(initial_artists[0].1, 0);
        drop(conn);

        // Step 2: Rescan — should now read TXXX=ARTISTS and add extra artists
        let serializer = tokio::sync::Semaphore::new(1);
        let file_issues = FileIssueLog::new();
        rescan_source(&pool, &file_path, None, &serializer, true, &file_issues)
            .await
            .unwrap();

        // Verify all three artists now exist
        let mut conn = pool.acquire("test.verify.after").await.unwrap();
        let artists_after: Vec<String> = sqlx::query_scalar(
            "SELECT a.name
             FROM recording_artist ra
             JOIN artist a ON a.id = ra.artist_id
             WHERE ra.recording_id = (
                 SELECT recording_id FROM source WHERE file_path = ? AND source_type = 'local_file'
             )
             ORDER BY ra.position",
        )
        .bind(&path_str)
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        assert_eq!(
            artists_after,
            vec!["Main Artist", "Extra Artist 1", "Extra Artist 2"],
            "Rescan should have added TXXX=ARTISTS artists"
        );
    }

    #[tokio::test]
    async fn test_guest_appearances_exclude_album_artist() {
        // When an artist appears in both TXXX=ARTISTS AND as the album artist
        // (release_group_artist.position = 0), they should NOT show up as a
        // "guest appearance" — they're the album's primary artist.

        let tmp = TempDir::new().unwrap();

        // Build a synthetic MP3 with album_artist matching TXXX=ARTISTS value
        // TIT2 = "Test Song", TPE1 = "Track Artist", TPE2 = "Album Artist",
        // TXXX=ARTISTS = "Album Artist"
        let mut artists_data = vec![0x03u8];
        artists_data.extend_from_slice(b"ARTISTS\x00");
        artists_data.extend_from_slice(b"Album Artist");

        let frames = vec![
            crate::library::scanner::id3_frame(b"TIT2", b"\x03Test Song"),
            crate::library::scanner::id3_frame(b"TPE1", b"\x03Track Artist"),
            crate::library::scanner::id3_frame(b"TPE2", b"\x03Album Artist"),
            crate::library::scanner::id3_frame(b"TXXX", &artists_data),
        ];
        let mp3_data = crate::library::scanner::synth_mp3_with_id3(&frames);
        let file_path = tmp.path().join("test.mp3");
        std::fs::write(&file_path, &mp3_data).unwrap();

        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();
        let path_str = file_path.to_string_lossy().to_string();
        let (file_size, file_mtime_ms) = file_identity(&file_path).unwrap();
        let hash = file_sha256(&file_path).unwrap();

        let meta = TrackMetadata {
            title: Some("Test Song".into()),
            artist: Some("Track Artist".into()),
            album_artist: Some("Album Artist".into()),
            album: Some("Test Album".into()),
            duration_ms: 200000,
            format: "mp3".into(),
            track_number: Some(1),
            track_total: Some(1),
            disc_number: Some(1),
            artists: Some("Album Artist".into()),
            ..Default::default()
        };

        let prepared = PreparedImport {
            path: file_path,
            path_str,
            existing_source_id: None,
            hash,
            meta,
            warnings: vec![],
            file_size,
            file_mtime_ms,
            fp: None,
            acoustid_match: None,
            raw_tags_json: "[]".into(),
        };

        store_prepared_import(&pool, prepared).await.unwrap();

        // Find the album artist's ID — they should have zero guest appearances
        // since they are the album artist for the only release group.
        let mut conn = pool.acquire("test.ga").await.unwrap();
        let album_artist_id: String =
            sqlx::query_scalar("SELECT id FROM artist WHERE name = 'Album Artist'")
                .fetch_one(&mut *conn)
                .await
                .unwrap();

        // This mirrors the guest-appearance query from get_artist_detail
        let ga_release_groups: Vec<String> = sqlx::query_scalar(
            "SELECT rg.id
             FROM recording_artist ra
             JOIN track t ON t.recording_id = ra.recording_id
             JOIN medium m ON m.id = t.medium_id
             JOIN release rel ON rel.id = m.release_id
             JOIN release_group rg ON rg.id = rel.release_group_id
             WHERE ra.artist_id = ?
               AND ra.position > 0
               AND NOT EXISTS (
                   SELECT 1 FROM release_group_artist rga2
                   WHERE rga2.release_group_id = rg.id
                     AND rga2.artist_id = ra.artist_id
                     AND rga2.position = 0
               )",
        )
        .bind(&album_artist_id)
        .fetch_all(&mut *conn)
        .await
        .unwrap();

        assert!(
            ga_release_groups.is_empty(),
            "Album Artist should not appear in guest appearances when they are the album artist; got {ga_release_groups:?}"
        );
    }

    // ── Duplicate-frame integration tests ───────────────────────────────────
    //
    // Uses duplicate frame IDs within a single ID3v2 tag (e.g. two TIT2 frames).
    // Lofty merges duplicates by taking the last value, while our raw scanner
    // (`get_first_id3v2_frame_values`) reports the first. A DuplicateFrame
    // issue is created when these values differ.

    /// Short-hand: MP3 with duplicate frame IDs for each tag type.
    fn duplicate_frame_mp3() -> Vec<u8> {
        let frames = vec![
            crate::library::scanner::id3_frame(b"TIT2", b"\x03Corrected Title"),
            crate::library::scanner::id3_frame(b"TIT2", b"\x03Lofty Title"),
            crate::library::scanner::id3_frame(b"TPE1", b"\x03Corrected Artist"),
            crate::library::scanner::id3_frame(b"TPE1", b"\x03Lofty Artist"),
            crate::library::scanner::id3_frame(b"TPE2", b"\x03Corrected Album Artist"),
            crate::library::scanner::id3_frame(b"TPE2", b"\x03Lofty Album Artist"),
            crate::library::scanner::id3_frame(b"TALB", b"\x03Corrected Album"),
            crate::library::scanner::id3_frame(b"TALB", b"\x03Lofty Album"),
        ];
        crate::library::scanner::synth_mp3_with_id3(&frames)
    }

    /// Short-hand: clean MP3 with no duplicate frame IDs.
    fn clean_mp3() -> Vec<u8> {
        let frames = vec![
            crate::library::scanner::id3_frame(b"TIT2", b"\x03Clean Title"),
            crate::library::scanner::id3_frame(b"TPE1", b"\x03Clean Artist"),
            crate::library::scanner::id3_frame(b"TPE2", b"\x03Clean Album Artist"),
            crate::library::scanner::id3_frame(b"TALB", b"\x03Clean Album"),
        ];
        crate::library::scanner::synth_mp3_with_id3(&frames)
    }

    /// Import the given MP3 data as a source at `file_path`.
    async fn import_mp3_as_source(
        pool: &crate::db::DbPool,
        file_path: &std::path::Path,
        mp3_data: &[u8],
    ) {
        std::fs::write(file_path, mp3_data).unwrap();
        store_prepared_import(
            pool,
            prepared_import_for_path(
                file_path,
                tagged_meta(
                    "Lofty Title",
                    "Lofty Artist",
                    "Lofty Album Artist",
                    "Lofty Album",
                    1,
                ),
                None,
            ),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_rescan_detects_duplicate_frame_issue() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();
        let serializer = tokio::sync::Semaphore::new(1);
        let file_issues = FileIssueLog::new();

        let file_path = tmp.path().join("test_dup.mp3");
        import_mp3_as_source(&pool, &file_path, &duplicate_frame_mp3()).await;
        let _rec_id = recording_id_for_path(&pool, &file_path).await;

        rescan_source(&pool, &file_path, None, &serializer, true, &file_issues)
            .await
            .unwrap();

        let issues = file_issues.all();
        let dup_issues_for_file: Vec<_> = issues
            .iter()
            .filter(|i| {
                i.kind == FileIssueKind::DuplicateFrame
                    && i.file_path == file_path.to_string_lossy()
            })
            .collect();

        assert!(
            !dup_issues_for_file.is_empty(),
            "rescan of file with duplicate frames should create DuplicateFrame issues"
        );

        for expected_frame in &["TIT2", "TPE1", "TPE2", "TALB"] {
            assert!(
                dup_issues_for_file
                    .iter()
                    .any(|i| i.frame_id.as_deref() == Some(expected_frame)),
                "expected a DuplicateFrame issue for frame {expected_frame}, got: {dup_issues_for_file:#?}"
            );
        }

        for issue in &dup_issues_for_file {
            match issue.frame_id.as_deref() {
                Some("TIT2") => {
                    assert_eq!(issue.corrected_value.as_deref(), Some("Corrected Title"));
                    assert_eq!(issue.lofty_value.as_deref(), Some("Lofty Title"));
                }
                Some("TPE1") => {
                    assert_eq!(issue.corrected_value.as_deref(), Some("Corrected Artist"));
                    assert_eq!(issue.lofty_value.as_deref(), Some("Lofty Artist"));
                }
                Some("TPE2") => {
                    assert_eq!(
                        issue.corrected_value.as_deref(),
                        Some("Corrected Album Artist")
                    );
                    assert_eq!(issue.lofty_value.as_deref(), Some("Lofty Album Artist"));
                }
                Some("TALB") => {
                    assert_eq!(issue.corrected_value.as_deref(), Some("Corrected Album"));
                    assert_eq!(issue.lofty_value.as_deref(), Some("Lofty Album"));
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn test_rescan_duplicate_frame_not_duplicated_on_second_rescan() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();
        let serializer = tokio::sync::Semaphore::new(1);
        let file_issues = FileIssueLog::new();

        let file_path = tmp.path().join("test_dup.mp3");
        import_mp3_as_source(&pool, &file_path, &duplicate_frame_mp3()).await;
        let _rec_id = recording_id_for_path(&pool, &file_path).await;

        rescan_source(&pool, &file_path, None, &serializer, true, &file_issues)
            .await
            .unwrap();
        let count_after_first = file_issues
            .all()
            .iter()
            .filter(|i| {
                i.kind == FileIssueKind::DuplicateFrame
                    && i.file_path == file_path.to_string_lossy()
            })
            .count();

        rescan_source(&pool, &file_path, None, &serializer, true, &file_issues)
            .await
            .unwrap();
        let count_after_second = file_issues
            .all()
            .iter()
            .filter(|i| {
                i.kind == FileIssueKind::DuplicateFrame
                    && i.file_path == file_path.to_string_lossy()
            })
            .count();

        assert_eq!(
            count_after_second, count_after_first,
            "rescanned the same file twice — DuplicateFrame count should not grow"
        );
    }

    #[tokio::test]
    async fn test_rescan_clears_duplicate_frame_issue_when_file_is_fixed() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();
        let serializer = tokio::sync::Semaphore::new(1);
        let file_issues = FileIssueLog::new();

        let file_path = tmp.path().join("test_dup.mp3");
        import_mp3_as_source(&pool, &file_path, &duplicate_frame_mp3()).await;
        let _rec_id = recording_id_for_path(&pool, &file_path).await;

        rescan_source(&pool, &file_path, None, &serializer, true, &file_issues)
            .await
            .unwrap();

        assert!(
            file_issues
                .all()
                .iter()
                .any(|i| i.kind == FileIssueKind::DuplicateFrame
                    && i.file_path == file_path.to_string_lossy()),
            "first rescan should detect duplicate frames and create issues"
        );

        std::fs::write(&file_path, clean_mp3()).unwrap();

        rescan_source(&pool, &file_path, None, &serializer, true, &file_issues)
            .await
            .unwrap();

        let remaining: Vec<_> = file_issues
            .all()
            .into_iter()
            .filter(|i| {
                i.kind == FileIssueKind::DuplicateFrame
                    && i.file_path == file_path.to_string_lossy()
            })
            .collect();

        assert!(
            remaining.is_empty(),
            "after replacing with a clean file, DuplicateFrame issues should be removed, got: {remaining:#?}"
        );
    }

    // ── Dual consecutive ID3v2 tag tests ────────────────────────────────────
    //
    // Some real-world MP3s have two consecutive ID3v2 tags with overlapping
    // frames (second tag's values overwrite the first). Our raw scanner
    // (`get_first_id3v2_frame_values`) detects these duplicates, but Lofty
    // currently fails to parse such files. The `duplicate_frames` field is
    // only populated in the Lofty code path, so dual-tag files currently
    // bypass duplicate detection entirely. These tests document that
    // limitation.

    /// Build a synthetic MP3 with two consecutive ID3v2.4 tags, each
    /// containing different frame values.
    fn build_dual_tag_mp3(first_frames: &[Vec<u8>], second_frames: &[Vec<u8>]) -> Vec<u8> {
        fn build_id3_tag(frames: &[Vec<u8>]) -> Vec<u8> {
            let mut tag_data = Vec::new();
            for f in frames {
                tag_data.extend_from_slice(f);
            }
            let mut header = Vec::new();
            header.extend_from_slice(b"ID3");
            header.extend_from_slice(&[0x04, 0x00]); // version 2.4
            header.push(0x00); // flags
            header.extend_from_slice(&crate::library::scanner::synchsafe(tag_data.len() as u32));
            header.extend_from_slice(&tag_data);
            header
        }

        let mut file = Vec::new();
        file.extend_from_slice(&build_id3_tag(first_frames));
        file.extend_from_slice(&build_id3_tag(second_frames));
        // Minimal valid MPEG-1 Audio Layer 3 frame (silent)
        file.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        file.resize(file.len() + 413, 0u8);
        file
    }

    #[tokio::test]
    async fn test_dual_tag_lofty_fails_to_parse() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();
        let serializer = tokio::sync::Semaphore::new(1);
        let file_issues = FileIssueLog::new();

        let tag1_frames = vec![
            crate::library::scanner::id3_frame(b"TIT2", b"\x03First Title"),
            crate::library::scanner::id3_frame(b"TPE1", b"\x03First Artist"),
        ];
        let tag2_frames = vec![
            crate::library::scanner::id3_frame(b"TIT2", b"\x03Second Title"),
            crate::library::scanner::id3_frame(b"TPE1", b"\x03Second Artist"),
        ];
        let mp3_data = build_dual_tag_mp3(&tag1_frames, &tag2_frames);
        let file_path = tmp.path().join("test_dual.mp3");
        std::fs::write(&file_path, &mp3_data).unwrap();

        store_prepared_import(
            &pool,
            prepared_import_for_path(
                &file_path,
                tagged_meta("Second Title", "Second Artist", "Album Artist", "Album", 1),
                None,
            ),
        )
        .await
        .unwrap();

        let _rec_id = recording_id_for_path(&pool, &file_path).await;

        rescan_source(&pool, &file_path, None, &serializer, true, &file_issues)
            .await
            .unwrap();

        let dup_count = file_issues
            .all()
            .iter()
            .filter(|i| {
                i.kind == FileIssueKind::DuplicateFrame
                    && i.file_path == file_path.to_string_lossy()
            })
            .count();

        assert!(
            dup_count > 0,
            "TagLib fallback should detect duplicate frames from raw ID3v2 bytes, got {dup_count}"
        );
    }

    #[tokio::test]
    async fn test_fix_duplicate_track_entries_removes_duplicates() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();

        // Import a recording on "History of beatmania IIDX" at track 15.
        let presto_path = tmp.path().join("presto.mp3");
        write_tagged_mp3(
            &presto_path,
            "Presto",
            "Osamu Kubota",
            "Osamu Kubota",
            "History of beatmania IIDX",
        );
        store_prepared_import(
            &pool,
            prepared_import_for_path(
                &presto_path,
                tagged_meta(
                    "Presto",
                    "Osamu Kubota",
                    "Osamu Kubota",
                    "History of beatmania IIDX",
                    15,
                ),
                None,
            ),
        )
        .await
        .unwrap();

        let rec_id = recording_id_for_path(&pool, &presto_path).await;

        // Add a duplicate track on the same release group & position.
        let mut conn = pool.acquire("test.setup.hob_dup").await.unwrap();
        let hob_medium_id: String = sqlx::query_scalar(
            "SELECT m.id FROM medium m
             JOIN release rel ON rel.id = m.release_id
             JOIN release_group rg ON rg.id = rel.release_group_id
             WHERE rg.title = 'History of beatmania IIDX'
             LIMIT 1",
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO track (id, medium_id, recording_id, position)
             VALUES (?, ?, ?, 15)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&hob_medium_id)
        .bind(&rec_id)
        .execute(&mut *conn)
        .await
        .unwrap();

        // Add a legitimate third track on a different album at position 24.
        let mut tx = conn.begin().await.unwrap();
        let artist_id = get_or_create_artist(&mut tx, "Osamu Kubota").await.unwrap();
        let iidx3_rg_id = get_or_create_release_group(
            &mut tx,
            "beatmania IIDX 3rd Style Original Soundtracks",
            &artist_id,
        )
        .await
        .unwrap();
        let iidx3_release_id = get_or_create_release(
            &mut tx,
            &iidx3_rg_id,
            "beatmania IIDX 3rd Style Original Soundtracks",
            None,
        )
        .await
        .unwrap();
        let iidx3_medium_id = get_or_create_medium(&mut tx, &iidx3_release_id, 1)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO track (id, medium_id, recording_id, position)
             VALUES (?, ?, ?, 24)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&iidx3_medium_id)
        .bind(&rec_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(conn);

        // Verify: 3 track rows (2 on HOB IIDX + 1 on IIDX 3rd Style).
        let mut verify = pool.acquire("test.verify_before").await.unwrap();
        let track_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM track WHERE recording_id = ?")
                .bind(&rec_id)
                .fetch_one(&mut *verify)
                .await
                .unwrap();
        assert_eq!(track_count, 3, "test setup should create 3 track rows");

        let album_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT rg.id)
             FROM track t
             JOIN medium m ON m.id = t.medium_id
             JOIN release rel ON rel.id = m.release_id
             JOIN release_group rg ON rg.id = rel.release_group_id
             WHERE t.recording_id = ?",
        )
        .bind(&rec_id)
        .fetch_one(&mut *verify)
        .await
        .unwrap();
        assert_eq!(
            album_count, 2,
            "test setup should assert 2 distinct release groups"
        );
        drop(verify);

        // Run the fix.
        let deleted = fix_duplicate_track_entries(&pool).await.unwrap();
        assert!(
            deleted > 0,
            "should have deleted at least one duplicate track"
        );

        // Verify: 2 tracks (1 per album), no duplicates.
        let remaining_tracks: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM track WHERE recording_id = ?")
                .bind(&rec_id)
                .fetch_one(&mut *pool.acquire("test.verify_after").await.unwrap())
                .await
                .unwrap();
        assert_eq!(
            remaining_tracks, 2,
            "should have exactly 2 tracks after dedup"
        );
        assert_eq!(
            release_titles_for_recording(&pool, &rec_id).await,
            vec![
                "History of beatmania IIDX",
                "beatmania IIDX 3rd Style Original Soundtracks"
            ],
            "recording should still appear on both albums"
        );

        // Verify the duplicate was not the only HOB IIDX track left.
        let hob_track_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM track t
             JOIN medium m ON m.id = t.medium_id
             JOIN release rel ON rel.id = m.release_id
             JOIN release_group rg ON rg.id = rel.release_group_id
             WHERE t.recording_id = ? AND rg.title = 'History of beatmania IIDX'",
        )
        .bind(&rec_id)
        .fetch_one(&mut *pool.acquire("test.verify_hob").await.unwrap())
        .await
        .unwrap();
        assert_eq!(
            hob_track_count, 1,
            "should have exactly 1 track on History of beatmania IIDX"
        );
    }
}
