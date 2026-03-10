use super::scanner::{is_audio_file, read_metadata};
use crate::fingerprint::{self, AcoustIdMatch};
use crate::models::{ImportStats, TrackMetadata};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, SqlitePool, Transaction};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use uuid::Uuid;
use walkdir::WalkDir;

pub(crate) struct PreparedImport {
    path: PathBuf,
    path_str: String,
    existing_source_id: Option<String>,
    hash: String,
    meta: TrackMetadata,
    file_size: i64,
    file_mtime_ms: i64,
    fp: Option<fingerprint::FingerprintResult>,
    acoustid_match: Option<AcoustIdMatch>,
}

pub async fn import_paths(
    db: &SqlitePool,
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
                                stats.errors += 1;
                                stats.error_messages.push(e.to_string());
                            }
                        }
                    }
                    Err(e) => {
                        let msg = match e.path() {
                            Some(p) => format!("{}: {}", p.display(), e),
                            None => e.to_string(),
                        };
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
                    stats.errors += 1;
                    stats.error_messages.push(e.to_string());
                }
            }
        }
    }

    Ok(stats)
}

/// Returns `Ok(true)` if imported, `Ok(false)` if skipped (already exists).
pub(crate) async fn import_file(
    db: &SqlitePool,
    path: &Path,
    acoustid_key: Option<&str>,
) -> Result<bool> {
    let Some(prepared) = prepare_import(db, path, acoustid_key).await? else {
        return Ok(false);
    };

    store_prepared_import(db, prepared).await
}

pub(crate) async fn prepare_import(
    db: &SqlitePool,
    path: &Path,
    acoustid_key: Option<&str>,
) -> Result<Option<PreparedImport>> {
    let (file_size, file_mtime_ms) = file_identity(path).context("Failed to read file metadata")?;
    let path_str = path.to_string_lossy().to_string();
    let existing_source = sqlx::query_as::<_, (String, Option<i64>, Option<i64>)>(
        "SELECT id, file_size, file_mtime_ms FROM source WHERE file_path = ?",
    )
    .bind(&path_str)
    .fetch_optional(db)
    .await
    .context("DB error checking existing path")?;

    if let Some((_, existing_size, existing_mtime_ms)) = &existing_source {
        if existing_size == &Some(file_size) && existing_mtime_ms == &Some(file_mtime_ms) {
            return Ok(None);
        }
    }

    let existing_source_id = existing_source.as_ref().map(|(id, _, _)| id.as_str());

    let hash = file_sha256(path).context("Failed to hash file")?;
    let meta = read_metadata(path).context("Failed to read metadata")?;

    let fp = {
        let p = path.to_path_buf();
        match tokio::task::spawn_blocking(move || fingerprint::generate_fingerprint(&p)).await {
            Ok(Ok(fp)) => Some(fp),
            Ok(Err(e)) => {
                tracing::warn!(path = %path.display(), "Fingerprint generation failed: {e}");
                None
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "Fingerprint task panicked: {e}");
                None
            }
        }
    };

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
        file_size,
        file_mtime_ms,
        fp,
        acoustid_match,
    }))
}

pub(crate) async fn store_prepared_import(
    db: &SqlitePool,
    prepared: PreparedImport,
) -> Result<bool> {
    let PreparedImport {
        path,
        path_str,
        existing_source_id,
        hash,
        meta,
        file_size,
        file_mtime_ms,
        fp,
        acoustid_match,
    } = prepared;

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM source
         WHERE file_hash = ?
           AND (? IS NULL OR id != ?)",
    )
    .bind(&hash)
    .bind(existing_source_id.as_deref())
    .bind(existing_source_id.as_deref())
    .fetch_one(db)
    .await
    .context("DB error checking hash")?;
    if count > 0 {
        return Ok(false);
    }

    let mut tx = db
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
    Ok(true)
}

// ─────────────────────────────────────────────────────────────────────────────
// Recording deduplication (3 levels)
// ─────────────────────────────────────────────────────────────────────────────

/// Find an existing recording or create a new one.
///
/// Deduplication order:
/// 1. AcoustID match — same fingerprint → same recording.
/// 2. Tag-based match — same title + artist + duration (±5 s).
/// 3. Create new.
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

    // ── Level 2: tags ────────────────────────────────────────────────────────
    let title_lower = title.to_lowercase();
    let dur_min = duration - 5000;
    let dur_max = duration + 5000;

    if let Some(id) = sqlx::query_scalar::<_, String>(
        "SELECT r.id FROM recording r
         JOIN recording_artist ra ON ra.recording_id = r.id
         WHERE lower(r.title) = ?
           AND ra.artist_id = ?
           AND ra.position = 0
           AND r.duration_ms BETWEEN ? AND ?
         LIMIT 1",
    )
    .bind(&title_lower)
    .bind(artist_id)
    .bind(dur_min)
    .bind(dur_max)
    .fetch_optional(&mut **tx)
    .await?
    {
        // Backfill acoustid/mbid if we have them and the recording doesn't yet.
        if let Some(aid) = acoustid_match {
            sqlx::query(
                "UPDATE recording SET acoustid = ?, mbid = ?
                 WHERE id = ? AND acoustid IS NULL",
            )
            .bind(&aid.acoustid)
            .bind(aid.recording_mbid.as_deref())
            .bind(&id)
            .execute(&mut **tx)
            .await?;
        }
        return Ok(id);
    }

    // ── Level 3: create new ──────────────────────────────────────────────────
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
