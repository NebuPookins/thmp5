use super::scanner::{is_audio_file, read_metadata};
use crate::db::DbPool;
use crate::fingerprint::{self, AcoustIdMatch};
use crate::models::{ImportStats, TrackMetadata};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Sqlite, Transaction};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::sync::Semaphore;
use uuid::Uuid;
use walkdir::WalkDir;

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
) -> Result<()> {
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
        Ok::<_, anyhow::Error>(BlockingResult {
            hash,
            meta: metadata_read.meta,
            warnings: metadata_read.warning.into_iter().collect(),
            fp,
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
    let release_group_id =
        get_or_create_release_group(&mut tx, &album_title, &album_artist_id).await?;
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

    let existing_track_id = sqlx::query_scalar::<_, String>(
        "SELECT id FROM track WHERE recording_id = ? ORDER BY id LIMIT 1",
    )
    .bind(&recording_id)
    .fetch_optional(&mut *tx)
    .await
    .context("Failed to load track row for source rescan")?;

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

    if let Some(track_id) = existing_track_id {
        sqlx::query(
            "UPDATE track
             SET medium_id = ?, position = ?, title = NULL, duration_ms = NULL
             WHERE id = ?",
        )
        .bind(&medium_id)
        .bind(track_position)
        .bind(&track_id)
        .execute(&mut *tx)
        .await
        .context("Failed to update track placement during source rescan")?;
    } else {
        get_or_create_track(&mut tx, &medium_id, &recording_id, track_position).await?;
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

    Ok(())
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

    struct BlockingResult {
        hash: String,
        meta: TrackMetadata,
        warnings: Vec<String>,
        fp: Option<fingerprint::FingerprintResult>,
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
        Ok::<_, anyhow::Error>(BlockingResult {
            hash,
            meta: metadata_read.meta,
            warnings: metadata_read.warning.into_iter().collect(),
            fp,
        })
    })
    .await;

    let blocking = match blocking {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e),
        Err(e) => anyhow::bail!("Blocking import task panicked: {e}"),
    };

    let (hash, meta, warnings, fp) = (blocking.hash, blocking.meta, blocking.warnings, blocking.fp);

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
                replay_gain_album_db, replay_gain_album_peak
             ) VALUES (?, ?, 'local_file', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
                replay_gain_album_peak = excluded.replay_gain_album_peak",
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
            replay_gain_album_db, replay_gain_album_peak
         ) VALUES (?, ?, 'local_file', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
            replay_gain_album_peak = excluded.replay_gain_album_peak",
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
        return Ok(id);
    }

    let id = Uuid::new_v4().to_string();
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
        return Ok(id);
    }

    let id = Uuid::new_v4().to_string();
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
        return Ok(id);
    }

    let id = Uuid::new_v4().to_string();
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
    use tempfile::TempDir;

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
        rescan_source(&pool, &file_path, None, &serializer, true)
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
}
