use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use serde::Deserialize;
use std::path::Path;
use std::sync::OnceLock;
use symphonia::core::{
    audio::SampleBuffer, codecs::DecoderOptions, errors::Error as SymphoniaError,
    formats::FormatOptions, io::MediaSourceStream, meta::MetadataOptions, probe::Hint,
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
    let file =
        std::fs::File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("Failed to probe audio format")?;

    let mut format = probed.format;

    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("No default audio track found"))?;

    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let sample_rate = codec_params.sample_rate.unwrap_or(44100);
    let n_channels = codec_params.channels.map(|c| c.count() as u32).unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .context("Failed to create audio decoder")?;

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
