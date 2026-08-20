use anyhow::{ensure, Result};

/// Default padding size in bytes to append after the last frame.
const DEFAULT_PADDING: usize = 2048;

/// Encode a 32-bit value as a 4-byte synchsafe integer (each byte uses only 7 bits).
pub fn u32_to_synchsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

/// Decode a 4-byte synchsafe integer.
fn synchsafe_to_u32(b: &[u8]) -> u32 {
    ((b[0] as u32) << 21) | ((b[1] as u32) << 14) | ((b[2] as u32) << 7) | (b[3] as u32)
}

/// Write the 10-byte ID3v2 tag header.
///
/// The `tag_size` is the combined size of all frames + padding (everything after
/// the 10-byte header). The tag size is synchsafe for every ID3v2 version (only
/// *frame* sizes differ — big-endian in v2.3, synchsafe in v2.4).
fn serialize_header(version: u8, tag_size: u32) -> [u8; 10] {
    let mut header = [0u8; 10];
    header[..3].copy_from_slice(b"ID3");
    header[3] = version;
    header[4] = 0; // revision
    header[5] = 0; // flags (none)
    header[6..10].copy_from_slice(&u32_to_synchsafe(tag_size));
    header
}

/// Encode a string value as an ID3v2 text frame payload using UTF-8 encoding.
///
/// Format: `[encoding_byte = 0x03][UTF-8 encoded text]`
fn encode_text_value(value: &str) -> Vec<u8> {
    let mut payload = vec![0x03u8]; // UTF-8
    payload.extend_from_slice(value.as_bytes());
    payload
}

/// Serialize a single ID3v2 text frame to bytes.
///
/// For standard text frames (starts with 'T', not TXXX): frame_id + size + flags + payload.
/// For comment frames (COMM): frame_id + size + flags + payload (with language bytes).
///
/// `frame_id` is a 4-byte ASCII string (e.g. "TIT2", "TPE1", "COMM").
/// `version` determines the frame size encoding: synchsafe for v2.4+, big-endian for v2.3.
fn serialize_frame(
    frame_id: &str,
    payload: &[u8],
    version: u8,
    frame_sizes_synchsafe: bool,
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(10 + payload.len());
    frame.extend_from_slice(frame_id.as_bytes());

    let size = payload.len() as u32;
    if frame_sizes_synchsafe || version >= 4 {
        frame.extend_from_slice(&u32_to_synchsafe(size));
    } else {
        frame.extend_from_slice(&size.to_be_bytes());
    }

    frame.extend_from_slice(&[0, 0]); // flags
    frame.extend_from_slice(payload);
    frame
}

/// Serialize the payload of a described frame — TXXX or COMM.
///
/// Format: `[encoding_byte][language][description]["\0"][value]`, where
/// `language` is the 3-byte ISO-639-2 code COMM carries and is empty for TXXX.
/// Descriptions round-trip, but — as with the text encoding — the language is
/// normalized rather than preserved: every comment we write is tagged "eng".
fn serialize_described_payload(language: &[u8], description: &str, value: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2 + language.len() + description.len() + value.len());
    payload.push(0x03); // UTF-8
    payload.extend_from_slice(language);
    payload.extend_from_slice(description.as_bytes());
    payload.push(0x00); // content-descriptor terminator
    payload.extend_from_slice(value.as_bytes());
    payload
}

/// Serialize a complete ID3v2 tag (header + frames + padding) to bytes.
///
/// `frames` is a list of `(frame_id, value)` tuples for text frames to include.
/// For TXXX frames, frame_id should be "TXXX:description" (e.g. "TXXX:MusicBrainz Artist Id").
/// `preserved` is raw bytes of non-text frames (APIC, UFID, etc.) to pass
/// through verbatim (must include the 10-byte frame header for each).
/// `version` is the ID3v2 major version (2, 3, or 4) to target.
///
/// Returns the complete tag bytes, ready to be written before the audio data.
pub fn serialize_tag(
    frames: &[(String, String)],
    preserved: &[Vec<u8>],
    version: u8,
) -> Result<Vec<u8>> {
    ensure!(
        version >= 2 && version <= 4,
        "Unsupported ID3v2 version: {version}",
    );

    // Frame sizes: synchsafe for v2.4+, big-endian for v2.3 and earlier.
    let frame_sizes_synchsafe = version >= 4;

    let mut tag_body = Vec::new();

    for (frame_id, value) in frames {
        if !super::is_text_frame(frame_id) {
            continue;
        }

        let (base_id, description) = super::split_frame_key(frame_id);
        let payload = match base_id {
            "COMM" => serialize_described_payload(b"eng", description, value),
            "TXXX" => serialize_described_payload(b"", description, value),
            _ => encode_text_value(value),
        };

        let frame_bytes = serialize_frame(base_id, &payload, version, frame_sizes_synchsafe);
        tag_body.extend_from_slice(&frame_bytes);
    }

    // Append preserved non-text frames verbatim.
    for frame_bytes in preserved {
        tag_body.extend_from_slice(frame_bytes);
    }

    // Add padding
    let padding = vec![0u8; DEFAULT_PADDING];
    tag_body.extend_from_slice(&padding);

    // Build header
    let tag_size = tag_body.len() as u32;
    let header = serialize_header(version, tag_size);

    let mut result = Vec::with_capacity(10 + tag_body.len());
    result.extend_from_slice(&header);
    result.extend_from_slice(&tag_body);
    Ok(result)
}

/// Serialize only the tag header bytes (for when we need to update just the header).
pub fn serialize_header_only(version: u8, tag_size: u32) -> [u8; 10] {
    serialize_header(version, tag_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_synchsafe_roundtrip() {
        let values = [0u32, 1, 127, 128, 16383, 16384, 2097151, 2097152, 268435455];
        for &v in &values {
            let encoded = u32_to_synchsafe(v);
            let decoded = synchsafe_to_u32(&encoded);
            assert_eq!(v, decoded, "synchsafe roundtrip failed for {v}");
        }
    }

    #[test]
    fn test_serialize_header_v4() {
        let header = serialize_header(4, 1000);
        assert_eq!(&header[..3], b"ID3");
        assert_eq!(header[3], 4);
        assert_eq!(header[4], 0);
        assert_eq!(header[5], 0);
        // 1000 in synchsafe
        assert_eq!(header[6..10], u32_to_synchsafe(1000));
    }

    #[test]
    fn test_serialize_header_v3() {
        let header = serialize_header(3, 1000);
        assert_eq!(&header[..3], b"ID3");
        assert_eq!(header[3], 3);
        // The v2.3 tag size is synchsafe (only frame sizes are big-endian in
        // v2.3), so a size >= 128 must not be written as plain big-endian.
        assert_eq!(header[6..10], u32_to_synchsafe(1000));
    }

    #[test]
    fn test_serialize_frame_v4() {
        let payload = b"\x03Hello";
        let bytes = serialize_frame("TIT2", payload, 4, true);
        assert_eq!(&bytes[..4], b"TIT2");
        // v2.4: frame size is synchsafe
        assert_eq!(bytes[4..8], u32_to_synchsafe(payload.len() as u32));
        assert_eq!(bytes[8..10], [0, 0]);
        assert_eq!(&bytes[10..], payload);
    }

    #[test]
    fn test_serialize_frame_v3() {
        let payload = b"\x03Hello";
        let bytes = serialize_frame("TIT2", payload, 3, false);
        assert_eq!(&bytes[..4], b"TIT2");
        // v2.3: frame size is big-endian
        assert_eq!(bytes[4..8], (payload.len() as u32).to_be_bytes());
        assert_eq!(bytes[8..10], [0, 0]);
        assert_eq!(&bytes[10..], payload);
    }

    #[test]
    fn test_encode_text_value_is_utf8() {
        let value = "Café & Résumé — 中文";
        let encoded = encode_text_value(value);
        assert_eq!(encoded[0], 0x03); // UTF-8 encoding
        let decoded = std::str::from_utf8(&encoded[1..]).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn test_serialize_tag_v4() {
        let frames = vec![
            ("TIT2".to_string(), "Test Song".to_string()),
            ("TPE1".to_string(), "Test Artist".to_string()),
        ];
        let tag_bytes = serialize_tag(&frames, &[], 4).unwrap();
        // Header
        assert_eq!(&tag_bytes[..3], b"ID3");
        assert_eq!(tag_bytes[3], 4);

        // Parse back and verify frames
        let tag_size = synchsafe_to_u32(&tag_bytes[6..10]) as usize;
        assert!(tag_size > 0);
        assert!(tag_size <= tag_bytes.len() - 10);

        // First frame: TIT2
        assert_eq!(&tag_bytes[10..14], b"TIT2");
        let tit2_size = synchsafe_to_u32(&tag_bytes[14..18]) as usize;
        let tit2_frame_header = 10; // frame ID(4) + size(4) + flags(2)
        let tit2_payload_start = 10 + tit2_frame_header; // after ID3 header
        assert_eq!(
            &tag_bytes[tit2_payload_start..tit2_payload_start + tit2_size],
            b"\x03Test Song"
        );

        assert!(tag_bytes.windows(4).any(|w| w == b"TPE1"));
        assert!(tag_bytes.windows(12).any(|w| w == b"\x03Test Artist"));
    }

    #[test]
    fn test_serialize_tag_v3() {
        let frames = vec![("TIT2".to_string(), "Song".to_string())];
        let tag_bytes = serialize_tag(&frames, &[], 3).unwrap();

        assert_eq!(tag_bytes[3], 3);

        // In v2.3, frame sizes are big-endian
        let payload = b"\x03Song";
        assert_eq!(&tag_bytes[10..14], b"TIT2");
        let size = u32::from_be_bytes([tag_bytes[14], tag_bytes[15], tag_bytes[16], tag_bytes[17]])
            as usize;
        assert_eq!(size, payload.len());
    }

    #[test]
    fn test_serialize_tag_with_comment() {
        let frames = vec![("COMM".to_string(), "A comment".to_string())];
        let tag_bytes = serialize_tag(&frames, &[], 4).unwrap();

        // Frame ID
        assert!(tag_bytes.windows(4).any(|w| w == b"COMM"));
        // The comment payload should contain "eng\0comment"
        let payload = b"\x03eng\x00A comment";
        assert!(tag_bytes.windows(payload.len()).any(|w| w == payload));
    }

    #[test]
    fn test_serialize_tag_with_described_comment() {
        let frames = vec![("COMM:ID3v1 Comment".to_string(), "A comment".to_string())];
        let tag_bytes = serialize_tag(&frames, &[], 4).unwrap();

        assert!(tag_bytes.windows(4).any(|w| w == b"COMM"));
        // Language, then the description, then the terminator, then the text.
        let payload = b"\x03engID3v1 Comment\x00A comment";
        assert!(tag_bytes.windows(payload.len()).any(|w| w == payload));
    }

    #[test]
    fn test_serialize_tag_rejects_bad_version() {
        let frames = vec![];
        assert!(serialize_tag(&frames, &[], 1).is_err());
        assert!(serialize_tag(&frames, &[], 5).is_err());
    }

    #[test]
    fn test_serialize_tag_empty() {
        // An empty tag (no frames, just padding) is valid
        let tag_bytes = serialize_tag(&[], &[], 4).unwrap();
        assert_eq!(&tag_bytes[..3], b"ID3");
        let tag_size = synchsafe_to_u32(&tag_bytes[6..10]) as usize;
        // Should be just padding (DEFAULT_PADDING bytes)
        assert_eq!(tag_size, DEFAULT_PADDING);
    }

    #[test]
    fn test_serialize_tag_includes_padding() {
        let frames = vec![("TIT2".to_string(), "X".to_string())];
        let tag_bytes = serialize_tag(&frames, &[], 4).unwrap();
        let tag_size = synchsafe_to_u32(&tag_bytes[6..10]) as usize;

        // Frames take 10 (header) + 6 (payload) + 2048 (padding) = 2064
        // But "X" is 1 byte, payload is 2 (encoding + X), so frame size = 2
        // Frame bytes: 10 + 2 = 12 bytes for TIT2
        // Plus 2048 padding = 2060
        let expected_frame_body = 12;
        assert_eq!(tag_size, expected_frame_body + DEFAULT_PADDING);

        // Check that the padding area is all zeros
        let padding_start = 10 + expected_frame_body;
        let padding = &tag_bytes[padding_start..padding_start + DEFAULT_PADDING];
        assert!(padding.iter().all(|&b| b == 0));
    }
}
