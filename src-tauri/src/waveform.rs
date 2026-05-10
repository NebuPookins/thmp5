use crate::audio_probe::{id3v2_end_offset, open_wave_mp3_payload, probe_media_source};
use anyhow::{anyhow, Context, Result};
#[cfg(feature = "opus")]
use opus::Decoder as OpusDecoder;
use std::fs::File;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error as SymphoniaError;

/// Default number of waveform data points (amplitude peaks) per track.
pub const WAVEFORM_RESOLUTION: usize = 500;

/// Compute waveform peak data for an audio file.
///
/// Returns `resolution` normalized peak amplitude values in `[0.0, 1.0]`.
///
/// CPU-bound — call via `tokio::task::spawn_blocking` from async code.
pub fn compute_waveform(path: &Path, resolution: usize) -> Result<Vec<f32>> {
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
                            "Retrying waveform probe after skipping malformed ID3v2 header"
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
                            "Retrying waveform probe by decoding MP3 payload from RIFF/WAVE wrapper"
                        );
                        probe_media_source(path, segment, Some("mp3"))?
                    } else {
                        return Err(first_err);
                    }
                }
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
    let sample_rate = codec_params.sample_rate.unwrap_or(44100);

    // ── Opus path (libopus, not symphonia) ──────────────────────────────────
    #[cfg(feature = "opus")]
    if codec_params.codec == CODEC_TYPE_OPUS {
        return compute_waveform_opus(&mut format, track_id, n_channels, sample_rate, resolution);
    }

    // ── Symphonia path (all other codecs) ──────────────────────────────────
    #[cfg(not(feature = "opus"))]
    if codec_params.codec == CODEC_TYPE_OPUS {
        return Err(anyhow!(
            "Opus codec not supported (rebuild with the 'opus' feature and libopus)"
        ));
    }

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|_| anyhow!("Unsupported audio codec for waveform"))?;

    // We decode at a fine time resolution (~20ms slices) then downsample.
    let slices_per_sec: u64 = 50; // 50 slices/sec = 20ms per slice
    let frames_per_slice = (sample_rate / slices_per_sec as u32).max(1) as u64;

    // Compute RMS per slice — tracks average energy, not brief peaks.
    // This gives much better visual contrast between loud and quiet sections.
    let mut slices: Vec<f32> = Vec::new();
    let mut slice_frame_count: u64 = 0;
    let mut slice_sum_sq: f64 = 0.0;
    let mut slice_n_samples: u64 = 0;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
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
                tracing::debug!("Waveform decode stopped: {e}");
                break;
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => {
                tracing::debug!("Waveform decode error: {e}");
                break;
            }
        };

        let spec = *decoded.spec();
        let buf = sample_buf
            .get_or_insert_with(|| SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        buf.copy_interleaved_ref(decoded);
        let samples = buf.samples();
        let n_ch = spec.channels.count() as u32;

        // Process frame-by-frame, accumulating sum of squares for RMS.
        for frame in samples.chunks(n_ch as usize) {
            for &s in frame {
                let f = s as f64;
                slice_sum_sq += f * f;
            }
            slice_n_samples += n_ch as u64;
            slice_frame_count += 1;

            if slice_frame_count >= frames_per_slice {
                let rms = (slice_sum_sq / slice_n_samples as f64).sqrt() as f32;
                slices.push(rms);
                slice_sum_sq = 0.0;
                slice_n_samples = 0;
                slice_frame_count = 0;
            }
        }

        // Safety valve: stop if we have many more slices than resolution
        // (e.g. an hours-long file).  resolution * 10 is plenty of headroom.
        if slices.len() > resolution * 10 {
            tracing::debug!(
                "Stopping early (slices={}), trimming to first {}",
                slices.len(),
                resolution
            );
            break;
        }
    }

    // Push the final partial slice if there is one.
    if slice_frame_count > 0 && slice_n_samples > 0 {
        let rms = (slice_sum_sq / slice_n_samples as f64).sqrt() as f32;
        slices.push(rms);
    }

    let mut peaks = downsample_max(&slices, resolution);
    // Apply power curve to spread values — RMS clusters near zero, so a
    // root < 1 boosts quiet sections upward, making differences more visible.
    for p in peaks.iter_mut() {
        *p = p.powf(0.45);
    }
    Ok(peaks)
}

/// Opus-specific decode path.
#[cfg(feature = "opus")]
fn compute_waveform_opus(
    format: &mut Box<dyn symphonia::core::formats::FormatReader>,
    track_id: u32,
    n_channels: u32,
    _sample_rate: u32,
    resolution: usize,
) -> Result<Vec<f32>> {
    let opus_channels = if n_channels == 1 {
        opus::Channels::Mono
    } else {
        opus::Channels::Stereo
    };
    let mut opus_dec = OpusDecoder::new(48_000, opus_channels)
        .map_err(|e| anyhow!("Failed to create Opus decoder: {e}"))?;

    const OPUS_RATE: u32 = 48_000;
    let slices_per_sec: u64 = 50;
    let frames_per_slice = (OPUS_RATE / slices_per_sec as u32).max(1) as u64;

    let mut slices: Vec<f32> = Vec::new();
    let mut slice_frame_count: u64 = 0;
    let mut slice_sum_sq: f64 = 0.0;
    let mut slice_n_samples: u64 = 0;
    let mut f32_buf = vec![0.0f32; 5760 * n_channels as usize];

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => {
                tracing::debug!("Opus waveform decode stopped: {e}");
                break;
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        let n_frames = match opus_dec.decode_float(&packet.data, &mut f32_buf, false) {
            Ok(n) => n,
            Err(_) => continue,
        };

        for frame in f32_buf[..n_frames * n_channels as usize].chunks(n_channels as usize) {
            for &s in frame {
                let f = s as f64;
                slice_sum_sq += f * f;
            }
            slice_n_samples += n_channels as u64;
            slice_frame_count += 1;

            if slice_frame_count >= frames_per_slice {
                let rms = (slice_sum_sq / slice_n_samples as f64).sqrt() as f32;
                slices.push(rms);
                slice_sum_sq = 0.0;
                slice_n_samples = 0;
                slice_frame_count = 0;
            }
        }

        if slices.len() > resolution * 10 {
            break;
        }
    }

    if slice_frame_count > 0 && slice_n_samples > 0 {
        let rms = (slice_sum_sq / slice_n_samples as f64).sqrt() as f32;
        slices.push(rms);
    }

    let mut peaks = downsample_max(&slices, resolution);
    for p in peaks.iter_mut() {
        *p = p.powf(0.45);
    }
    Ok(peaks)
}

/// Downsample `data` to `target_len` by taking the max value in each bucket.
fn downsample_max(data: &[f32], target_len: usize) -> Vec<f32> {
    if data.is_empty() || target_len == 0 {
        return vec![0.0f32; target_len];
    }
    if data.len() <= target_len {
        let mut result = data.to_vec();
        result.resize(target_len, 0.0);
        normalize_peaks(&mut result);
        return result;
    }

    let mut result = vec![0.0f32; target_len];
    let base = data.len() / target_len;
    let extra = data.len() % target_len;

    let mut src_idx = 0;
    for dst_idx in 0..target_len {
        let count = if dst_idx < extra { base + 1 } else { base };
        let mut max_val = 0.0f32;
        for _ in 0..count {
            let v = data[src_idx];
            if v > max_val {
                max_val = v;
            }
            src_idx += 1;
        }
        result[dst_idx] = max_val;
    }

    normalize_peaks(&mut result);
    result
}

/// In-place normalize to [0.0, 1.0].
fn normalize_peaks(peaks: &mut [f32]) {
    let max = peaks.iter().cloned().fold(0.0f32, f32::max);
    if max > 0.0 {
        for p in peaks.iter_mut() {
            *p /= max;
        }
    }
}
