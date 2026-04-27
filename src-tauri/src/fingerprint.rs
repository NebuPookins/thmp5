use crate::audio_probe::{open_wave_mp3_payload, probe_media_source};
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
#[cfg(feature = "opus")]
use opus::Decoder as OpusDecoder;
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use serde::Deserialize;
use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{DecoderOptions, CODEC_TYPE_OPUS},
    errors::Error as SymphoniaError,
};

/// Decode only the first 30 seconds of audio for the initial fingerprint pass.
const MAX_FINGERPRINT_MS: u64 = 30_000;

// ── Fingerprint generation ────────────────────────────────────────────────────

pub struct FingerprintResult {
    /// Chromaprint compressed + base64-encoded fingerprint (what AcoustID expects).
    pub fingerprint: String,
    /// Duration of the decoded segment used for fingerprinting.
    pub duration_ms: u64,
}

/// Decode audio from `path` and generate a Chromaprint fingerprint.
///
/// CPU-bound — call via `tokio::task::spawn_blocking` from async code.
pub fn generate_fingerprint(path: &Path) -> Result<FingerprintResult> {
    let config = Configuration::preset_test2();
    let (raw, duration_ms) = decode_chromaprint(path, &config)?;
    let compressed = FingerprintCompressor::from(&config).compress(&raw);
    Ok(FingerprintResult {
        fingerprint: BASE64.encode(&compressed),
        duration_ms,
    })
}

/// Return `true` if two audio files appear to be the same recording.
///
/// Compares raw Chromaprint fingerprints using bit error rate (BER).
/// A BER below 0.4 indicates the same song; above 0.4 indicates different content.
///
/// CPU-bound — call via `tokio::task::spawn_blocking` from async code.
pub fn same_recording(path_a: &Path, path_b: &Path) -> Result<bool> {
    let config = Configuration::preset_test2();
    let (a, _) = decode_chromaprint(path_a, &config)?;
    let (b, _) = decode_chromaprint(path_b, &config)?;
    Ok(fingerprint_ber(&a, &b) < 0.4)
}

// ── Public helpers for local comparison ──────────────────────────────────────

/// Generate raw Chromaprint fingerprint integers for `path`.
///
/// Use [`ber`] to compare two results.  CPU-bound — call via
/// `tokio::task::spawn_blocking` from async code.
pub fn raw_fingerprint(path: &Path) -> Result<Vec<u32>> {
    let config = Configuration::preset_test2();
    let (raw, _) = decode_chromaprint(path, &config)?;
    Ok(raw)
}

/// Bit error rate between two raw Chromaprint fingerprints.
/// Returns a value in `[0.0, 1.0]`; below ~0.4 means the same recording.
pub fn ber(a: &[u32], b: &[u32]) -> f32 {
    fingerprint_ber(a, b)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Decode up to `MAX_FINGERPRINT_MS` of audio and run the Chromaprint algorithm.
/// Returns the raw fingerprint integers and the duration of audio decoded.
fn decode_chromaprint(path: &Path, config: &Configuration) -> Result<(Vec<u32>, u64)> {
    let file = File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    let probed = match probe_media_source(path, file, None) {
        Ok(probed) => probed,
        Err(first_err) => {
            if let Some(segment) = open_wave_mp3_payload(path)? {
                tracing::warn!(
                    path = %path.display(),
                    error = %first_err,
                    "Retrying fingerprint probe by decoding MP3 payload from RIFF/WAVE wrapper"
                );
                probe_media_source(path, segment, Some("mp3"))?
            } else {
                return Err(first_err);
            }
        }
    };

    let mut format = probed.format;

    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("No default audio track found"))?;

    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let n_channels = codec_params.channels.map(|c| c.count() as u32).unwrap_or(2);

    // Symphonia has no Opus codec; use libopus directly for OGG/Opus files.
    #[cfg(feature = "opus")]
    if codec_params.codec == CODEC_TYPE_OPUS {
        let opus_channels = if n_channels == 1 {
            opus::Channels::Mono
        } else {
            opus::Channels::Stereo
        };
        let mut opus_dec = OpusDecoder::new(48_000, opus_channels)
            .map_err(|e| anyhow!("Failed to create Opus decoder: {e}"))?;
        const OPUS_RATE: u32 = 48_000;
        let mut fingerprinter = Fingerprinter::new(config);
        fingerprinter
            .start(OPUS_RATE, n_channels)
            .map_err(|e| anyhow!("Chromaprint start: {e:?}"))?;
        let max_samples = MAX_FINGERPRINT_MS * OPUS_RATE as u64 * n_channels as u64 / 1000;
        let mut total_samples: u64 = 0;
        let mut f32_buf = vec![0.0f32; 5760 * n_channels as usize];
        loop {
            if total_samples >= max_samples {
                break;
            }
            let packet = match format.next_packet() {
                Ok(p) => p,
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(e) => {
                    tracing::debug!("Packet read stopped during fingerprinting: {e}");
                    break;
                }
            };
            if packet.track_id() != track_id {
                continue;
            }
            let n_frames = match opus_dec.decode_float(&packet.data, &mut f32_buf, false) {
                Ok(n) => n,
                Err(_) => continue, // skip header / corrupt packets
            };
            let n_interleaved = n_frames * n_channels as usize;
            let i16_samples: Vec<i16> = f32_buf[..n_interleaved]
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                .collect();
            fingerprinter.consume(&i16_samples);
            total_samples += i16_samples.len() as u64;
        }
        fingerprinter.finish();
        let raw = fingerprinter.fingerprint();
        if raw.is_empty() {
            return Err(anyhow!("Chromaprint produced an empty fingerprint"));
        }
        let duration_ms = if n_channels > 0 {
            total_samples * 1000 / (OPUS_RATE as u64 * n_channels as u64)
        } else {
            0
        };
        return Ok((raw.to_vec(), duration_ms));
    }

    let sample_rate = codec_params.sample_rate.unwrap_or(44100);
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|_| {
            if codec_params.codec == CODEC_TYPE_OPUS {
                anyhow!("Opus codec not supported (rebuild with the 'opus' feature and libopus)")
            } else {
                anyhow!("Unsupported audio codec: {:?}", codec_params.codec)
            }
        })?;

    // rusty-chromaprint internally resamples to 11025 Hz; we feed the native rate.
    let mut fingerprinter = Fingerprinter::new(config);
    fingerprinter
        .start(sample_rate, n_channels)
        .map_err(|e| anyhow!("Chromaprint start: {e:?}"))?;

    let mut sample_buf: Option<SampleBuffer<i16>> = None;
    let mut total_samples: u64 = 0;
    // Max interleaved samples for MAX_FINGERPRINT_MS at the file's native rate.
    let max_samples = MAX_FINGERPRINT_MS * sample_rate as u64 * n_channels as u64 / 1000;

    'decode: loop {
        if total_samples >= max_samples {
            break;
        }

        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => {
                tracing::debug!("Packet read stopped during fingerprinting: {e}");
                break 'decode;
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(e)) => {
                tracing::debug!("Decode error (skipping packet): {e}");
                continue;
            }
            Err(e) => {
                tracing::debug!("Decode stopped: {e}");
                break 'decode;
            }
        };

        let spec = *decoded.spec();
        let buf = sample_buf
            .get_or_insert_with(|| SampleBuffer::<i16>::new(decoded.capacity() as u64, spec));
        buf.copy_interleaved_ref(decoded);

        let samples = buf.samples();
        fingerprinter.consume(samples);
        total_samples += samples.len() as u64;
    }

    fingerprinter.finish();

    let raw = fingerprinter.fingerprint();
    if raw.is_empty() {
        return Err(anyhow!("Chromaprint produced an empty fingerprint"));
    }

    let duration_ms = if n_channels > 0 && sample_rate > 0 {
        total_samples * 1000 / (sample_rate as u64 * n_channels as u64)
    } else {
        0
    };

    Ok((raw.to_vec(), duration_ms))
}

/// Bit error rate between two raw Chromaprint fingerprints.
/// Compares only the overlapping prefix; returns 1.0 if either is too short.
fn fingerprint_ber(a: &[u32], b: &[u32]) -> f32 {
    let len = a.len().min(b.len());
    if len < 10 {
        return 1.0;
    }
    let differing_bits: u32 = a[..len]
        .iter()
        .zip(b[..len].iter())
        .map(|(&x, &y)| (x ^ y).count_ones())
        .sum();
    differing_bits as f32 / (len as f32 * 32.0)
}

// ── AcoustID HTTP lookup ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AcoustIdResponse {
    status: String,
    results: Option<Vec<AcoustIdEntry>>,
}

#[derive(Deserialize)]
struct AcoustIdEntry {
    id: String,
    score: f32,
    recordings: Option<Vec<AcoustIdRecording>>,
}

#[derive(Deserialize)]
struct AcoustIdRecording {
    id: String,
}

/// Minimum confidence score to accept an AcoustID match.
const MIN_SCORE: f32 = 0.85;

pub struct AcoustIdMatch {
    pub acoustid: String,
    pub score: f32,
    /// MBID of the best-matching MusicBrainz recording, if the API returned one.
    pub recording_mbid: Option<String>,
}

/// Query the AcoustID API and return the best match above the confidence threshold.
///
/// Returns `Ok(None)` if the lookup succeeds but no confident match was found.
pub async fn lookup_acoustid(
    api_key: &str,
    fp: &FingerprintResult,
) -> Result<Option<AcoustIdMatch>> {
    static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build shared AcoustID HTTP client")
    });

    let duration_s = (fp.duration_ms / 1000).to_string();

    let response = client
        .get("https://api.acoustid.org/v2/lookup")
        .query(&[
            ("client", api_key),
            ("meta", "recordings"),
            ("duration", &duration_s),
            ("fingerprint", &fp.fingerprint),
        ])
        .send()
        .await
        .context("AcoustID HTTP request failed")?
        .json::<AcoustIdResponse>()
        .await
        .context("Failed to parse AcoustID response")?;

    if response.status != "ok" {
        return Ok(None);
    }

    let best = response
        .results
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.score >= MIN_SCORE)
        .max_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

    Ok(best.map(|entry| {
        let recording_mbid = entry
            .recordings
            .unwrap_or_default()
            .into_iter()
            .next()
            .map(|r| r.id);
        AcoustIdMatch {
            acoustid: entry.id,
            score: entry.score,
            recording_mbid,
        }
    }))
}
