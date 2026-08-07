pub mod serialize;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::Digest;
use std::path::Path;

/// Result of a tag write operation.
#[derive(Debug, Clone, Serialize)]
pub struct TagWriteResult {
    /// Path to the backup file (never auto-deleted).
    pub backup_path: String,
    /// SHA-256 of the audio content before the write.
    pub pre_audio_hash: Vec<u8>,
    /// SHA-256 of the audio content after the write (should match pre).
    pub post_audio_hash: Vec<u8>,
    /// SHA-256 of the entire file before the write.
    pub pre_full_hash: Vec<u8>,
    /// Number of frames in the rewritten tag.
    pub frame_count: usize,
}

// ── Backup / restore ─────────────────────────────────────────────────────────

/// Create a timestamped backup of the file at `path`.
///
/// Returns the backup path, e.g. `/path/to/file.mp3.20260522-140431.thmp5bak`.
fn backup_file(path: &Path) -> Result<String> {
    let timestamp = chrono_or_fallback();
    let backup_name = format!(
        "{}.{}.thmp5bak",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_else(|| path.to_string_lossy()),
        timestamp,
    );
    let backup_path = path
        .parent()
        .map(|p| p.join(&backup_name))
        .unwrap_or_else(|| Path::new(&backup_name).to_path_buf());

    std::fs::copy(path, &backup_path).context("Failed to create backup file")?;
    Ok(backup_path.to_string_lossy().to_string())
}

/// Generate a timestamp string for the backup filename.
fn chrono_or_fallback() -> String {
    // Try chrono first (if available as a dep), otherwise use std::time
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        let secs = now.as_secs();
        // Format as YYYYMMDD-HHmmss from Unix timestamp
        let days_since_epoch = secs / 86400;
        let time_secs = secs % 86400;
        let hours = time_secs / 3600;
        let minutes = (time_secs % 3600) / 60;
        let seconds = time_secs % 60;

        // Simple date calculation from Unix epoch (2000-03-01 based)
        let (year, month, day) = civil_from_days(days_since_epoch as i64);
        format!("{year:04}{month:02}{day:02}-{hours:02}{minutes:02}{seconds:02}")
    } else {
        "unknown".to_string()
    }
}

/// Convert days since Unix epoch (1970-01-01) to (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Restore a file from its backup.
pub fn restore_backup(backup_path: &str, target_path: &str) -> Result<()> {
    std::fs::copy(backup_path, target_path).context("Failed to restore from backup")?;
    Ok(())
}

// ── Write operations ─────────────────────────────────────────────────────────

/// Modify a single frame's value in a file, with full safety guarantees.
///
/// 1. Reads the entire file into memory
/// 2. Parses all ID3v2 text frames
/// 3. Finds the first frame matching `frame_id` and replaces its value
///    (if no match, appends a new frame — upsert semantics)
/// 4. Creates a timestamped backup (`.thmp5bak`)
/// 5. Serializes the new tag and writes to a temp file, then atomically renames
/// 6. Verifies audio-content hash is unchanged
/// 7. Verifies the modified frame is present in the rewritten file
/// 8. On verification failure: restores from backup
pub fn write_single_frame(path: &Path, frame_id: &str, new_value: &str) -> Result<TagWriteResult> {
    let original_data =
        std::fs::read(path).with_context(|| format!("Failed to read file: {}", path.display()))?;

    let (mut frames, id3v2_version) = parse_text_frames(&original_data)
        .ok_or_else(|| anyhow::anyhow!("No ID3v2 tag found in {}", path.display()))?;

    // Pre-compute hashes
    let pre_audio_hash = audio_hash(&original_data);
    let pre_full_hash = full_hash(&original_data);

    // Upsert: replace first matching frame, or append
    let mut found = false;
    for (id, val) in &mut frames {
        if id == frame_id {
            *val = new_value.to_string();
            found = true;
            break;
        }
    }
    if !found {
        frames.push((frame_id.to_string(), new_value.to_string()));
    }

    // Backup
    let backup_path = backup_file(path)?;

    // Write
    write_modified_file(path, &original_data, &frames, id3v2_version)?;

    // Verify
    let written_data = std::fs::read(path)
        .with_context(|| format!("Failed to re-read written file: {}", path.display()))?;

    let post_audio_hash = audio_hash(&written_data);

    if post_audio_hash != pre_audio_hash {
        // Audio corrupted — restore from backup
        restore_from_backup_internal(path, &backup_path)?;
        bail!(
            "Audio content hash changed after write — file restored from backup. \
             Backup at: {backup_path}"
        );
    }

    // Verify the frame edit is present
    let (reparsed, _) = parse_text_frames(&written_data)
        .ok_or_else(|| anyhow::anyhow!("Failed to re-parse ID3v2 tags after write"))?;

    let edit_found = reparsed
        .iter()
        .any(|(id, val)| id == frame_id && val == new_value);
    if !edit_found {
        restore_from_backup_internal(path, &backup_path)?;
        bail!(
            "Frame edit verification failed — '{frame_id}' not found with expected value. \
             File restored from backup at: {backup_path}"
        );
    }

    Ok(TagWriteResult {
        backup_path,
        pre_audio_hash,
        post_audio_hash,
        pre_full_hash,
        frame_count: frames.len(),
    })
}

/// Delete all frames matching `frame_id` from a file, with full safety guarantees.
pub fn delete_frame(path: &Path, frame_id: &str) -> Result<TagWriteResult> {
    let original_data =
        std::fs::read(path).with_context(|| format!("Failed to read file: {}", path.display()))?;

    let (frames, id3v2_version) = parse_text_frames(&original_data)
        .ok_or_else(|| anyhow::anyhow!("No ID3v2 tag found in {}", path.display()))?;

    let pre_audio_hash = audio_hash(&original_data);
    let pre_full_hash = full_hash(&original_data);

    // Remove matching frames. Removing every frame is allowed — we still write
    // a valid (empty) tag in that case.
    let modified_frames: Vec<_> = frames.into_iter().filter(|(id, _)| id != frame_id).collect();

    let backup_path = backup_file(path)?;
    write_modified_file(path, &original_data, &modified_frames, id3v2_version)?;

    // Verify
    let written_data = std::fs::read(path)
        .with_context(|| format!("Failed to re-read written file: {}", path.display()))?;

    let post_audio_hash = audio_hash(&written_data);
    if post_audio_hash != pre_audio_hash {
        restore_from_backup_internal(path, &backup_path)?;
        bail!(
            "Audio content hash changed after delete — file restored from backup. \
             Backup at: {backup_path}"
        );
    }

    let (reparsed, _) = parse_text_frames(&written_data)
        .ok_or_else(|| anyhow::anyhow!("Failed to re-parse ID3v2 tags after delete"))?;

    let still_present = reparsed.iter().any(|(id, _)| id == frame_id);
    if still_present {
        restore_from_backup_internal(path, &backup_path)?;
        bail!(
            "Frame deletion verification failed — '{frame_id}' still present. \
             File restored from backup at: {backup_path}"
        );
    }

    Ok(TagWriteResult {
        backup_path,
        pre_audio_hash,
        post_audio_hash,
        pre_full_hash,
        frame_count: modified_frames.len(),
    })
}

/// Restore from backup internally (no Result wrapper needed — always tries its best).
fn restore_from_backup_internal(path: &Path, backup_path: &str) -> Result<()> {
    std::fs::copy(backup_path, path)
        .context("Failed to restore from backup during rollback")
        .map(|_| ())
}

// ── File I/O ─────────────────────────────────────────────────────────────────

/// Write a modified tag to a file, preserving audio content verbatim.
///
/// Writes to a temp file first, then atomically renames to the target path.
fn write_modified_file(
    path: &Path,
    original_data: &[u8],
    frames: &[(String, String)],
    id3v2_version: u8,
) -> Result<()> {
    let new_data = rebuild_file(original_data, frames, id3v2_version)?;
    let temp_path = path.with_extension("tmp_thmp5_write");
    std::fs::write(&temp_path, &new_data).context("Failed to write temp file")?;
    std::fs::rename(&temp_path, path).context("Failed to rename temp file to target")?;
    Ok(())
}

/// Collect raw frame bytes for non-text frames from the original file.
///
/// These frames (APIC, UFID, etc.) cannot be re-serialized by our encoder,
/// so we pass them through verbatim to avoid data loss.
fn collect_preserved_frames(data: &[u8]) -> Vec<Vec<u8>> {
    if data.len() < 10 || &data[0..3] != b"ID3" {
        return Vec::new();
    }

    let version = data[3];
    if version < 2 || version > 4 {
        return Vec::new();
    }

    let frame_sizes_synchsafe = version >= 4;
    let tag_size = synchsafe_to_u32(&data[6..10]) as usize;
    let end = (10 + tag_size).min(data.len());
    let mut pos = skip_extended_header(data, version, 10, end);
    let mut preserved: Vec<Vec<u8>> = Vec::new();

    while pos + 10 <= end {
        if data[pos] == 0 {
            break;
        }

        let frame_id = match std::str::from_utf8(&data[pos..pos + 4]) {
            Ok(id) if id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => id.to_string(),
            _ => break,
        };

        let frame_size = if frame_sizes_synchsafe {
            synchsafe_to_u32(&data[pos + 4..pos + 8]) as usize
        } else {
            u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize
        };

        let frame_end = pos + 10 + frame_size;
        if frame_end > end {
            break;
        }

        // Collect all frames our serializer can't handle.
        if !is_text_frame(&frame_id) {
            preserved.push(data[pos..frame_end].to_vec());
        }

        pos = frame_end;
    }

    preserved
}

// ── Rebuild file ─────────────────────────────────────────────────────────────

/// Given a parsed list of frames, produce the new file bytes.
fn rebuild_file(
    original_data: &[u8],
    frames: &[(String, String)],
    id3v2_version: u8,
) -> Result<Vec<u8>> {
    let audio_start = find_audio_offset(original_data);
    let audio_bytes = &original_data[audio_start..];
    let preserved = collect_preserved_frames(original_data);
    let new_tag = serialize::serialize_tag(frames, &preserved, id3v2_version)?;

    let mut result = Vec::with_capacity(new_tag.len() + audio_bytes.len());
    result.extend_from_slice(&new_tag);
    result.extend_from_slice(audio_bytes);
    Ok(result)
}

// ── Hashing ──────────────────────────────────────────────────────────────────

/// SHA-256 of the audio content only (bytes after all ID3v2 tags).
pub fn audio_hash(data: &[u8]) -> Vec<u8> {
    let audio_start = find_audio_offset(data);
    sha2::Sha256::digest(&data[audio_start..]).to_vec()
}

/// SHA-256 of the entire file.
pub fn full_hash(data: &[u8]) -> Vec<u8> {
    sha2::Sha256::digest(data).to_vec()
}

// ── Low-level parsing ────────────────────────────────────────────────────────

/// Find the offset where audio data begins (past all consecutive ID3v2 tags).
pub fn find_audio_offset(data: &[u8]) -> usize {
    let mut offset = 0;
    loop {
        if offset + 10 > data.len() || &data[offset..offset + 3] != b"ID3" {
            return offset;
        }
        let tag_size = synchsafe_to_u32(&data[offset + 6..offset + 10]) as usize;
        offset += 10 + tag_size;

        if offset > 10 {
            let flags_pos = if offset >= tag_size + 10 {
                offset - tag_size - 10 + 5
            } else {
                5
            };
            if flags_pos < data.len() && data[flags_pos] & 0x10 != 0 {
                offset += 10;
            }
        }
    }
}

/// Parse all serializable text frames from raw file data.
///
/// Returns (frames, id3v2_version) where frames is a list of (frame_id, value).
pub fn parse_text_frames(data: &[u8]) -> Option<(Vec<(String, String)>, u8)> {
    if data.len() < 10 || &data[0..3] != b"ID3" {
        return None;
    }

    let version = data[3];
    if version < 2 || version > 4 {
        return None;
    }

    let frame_sizes_synchsafe = version >= 4;
    let tag_size = synchsafe_to_u32(&data[6..10]) as usize;
    let end = (10 + tag_size).min(data.len());

    let mut pos = skip_extended_header(data, version, 10, end);
    let mut frames: Vec<(String, String)> = Vec::new();

    while pos + 10 <= end {
        if data[pos] == 0 {
            break;
        }

        let frame_id = match std::str::from_utf8(&data[pos..pos + 4]) {
            Ok(id) if id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => id.to_string(),
            _ => break,
        };

        let frame_size = if frame_sizes_synchsafe {
            synchsafe_to_u32(&data[pos + 4..pos + 8]) as usize
        } else {
            u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize
        };

        let data_start = pos + 10;
        let data_end = data_start + frame_size;
        if data_end > end {
            break;
        }

        if is_text_frame(&frame_id) && frame_size > 1 {
            let encoding = data[data_start];
            let payload = &data[data_start + 1..data_end];
            match language_prefix_len(&frame_id) {
                Some(lang_len) if payload.len() >= lang_len => {
                    let (desc_bytes, value_bytes) =
                        split_description(&payload[lang_len..], encoding);
                    frames.push(described_frame(&frame_id, desc_bytes, value_bytes, encoding));
                }
                // Malformed described frame — too short to hold its language code.
                Some(_) => {}
                None => frames.push((frame_id, decode_id3v2_text(payload, encoding))),
            }
        }

        pos = data_end;
    }

    Some((frames, version))
}

/// The number of language-code bytes that precede the description in a
/// described frame's payload, or `None` for frames that carry no description.
///
/// TXXX is `[encoding][description][terminator][value]`; COMM is the same with
/// a 3-byte ISO-639-2 language code in front of the description.
fn language_prefix_len(frame_id: &str) -> Option<usize> {
    match frame_id {
        "TXXX" => Some(0),
        "COMM" => Some(3),
        _ => None,
    }
}

/// Split a `description ~ terminator ~ value` payload (used by TXXX and COMM)
/// into its two halves.
///
/// The terminator is two NUL bytes for the UTF-16 encodings (0x01/0x02) and a
/// single NUL byte otherwise. A payload with no terminator is treated as being
/// entirely the value, with an empty description.
fn split_description(payload: &[u8], encoding: u8) -> (&[u8], &[u8]) {
    if encoding == 0x01 || encoding == 0x02 {
        let mut i = 0;
        while i + 1 < payload.len() {
            if payload[i] == 0 && payload[i + 1] == 0 {
                return (&payload[..i], &payload[i + 2..]);
            }
            i += 2;
        }
    } else if let Some(n) = payload.iter().position(|&b| b == 0) {
        return (&payload[..n], &payload[n + 1..]);
    }
    (&[], payload)
}

/// Build the `(key, value)` pair for a frame that carries a description,
/// keying it as `"<base>:<description>"` (or just `"<base>"` when the
/// description is empty) so the description survives a rewrite.
fn described_frame(
    base_id: &str,
    desc_bytes: &[u8],
    value_bytes: &[u8],
    encoding: u8,
) -> (String, String) {
    let desc = decode_id3v2_text(desc_bytes, encoding);
    let value = decode_id3v2_text(value_bytes, encoding);
    let key = if desc.is_empty() {
        base_id.to_string()
    } else {
        format!("{base_id}:{desc}")
    };
    (key, value)
}

/// Split a frame key back into its base ID and description — the inverse of
/// the key built by [`described_frame`].
fn split_frame_key(key: &str) -> (&str, &str) {
    key.split_once(':').unwrap_or((key, ""))
}

fn skip_extended_header(data: &[u8], version: u8, pos: usize, end: usize) -> usize {
    if version >= 3 && (data[5] & 0x40) != 0 && pos + 4 <= end {
        let ext_size = if version == 4 {
            synchsafe_to_u32(&data[pos..pos + 4]) as usize
        } else {
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize
        };
        pos + ext_size
    } else {
        pos
    }
}

fn synchsafe_to_u32(b: &[u8]) -> u32 {
    ((b[0] as u32) << 21) | ((b[1] as u32) << 14) | ((b[2] as u32) << 7) | (b[3] as u32)
}

/// Whether a wire frame ID — or a `base:description` key produced by
/// [`described_frame`] — names a text frame this module can parse and
/// re-serialize.
fn is_text_frame(frame_id: &str) -> bool {
    let base = split_frame_key(frame_id).0;
    // Standard text frames plus the two described frames we understand.
    (base.len() == 4 && base.starts_with('T') && base != "TXXX") || matches!(base, "TXXX" | "COMM")
}

fn decode_id3v2_text(data: &[u8], encoding: u8) -> String {
    match encoding {
        0x00 => data.iter().map(|&b| b as char).collect(),
        0x01 => {
            let (bom, rest) = if data.len() >= 2 {
                data.split_at(2)
            } else {
                return String::new();
            };
            let u16_data: Vec<u16> = rest
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| {
                    if bom == b"\xFF\xFE" {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }
                })
                .take_while(|&c| c != 0)
                .collect();
            String::from_utf16_lossy(&u16_data)
        }
        0x02 => {
            let u16_data: Vec<u16> = data
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            String::from_utf16_lossy(&u16_data)
        }
        0x03 => String::from_utf8_lossy(data)
            .trim_end_matches('\0')
            .to_string(),
        _ => String::from_utf8_lossy(data).to_string(),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn synchsafe(n: u32) -> [u8; 4] {
        serialize::u32_to_synchsafe(n)
    }

    fn id3_frame(frame_id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(frame_id);
        buf.extend_from_slice(&synchsafe(data.len() as u32));
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(data);
        buf
    }

    fn synth_mp3(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut tag_data = Vec::new();
        for f in frames {
            tag_data.extend_from_slice(f);
        }
        let mut file = Vec::new();
        file.extend_from_slice(b"ID3");
        file.extend_from_slice(&[0x04, 0x00]);
        file.push(0x00);
        file.extend_from_slice(&synchsafe(tag_data.len() as u32));
        file.extend_from_slice(&tag_data);
        file.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        file.resize(file.len() + 413, 0u8);
        file
    }

    #[test]
    fn test_find_audio_offset_no_id3() {
        assert_eq!(find_audio_offset(b"\xFF\xFB\x90\x00audio"), 0);
    }

    #[test]
    fn test_find_audio_offset_with_id3() {
        let mut data = Vec::new();
        data.extend_from_slice(b"ID3");
        data.extend_from_slice(&[0x04, 0x00, 0x00]);
        data.extend_from_slice(&synchsafe(100));
        data.extend_from_slice(&vec![0u8; 100]);
        data.extend_from_slice(b"audio_data");
        assert_eq!(find_audio_offset(&data), 110);
    }

    #[test]
    fn test_parse_text_frames_basic() {
        let frames = vec![
            id3_frame(b"TIT2", b"\x03Test Song"),
            id3_frame(b"TPE1", b"\x03Artist"),
        ];
        let data = synth_mp3(&frames);
        let (parsed, version) = parse_text_frames(&data).unwrap();
        assert_eq!(version, 4);
        assert_eq!(
            parsed,
            vec![
                ("TIT2".into(), "Test Song".into()),
                ("TPE1".into(), "Artist".into()),
            ]
        );
    }

    #[test]
    fn test_full_roundtrip() {
        let frames = vec![
            id3_frame(b"TIT2", b"\x03Song"),
            id3_frame(b"TPE1", b"\x03Artist"),
            id3_frame(b"TALB", b"\x03Album"),
        ];
        let original = synth_mp3(&frames);
        let (parsed, version) = parse_text_frames(&original).unwrap();
        let rebuilt = rebuild_file(&original, &parsed, version).unwrap();
        let (reparsed, _) = parse_text_frames(&rebuilt).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn test_audio_hash_preserved() {
        let frames = vec![id3_frame(b"TIT2", b"\x03Song")];
        let original = synth_mp3(&frames);
        let orig_hash = audio_hash(&original);
        let (parsed, version) = parse_text_frames(&original).unwrap();
        let rebuilt = rebuild_file(&original, &parsed, version).unwrap();
        assert_eq!(orig_hash, audio_hash(&rebuilt));
    }

    #[test]
    fn test_delete_frame_rebuild() {
        let frames = vec![
            id3_frame(b"TIT2", b"\x03Song"),
            id3_frame(b"TPE1", b"\x03Artist"),
        ];
        let original = synth_mp3(&frames);
        let (parsed, version) = parse_text_frames(&original).unwrap();
        let filtered: Vec<_> = parsed.into_iter().filter(|(id, _)| id != "TPE1").collect();
        let new_data = rebuild_file(&original, &filtered, version).unwrap();
        let (reparsed, _) = parse_text_frames(&new_data).unwrap();
        assert_eq!(reparsed.len(), 1);
        assert_eq!(reparsed[0].0, "TIT2");
    }

    #[test]
    fn test_add_frame_rebuild() {
        let frames = vec![id3_frame(b"TIT2", b"\x03Song")];
        let original = synth_mp3(&frames);
        let (mut parsed, version) = parse_text_frames(&original).unwrap();
        parsed.push(("TCON".into(), "Rock".into()));
        let new_data = rebuild_file(&original, &parsed, version).unwrap();
        let (reparsed, _) = parse_text_frames(&new_data).unwrap();
        assert!(reparsed
            .iter()
            .any(|(id, val)| id == "TCON" && val == "Rock"));
    }

    #[test]
    fn test_write_single_frame_to_disk() {
        let frames = vec![id3_frame(b"TIT2", b"\x03Original")];
        let data = synth_mp3(&frames);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();

        let result = write_single_frame(&path, "TIT2", "Modified").unwrap();
        assert!(result.backup_path.ends_with(".thmp5bak"));
        assert_eq!(result.pre_audio_hash, result.post_audio_hash);

        // Verify the file on disk
        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert!(reparsed
            .iter()
            .any(|(id, val)| id == "TIT2" && val == "Modified"));

        // Verify backup exists
        assert!(std::path::Path::new(&result.backup_path).exists());
    }

    #[test]
    fn test_delete_frame_from_disk() {
        let frames = vec![
            id3_frame(b"TIT2", b"\x03Song"),
            id3_frame(b"TPE1", b"\x03Artist"),
        ];
        let data = synth_mp3(&frames);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();

        let result = delete_frame(&path, "TPE1").unwrap();
        assert_eq!(result.pre_audio_hash, result.post_audio_hash);

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert!(!reparsed.iter().any(|(id, _)| id == "TPE1"));
    }

    #[test]
    fn test_upsert_adds_new_frame() {
        let frames = vec![id3_frame(b"TIT2", b"\x03Song")];
        let data = synth_mp3(&frames);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();

        write_single_frame(&path, "TCON", "Rock").unwrap();

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert!(reparsed
            .iter()
            .any(|(id, val)| id == "TCON" && val == "Rock"));
    }

    #[test]
    fn test_restore_backup() {
        let frames = vec![id3_frame(b"TIT2", b"\x03Original")];
        let data = synth_mp3(&frames);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();

        let result = write_single_frame(&path, "TIT2", "Modified").unwrap();

        // Restore
        restore_backup(&result.backup_path, path.to_str().unwrap()).unwrap();

        // Verify original
        let restored = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&restored).unwrap();
        assert!(reparsed
            .iter()
            .any(|(id, val)| id == "TIT2" && val == "Original"));
    }

    #[test]
    fn test_civil_from_days_known_date() {
        // 2026-05-22 is approximately 20588 days after 1970-01-01
        // Let's verify: 2026-05-22
        let epoch_days = 365 * 56 + 14 + 31 + 28 + 31 + 30 + 22 - 1; // approx
        let expected = (1970 + 56, 5u32, 22u32);
        let (y, m, d) = civil_from_days(epoch_days as i64);
        assert_eq!((y, m, d), expected, "epoch_days={epoch_days}");
    }

    #[test]
    fn test_backup_file_creates_thmp5bak() {
        let frames = vec![id3_frame(b"TIT2", b"\x03Test")];
        let data = synth_mp3(&frames);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.mp3");
        std::fs::write(&path, &data).unwrap();

        let backup_path = backup_file(&path).unwrap();
        assert!(backup_path.ends_with(".thmp5bak"));
        assert!(std::path::Path::new(&backup_path).exists());

        // Backup should have same content as original
        let backup_data = std::fs::read(&backup_path).unwrap();
        assert_eq!(backup_data, data);
    }

    // ── TXXX tests ─────────────────────────────────────────────────────────────

    fn txxx_frame(description: &str, value: &str) -> Vec<u8> {
        let mut payload = vec![0x03u8]; // UTF-8
        payload.extend_from_slice(description.as_bytes());
        payload.push(0x00);
        payload.extend_from_slice(value.as_bytes());
        id3_frame(b"TXXX", &payload)
    }

    #[test]
    fn test_parse_txxx_frames() {
        let frames = vec![
            txxx_frame("MusicBrainz Artist Id", "some-uuid"),
            txxx_frame("MusicBrainz Album Id", "album-uuid"),
        ];
        let data = synth_mp3(&frames);
        let (parsed, _) = parse_text_frames(&data).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("TXXX:MusicBrainz Artist Id".into(), "some-uuid".into()),
                ("TXXX:MusicBrainz Album Id".into(), "album-uuid".into()),
            ]
        );
    }

    #[test]
    fn test_txxx_roundtrip() {
        let frames = vec![
            id3_frame(b"TIT2", b"\x03Song"),
            txxx_frame("MusicBrainz Artist Id", "some-uuid"),
            txxx_frame("MusicBrainz Album Id", "uuid-2"),
        ];
        let original = synth_mp3(&frames);
        let (parsed, version) = parse_text_frames(&original).unwrap();
        let rebuilt = rebuild_file(&original, &parsed, version).unwrap();
        let (reparsed, _) = parse_text_frames(&rebuilt).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn test_txxx_preserved_across_edit_of_other_frame() {
        let frames = vec![
            id3_frame(b"TIT2", b"\x03Song"),
            txxx_frame("MusicBrainz Artist Id", "artist-uuid"),
            id3_frame(b"TPE1", b"\x03Original Artist"),
        ];
        let original = synth_mp3(&frames);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &original).unwrap();

        // Edit the TPE1 frame — TXXX should survive
        write_single_frame(&path, "TPE1", "Modified Artist").unwrap();

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert!(reparsed
            .iter()
            .any(|(id, val)| id == "TPE1" && val == "Modified Artist"));
        assert!(reparsed
            .iter()
            .any(|(id, val)| id == "TXXX:MusicBrainz Artist Id" && val == "artist-uuid"));
        assert!(reparsed
            .iter()
            .any(|(id, val)| id == "TIT2" && val == "Song"));
    }

    #[test]
    fn test_write_txxx_frame_to_disk() {
        let frames = vec![
            id3_frame(b"TIT2", b"\x03Song"),
            txxx_frame("MusicBrainz Artist Id", "old-uuid"),
        ];
        let original = synth_mp3(&frames);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &original).unwrap();

        let result = write_single_frame(&path, "TXXX:MusicBrainz Artist Id", "new-uuid").unwrap();
        assert_eq!(result.pre_audio_hash, result.post_audio_hash);

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert!(reparsed
            .iter()
            .any(|(id, val)| id == "TXXX:MusicBrainz Artist Id" && val == "new-uuid"));
        // Original TIT2 should still be there
        assert!(reparsed.iter().any(|(id, _)| id == "TIT2"));
    }

    // ── COMM tests ─────────────────────────────────────────────────────────────

    fn comm_frame(description: &str, text: &str) -> Vec<u8> {
        let mut payload = vec![0x03u8]; // UTF-8
        payload.extend_from_slice(b"eng"); // language
        payload.extend_from_slice(description.as_bytes());
        payload.push(0x00);
        payload.extend_from_slice(text.as_bytes());
        id3_frame(b"COMM", &payload)
    }

    #[test]
    fn test_parse_comm_frame() {
        let data = synth_mp3(&[comm_frame("", "A comment")]);
        let (parsed, _) = parse_text_frames(&data).unwrap();
        assert_eq!(parsed, vec![("COMM".into(), "A comment".into())]);
    }

    #[test]
    fn test_parse_comm_frame_with_description() {
        let data = synth_mp3(&[comm_frame("ID3v1 Comment", "Ripped by X")]);
        let (parsed, _) = parse_text_frames(&data).unwrap();
        assert_eq!(
            parsed,
            vec![("COMM:ID3v1 Comment".into(), "Ripped by X".into())]
        );
    }

    #[test]
    fn test_parse_comm_frame_utf16() {
        // UTF-16LE with BOM: encoding 0x01, empty description, 2-byte terminator.
        let mut payload = vec![0x01u8];
        payload.extend_from_slice(b"eng");
        payload.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x00]); // BOM + terminator
        payload.extend_from_slice(&[0xFF, 0xFE]); // BOM for the text
        for c in "Hi".encode_utf16() {
            payload.extend_from_slice(&c.to_le_bytes());
        }
        let data = synth_mp3(&[id3_frame(b"COMM", &payload)]);
        let (parsed, _) = parse_text_frames(&data).unwrap();
        assert_eq!(parsed, vec![("COMM".into(), "Hi".into())]);
    }

    #[test]
    fn test_comm_roundtrip() {
        let original = synth_mp3(&[
            id3_frame(b"TIT2", b"\x03Song"),
            comm_frame("", "A comment"),
            comm_frame("ID3v1 Comment", "Ripped by X"),
        ]);
        let (parsed, version) = parse_text_frames(&original).unwrap();
        let rebuilt = rebuild_file(&original, &parsed, version).unwrap();
        let (reparsed, _) = parse_text_frames(&rebuilt).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn test_comm_preserved_across_edit_of_other_frame() {
        let original = synth_mp3(&[
            id3_frame(b"TIT2", b"\x03Song"),
            comm_frame("", "A comment"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &original).unwrap();

        write_single_frame(&path, "TIT2", "New Song").unwrap();

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert!(reparsed
            .iter()
            .any(|(id, val)| id == "TIT2" && val == "New Song"));
        assert!(reparsed
            .iter()
            .any(|(id, val)| id == "COMM" && val == "A comment"));
    }

    #[test]
    fn test_write_comm_frame_to_disk() {
        let original = synth_mp3(&[
            id3_frame(b"TIT2", b"\x03Song"),
            comm_frame("", "Old comment"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &original).unwrap();

        let result = write_single_frame(&path, "COMM", "New comment").unwrap();
        assert_eq!(result.pre_audio_hash, result.post_audio_hash);

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        let comments: Vec<_> = reparsed
            .into_iter()
            .filter(|(id, _)| id.starts_with("COMM"))
            .collect();
        assert_eq!(comments, vec![("COMM".into(), "New comment".into())]);
    }

    #[test]
    fn test_write_comm_leaves_described_comments_alone() {
        // iTunes stores gapless/normalization data in described COMM frames.
        // Editing "the comment" must not overwrite them.
        let original = synth_mp3(&[
            comm_frame("iTunNORM", "0000 0001"),
            comm_frame("", "Old comment"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &original).unwrap();

        write_single_frame(&path, "COMM", "New comment").unwrap();

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert_eq!(
            reparsed,
            vec![
                ("COMM:iTunNORM".into(), "0000 0001".into()),
                ("COMM".into(), "New comment".into()),
            ]
        );
    }

    #[test]
    fn test_delete_comm_frame() {
        let original = synth_mp3(&[
            id3_frame(b"TIT2", b"\x03Song"),
            comm_frame("iTunNORM", "0000 0001"),
            comm_frame("", "A comment"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &original).unwrap();

        delete_frame(&path, "COMM").unwrap();

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        // Only the plain comment goes; described comments are left intact.
        assert_eq!(
            reparsed,
            vec![
                ("TIT2".into(), "Song".into()),
                ("COMM:iTunNORM".into(), "0000 0001".into()),
            ]
        );
    }

    #[test]
    fn test_delete_txxx_frame() {
        let frames = vec![
            id3_frame(b"TIT2", b"\x03Song"),
            txxx_frame("MusicBrainz Artist Id", "some-uuid"),
        ];
        let original = synth_mp3(&frames);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &original).unwrap();

        let result = delete_frame(&path, "TXXX:MusicBrainz Artist Id").unwrap();
        assert_eq!(result.pre_audio_hash, result.post_audio_hash);

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert!(!reparsed
            .iter()
            .any(|(id, _)| id == "TXXX:MusicBrainz Artist Id"));
        // TIT2 should still be there
        assert!(reparsed.iter().any(|(id, _)| id == "TIT2"));
    }
}
