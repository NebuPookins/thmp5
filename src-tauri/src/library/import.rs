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

pub async fn rescan_source(db: &DbPool, path: &Path, acoustid_key: Option<&str>) -> Result<()> {
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
    drop(conn);

    let (source_id, recording_id) = existing_source;
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

    let mut db_conn = db
        .acquire(format!("source_rescan.transaction path={path_str}"))
        .await
        .context("Failed to acquire DB connection for source rescan transaction")?;
    let mut tx = db_conn
        .begin()
        .await
        .context("Failed to start source rescan transaction")?;

    let artist_name = blocking
        .meta
        .album_artist
        .as_deref()
        .or(blocking.meta.artist.as_deref())
        .unwrap_or("Unknown Artist")
        .to_string();
    let artist_id = get_or_create_artist(&mut tx, &artist_name).await?;

    let album_title = blocking
        .meta
        .album
        .as_deref()
        .unwrap_or("Unknown Album")
        .to_string();
    let release_group_id = get_or_create_release_group(&mut tx, &album_title, &artist_id).await?;
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

    sqlx::query("DELETE FROM recording_artist WHERE recording_id = ?")
        .bind(&recording_id)
        .execute(&mut *tx)
        .await
        .context("Failed to replace recording artist during source rescan")?;
    insert_recording_artist(&mut tx, &recording_id, &artist_id, 0, "main").await?;

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

    let mut prune_conn = db
        .acquire(format!("source_rescan.prune path={path_str}"))
        .await
        .context("Failed to acquire DB connection for source rescan cleanup")?;
    let mut prune_tx = prune_conn
        .begin()
        .await
        .context("Failed to start source rescan cleanup transaction")?;
    prune_empty_library_entities(&mut prune_tx).await?;
    prune_tx
        .commit()
        .await
        .context("Failed to commit source rescan cleanup transaction")?;

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

    let mut conn = db
        .acquire(format!("import.store.transaction path={path_str}"))
        .await
        .context("Failed to acquire DB connection for import transaction")?;
    let mut tx = conn
        .begin()
        .await
        .context("Failed to start import transaction")?;

    // ── 6. Artist ─────────────────────────────────────────────────────────────
    let artist_name = meta
        .album_artist
        .as_deref()
        .or(meta.artist.as_deref())
        .unwrap_or("Unknown Artist")
        .to_string();
    let artist_id = get_or_create_artist(&mut tx, &artist_name).await?;

    // ── 7. Find or create Recording (3-level dedup) ───────────────────────────
    let recording_id =
        find_or_create_recording(&mut tx, &meta, &artist_id, acoustid_match.as_ref()).await?;
    sync_tags_from_comment(&mut tx, &recording_id, meta.comment.as_deref()).await?;

    // ── 8. ReleaseGroup / Release / Medium ────────────────────────────────────
    let album_title = meta.album.as_deref().unwrap_or("Unknown Album").to_string();
    let release_group_id = get_or_create_release_group(&mut tx, &album_title, &artist_id).await?;

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

    // ── 9. Track ──────────────────────────────────────────────────────────────
    let track_position = meta.track_number.unwrap_or(0) as i64;
    get_or_create_track(&mut tx, &medium_id, &recording_id, track_position).await?;

    let source_id = existing_source_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let fingerprint_str = fp.as_ref().map(|f| f.fingerprint.as_str());
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
    .bind(&recording_id)
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

async fn get_or_create_artist(tx: &mut Transaction<'_, Sqlite>, name: &str) -> Result<String> {
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
