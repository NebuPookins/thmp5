pub mod serialize;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::{HashMap, HashSet};
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

    let (frame_groups, id3v2_version) = parse_all_text_frames(&original_data)
        .ok_or_else(|| anyhow::anyhow!("No ID3v2 tag found in {}", path.display()))?;

    // Collapse all consecutive tags into a single flat frame list, keeping the
    // first value for any frame that appears more than once.
    let mut frames: Vec<(String, String)> = frame_groups
        .into_iter()
        .map(|(id, values)| (id, values.into_iter().next().unwrap_or_default()))
        .collect();

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
    let preserved = collect_preserved_frames_all(&original_data, id3v2_version);
    write_modified_file(path, &original_data, &frames, id3v2_version, &preserved)?;

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

    let (frame_groups, id3v2_version) = parse_all_text_frames(&original_data)
        .ok_or_else(|| anyhow::anyhow!("No ID3v2 tag found in {}", path.display()))?;

    let pre_audio_hash = audio_hash(&original_data);
    let pre_full_hash = full_hash(&original_data);

    // Collapse all consecutive tags into a single flat frame list, then remove
    // the matching frame. Removing every frame is allowed — we still write a
    // valid (empty) tag in that case.
    let modified_frames: Vec<_> = frame_groups
        .into_iter()
        .map(|(id, values)| (id, values.into_iter().next().unwrap_or_default()))
        .filter(|(id, _)| id != frame_id)
        .collect();

    let backup_path = backup_file(path)?;
    let preserved = collect_preserved_frames_all(&original_data, id3v2_version);
    write_modified_file(
        path,
        &original_data,
        &modified_frames,
        id3v2_version,
        &preserved,
    )?;

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

/// A single conflicting frame discovered across consecutive ID3v2 tags.
#[derive(Debug, Clone, Serialize)]
pub struct MergeConflict {
    /// Wire frame ID (e.g. "TIT2", or "TXXX:description").
    pub frame_id: String,
    /// Human-readable field name for the frame.
    pub field_name: String,
    /// Ordered distinct values found across the tags.
    pub values: Vec<String>,
}

/// The user's chosen value for one conflicting frame during a merge.
#[derive(Debug, Clone, Deserialize)]
pub struct MergeDecision {
    pub frame_id: String,
    pub value: String,
}

/// Enumerate conflicting text frames across *all* consecutive ID3v2 tags.
///
/// Only frames whose value differs between tags are returned; a frame that
/// appears once (or identically in both tags) needs no user decision.
pub fn preview_merge(path: &Path) -> Result<Vec<MergeConflict>> {
    let data =
        std::fs::read(path).with_context(|| format!("Failed to read file: {}", path.display()))?;
    let (frames, _version) = parse_all_text_frames(&data)
        .ok_or_else(|| anyhow::anyhow!("No ID3v2 tag found in {}", path.display()))?;

    Ok(frames
        .into_iter()
        .filter(|(_, values)| values.len() >= 2)
        .map(|(frame_id, values)| MergeConflict {
            // Described frames are keyed "TXXX:description"; look up the bare ID.
            field_name: crate::library::scanner::frame_id_to_field_name(
                split_frame_key(&frame_id).0,
            )
            .to_string(),
            frame_id,
            values,
        })
        .collect())
}

/// Merge all consecutive ID3v2 tags into a single tag, applying the user's
/// decisions for conflicting frames. Non-text frames from every tag are
/// preserved (deduplicated), and audio content is left byte-identical.
pub fn apply_merge(path: &Path, decisions: &[MergeDecision]) -> Result<TagWriteResult> {
    let original_data =
        std::fs::read(path).with_context(|| format!("Failed to read file: {}", path.display()))?;

    let (frame_groups, id3v2_version) = parse_all_text_frames(&original_data)
        .ok_or_else(|| anyhow::anyhow!("No ID3v2 tag found in {}", path.display()))?;

    let decision_map: HashMap<&str, &str> = decisions
        .iter()
        .map(|d| (d.frame_id.as_str(), d.value.as_str()))
        .collect();

    let mut frames: Vec<(String, String)> = Vec::with_capacity(frame_groups.len());
    for (frame_id, values) in &frame_groups {
        let chosen = decision_map
            .get(frame_id.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| values.first().cloned().unwrap_or_default());
        frames.push((frame_id.clone(), chosen));
    }

    let pre_audio_hash = audio_hash(&original_data);
    let pre_full_hash = full_hash(&original_data);

    let backup_path = backup_file(path)?;
    let preserved = collect_preserved_frames_all(&original_data, id3v2_version);
    write_modified_file(path, &original_data, &frames, id3v2_version, &preserved)?;

    // Verify audio is unchanged.
    let written_data = std::fs::read(path)
        .with_context(|| format!("Failed to re-read written file: {}", path.display()))?;
    let post_audio_hash = audio_hash(&written_data);
    if post_audio_hash != pre_audio_hash {
        restore_from_backup_internal(path, &backup_path)?;
        bail!(
            "Audio content hash changed after merge — file restored from backup. \
             Backup at: {backup_path}"
        );
    }

    // Verify each decided frame now reads back as the chosen value (the merge
    // always produces a single tag, so parse the first tag only).
    let (reparsed, _) = parse_text_frames(&written_data)
        .ok_or_else(|| anyhow::anyhow!("Failed to re-parse ID3v2 tags after merge"))?;
    for decision in decisions {
        let ok = reparsed
            .iter()
            .any(|(id, val)| id == &decision.frame_id && val == &decision.value);
        if !ok {
            restore_from_backup_internal(path, &backup_path)?;
            bail!(
                "Merge verification failed — '{}' not found with the chosen value. \
                 File restored from backup at: {backup_path}",
                decision.frame_id
            );
        }
    }

    Ok(TagWriteResult {
        backup_path,
        pre_audio_hash,
        post_audio_hash,
        pre_full_hash,
        frame_count: frames.len(),
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
    preserved: &[Vec<u8>],
) -> Result<()> {
    let new_data = rebuild_file(original_data, frames, id3v2_version, preserved)?;
    let temp_path = path.with_extension("tmp_thmp5_write");
    std::fs::write(&temp_path, &new_data).context("Failed to write temp file")?;
    std::fs::rename(&temp_path, path).context("Failed to rename temp file to target")?;
    Ok(())
}

/// Visit each frame in a tag body, calling
/// `visit(frame_id, frame_start, data_start, data_end)` with the frame's 4-byte
/// ID, the offset of its 10-byte header, and the byte range of its payload
/// (encoding byte included). Stops early on padding or a malformed frame.
fn for_each_frame(data: &[u8], tag: &TagSpan, mut visit: impl FnMut(&str, usize, usize, usize)) {
    let mut pos = skip_extended_header(data, tag.version, tag.flags, tag.body_start, tag.body_end);
    while pos + 10 <= tag.body_end {
        if data[pos] == 0 {
            break;
        }

        let frame_id = match std::str::from_utf8(&data[pos..pos + 4]) {
            Ok(id) if id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => id,
            _ => break,
        };

        let frame_size = if tag.synchsafe {
            synchsafe_to_u32(&data[pos + 4..pos + 8]) as usize
        } else {
            u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize
        };

        let frame_start = pos;
        let data_start = pos + 10;
        let data_end = data_start + frame_size;
        if data_end > tag.body_end {
            break;
        }

        visit(frame_id, frame_start, data_start, data_end);
        pos = data_end;
    }
}

/// Collect raw frame bytes for non-text frames from a single tag body.
fn collect_preserved_frames_in(data: &[u8], tag: &TagSpan) -> Vec<Vec<u8>> {
    let mut preserved: Vec<Vec<u8>> = Vec::new();
    for_each_frame(data, tag, |frame_id, frame_start, _data_start, data_end| {
        // Collect all frames our serializer can't handle, header included.
        if !is_text_frame(frame_id) {
            preserved.push(data[frame_start..data_end].to_vec());
        }
    });
    preserved
}

/// Collect non-text frames from *all* consecutive ID3v2 tags, deduplicated by
/// exact bytes (identical artwork present in two tags must not be duplicated).
/// Each frame's size header is re-encoded to `target_version`'s encoding so a
/// v2.3 frame copied into a v2.4 tag (or vice versa) parses correctly.
fn collect_preserved_frames_all(data: &[u8], target_version: u8) -> Vec<Vec<u8>> {
    let target_synchsafe = target_version >= 4;
    let mut preserved: Vec<Vec<u8>> = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for tag in iter_tags(data) {
        for frame in collect_preserved_frames_in(data, &tag) {
            let frame = if tag.synchsafe == target_synchsafe {
                frame
            } else {
                reencode_frame_size(&frame, tag.synchsafe, target_synchsafe)
            };
            if seen.insert(frame.clone()) {
                preserved.push(frame);
            }
        }
    }
    preserved
}

/// Re-encode a frame's 4-byte size header between synchsafe and big-endian,
/// leaving the frame ID, flags, and payload bytes untouched.
fn reencode_frame_size(frame: &[u8], from_synchsafe: bool, to_synchsafe: bool) -> Vec<u8> {
    let size = if from_synchsafe {
        synchsafe_to_u32(&frame[4..8])
    } else {
        u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]])
    };
    let mut out = frame.to_vec();
    let encoded = if to_synchsafe {
        serialize::u32_to_synchsafe(size)
    } else {
        size.to_be_bytes()
    };
    out[4..8].copy_from_slice(&encoded);
    out
}

// ── Rebuild file ─────────────────────────────────────────────────────────────

/// Given a parsed list of frames, produce the new file bytes.
fn rebuild_file(
    original_data: &[u8],
    frames: &[(String, String)],
    id3v2_version: u8,
    preserved: &[Vec<u8>],
) -> Result<Vec<u8>> {
    let audio_start = find_audio_offset(original_data);
    let audio_bytes = &original_data[audio_start..];
    let new_tag = serialize::serialize_tag(frames, preserved, id3v2_version)?;

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

/// A single consecutive ID3v2 tag located within the file data.
struct TagSpan {
    /// ID3v2 major version (2, 3, or 4).
    version: u8,
    /// Whether this tag's frame sizes are synchsafe-encoded (v2.4) or big-endian.
    synchsafe: bool,
    /// The tag's flags byte (used for footer / extended-header detection).
    flags: u8,
    /// Absolute offset of the first frame byte (header + 10, before any
    /// extended-header skip).
    body_start: usize,
    /// Absolute offset one past the last frame byte (exclusive).
    body_end: usize,
    /// Absolute offset immediately after this tag (body_end + optional footer).
    next_offset: usize,
}

/// Enumerate every consecutive ID3v2 tag at the start of `data`.
fn iter_tags(data: &[u8]) -> Vec<TagSpan> {
    let mut tags = Vec::new();
    let mut offset = 0usize;
    loop {
        if offset + 10 > data.len() || &data[offset..offset + 3] != b"ID3" {
            break;
        }
        let version = data[offset + 3];
        if version < 2 || version > 4 {
            break;
        }

        let tag_size = synchsafe_to_u32(&data[offset + 6..offset + 10]) as usize;
        // The tag size is synchsafe for every ID3v2 version; only the frame
        // sizes differ (synchsafe in v2.4, big-endian in v2.3 and earlier).
        let synchsafe = version >= 4;

        let flags = data[offset + 5];
        let body_start = offset + 10;
        let body_end = (body_start + tag_size).min(data.len());
        let next_offset = (body_end + if flags & 0x10 != 0 { 10 } else { 0 }).min(data.len());

        tags.push(TagSpan {
            version,
            synchsafe,
            flags,
            body_start,
            body_end,
            next_offset,
        });
        offset = next_offset;
    }
    tags
}

/// Find the offset where audio data begins (past all consecutive ID3v2 tags).
pub fn find_audio_offset(data: &[u8]) -> usize {
    iter_tags(data).last().map(|t| t.next_offset).unwrap_or(0)
}

/// Parse all serializable text frames from a single tag body.
fn parse_text_frames_in(data: &[u8], tag: &TagSpan) -> Vec<(String, String)> {
    let mut frames: Vec<(String, String)> = Vec::new();
    for_each_frame(data, tag, |frame_id, _frame_start, data_start, data_end| {
        // Skip only truly empty frames (no payload); a 1-byte payload is a valid
        // empty text frame (encoding byte with no text).
        if !is_text_frame(frame_id) || data_end == data_start {
            return;
        }
        let encoding = data[data_start];
        let payload = &data[data_start + 1..data_end];
        match language_prefix_len(frame_id) {
            Some(lang_len) if payload.len() >= lang_len => {
                let (desc_bytes, value_bytes) = split_description(&payload[lang_len..], encoding);
                frames.push(described_frame(frame_id, desc_bytes, value_bytes, encoding));
            }
            // Malformed described frame — too short to hold its language code.
            Some(_) => {}
            None => frames.push((frame_id.to_string(), decode_id3v2_text(payload, encoding))),
        }
    });
    frames
}

/// Parse all serializable text frames from raw file data (first tag only).
///
/// Returns (frames, id3v2_version) where frames is a list of (frame_id, value).
pub fn parse_text_frames(data: &[u8]) -> Option<(Vec<(String, String)>, u8)> {
    let tag = iter_tags(data).into_iter().next()?;
    Some((parse_text_frames_in(data, &tag), tag.version))
}

/// Parse text frames from *all* consecutive ID3v2 tags, collapsing duplicate
/// frame IDs into an ordered list of distinct values.
///
/// Returns (frames, id3v2_version) where `frames` preserves first-appearance
/// order and each entry is `(frame_id, distinct_values_in_file_order)`.
fn parse_all_text_frames(data: &[u8]) -> Option<(Vec<(String, Vec<String>)>, u8)> {
    let tags = iter_tags(data);
    let version = tags.first()?.version;

    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for tag in &tags {
        for (id, val) in parse_text_frames_in(data, tag) {
            let values = groups.entry(id.clone()).or_insert_with(|| {
                order.push(id.clone());
                Vec::new()
            });
            if !values.contains(&val) {
                values.push(val);
            }
        }
    }

    Some((
        order
            .into_iter()
            .map(|id| {
                let values = groups.remove(&id).unwrap_or_default();
                (id, values)
            })
            .collect(),
        version,
    ))
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

fn skip_extended_header(data: &[u8], version: u8, flags: u8, pos: usize, end: usize) -> usize {
    if version >= 3 && (flags & 0x40) != 0 && pos + 4 <= end {
        // v2.4's extended-header size is synchsafe and counts the size field
        // itself; v2.3's is big-endian and excludes its own 4 bytes.
        let ext_size = if version == 4 {
            synchsafe_to_u32(&data[pos..pos + 4]) as usize
        } else {
            4 + u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize
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
        // ISO-8859-1: a trailing NUL is a terminator artifact, mirroring the
        // trim already applied to the UTF-8 branch below.
        0x00 => data
            .iter()
            .map(|&b| b as char)
            .collect::<String>()
            .trim_end_matches('\0')
            .to_string(),
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
        let rebuilt = rebuild_file(&original, &parsed, version, &[]).unwrap();
        let (reparsed, _) = parse_text_frames(&rebuilt).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn test_audio_hash_preserved() {
        let frames = vec![id3_frame(b"TIT2", b"\x03Song")];
        let original = synth_mp3(&frames);
        let orig_hash = audio_hash(&original);
        let (parsed, version) = parse_text_frames(&original).unwrap();
        let rebuilt = rebuild_file(&original, &parsed, version, &[]).unwrap();
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
        let new_data = rebuild_file(&original, &filtered, version, &[]).unwrap();
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
        let new_data = rebuild_file(&original, &parsed, version, &[]).unwrap();
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
        let rebuilt = rebuild_file(&original, &parsed, version, &[]).unwrap();
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
        let rebuilt = rebuild_file(&original, &parsed, version, &[]).unwrap();
        let (reparsed, _) = parse_text_frames(&rebuilt).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn test_comm_preserved_across_edit_of_other_frame() {
        let original = synth_mp3(&[id3_frame(b"TIT2", b"\x03Song"), comm_frame("", "A comment")]);
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

    /// Build a synthetic MP3 with two consecutive ID3v2 tags, then audio.
    fn synth_mp3_two_tags(tag1: &[Vec<u8>], tag2: &[Vec<u8>]) -> Vec<u8> {
        let mut file = Vec::new();
        for (frames) in [tag1, tag2] {
            let mut tag_data = Vec::new();
            for f in frames {
                tag_data.extend_from_slice(f);
            }
            file.extend_from_slice(b"ID3");
            file.extend_from_slice(&[0x04, 0x00]);
            file.push(0x00);
            file.extend_from_slice(&synchsafe(tag_data.len() as u32));
            file.extend_from_slice(&tag_data);
        }
        file.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        file.resize(file.len() + 413, 0u8);
        file
    }

    fn merge_decision(frame_id: &str, value: &str) -> MergeDecision {
        MergeDecision {
            frame_id: frame_id.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn test_preview_merge_reports_conflicts() {
        let tag1 = vec![
            id3_frame(b"TIT2", b"\x03First Title"),
            id3_frame(b"TPE1", b"\x03Shared Artist"),
        ];
        let tag2 = vec![
            id3_frame(b"TIT2", b"\x03Second Title"),
            id3_frame(b"TPE1", b"\x03Shared Artist"),
            id3_frame(b"TALB", b"\x03Album"),
        ];
        let data = synth_mp3_two_tags(&tag1, &tag2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();

        let conflicts = preview_merge(&path).unwrap();
        // Only TIT2 differs between the tags. TPE1 matches, TALB is unique.
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].frame_id, "TIT2");
        assert_eq!(conflicts[0].values, vec!["First Title", "Second Title"]);
    }

    #[test]
    fn test_preview_merge_ignores_trailing_null_only_difference() {
        // decode_id3v2_text strips trailing NULs for ISO-8859-1, so "Foo" and
        // "Foo\0" read as the same value and shouldn't be shown as a conflict.
        let tag1 = vec![id3_frame(b"TIT2", b"\x00Foo")];
        let tag2 = vec![id3_frame(b"TIT2", b"\x00Foo\x00")];
        let data = synth_mp3_two_tags(&tag1, &tag2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();

        let conflicts = preview_merge(&path).unwrap();
        assert!(conflicts.is_empty());
    }

    #[test]
    fn test_apply_merge_collapses_and_preserves() {
        let tag1 = vec![
            id3_frame(b"TIT2", b"\x03First Title"),
            id3_frame(b"TPE1", b"\x03Shared Artist"),
        ];
        let tag2 = vec![
            id3_frame(b"TIT2", b"\x03Second Title"),
            id3_frame(b"TPE1", b"\x03Shared Artist"),
            id3_frame(b"TALB", b"\x03Album"),
        ];
        let data = synth_mp3_two_tags(&tag1, &tag2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();
        let pre_audio = audio_hash(&data);

        let result = apply_merge(&path, &[merge_decision("TIT2", "Second Title")]).unwrap();
        assert_eq!(result.pre_audio_hash, result.post_audio_hash);
        assert_eq!(result.pre_audio_hash, pre_audio);

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        // Chosen value for TIT2 wins.
        assert!(reparsed
            .iter()
            .any(|(id, v)| id == "TIT2" && v == "Second Title"));
        // Matching frame survives.
        assert!(reparsed
            .iter()
            .any(|(id, v)| id == "TPE1" && v == "Shared Artist"));
        // Frame unique to the second tag is preserved (not dropped).
        assert!(reparsed.iter().any(|(id, v)| id == "TALB" && v == "Album"));
        // Only one tag remains (the two consecutive tags collapsed into one).
        assert_eq!(iter_tags(&written).len(), 1);
    }

    #[test]
    fn test_apply_merge_keeps_first_when_not_decided() {
        let tag1 = vec![id3_frame(b"TIT2", b"\x03First Title")];
        let tag2 = vec![id3_frame(b"TIT2", b"\x03Second Title")];
        let data = synth_mp3_two_tags(&tag1, &tag2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();

        apply_merge(&path, &[]).unwrap();
        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert_eq!(
            reparsed,
            vec![("TIT2".to_string(), "First Title".to_string())]
        );
    }

    #[test]
    fn test_apply_merge_verifies_empty_value() {
        // One tag has "Foo", the other an empty TIT2 (encoding byte only).
        // Choosing the empty value must succeed rather than rolling back.
        let tag1 = vec![id3_frame(b"TIT2", b"\x03Foo")];
        let tag2 = vec![id3_frame(b"TIT2", b"\x03")];
        let data = synth_mp3_two_tags(&tag1, &tag2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();

        let result = apply_merge(&path, &[merge_decision("TIT2", "")]).unwrap();
        assert_eq!(result.pre_audio_hash, result.post_audio_hash);

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert!(reparsed.iter().any(|(id, v)| id == "TIT2" && v.is_empty()));
    }

    #[test]
    fn test_apply_merge_dedups_identical_preserved_frames() {
        let apic = id3_frame(b"APIC", b"\x00image/jpeg\x00\x03\xff\xd8\xff\xe0");
        let tag1 = vec![apic.clone(), id3_frame(b"TIT2", b"\x03First Title")];
        let tag2 = vec![apic, id3_frame(b"TIT2", b"\x03Second Title")];
        let data = synth_mp3_two_tags(&tag1, &tag2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();

        apply_merge(&path, &[merge_decision("TIT2", "Second Title")]).unwrap();
        let written = std::fs::read(&path).unwrap();
        // The identical APIC from both tags is deduplicated to a single frame.
        let preserved = collect_preserved_frames_all(&written, 4);
        assert_eq!(preserved.len(), 1);
    }

    // ── v2.3 helpers ─────────────────────────────────────────────────────────

    /// Build a v2.3 frame: frame ID + big-endian size + flags + payload.
    fn id3_frame_be(frame_id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(frame_id);
        buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(data);
        buf
    }

    /// Build a synthetic v2.3 MP3. Per the ID3v2.3 spec the tag *size* is
    /// synchsafe (like v2.4), while *frame* sizes are big-endian.
    fn synth_mp3_v3(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut tag_data = Vec::new();
        for f in frames {
            tag_data.extend_from_slice(f);
        }
        let mut file = Vec::new();
        file.extend_from_slice(b"ID3");
        file.extend_from_slice(&[0x03, 0x00, 0x00]); // v3, revision 0, flags 0
        file.extend_from_slice(&synchsafe(tag_data.len() as u32));
        file.extend_from_slice(&tag_data);
        file.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        file.resize(file.len() + 413, 0u8);
        file
    }

    /// Build a v2.3 MP3 with an extended header (flags = 0x40). The v2.3
    /// extended-header size field excludes its own 4 bytes, so a 10-byte
    /// extended header stores the value 6.
    fn synth_mp3_v3_extended(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&6u32.to_be_bytes()); // ext size (excludes itself)
        body.extend_from_slice(&[0x00, 0x00]); // extended flags
        body.extend_from_slice(&0u32.to_be_bytes()); // size of padding
        for f in frames {
            body.extend_from_slice(f);
        }
        let mut file = Vec::new();
        file.extend_from_slice(b"ID3");
        file.extend_from_slice(&[0x03, 0x00, 0x40]); // v3, flags = extended header
        file.extend_from_slice(&synchsafe(body.len() as u32));
        file.extend_from_slice(&body);
        file.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        file.resize(file.len() + 413, 0u8);
        file
    }

    #[test]
    fn test_v3_tag_size_is_synchsafe() {
        // A v2.3 tag body >= 128 bytes: the tag-size header must be read as
        // synchsafe, not big-endian (which would mis-slice the audio).
        let title: Vec<u8> = std::iter::once(0x03u8)
            .chain(std::iter::repeat(b'A').take(200))
            .collect();
        let frames = vec![id3_frame_be(b"TIT2", &title)];
        let data = synth_mp3_v3(&frames);
        let expected_audio = 10 + frames.iter().map(|f| f.len()).sum::<usize>();
        assert_eq!(find_audio_offset(&data), expected_audio);
    }

    #[test]
    fn test_v3_extended_header_skip_is_correct() {
        let frames = vec![id3_frame_be(b"TIT2", b"\x03Song")];
        let data = synth_mp3_v3_extended(&frames);
        let (parsed, version) = parse_text_frames(&data).unwrap();
        assert_eq!(version, 3);
        assert_eq!(parsed, vec![("TIT2".to_string(), "Song".to_string())]);
    }

    #[test]
    fn test_write_single_frame_preserves_second_tag_frames() {
        let tag1 = vec![
            id3_frame(b"TIT2", b"\x03Song"),
            id3_frame(b"TPE1", b"\x03Original Artist"),
        ];
        let tag2 = vec![
            id3_frame(b"TIT2", b"\x03Song"),
            comm_frame("", "A comment"),
            id3_frame(b"TALB", b"\x03Album"),
        ];
        let data = synth_mp3_two_tags(&tag1, &tag2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();

        write_single_frame(&path, "TPE1", "New Artist").unwrap();

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert!(reparsed
            .iter()
            .any(|(id, v)| id == "TPE1" && v == "New Artist"));
        assert!(reparsed.iter().any(|(id, v)| id == "TIT2" && v == "Song"));
        // Frames unique to the second tag must survive the edit.
        assert!(reparsed
            .iter()
            .any(|(id, v)| id == "COMM" && v == "A comment"));
        assert!(reparsed.iter().any(|(id, v)| id == "TALB" && v == "Album"));
    }

    #[test]
    fn test_delete_frame_preserves_second_tag_frames() {
        let tag1 = vec![
            id3_frame(b"TIT2", b"\x03Song"),
            id3_frame(b"TPE1", b"\x03Artist"),
        ];
        let tag2 = vec![
            id3_frame(b"TIT2", b"\x03Song"),
            id3_frame(b"TALB", b"\x03Album"),
        ];
        let data = synth_mp3_two_tags(&tag1, &tag2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).unwrap();

        delete_frame(&path, "TPE1").unwrap();

        let written = std::fs::read(&path).unwrap();
        let (reparsed, _) = parse_text_frames(&written).unwrap();
        assert!(!reparsed.iter().any(|(id, _)| id == "TPE1"));
        assert!(reparsed.iter().any(|(id, v)| id == "TIT2" && v == "Song"));
        // Frame unique to the second tag must survive the delete.
        assert!(reparsed.iter().any(|(id, v)| id == "TALB" && v == "Album"));
    }
}
