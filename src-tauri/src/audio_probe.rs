use anyhow::{Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, Track, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Duration;

pub(crate) fn probe_media_source<M>(
    path: &Path,
    media_source: M,
    force_extension: Option<&str>,
) -> Result<Box<dyn FormatReader>>
where
    M: MediaSource + 'static,
{
    let media_source = MediaSourceStream::new(Box::new(media_source), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) =
        force_extension.or_else(|| path.extension().and_then(|ext| ext.to_str()))
    {
        hint.with_extension(extension);
    }

    symphonia::default::get_probe()
        .probe(
            &hint,
            media_source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("Failed to probe audio format")
}

/// The default audio track of `format` together with its audio codec parameters.
///
/// `None` if the container has no audio track or the track's codec parameters are unknown.
pub(crate) fn default_audio_track(
    format: &dyn FormatReader,
) -> Option<(&Track, &AudioCodecParameters)> {
    let track = format.default_track(TrackType::Audio)?;
    match track.codec_params.as_ref()? {
        CodecParameters::Audio(params) => Some((track, params)),
        _ => None,
    }
}

/// Create a decoder for `params` that emits every frame the container holds (no gapless
/// trimming of encoder delay/padding).
///
/// Symphonia 0.6's AAC decoder rejects HE-AAC streams whose AudioSpecificConfig explicitly
/// signals SBR/PS, where 0.5 decoded just the AAC-LC core. For those streams, retry with the
/// signalling stripped so the core is decoded as before.
pub(crate) fn make_audio_decoder(
    params: &AudioCodecParameters,
) -> symphonia::core::errors::Result<Box<dyn AudioDecoder>> {
    let codecs = symphonia::default::get_codecs();
    let opts = AudioDecoderOptions::default().gapless(false);
    codecs
        .make_audio_decoder(params, &opts)
        .or_else(|err| match aac_core_params(params) {
            Some(core) => codecs.make_audio_decoder(&core, &opts),
            None => Err(err),
        })
}

/// `params` with an explicit SBR/PS AudioSpecificConfig (hierarchical signalling: object type 5
/// or 29 wrapping AAC-LC) rewritten to the plain AAC-LC configuration of the core stream.
fn aac_core_params(params: &AudioCodecParameters) -> Option<AudioCodecParameters> {
    if params.codec != CODEC_ID_AAC {
        return None;
    }
    let core = aac_core_config(params.extra_data.as_deref()?)?;
    let mut core_params = params.clone();
    core_params.with_extra_data(core.into());
    Some(core_params)
}

/// Rewrite `AOT(5|29) rate chan extRate AOT(2) GASpecificConfig` as `AOT(2) rate chan
/// GASpecificConfig` (ISO/IEC 14496-3 1.6.2.1). `None` for any other layout.
fn aac_core_config(asc: &[u8]) -> Option<Vec<u8>> {
    const AOT_LC: u32 = 2;
    const AOT_SBR: u32 = 5;
    const AOT_PS: u32 = 29;
    const RATE_ESCAPE: u32 = 15;

    // Reads `n` (<= 32) bits starting at bit `pos`.
    let bits = |pos: usize, n: usize| -> Option<u32> {
        (pos + n <= asc.len() * 8).then(|| {
            (pos..pos + n).fold(0u32, |acc, i| {
                (acc << 1) | u32::from(asc[i / 8] >> (7 - i % 8) & 1)
            })
        })
    };
    // Width of a sampling-frequency field: an index, or an index escape plus explicit rate.
    let rate_len =
        |pos: usize| -> Option<usize> { Some(if bits(pos, 4)? == RATE_ESCAPE { 28 } else { 4 }) };

    let aot = bits(0, 5)?;
    if aot != AOT_SBR && aot != AOT_PS {
        return None;
    }
    let rate_pos = 5;
    let rate_len_core = rate_len(rate_pos)?;
    let chan_pos = rate_pos + rate_len_core;
    let ext_rate_pos = chan_pos + 4;
    let aot2_pos = ext_rate_pos + rate_len(ext_rate_pos)?;
    if bits(aot2_pos, 5)? != AOT_LC {
        return None;
    }
    // GASpecificConfig: frameLengthFlag, dependsOnCoreCoder, extensionFlag. Only the plain
    // form (no core-coder delay, no extension) is rewritten.
    let ga_pos = aot2_pos + 5;
    let ga = bits(ga_pos, 3)?;
    if ga & 0b011 != 0 {
        return None;
    }

    let fields = [
        (AOT_LC, 5),
        (bits(rate_pos, rate_len_core)?, rate_len_core),
        (bits(chan_pos, 4)?, 4),
        (ga, 3),
    ];
    let bit_stream: Vec<bool> = fields
        .iter()
        .flat_map(|&(value, len)| (0..len).rev().map(move |i| value >> i & 1 == 1))
        .collect();
    Some(
        bit_stream
            .chunks(8)
            .map(|byte| {
                (0..8).fold(0u8, |acc, i| {
                    (acc << 1) | u8::from(byte.get(i).copied().unwrap_or(false))
                })
            })
            .collect(),
    )
}

pub(crate) fn open_wave_mp3_payload(path: &Path) -> Result<Option<FileSegment>> {
    let mut file = File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    let Some((data_offset, data_len)) = wave_mp3_data_range(&mut file)? else {
        return Ok(None);
    };
    Ok(Some(FileSegment::new(file, data_offset, data_len)?))
}

pub(crate) struct FileSegment {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
}

impl FileSegment {
    fn new(mut file: File, start: u64, len: u64) -> Result<Self> {
        file.seek(SeekFrom::Start(start))?;
        Ok(Self {
            file,
            start,
            len,
            pos: 0,
        })
    }
}

impl Read for FileSegment {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.len.saturating_sub(self.pos) as usize;
        if remaining == 0 {
            return Ok(0);
        }
        let to_read = remaining.min(buf.len());
        let read = self.file.read(&mut buf[..to_read])?;
        self.pos = self.pos.saturating_add(read as u64);
        Ok(read)
    }
}

impl Seek for FileSegment {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let next = match pos {
            SeekFrom::Start(offset) => offset as i128,
            SeekFrom::End(offset) => i128::from(self.len) + i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.pos) + i128::from(offset),
        };
        if next < 0 || next > i128::from(self.len) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek out of bounds",
            ));
        }
        let next = next as u64;
        self.file.seek(SeekFrom::Start(self.start + next))?;
        self.pos = next;
        Ok(self.pos)
    }
}

impl MediaSource for FileSegment {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

pub(crate) fn id3v2_end_offset(file: &mut File) -> Option<u64> {
    let mut header = [0u8; 10];
    file.read_exact(&mut header).ok()?;
    if &header[0..3] != b"ID3" {
        return None;
    }
    let size = ((header[6] as u64) << 21)
        | ((header[7] as u64) << 14)
        | ((header[8] as u64) << 7)
        | (header[9] as u64);
    Some(10 + size)
}

fn wave_mp3_data_range(file: &mut File) -> Result<Option<(u64, u64)>> {
    // Some files have multiple consecutive ID3v2 tags before the RIFF/WAVE
    // header (e.g. ID3v2.3 followed by ID3v2.4).  Skip all of them.
    let mut riff_offset = id3v2_end_offset(file).unwrap_or_default();
    loop {
        file.seek(SeekFrom::Start(riff_offset))?;
        let mut tag_header = [0u8; 10];
        if file.read_exact(&mut tag_header).is_err() || &tag_header[0..3] != b"ID3" {
            break;
        }
        let size = ((tag_header[6] as u64) << 21)
            | ((tag_header[7] as u64) << 14)
            | ((tag_header[8] as u64) << 7)
            | (tag_header[9] as u64);
        riff_offset += 10 + size;
    }

    file.seek(SeekFrom::Start(riff_offset))?;

    let mut header = [0u8; 12];
    if file.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return Ok(None);
    }

    let mut format_tag = None;
    let mut data_range = None;
    loop {
        let mut chunk_header = [0u8; 8];
        if file.read_exact(&mut chunk_header).is_err() {
            break;
        }

        let chunk_size =
            u32::from_le_bytes(chunk_header[4..8].try_into().expect("chunk size slice")) as u64;
        let chunk_data_offset = file.stream_position()?;

        match &chunk_header[0..4] {
            b"fmt " if chunk_size >= 2 => {
                let mut tag = [0u8; 2];
                file.read_exact(&mut tag)?;
                format_tag = Some(u16::from_le_bytes(tag));
            }
            b"data" => {
                data_range = Some((chunk_data_offset, chunk_size));
            }
            _ => {}
        }

        let padded_size = chunk_size + (chunk_size % 2);
        file.seek(SeekFrom::Start(chunk_data_offset + padded_size))?;

        if format_tag.is_some() && data_range.is_some() {
            break;
        }
    }

    let is_mp3_wave = matches!(format_tag, Some(0x0050 | 0x0055));
    Ok(if is_mp3_wave { data_range } else { None })
}

/// Probe `path`, retrying past a malformed ID3v2 header or inside a RIFF/WAVE wrapper when the
/// plain probe fails.
pub(crate) fn probe_with_fallbacks(path: &Path) -> Result<Box<dyn FormatReader>> {
    let file = File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    let probed = match probe_media_source(path, file, None) {
        Ok(probed) => probed,
        Err(first_err) => {
            let msg = format!("{first_err:#}");
            let id3_issue = msg.contains("id3v2") || msg.contains("malformed");
            let retry = if id3_issue {
                File::open(path)
                    .ok()
                    .and_then(|mut f| {
                        let offset = id3v2_end_offset(&mut f)?;
                        use std::io::Seek;
                        f.seek(std::io::SeekFrom::Start(offset)).ok()?;
                        tracing::warn!(
                            path = %path.display(),
                            error = %first_err,
                            "Retrying probe after skipping malformed ID3v2 header"
                        );
                        Some(f)
                    })
                    .and_then(|f2| probe_media_source(path, f2, None).ok())
            } else {
                None
            };
            match retry {
                Some(result) => result,
                None => {
                    if let Some(segment) = open_wave_mp3_payload(path)? {
                        tracing::warn!(
                            path = %path.display(),
                            error = %first_err,
                            "Retrying probe by decoding MP3 payload from RIFF/WAVE wrapper"
                        );
                        probe_media_source(path, segment, Some("mp3"))?
                    } else {
                        return Err(first_err);
                    }
                }
            }
        }
    };
    Ok(probed)
}

/// Maximum disagreement between the header-derived and the measured duration that is still
/// treated as agreement (encoder delay/padding and rounding account for tens of milliseconds).
const DURATION_TOLERANCE_MS: u64 = 1_000;

/// Measure the real duration of the default audio track by summing the duration of every
/// packet actually present in the container, rather than trusting header fields (Xing/VBRI
/// frame counts, bitrate-based estimates) that can be wrong or missing.
///
/// Demuxes only — no audio is decoded — so it is cheap relative to a full decode.
pub(crate) fn measure_duration_ms(path: &Path) -> Result<u64> {
    let mut format = probe_with_fallbacks(path)?;
    let (track, _) =
        default_audio_track(format.as_ref()).context("No default audio track found")?;
    let track_id = track.id;
    let time_base = track.time_base.context("Track has no time base")?;

    let mut total = Duration::ZERO;
    loop {
        match format.next_packet() {
            // Every frame the decoder emits for the packet, including encoder delay/padding:
            // `dur` alone omits trimmed frames and some readers misreport trimmed mid-stream
            // frames as padding (e.g. Ogg files with irregular granule positions).
            Ok(Some(packet)) if packet.track_id == track_id => {
                total = total.saturating_add(packet.block_dur())
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            // Any other error means the sum is incomplete; report failure rather than a
            // partial duration so callers keep the header value.
            Err(e) => return Err(anyhow::Error::new(e).context("Failed while measuring duration")),
        }
    }

    let time = time_base
        .calc_duration(total)
        .context("Measured duration overflows")?;
    u64::try_from(time.as_millis()).context("Measured duration is negative")
}

/// Decide which duration to trust. Returns the measured value when it disagrees with the
/// header-derived one by more than [`DURATION_TOLERANCE_MS`], otherwise `None`.
pub(crate) fn duration_correction(
    header_ms: u64,
    measured_ms: u64,
) -> Option<crate::models::DurationCorrection> {
    (measured_ms > 0 && header_ms.abs_diff(measured_ms) > DURATION_TOLERANCE_MS).then_some(
        crate::models::DurationCorrection {
            header_ms,
            measured_ms,
        },
    )
}

#[cfg(test)]
mod duration_tests {
    use super::*;

    #[test]
    fn agreeing_durations_need_no_correction() {
        assert_eq!(duration_correction(1_398_230, 1_398_300), None);
    }

    #[test]
    fn disagreeing_durations_report_the_measured_value() {
        let c = duration_correction(2_291_905, 1_398_230).unwrap();
        assert_eq!((c.header_ms, c.measured_ms), (2_291_905, 1_398_230));
    }

    #[test]
    fn failed_measurement_never_corrects() {
        assert_eq!(duration_correction(1_000_000, 0), None);
    }
}

#[cfg(test)]
mod aac_core_tests {
    use super::*;

    #[test]
    fn explicit_sbr_config_is_reduced_to_the_aac_lc_core() {
        // AOT 5 (SBR), 22050 Hz, mono, extension rate 44100 Hz, AOT 2 (LC), plain GASpecificConfig.
        assert_eq!(
            aac_core_config(&[0x2B, 0x8A, 0x08, 0x00]),
            Some(vec![0x13, 0x88])
        );
    }

    #[test]
    fn explicit_ps_config_is_reduced_to_the_aac_lc_core() {
        // AOT 29 (PS), 22050 Hz, mono, extension rate 44100 Hz, AOT 2 (LC), plain GASpecificConfig.
        assert_eq!(
            aac_core_config(&[0xEB, 0x8A, 0x08, 0x00]),
            Some(vec![0x13, 0x88])
        );
    }

    #[test]
    fn plain_aac_lc_config_is_left_alone() {
        assert_eq!(aac_core_config(&[0x13, 0x88]), None);
    }

    #[test]
    fn truncated_config_is_rejected() {
        assert_eq!(aac_core_config(&[0x2B, 0x8A]), None);
    }
}
