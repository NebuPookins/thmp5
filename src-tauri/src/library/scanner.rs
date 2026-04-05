use crate::models::TrackMetadata;
use anyhow::{Context, Result};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::ItemKey;
use std::path::Path;

/// Decode a 4-byte synchsafe integer (used in ID3v2.3+ tag/frame sizes).
fn synchsafe_to_u32(b: &[u8]) -> u32 {
    ((b[0] as u32) << 21) | ((b[1] as u32) << 14) | ((b[2] as u32) << 7) | (b[3] as u32)
}

/// Map a common ID3 frame ID to a human-readable field name.
fn frame_id_to_field_name(id: &str) -> &'static str {
    match id {
        "TIT2" | "TT2" => "title",
        "TPE1" | "TP1" => "artist",
        "TPE2" | "TP2" => "album artist",
        "TALB" | "TAL" => "album",
        "TRCK" | "TRK" => "track number",
        "TPOS" | "TPA" => "disc number",
        "TYER" | "TYE" | "TDRC" => "year",
        "TCON" | "TCO" => "genre",
        "TXXX" | "TXX" => "user-defined text",
        "TBPM" | "TBP" => "BPM",
        "TCOM" | "TCM" => "composer",
        "TPUB" | "TPB" => "publisher",
        "TIT1" | "TT1" => "content group",
        "TIT3" | "TT3" => "subtitle",
        "TOPE" | "TOA" => "original artist",
        "COMM" | "COM" => "comment",
        _ => "unknown field",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagDecodeErrorKind {
    Utf16OddLength,
    InvalidUtf8,
}

/// Scan raw ID3v2 bytes to find the first text frame that matches the given decode failure mode.
/// Returns the frame ID string (e.g. "TIT2") if one is found.
///
/// TODO: If https://github.com/Serial-ATA/lofty-rs/issues/639 is resolved, remove this
/// function and the surrounding error-enrichment code and rely on lofty's own frame ID
/// reporting instead.
fn find_problematic_id3_frame(path: &Path, error_kind: TagDecodeErrorKind) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 10 || &data[0..3] != b"ID3" {
        return None;
    }

    let version = data[3]; // 2, 3, or 4
    let flags = data[5];
    let tag_size = synchsafe_to_u32(&data[6..10]) as usize;
    let end = (10 + tag_size).min(data.len());

    let mut pos = 10;

    // Skip extended header (ID3v2.3+, flag bit 6).
    if version >= 3 && (flags & 0x40) != 0 && pos + 4 <= end {
        let ext_size = if version == 4 {
            synchsafe_to_u32(&data[pos..pos + 4]) as usize
        } else {
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize
        };
        pos += ext_size;
    }

    while pos < end {
        let (frame_id, frame_size) = if version == 2 {
            // ID3v2.2: 3-byte frame ID, 3-byte size.
            if pos + 6 > end {
                break;
            }
            if data[pos] == 0 {
                break; // padding
            }
            let id = std::str::from_utf8(&data[pos..pos + 3]).ok()?.to_string();
            let size = (data[pos + 3] as usize) << 16
                | (data[pos + 4] as usize) << 8
                | data[pos + 5] as usize;
            pos += 6;
            (id, size)
        } else {
            // ID3v2.3/v2.4: 4-byte frame ID, 4-byte size, 2-byte flags.
            if pos + 10 > end {
                break;
            }
            if data[pos] == 0 {
                break; // padding
            }
            let id = std::str::from_utf8(&data[pos..pos + 4]).ok()?.to_string();
            let size = if version == 4 {
                synchsafe_to_u32(&data[pos + 4..pos + 8]) as usize
            } else {
                u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize
            };
            pos += 10;
            (id, size)
        };

        if frame_size == 0 {
            continue;
        }

        let content_end = (pos + frame_size).min(end);
        let content = &data[pos..content_end];

        let is_text_frame = frame_id.starts_with('T') || frame_id == "COMM" || frame_id == "COM";
        if is_text_frame && content.len() > 1 {
            let encoding = content[0];
            let text_bytes = &content[1..];
            let is_problematic = match error_kind {
                // Encoding 1 = UTF-16 with BOM. UTF-16 content must have even byte length.
                TagDecodeErrorKind::Utf16OddLength => encoding == 1 && text_bytes.len() % 2 != 0,
                // Encoding 3 = UTF-8. Invalid byte sequences are what trigger lofty's
                // "Expected a UTF-8 string" decode failures.
                TagDecodeErrorKind::InvalidUtf8 => {
                    encoding == 3 && std::str::from_utf8(text_bytes).is_err()
                }
            };
            if is_problematic {
                return Some(frame_id);
            }
        }

        pos += frame_size;
    }

    None
}

/// Supported audio extensions.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "ogg", "flac", "wav", "m4a", "aac", "opus", "wma", "ape",
];

pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Read audio metadata from a file using lofty.
pub fn read_metadata(path: &Path) -> Result<TrackMetadata> {
    let tagged_file = Probe::open(path)
        .context("Failed to open file for metadata reading")?
        .guess_file_type()
        .context("Failed to detect file type")?
        .read()
        .map_err(|e| {
            let error_text = e.to_string();
            let error_kind = if error_text.contains("UTF-16 string has an odd length") {
                Some(TagDecodeErrorKind::Utf16OddLength)
            } else if error_text.contains("Expected a UTF-8 string") {
                Some(TagDecodeErrorKind::InvalidUtf8)
            } else {
                None
            };
            let frame_note = error_kind
                .and_then(|kind| find_problematic_id3_frame(path, kind))
                .map(|id| {
                    let field = frame_id_to_field_name(&id);
                    format!(" (frame {id} / {field})")
                })
                .unwrap_or_default();
            anyhow::anyhow!("Failed to read tags{frame_note}: {e}")
        })?;

    let properties = tagged_file.properties();
    let duration_ms = properties.duration().as_millis() as u64;

    let format = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("unknown")
        .to_lowercase();

    let mut meta = TrackMetadata {
        duration_ms,
        format,
        ..Default::default()
    };

    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    if let Some(tag) = tag {
        meta.title = tag.title().map(|s| s.into_owned());
        meta.artist = tag.artist().map(|s| s.into_owned());
        meta.album = tag.album().map(|s| s.into_owned());
        meta.year = tag.year();
        meta.track_number = tag.track();
        meta.track_total = tag.track_total();
        meta.disc_number = tag.disk();

        // Album artist (not in the Accessor trait; use the tag item key)
        meta.album_artist = tag
            .get_string(&ItemKey::AlbumArtist)
            .map(ToString::to_string);

        meta.genre = tag.genre().map(|s| s.into_owned());
        meta.comment = tag.comment().map(|s| s.into_owned());
        meta.bpm = tag
            .get_string(&ItemKey::Bpm)
            .and_then(|s| s.trim().parse().ok());
        meta.replay_gain_track_db = tag
            .get_string(&ItemKey::ReplayGainTrackGain)
            .and_then(|s| s.trim().trim_end_matches("dB").trim().parse().ok());
        meta.replay_gain_track_peak = tag
            .get_string(&ItemKey::ReplayGainTrackPeak)
            .and_then(|s| s.trim().parse().ok());
        meta.replay_gain_album_db = tag
            .get_string(&ItemKey::ReplayGainAlbumGain)
            .and_then(|s| s.trim().trim_end_matches("dB").trim().parse().ok());
        meta.replay_gain_album_peak = tag
            .get_string(&ItemKey::ReplayGainAlbumPeak)
            .and_then(|s| s.trim().parse().ok());
    }

    // Fall back: use filename as title if no title tag
    if meta.title.is_none() {
        meta.title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(ToString::to_string);
    }

    Ok(meta)
}

/// Parse a comment field into tags.
///
/// Two kinds of tags are extracted:
/// - `#token` runs (stored verbatim, including `#`): plain (`#chill`) and parameterized (`#TS:5/8`, `#drumdiff:7`)
/// - Delimiter-split plain text: the remainder after removing `#tokens` is split on `,`, `;`, and `\n`
pub fn parse_comment_tags(comment: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    let mut plain_buf = String::new();
    let mut rest = comment;

    while let Some(hash_pos) = rest.find('#') {
        plain_buf.push_str(&rest[..hash_pos]);
        rest = &rest[hash_pos..];
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        let token = &rest[..end];
        if token.len() > 1 {
            tags.push(token.to_string());
        }
        rest = &rest[end..];
    }
    plain_buf.push_str(rest);

    for part in plain_buf.split([',', ';', '\n']) {
        let t = part.trim();
        if !t.is_empty() {
            tags.push(t.to_string());
        }
    }

    tags
}

/// Extract embedded cover art from an audio file. Returns a data URL (data:image/...;base64,...) or None.
pub fn extract_cover_art(path: &Path) -> Result<Option<String>> {
    use lofty::picture::PictureType;

    let tagged_file = match Probe::open(path)
        .context("Failed to open file for cover art")?
        .guess_file_type()
        .context("Failed to detect file type")?
        .read()
    {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };

    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let Some(tag) = tag else {
        return Ok(None);
    };

    let picture = tag
        .pictures()
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| tag.pictures().first());

    let Some(picture) = picture else {
        return Ok(None);
    };

    let mime = picture
        .mime_type()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "image/jpeg".to_string());

    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(picture.data());
    Ok(Some(format!("data:{mime};base64,{encoded}")))
}
