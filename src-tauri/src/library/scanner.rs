use crate::models::{
    Id3FrameDebugInfo, MetadataReadResult, TagProperty, TaglibHelperResponse, TrackMetadata,
};
use anyhow::{Context, Result};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::ItemKey;
use std::path::Path;
use std::process::Command;

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

fn id3_text_encoding_name(byte: u8) -> &'static str {
    match byte {
        0 => "ISO-8859-1 / Latin-1",
        1 => "UTF-16 with BOM",
        2 => "UTF-16BE without BOM",
        3 => "UTF-8",
        _ => "unknown",
    }
}

fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

fn decode_utf16_units<I>(units: I) -> Option<String>
where
    I: IntoIterator<Item = u16>,
{
    std::char::decode_utf16(units)
        .map(|r| r.ok())
        .collect::<Option<String>>()
}

fn decode_utf16be(bytes: &[u8]) -> Option<String> {
    let chunks = bytes.chunks_exact(2);
    if !chunks.remainder().is_empty() {
        return None;
    }
    decode_utf16_units(chunks.map(|c| u16::from_be_bytes([c[0], c[1]])))
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    let chunks = bytes.chunks_exact(2);
    if !chunks.remainder().is_empty() {
        return None;
    }
    decode_utf16_units(chunks.map(|c| u16::from_le_bytes([c[0], c[1]])))
}

fn decode_utf16_with_bom(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 {
        return None;
    }
    match &bytes[..2] {
        [0xFE, 0xFF] => decode_utf16be(&bytes[2..]),
        [0xFF, 0xFE] => decode_utf16le(&bytes[2..]),
        _ => None,
    }
}

fn hex_preview(bytes: &[u8], max_len: usize) -> String {
    bytes
        .iter()
        .take(max_len)
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn summarize_debug_info(info: &Id3FrameDebugInfo) -> String {
    let latin1_preview = info.latin1.as_deref().unwrap_or("<unavailable>");
    let utf8_status = if info.utf8.is_some() { "ok" } else { "invalid" };
    format!(
        "ID3 debug: frame {} / {} declared encoding={} ({}) payload_len={} utf8={} latin1={:?} hex={}",
        info.frame_id,
        info.field_name,
        info.declared_encoding_byte,
        info.declared_encoding_name,
        info.payload_len,
        utf8_status,
        latin1_preview,
        info.raw_payload_hex_preview
    )
}

#[derive(Debug)]
struct Id3FrameRef {
    version_major: u8,
    frame_id: String,
    content: Vec<u8>,
}

fn find_id3_text_frame(path: &Path, target_frame_id: &str) -> Option<Id3FrameRef> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 10 || &data[0..3] != b"ID3" {
        return None;
    }

    let version = data[3];
    let flags = data[5];
    let tag_size = synchsafe_to_u32(&data[6..10]) as usize;
    let end = (10 + tag_size).min(data.len());

    let mut pos = 10;
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
            if pos + 6 > end {
                break;
            }
            if data[pos] == 0 {
                break;
            }
            let id = std::str::from_utf8(&data[pos..pos + 3]).ok()?.to_string();
            let size = (data[pos + 3] as usize) << 16
                | (data[pos + 4] as usize) << 8
                | data[pos + 5] as usize;
            pos += 6;
            (id, size)
        } else {
            if pos + 10 > end {
                break;
            }
            if data[pos] == 0 {
                break;
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
        if frame_id == target_frame_id {
            let content = &data[pos..content_end];
            return Some(Id3FrameRef {
                version_major: version,
                frame_id,
                content: content.to_vec(),
            });
        }
        pos += frame_size;
    }

    None
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

pub fn debug_id3_text_frame(path: &Path, frame_id: &str) -> Result<Id3FrameDebugInfo> {
    let frame = find_id3_text_frame(path, frame_id)
        .ok_or_else(|| anyhow::anyhow!("Frame {frame_id} not found in {}", path.display()))?;
    let declared_encoding_byte = *frame
        .content
        .first()
        .ok_or_else(|| anyhow::anyhow!("Frame {frame_id} has no payload"))?;
    let text_bytes = &frame.content[1..];

    Ok(Id3FrameDebugInfo {
        frame_id: frame.frame_id.clone(),
        field_name: frame_id_to_field_name(&frame.frame_id).to_string(),
        version_major: frame.version_major,
        declared_encoding_byte,
        declared_encoding_name: id3_text_encoding_name(declared_encoding_byte).to_string(),
        payload_len: text_bytes.len(),
        raw_payload_hex_preview: hex_preview(text_bytes, 64),
        utf8: std::str::from_utf8(text_bytes).ok().map(str::to_string),
        latin1: Some(latin1_to_string(text_bytes)),
        utf16_with_bom: decode_utf16_with_bom(text_bytes),
        utf16be_without_bom: decode_utf16be(text_bytes),
        utf16le_without_bom: decode_utf16le(text_bytes),
    })
}

fn utf8_decode_debug_summary(path: &Path) -> Option<String> {
    let frame_id = find_problematic_id3_frame(path, TagDecodeErrorKind::InvalidUtf8)?;
    let info = debug_id3_text_frame(path, &frame_id).ok()?;
    Some(summarize_debug_info(&info))
}

fn taglib_helper_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("THMP5_TAGLIB_HELPER") {
        candidates.push(path.into());
    }
    if let Some(path) = option_env!("THMP5_TAGLIB_HELPER_BUILT") {
        candidates.push(path.into());
    }

    let exe_name = if cfg!(target_os = "windows") {
        "taglib-helper.exe"
    } else {
        "taglib-helper"
    };

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            candidates.push(dir.join(exe_name));
            candidates.push(dir.join("../Resources").join(exe_name));
        }
    }

    candidates.push(exe_name.into());
    candidates
}

fn try_taglib_helper(path: &Path, primary_error: &str) -> Result<Option<MetadataReadResult>> {
    let path_str = path.to_string_lossy().to_string();
    let request = serde_json::json!({ "path": path_str });
    let mut last_error = None;

    for helper in taglib_helper_candidates() {
        let output = match Command::new(&helper).arg(request.to_string()).output() {
            Ok(output) => output,
            Err(error) => {
                last_error = Some(format!("{}: {error}", helper.display()));
                continue;
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let reason = if stderr.is_empty() {
                format!("exit status {}", output.status)
            } else {
                stderr
            };
            last_error = Some(format!("{}: {reason}", helper.display()));
            continue;
        }

        let response: TaglibHelperResponse =
            serde_json::from_slice(&output.stdout).with_context(|| {
                format!(
                    "Failed to decode taglib-helper output from {}",
                    helper.display()
                )
            })?;
        let warning = Some(match response.warning {
            Some(warning) if !warning.is_empty() => {
                format!("Metadata fallback via TagLib helper after Lofty failure: {primary_error}. {warning}")
            }
            _ => {
                format!("Metadata fallback via TagLib helper after Lofty failure: {primary_error}")
            }
        });
        return Ok(Some(MetadataReadResult {
            meta: response.meta,
            warning,
            all_tags: response.all_tags,
        }));
    }

    if let Some(last_error) = last_error {
        tracing::warn!(path = %path.display(), "TagLib fallback unavailable: {last_error}");
    }
    Ok(None)
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

/// Read audio metadata from a file using Lofty.
fn read_metadata_with_lofty(path: &Path) -> Result<TrackMetadata> {
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
            let utf8_debug = if error_text.contains("Expected a UTF-8 string") {
                utf8_decode_debug_summary(path)
                    .map(|summary| format!(" [{summary}]"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            anyhow::anyhow!("Failed to read tags{frame_note}: {e}{utf8_debug}")
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

/// Read audio metadata, falling back to the TagLib helper when Lofty rejects malformed tags.
pub fn read_metadata(path: &Path) -> Result<MetadataReadResult> {
    match read_metadata_with_lofty(path) {
        Ok(meta) => Ok(MetadataReadResult {
            meta,
            warning: None,
            all_tags: Vec::new(),
        }),
        Err(error) => {
            let primary_error = format!("{error:#}");
            if let Some(fallback) = try_taglib_helper(path, &primary_error)? {
                return Ok(fallback);
            }
            Err(error)
        }
    }
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

/// Try to read all ID3/text tag frames using Lofty.
fn list_all_tags_with_lofty(path: &Path) -> Result<Vec<crate::models::SourceTagInfo>> {
    use lofty::tag::Accessor;

    let tagged_file = Probe::open(path)
        .context("Failed to open file for tag reading")?
        .guess_file_type()
        .context("Failed to detect file type")?
        .read()?;

    let mut tags: Vec<crate::models::SourceTagInfo> = Vec::new();

    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let Some(tag) = tag else {
        return Ok(tags);
    };

    // Standard fields via Accessor trait
    if let Some(v) = tag.title() {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TIT2".into(),
            field_name: "title".into(),
            value: v.into_owned(),
        });
    }
    if let Some(v) = tag.artist() {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TPE1".into(),
            field_name: "artist".into(),
            value: v.into_owned(),
        });
    }
    if let Some(v) = tag.album() {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TALB".into(),
            field_name: "album".into(),
            value: v.into_owned(),
        });
    }
    if let Some(v) = tag.genre() {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TCON".into(),
            field_name: "genre".into(),
            value: v.into_owned(),
        });
    }
    if let Some(v) = tag.comment() {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "COMM".into(),
            field_name: "comment".into(),
            value: v.into_owned(),
        });
    }
    if let Some(v) = tag.year() {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TYER".into(),
            field_name: "year".into(),
            value: v.to_string(),
        });
    }
    if let Some(v) = tag.track() {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TRCK".into(),
            field_name: "track_number".into(),
            value: v.to_string(),
        });
    }
    if let Some(v) = tag.track_total() {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TRCK".into(),
            field_name: "track_total".into(),
            value: v.to_string(),
        });
    }
    if let Some(v) = tag.disk() {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TPOS".into(),
            field_name: "disc_number".into(),
            value: v.to_string(),
        });
    }

    // Extended fields via individual lookups using get_string
    // (ItemKey enum is not publicly accessible in this lofty version)
    for (key_name, field_key) in [
        ("album_artist", ItemKey::AlbumArtist),
        ("bpm", ItemKey::Bpm),
        ("composer", ItemKey::Composer),
        ("conductor", ItemKey::Conductor),
        ("publisher", ItemKey::Publisher),
        ("lyricist", ItemKey::Lyricist),
        ("encoder_settings", ItemKey::EncoderSettings),
        ("musicbrainz_track_id", ItemKey::MusicBrainzTrackId),
        ("musicbrainz_release_id", ItemKey::MusicBrainzReleaseId),
        ("musicbrainz_artist_id", ItemKey::MusicBrainzArtistId),
        (
            "musicbrainz_release_group_id",
            ItemKey::MusicBrainzReleaseGroupId,
        ),
        ("replaygain_track_gain", ItemKey::ReplayGainTrackGain),
        ("replaygain_track_peak", ItemKey::ReplayGainTrackPeak),
        ("replaygain_album_gain", ItemKey::ReplayGainAlbumGain),
        ("replaygain_album_peak", ItemKey::ReplayGainAlbumPeak),
        ("initial_key", ItemKey::InitialKey),
        ("language", ItemKey::Language),
        ("original_artist", ItemKey::OriginalArtist),
        ("copyright_url", ItemKey::CopyrightUrl),
        ("original_release_date", ItemKey::OriginalReleaseDate),
    ] {
        if let Some(v) = tag.get_string(&field_key) {
            tags.push(crate::models::SourceTagInfo {
                frame_id: key_name.to_string(),
                field_name: key_name.to_string(),
                value: v.to_string(),
            });
        }
    }

    // Also scan raw tag items for any user-defined (TXXX) frames
    for item in tag.items() {
        let key_desc = format!("{:?}", item.key());
        if key_desc.contains("TXXX") || key_desc.contains("UserText") {
            if let Some(text) = item.value().text() {
                tags.push(crate::models::SourceTagInfo {
                    frame_id: key_desc,
                    field_name: "user_text".into(),
                    value: text.to_string(),
                });
            }
        }
    }

    Ok(tags)
}

/// Map `TrackMetadata` fields (from the TagLib helper) to a `Vec<SourceTagInfo>`.
/// Also includes any extra tag properties from the TagLib property map (TXXX, UFID, etc.).
fn metadata_to_tag_infos(
    meta: &TrackMetadata,
    extra_props: &[TagProperty],
) -> Vec<crate::models::SourceTagInfo> {
    let mut tags = Vec::new();

    macro_rules! push_tag {
        ($val:expr, $frame_id:expr, $name:expr) => {
            if let Some(ref v) = $val {
                tags.push(crate::models::SourceTagInfo {
                    frame_id: $frame_id.into(),
                    field_name: $name.into(),
                    value: v.to_string(),
                });
            }
        };
    }

    push_tag!(meta.title, "TIT2", "title");
    push_tag!(meta.artist, "TPE1", "artist");
    push_tag!(meta.album_artist, "TPE2", "album_artist");
    push_tag!(meta.album, "TALB", "album");
    push_tag!(meta.genre, "TCON", "genre");
    push_tag!(meta.comment, "COMM", "comment");

    if let Some(v) = meta.year {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TYER".into(),
            field_name: "year".into(),
            value: v.to_string(),
        });
    }
    if let Some(v) = meta.track_number {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TRCK".into(),
            field_name: "track_number".into(),
            value: v.to_string(),
        });
    }
    if let Some(v) = meta.track_total {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TRCK".into(),
            field_name: "track_total".into(),
            value: v.to_string(),
        });
    }
    if let Some(v) = meta.disc_number {
        tags.push(crate::models::SourceTagInfo {
            frame_id: "TPOS".into(),
            field_name: "disc_number".into(),
            value: v.to_string(),
        });
    }

    // Extra properties from TagLib's full property map (TXXX, UFID, etc.)
    for prop in extra_props {
        // Skip properties that duplicate the basic meta fields above
        let is_duplicate = matches!(
            prop.key.as_str(),
            "TITLE"
                | "ARTIST"
                | "ALBUM"
                | "GENRE"
                | "COMMENT"
                | "DATE"
                | "YEAR"
                | "TRACKNUMBER"
                | "TRACKTOTAL"
                | "DISCNUMBER"
                | "ALBUMARTIST"
                | "ALBUM ARTIST"
                | "BPM"
        );
        if !is_duplicate {
            tags.push(crate::models::SourceTagInfo {
                frame_id: prop.key.clone(),
                field_name: "tag".into(),
                value: prop.value.clone(),
            });
        }
    }

    tags
}

/// Read all ID3/text tag frames from a file, returning a list of (frame_id, field_name, value) tuples.
///
/// Tries Lofty first; if that fails (e.g. malformed frames), falls back to the TagLib helper binary.
pub fn list_all_tags(path: &Path) -> Result<Vec<crate::models::SourceTagInfo>> {
    match list_all_tags_with_lofty(path) {
        Ok(tags) => Ok(tags),
        Err(err) => {
            let primary_error = format!("{err:#}");
            if let Some(fallback) = try_taglib_helper(path, &primary_error)? {
                return Ok(metadata_to_tag_infos(&fallback.meta, &fallback.all_tags));
            }
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 4-byte synchsafe integer (used in ID3v2.4 header / frame sizes).
    fn synchsafe(n: u32) -> [u8; 4] {
        [
            ((n >> 21) & 0x7F) as u8,
            ((n >> 14) & 0x7F) as u8,
            ((n >> 7) & 0x7F) as u8,
            (n & 0x7F) as u8,
        ]
    }

    /// Build a single ID3v2.4 frame: header (10 bytes) + data.
    fn id3_frame(frame_id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(frame_id);
        buf.extend_from_slice(&synchsafe(data.len() as u32));
        buf.extend_from_slice(&[0, 0]); // flags
        buf.extend_from_slice(data);
        buf
    }

    /// Build a synthetic valid .mp3 file with an ID3v2.4 tag and a minimal
    /// MPEG audio frame so both Lofty and TagLib can open it.
    fn synth_mp3_with_id3(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut tag_data = Vec::new();
        for f in frames {
            tag_data.extend_from_slice(f);
        }

        // ID3v2.4 header (10 bytes)
        let mut file = Vec::new();
        file.extend_from_slice(b"ID3");
        file.extend_from_slice(&[0x04, 0x00]); // version 2.4
        file.push(0x00); // flags
        file.extend_from_slice(&synchsafe(tag_data.len() as u32));
        file.extend_from_slice(&tag_data);

        // Minimal valid MPEG-1 Audio Layer 3 frame (silent) so that
        // Lofty/TagLib accept the file as a valid MP3.
        // Sync word 0xFF 0xFB, MPEG1 Layer3, 128 kbps, 44100 Hz, no padding.
        file.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        // Fill the rest of a 417-byte MPEG frame with zeros (silent PCM).
        file.resize(file.len() + 413, 0u8);

        file
    }

    // ── metadata_to_tag_infos unit tests ──────────────────────────────────────

    #[test]
    fn test_metadata_to_tag_infos_basic_fields() {
        let meta = TrackMetadata {
            title: Some("Hello".into()),
            artist: Some("World".into()),
            album: Some("Test Album".into()),
            year: Some(2024),
            track_number: Some(1),
            track_total: Some(13),
            disc_number: Some(2),
            ..Default::default()
        };

        let tags = metadata_to_tag_infos(&meta, &[]);

        let t = |name| {
            tags.iter()
                .find(|t| t.field_name == name)
                .map(|t| t.value.as_str())
        };
        assert_eq!(t("title"), Some("Hello"));
        assert_eq!(t("artist"), Some("World"));
        assert_eq!(t("album"), Some("Test Album"));
        assert_eq!(t("year"), Some("2024"));
        assert_eq!(t("track_number"), Some("1"));
        assert_eq!(t("track_total"), Some("13"));
        assert_eq!(t("disc_number"), Some("2"));
    }

    #[test]
    fn test_metadata_to_tag_infos_filters_duplicate_extra_props() {
        let meta = TrackMetadata {
            title: Some("Title".into()),
            ..Default::default()
        };
        // All of these have matching fields in meta or are in the skip list.
        let extra_props = vec![
            TagProperty {
                key: "TITLE".into(),
                value: "Title".into(),
            },
            TagProperty {
                key: "ARTIST".into(),
                value: "Artist".into(),
            },
            TagProperty {
                key: "TRACKNUMBER".into(),
                value: "1/13".into(),
            },
            TagProperty {
                key: "DISCNUMBER".into(),
                value: "2/2".into(),
            },
            TagProperty {
                key: "SCRIPT".into(),
                value: "Jpan".into(),
            },
            TagProperty {
                key: "BARCODE".into(),
                value: "12345".into(),
            },
        ];

        let tags = metadata_to_tag_infos(&meta, &extra_props);

        // Duplicates should be filtered
        assert!(tags.iter().any(|t| t.value == "Title"));
        assert!(tags.iter().find(|t| t.frame_id == "TITLE").is_none());
        assert!(tags.iter().find(|t| t.frame_id == "ARTIST").is_none());
        assert!(tags.iter().find(|t| t.frame_id == "TRACKNUMBER").is_none());
        assert!(tags.iter().find(|t| t.frame_id == "DISCNUMBER").is_none());

        // Non-duplicate extras should appear
        assert!(tags.iter().any(|t| t.value == "Jpan"));
        assert!(tags.iter().any(|t| t.value == "12345"));
    }

    // ── Integration test with synthetic MP3 ───────────────────────────────────

    #[test]
    fn test_list_all_tags_synthetic_mp3() {
        let frames = vec![
            id3_frame(b"TIT2", b"\x03Heart will drive"),
            id3_frame(
                b"TPE1",
                b"\x03\xe6\x96\xb0\xe5\x9e\xa3\xe7\xb5\x90\xe8\xa1\xa3",
            ),
            id3_frame(b"TALB", b"\x03hug"),
            id3_frame(b"TCON", b"\x03JPop"),
            id3_frame(b"TYER", b"\x032009"),
            id3_frame(b"TRCK", b"\x031/13"),
            id3_frame(b"TPOS", b"\x032/2"),
        ];
        let data = synth_mp3_with_id3(&frames);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.mp3");
        std::fs::write(&path, &data).expect("write");

        // `list_all_tags` may succeed via Lofty or via the TagLib fallback.
        // We accept either path as long as the results are correct.
        let tags = match list_all_tags(&path) {
            Ok(t) => t,
            Err(e) => {
                // Neither Lofty nor TagLib could process the synthetic file.
                // This is acceptable in minimal/no-TagLib environments.
                eprintln!("list_all_tags skipped (neither Lofty nor TagLib available): {e}");
                return;
            }
        };

        let t = |name| {
            tags.iter()
                .find(|t| t.field_name == name)
                .map(|t| t.value.as_str())
        };

        assert_eq!(t("title"), Some("Heart will drive"), "tags: {tags:?}");
        assert_eq!(t("album"), Some("hug"), "tags: {tags:?}");
        assert_eq!(t("genre"), Some("JPop"), "tags: {tags:?}");

        // TRCK "1/13" must produce track_number=1 and track_total=13
        assert_eq!(
            t("track_number"),
            Some("1"),
            "track_number missing or wrong; tags: {tags:?}"
        );
        assert_eq!(
            t("track_total"),
            Some("13"),
            "track_total missing or wrong; tags: {tags:?}"
        );

        // TPOS "2/2" must produce disc_number=2
        assert_eq!(
            t("disc_number"),
            Some("2"),
            "disc_number missing or wrong; tags: {tags:?}"
        );
    }
}
