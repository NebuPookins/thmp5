use super::scanner::{build_raw_tags_json, is_audio_file, read_metadata};
use crate::db::DbPool;
use crate::file_issues::{FileIssueKind, FileIssueLog};
use crate::fingerprint::{self, AcoustIdMatch};
use crate::models::{DuplicateFrameInfo, ImportStats, TrackMetadata};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::Connection;
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
    raw_tags_json: String,
}

pub async fn import_paths(
    db: &DbPool,
    paths: Vec<String>,
    acoustid_key: Option<&str>,
    serializer: &Semaphore,
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
                        match import_file(db, e.path(), acoustid_key, serializer).await {
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
            match import_file(db, path, acoustid_key, serializer).await {
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
    file_issues: &FileIssueLog,
) -> Result<String> {
    let path_str = path.to_string_lossy().to_string();
    let mut conn = db
        .acquire(format!("source_rescan.lookup path={path_str}"))
        .await
        .context("Failed to acquire DB connection for source rescan lookup")?;
    let existing_source = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM source
         WHERE file_path = ? AND source_type = 'local_file'",
    )
    .bind(&path_str)
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to load source for rescan")?
    .ok_or_else(|| anyhow::anyhow!("Source not found for path: {}", path.display()))?;

    let source_id = existing_source;
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

    let fingerprint_str = blocking.fp.as_ref().map(|f| f.fingerprint.as_str());

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

    tracing::info!(path = %path.display(), source_id = %source_id, "Rescanned source");
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

    // Detect backup files from tag edits (adjacent .thmp5bak files).
    //
    // Clear stale BackupFileExists issues for this file first, then
    // re-scan for backup files.
    file_issues.retain(|issue| {
        !(issue.kind == FileIssueKind::BackupFileExists
            && issue.file_path == path.display().to_string())
    });
    if let Some(parent) = path.parent() {
        let file_name = path.file_name().map(|n| n.to_string_lossy());
        if let Some(file_name) = file_name {
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.flatten() {
                    let entry_name = entry.file_name().to_string_lossy().to_string();
                    if entry_name.starts_with(&format!("{}.", file_name.as_ref()))
                        && entry_name.ends_with(".thmp5bak")
                    {
                        file_issues.push_backup_exists(
                            path.display().to_string(),
                            entry.path().display().to_string(),
                        );
                    }
                }
            }
        }
    }

    Ok(source_id)
}

/// Returns `Ok(true)` if imported, `Ok(false)` if skipped (already exists).
pub(crate) async fn import_file(
    db: &DbPool,
    path: &Path,
    acoustid_key: Option<&str>,
    serializer: &Semaphore,
) -> Result<bool> {
    let Some(prepared) = prepare_import(db, path, acoustid_key).await? else {
        return Ok(false);
    };

    store_prepared_import(db, prepared, serializer).await
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
    let existing_source = sqlx::query_as::<
        _,
        (
            String,
            Option<i64>,
            Option<i64>,
            bool,
            Option<String>,
            Option<String>,
        ),
    >(
        "SELECT id, file_size, file_mtime_ms, raw_tags_json IS NOT NULL,
                file_hash, fingerprint
         FROM source WHERE file_path = ?",
    )
    .bind(&path_str)
    .fetch_optional(&mut *conn)
    .await
    .context("DB error checking existing path")?;

    let file_unchanged = existing_source
        .as_ref()
        .is_some_and(|(_, es, em, _, _, _)| es == &Some(file_size) && em == &Some(file_mtime_ms));

    // Each operation has its own pre-requisite. If all are satisfied, skip entirely.
    let has_hash = existing_source
        .as_ref()
        .and_then(|(_, _, _, _, h, _)| h.as_deref())
        .is_some();
    let has_raw_tags = existing_source
        .as_ref()
        .is_some_and(|(_, _, _, r, _, _)| *r);
    let has_fingerprint = existing_source
        .as_ref()
        .and_then(|(_, _, _, _, _, f)| f.as_deref())
        .is_some();

    if file_unchanged && has_hash && has_raw_tags && has_fingerprint {
        return Ok(None);
    }

    let existing_source_id = existing_source
        .as_ref()
        .map(|(id, _, _, _, _, _)| id.as_str());
    let existing_hash = existing_source
        .as_ref()
        .and_then(|(_, _, _, _, h, _)| h.clone());

    // Only compute what's actually needed.
    let needs_hash = !file_unchanged || !has_hash;
    let needs_fingerprint = !file_unchanged || !has_fingerprint;
    // Metadata re-read is needed whenever we reach the blocking task (raw_tags missing
    // or file changed — otherwise the early-return above would have fired).

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

        // Hash: compute only when needed (file changed or no existing hash).
        let hash = if needs_hash {
            file_sha256(&p).context("Failed to hash file")?
        } else {
            // Safe: needs_hash false implies existing_hash is Some.
            existing_hash.unwrap()
        };

        // Metadata and raw_tags_json: always needed (see early-return above).
        let metadata_read = read_metadata(&p).context("Failed to read metadata")?;
        let raw_tags_json = build_raw_tags_json(&p, &metadata_read.meta, &metadata_read.all_tags);

        // Fingerprint: only when needed (file changed or no existing fingerprint).
        let fp = if needs_fingerprint {
            match fingerprint::generate_fingerprint(&p) {
                Ok(fp) => Some(fp),
                Err(e) => {
                    tracing::warn!(path = %p.display(), "Fingerprint generation failed: {e}");
                    None
                }
            }
        } else {
            None
        };

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

pub(crate) async fn store_prepared_import(
    db: &DbPool,
    prepared: PreparedImport,
    serializer: &Semaphore,
) -> Result<bool> {
    let PreparedImport {
        path,
        path_str,
        existing_source_id,
        hash,
        mut meta,
        warnings,
        file_size,
        file_mtime_ms,
        fp,
        acoustid_match: _,
        raw_tags_json,
    } = prepared;

    // Fallback: extract MusicBrainz Recording ID from raw_tags_json if the
    // scanner (Lofty/TagLib) didn't populate it. The raw ID3 scanner always
    // captures UFID:http://musicbrainz.org frames for ID3v2 files, so this
    // is more reliable than relying solely on the Lofty/TagLib extraction path.
    if meta.recording_mbid.is_none() {
        if let Ok(tags) = serde_json::from_str::<Vec<(String, String)>>(&raw_tags_json) {
            for (key, value) in &tags {
                if key == "UFID:http://musicbrainz.org" && !value.is_empty() {
                    meta.recording_mbid = Some(value.clone());
                    break;
                }
            }
        }
    }

    // Reverse: if the MBID was populated from the scanner (e.g. via Lofty or
    // TagLib) but isn't in raw_tags_json yet, inject it so the catalog can
    // group by MBID using raw_tags_json alone at load time.
    let raw_tags_json = if meta.recording_mbid.is_some() {
        let mut tags: Vec<(String, String)> =
            serde_json::from_str(&raw_tags_json).unwrap_or_default();
        let has_ufid = tags.iter().any(|(k, _)| k == "UFID:http://musicbrainz.org");
        if !has_ufid {
            if let Some(mbid) = &meta.recording_mbid {
                tags.push(("UFID:http://musicbrainz.org".into(), mbid.clone()));
            }
        }
        serde_json::to_string(&tags).unwrap_or(raw_tags_json)
    } else {
        raw_tags_json
    };

    let _permit = serializer
        .acquire()
        .await
        .context("Failed to acquire write serializer for import")?;

    let source_id = existing_source_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let fingerprint_str = fp.as_ref().map(|f| f.fingerprint.as_str());

    let mut conn = db
        .acquire(format!("import.store.transaction path={path_str}"))
        .await
        .context("Failed to acquire DB connection for import transaction")?;
    let mut tx = conn
        .begin()
        .await
        .context("Failed to start import transaction")?;

    sqlx::query(
        "INSERT INTO source (
            id, source_type, file_path, file_hash, format, duration_ms,
            fingerprint, file_size, file_mtime_ms, track_total,
            replay_gain_track_db, replay_gain_track_peak,
            replay_gain_album_db, replay_gain_album_peak,
            raw_tags_json
         ) VALUES (?, 'local_file', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(file_path) DO UPDATE SET
            file_hash = excluded.file_hash,
            format = excluded.format,
            duration_ms = excluded.duration_ms,
            fingerprint = COALESCE(excluded.fingerprint, fingerprint),
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
        prepared_import_with_mbid(path, meta, acoustid, None)
    }

    fn prepared_import_with_mbid(
        path: &Path,
        mut meta: TrackMetadata,
        acoustid: Option<&str>,
        recording_mbid: Option<&str>,
    ) -> PreparedImport {
        if let Some(m) = recording_mbid {
            meta.recording_mbid = Some(m.to_string());
        }
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
                recording_mbid: recording_mbid.map(|s| s.to_string()),
            }),
            raw_tags_json: "[]".into(),
        }
    }

    async fn source_id_for_path(pool: &crate::db::DbPool, path: &Path) -> String {
        let path_str = path.to_string_lossy().to_string();
        let mut conn = pool.acquire("test.source_id_for_path").await.unwrap();
        sqlx::query_scalar(
            "SELECT id FROM source WHERE file_path = ? AND source_type = 'local_file'",
        )
        .bind(path_str)
        .fetch_one(&mut *conn)
        .await
        .unwrap()
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
        serializer: &tokio::sync::Semaphore,
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
            serializer,
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
        import_mp3_as_source(&pool, &file_path, &duplicate_frame_mp3(), &serializer).await;
        let _rec_id = source_id_for_path(&pool, &file_path).await;

        rescan_source(&pool, &file_path, None, &serializer, &file_issues)
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
        import_mp3_as_source(&pool, &file_path, &duplicate_frame_mp3(), &serializer).await;
        let _rec_id = source_id_for_path(&pool, &file_path).await;

        rescan_source(&pool, &file_path, None, &serializer, &file_issues)
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

        rescan_source(&pool, &file_path, None, &serializer, &file_issues)
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
        import_mp3_as_source(&pool, &file_path, &duplicate_frame_mp3(), &serializer).await;
        let _rec_id = source_id_for_path(&pool, &file_path).await;

        rescan_source(&pool, &file_path, None, &serializer, &file_issues)
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

        rescan_source(&pool, &file_path, None, &serializer, &file_issues)
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
            &serializer,
        )
        .await
        .unwrap();

        let _rec_id = source_id_for_path(&pool, &file_path).await;

        rescan_source(&pool, &file_path, None, &serializer, &file_issues)
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

    // ── MusicBrainz Recording ID grouping tests ────────────────────────
    //
    // Two files sharing the same MBID should appear as two sources for the
    // same recording when viewed through the MemoryCatalog.  Grouping is no
    // longer done at import time (the recording table is gone); it happens
    // entirely in-memory at catalog load time.

    #[tokio::test]
    async fn test_same_mbid_creates_same_recording() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();
        let serializer = tokio::sync::Semaphore::new(1);

        let mbid = "b6e3c72a-5f0c-4e5d-9f0a-123456789abc";

        // Two files with different content → different SHA-256 → different hashes
        let file1 = tmp.path().join("song1.mp3");
        let file2 = tmp.path().join("song2.mp3");
        write_tagged_mp3(&file1, "Song One", "Artist", "Album Artist", "Album");
        write_tagged_mp3(&file2, "Song Two", "Artist", "Album Artist", "Album");

        // Import both with different acoustids but the same recording_mbid
        store_prepared_import(
            &pool,
            prepared_import_with_mbid(
                &file1,
                tagged_meta("Song One", "Artist", "Album Artist", "Album", 1),
                Some("acoustid-a"),
                Some(mbid),
            ),
            &serializer,
        )
        .await
        .unwrap();

        store_prepared_import(
            &pool,
            prepared_import_with_mbid(
                &file2,
                tagged_meta("Song Two", "Artist", "Album Artist", "Album", 1),
                Some("acoustid-b"),
                Some(mbid),
            ),
            &serializer,
        )
        .await
        .unwrap();

        // Verify via MemoryCatalog that both files land in the same recording.
        let catalog = crate::storage::memory::MemoryCatalog::load_from_db(&pool)
            .await
            .unwrap();
        let recs = crate::storage::CatalogReader::list_recordings(&catalog)
            .await
            .unwrap();

        let file1_path = file1.to_string_lossy().to_string();
        let file2_path = file2.to_string_lossy().to_string();
        let matching = recs
            .iter()
            .find(|r| r.source_paths.contains(&file1_path) && r.source_paths.contains(&file2_path));

        assert!(
            matching.is_some(),
            "two files with the same MusicBrainz Recording ID should be in the same recording, \
             but no recording contains both {} and {}",
            file1_path,
            file2_path
        );
    }

    #[tokio::test]
    async fn test_same_mbid_from_ufid_tag_creates_same_recording() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(&db_path).await.unwrap();
        let serializer = tokio::sync::Semaphore::new(1);

        let mbid = "b6e3c72a-5f0c-4e5d-9f0a-987654321abc";

        let ufid_payload = format!("http://musicbrainz.org\0{mbid}");
        let ufid_frame = crate::library::scanner::id3_frame(b"UFID", ufid_payload.as_bytes());

        fn meta_with_mbid(title: &str, recording_mbid: Option<String>) -> TrackMetadata {
            let mut meta = tagged_meta(title, "Artist", "Album Artist", "Album", 1);
            meta.recording_mbid = recording_mbid;
            meta
        }

        fn write_mp3_with_ufid(
            path: &Path,
            title: &str,
            artist: &str,
            album_artist: &str,
            album: &str,
            ufid_frame: &[u8],
        ) {
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
                ufid_frame.to_vec(),
            ];
            let mp3_data = crate::library::scanner::synth_mp3_with_id3(&frames);
            std::fs::write(path, mp3_data).unwrap();
        }

        let file1 = tmp.path().join("ufid_song1.mp3");
        let file2 = tmp.path().join("ufid_song2.mp3");
        write_mp3_with_ufid(
            &file1,
            "UFID Song One",
            "Artist",
            "Album Artist",
            "Album",
            &ufid_frame,
        );
        write_mp3_with_ufid(
            &file2,
            "UFID Song Two",
            "Artist",
            "Album Artist",
            "Album",
            &ufid_frame,
        );

        // Import both with no AcoustID lookup — the MBID comes from meta.recording_mbid
        // (populated from UFID:http://musicbrainz.org in the real scan path).
        store_prepared_import(
            &pool,
            prepared_import_for_path(
                &file1,
                meta_with_mbid("UFID Song One", Some(mbid.into())),
                None,
            ),
            &serializer,
        )
        .await
        .unwrap();

        store_prepared_import(
            &pool,
            prepared_import_for_path(
                &file2,
                meta_with_mbid("UFID Song Two", Some(mbid.into())),
                None,
            ),
            &serializer,
        )
        .await
        .unwrap();

        // Verify via MemoryCatalog that both files land in the same recording.
        let catalog = crate::storage::memory::MemoryCatalog::load_from_db(&pool)
            .await
            .unwrap();
        let recs = crate::storage::CatalogReader::list_recordings(&catalog)
            .await
            .unwrap();

        let file1_path = file1.to_string_lossy().to_string();
        let file2_path = file2.to_string_lossy().to_string();
        let matching = recs
            .iter()
            .find(|r| r.source_paths.contains(&file1_path) && r.source_paths.contains(&file2_path));

        assert!(
            matching.is_some(),
            "two files sharing a UFID:http://musicbrainz.org tag should be in the same recording, \
             but no recording contains both {} and {}",
            file1_path,
            file2_path
        );
    }
}
