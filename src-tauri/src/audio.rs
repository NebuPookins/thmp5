use crate::audio_probe::{
    default_audio_track, id3v2_end_offset, make_audio_decoder, open_wave_mp3_payload,
    probe_media_source as shared_probe_media_source,
};
use crate::file_issues::FileIssueLog;
use crate::models::{PlaybackStatus, PlayerState};
use crate::sleep_inhibitor::SleepInhibitor;
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
#[cfg(feature = "opus")]
use opus::Decoder as OpusDecoder;
use rtrb::{Consumer, Producer, RingBuffer};
use serde::Serialize;
use std::fs::File;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{well_known::CODEC_ID_OPUS, AudioCodecId};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{SeekMode, SeekTo};
use symphonia::core::units::Time;
use tauri::{AppHandle, Emitter};

pub const PLAYER_STATE_EVENT: &str = "player-state";
pub const PLAYER_POSITION_EVENT: &str = "player-position";
pub const PLAYER_TRACK_ENDED_EVENT: &str = "player-track-ended";
pub const PLAYER_ERROR_EVENT: &str = "player-error";

const PREBUFFER_FRAMES: usize = 8_192;
const MAX_BUFFER_FRAMES: usize = 96_000;
/// How often the engine thread drains and reports the callback's health counters.
const DIAGNOSTICS_INTERVAL: Duration = Duration::from_secs(1);

/// Playback status codes used with `AtomicU8` in `AudioCallbackCtx`.
const STATUS_STOPPED: u8 = 0;
const STATUS_LOADING: u8 = 1;
const STATUS_PLAYING: u8 = 2;
const STATUS_PAUSED: u8 = 3;

fn status_from_u8(v: u8) -> PlaybackStatus {
    match v {
        STATUS_LOADING => PlaybackStatus::Loading,
        STATUS_PLAYING => PlaybackStatus::Playing,
        STATUS_PAUSED => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    }
}

#[derive(Debug, Clone)]
pub struct PlayRequest {
    pub source_id: String,
    pub file_path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub normalization_gain: f32,
    pub normalization_source: String,
    /// Authoritative, non-zero track duration from the library DB. When absent, the duration
    /// the decoder derives from container headers is used instead.
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackEndedEvent {
    pub source_id: String,
    pub position_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerErrorEvent {
    pub message: String,
}

/// Events sent from the audio engine to a background task that handles
/// Last.fm now-playing and scrobble API calls without frontend involvement.
#[derive(Debug)]
pub enum LastFmAction {
    NowPlaying {
        source_id: String,
        artist: String,
        track: String,
    },
    Scrobble {
        artist: String,
        track: String,
        /// UNIX timestamp (seconds) when the track started playing.
        started_at_secs: i64,
        /// Number of milliseconds of audio that were actually played.
        played_ms: u64,
        /// Total track duration in milliseconds.
        duration_ms: u64,
    },
}

/// Lock-free state accessible from the real-time cpal audio callback.
/// The engine thread writes hot-path fields through atomics; the callback
/// reads them without acquiring `SharedState`'s mutex.  Samples travel from the
/// decoder thread to the callback through a lock-free SPSC ring buffer; the
/// `current_track` handle itself is only touched via `try_lock` (once per
/// callback) and is contended only while a track is being swapped.
struct AudioCallbackCtx {
    status: AtomicU8,
    volume: AtomicU32,
    /// Linear gain factor used for loudness normalization (f32 bits).
    /// Always set per-track in start_playback(); the callback multiplies
    /// this by `volume` when `normalization_enabled` is true.
    normalization_gain: AtomicU32,
    /// Whether to apply normalization_gain on top of the user's volume.
    normalization_enabled: AtomicBool,
    output_frame_position: AtomicU64,
    last_position_emit_ms: AtomicU64,
    output_sample_rate: AtomicU32,
    output_channels: AtomicU16,
    track_duration_ms: AtomicU64,
    /// The track buffer – engine thread writes via `lock()`, callback
    /// reads via `try_lock()`, falling back to silence on contention.
    current_track: Mutex<Option<Arc<TrackBuffer>>>,
    /// Real-time health counters, written by the callback and drained by the
    /// engine thread (see `AudioDiagnostics`).
    diagnostics: AudioDiagnostics,
    /// Set to true by the cpal stream error callback to signal the engine
    /// thread that the output device died and needs to be rebuilt.
    stream_rebuild_needed: AtomicBool,
    current_source_id: Mutex<Option<String>>,
    /// Channels for sending events from the callback to the engine thread.
    position_tx: Sender<u64>,
    state_tx: Sender<PlayerState>,
    track_ended_tx: Sender<TrackEndedEvent>,
}

impl AudioCallbackCtx {
    fn new(
        output_sample_rate: u32,
        output_channels: u16,
        position_tx: Sender<u64>,
        state_tx: Sender<PlayerState>,
        track_ended_tx: Sender<TrackEndedEvent>,
    ) -> Self {
        Self {
            status: AtomicU8::new(STATUS_STOPPED),
            volume: AtomicU32::new(f32::to_bits(1.0)),
            normalization_gain: AtomicU32::new(f32::to_bits(1.0)),
            normalization_enabled: AtomicBool::new(false),
            output_frame_position: AtomicU64::new(0),
            last_position_emit_ms: AtomicU64::new(0),
            output_sample_rate: AtomicU32::new(output_sample_rate),
            output_channels: AtomicU16::new(output_channels),
            track_duration_ms: AtomicU64::new(0),
            stream_rebuild_needed: AtomicBool::new(false),
            current_track: Mutex::new(None),
            diagnostics: AudioDiagnostics::default(),
            current_source_id: Mutex::new(None),
            position_tx,
            state_tx,
            track_ended_tx,
        }
    }

    fn position_ms(&self) -> u64 {
        let rate = self.output_sample_rate.load(Ordering::Relaxed);
        if rate == 0 {
            return 0;
        }
        let pos = self.output_frame_position.load(Ordering::Relaxed);
        pos.saturating_mul(1000) / u64::from(rate)
    }
}

enum AudioCommand {
    Play(PlayRequest),
    Pause,
    Resume,
    Seek(u64),
    SetVolume(f32),
    SetNormalizationEnabled(bool),
    Stop,
}

#[derive(Clone)]
pub struct AudioEngineHandle {
    tx: Sender<AudioCommand>,
    shared: Arc<Mutex<SharedState>>,
    ctx: Arc<AudioCallbackCtx>,
}

impl AudioEngineHandle {
    pub fn new(
        app: AppHandle,
        file_issues: FileIssueLog,
        lastfm_tx: Option<tokio::sync::mpsc::Sender<LastFmAction>>,
    ) -> Result<Self> {
        let sleep_inhibitor = Arc::new(SleepInhibitor::new("thmp5", "Music playback in progress"));
        let mut shared_state = SharedState::new(sleep_inhibitor);
        shared_state.lastfm_tx = lastfm_tx;
        let shared = Arc::new(Mutex::new(shared_state));
        let (tx, rx) = mpsc::channel();
        let command_shared = Arc::clone(&shared);

        // Event channels: callback → engine thread
        let (position_tx, position_rx) = mpsc::channel();
        let (state_tx, state_rx) = mpsc::channel();
        let (track_ended_tx, track_ended_rx) = mpsc::channel();

        let ctx = Arc::new(AudioCallbackCtx::new(
            48_000,
            2,
            position_tx,
            state_tx,
            track_ended_tx,
        ));
        let command_ctx = Arc::clone(&ctx);
        let events = EventReceivers {
            position: position_rx,
            state: state_rx,
            track_ended: track_ended_rx,
        };

        thread::Builder::new()
            .name("audio-engine".to_string())
            .spawn(move || {
                let mut stream: Option<cpal::Stream> = None;
                let events = Some(events);
                let mut last_diagnostics_report = Instant::now();
                tracing::info!("Audio engine thread started");

                loop {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(command) => {
                            if matches!(command, AudioCommand::Play(_) | AudioCommand::Resume) {
                                if let Err(error) = ensure_output_stream(
                                    &mut stream,
                                    &command_shared,
                                    &command_ctx,
                                    &app,
                                ) {
                                    set_engine_error(&command_shared, &app, error.to_string());
                                    continue;
                                }
                            }

                            if let Err(error) = handle_command(
                                command,
                                &command_shared,
                                &command_ctx,
                                &app,
                                &file_issues,
                            ) {
                                set_engine_error(&command_shared, &app, format!("{error:#}"));
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => { /* drain events below */ }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }

                    if last_diagnostics_report.elapsed() >= DIAGNOSTICS_INTERVAL {
                        last_diagnostics_report = Instant::now();
                        report_diagnostics(command_ctx.diagnostics.take());
                    }

                    // Drain event channels from the cpal callback.
                    if let Some(ref ev) = events {
                        drain_events(&command_shared, &command_ctx, ev, &app);
                    }

                    // If the output stream error callback fired (e.g. ALSA/PulseAudio
                    // restarted, device hotplug, system resume), rebuild the stream
                    // automatically so playback resumes without requiring a manual
                    // pause/resume cycle from the user.
                    if command_ctx
                        .stream_rebuild_needed
                        .swap(false, Ordering::Acquire)
                    {
                        tracing::warn!("Output stream error detected, attempting to rebuild");
                        if let Err(error) = ensure_output_stream(
                            &mut stream,
                            &command_shared,
                            &command_ctx,
                            &app,
                        ) {
                            tracing::error!(%error, "Failed to rebuild output stream, stopping playback");
                            set_engine_error(
                                &command_shared,
                                &app,
                                format!("Stream rebuild failed: {error}"),
                            );
                            if let Ok(mut state) = command_shared.lock() {
                                state.clear_track(&command_ctx);
                            }
                        }
                    }
                }
            })
            .context("Failed to start audio engine thread")?;

        Ok(Self { tx, shared, ctx })
    }

    pub fn play(&self, request: PlayRequest) -> Result<()> {
        // Set eagerly so snapshot() returns the correct value before the
        // engine thread processes the Play command.
        self.ctx
            .normalization_gain
            .store(request.normalization_gain.to_bits(), Ordering::Relaxed);
        if let Ok(mut state) = self.shared.lock() {
            state.normalization_source = request.normalization_source.clone();
        }
        self.send_command(AudioCommand::Play(request))
    }

    pub fn pause(&self) -> Result<()> {
        self.send_command(AudioCommand::Pause)
    }

    pub fn resume(&self) -> Result<()> {
        self.send_command(AudioCommand::Resume)
    }

    pub fn seek(&self, position_ms: u64) -> Result<()> {
        self.send_command(AudioCommand::Seek(position_ms))
    }

    pub fn set_volume(&self, volume: f32) -> Result<()> {
        self.send_command(AudioCommand::SetVolume(volume.clamp(0.0, 1.5)))
    }

    pub fn set_normalization_enabled(&self, enabled: bool) -> Result<()> {
        self.send_command(AudioCommand::SetNormalizationEnabled(enabled))
    }

    pub fn stop(&self) -> Result<()> {
        self.send_command(AudioCommand::Stop)
    }

    pub fn snapshot(&self) -> PlayerState {
        let ctx = &self.ctx;
        let status = status_from_u8(ctx.status.load(Ordering::Acquire));
        let volume = f32::from_bits(ctx.volume.load(Ordering::Relaxed));
        let position_ms = ctx.position_ms();
        let duration_ms = {
            let d = ctx.track_duration_ms.load(Ordering::Relaxed);
            (d > 0).then_some(d)
        };
        let source_id = ctx.current_source_id.lock().ok().and_then(|r| r.clone());

        // Metadata only kept in SharedState.
        let (title, artist) = self
            .shared
            .lock()
            .ok()
            .map(|s| (s.current_title.clone(), s.current_artist.clone()))
            .unwrap_or((None, None));

        let normalization_source = self
            .shared
            .lock()
            .ok()
            .map(|s| s.normalization_source.clone())
            .unwrap_or_default();

        PlayerState {
            status,
            source_id,
            title,
            artist,
            duration_ms,
            position_ms,
            volume,
            normalization_enabled: ctx.normalization_enabled.load(Ordering::Relaxed),
            normalization_gain: f32::from_bits(ctx.normalization_gain.load(Ordering::Relaxed)),
            normalization_source,
        }
    }

    fn send_command(&self, command: AudioCommand) -> Result<()> {
        self.tx.send(command).map_err(|_| {
            self.shared
                .lock()
                .ok()
                .and_then(|shared| shared.engine_error.clone())
                .map(anyhow::Error::msg)
                .unwrap_or_else(|| anyhow!("Audio engine is unavailable"))
        })
    }
}

struct SharedState {
    sleep_inhibitor: Arc<SleepInhibitor>,
    engine_error: Option<String>,
    current_title: Option<String>,
    current_artist: Option<String>,
    current_file_path: Option<String>,
    normalization_source: String,
    /// Sender for Last.fm scrobble/now-playing events.
    lastfm_tx: Option<tokio::sync::mpsc::Sender<LastFmAction>>,
    /// UNIX second when the current track started playing (set in start_playback).
    lastfm_track_started_at: Option<i64>,
}

impl SharedState {
    fn new(sleep_inhibitor: Arc<SleepInhibitor>) -> Self {
        Self {
            sleep_inhibitor,
            engine_error: None,
            current_title: None,
            current_artist: None,
            current_file_path: None,
            normalization_source: String::from("None"),
            lastfm_tx: None,
            lastfm_track_started_at: None,
        }
    }

    fn stop_decoder(&self, ctx: &AudioCallbackCtx) {
        if let Ok(track) = ctx.current_track.lock() {
            if let Some(buffer) = track.as_ref() {
                buffer.stop_requested.store(true, Ordering::Release);
            }
        }
    }

    fn clear_track(&mut self, ctx: &AudioCallbackCtx) {
        self.stop_decoder(ctx);
        // Clear callback context fields.
        ctx.status.store(STATUS_STOPPED, Ordering::Release);
        if let Ok(mut track) = ctx.current_track.lock() {
            *track = None;
        }
        if let Ok(mut id) = ctx.current_source_id.lock() {
            *id = None;
        }
        ctx.output_frame_position.store(0, Ordering::Relaxed);
        ctx.track_duration_ms.store(0, Ordering::Relaxed);
        // Clear SharedState metadata.
        self.current_title = None;
        self.current_artist = None;
        self.current_file_path = None;
        self.normalization_source = String::from("None");
        self.lastfm_track_started_at = None;
    }
}

/// Counters the audio callback bumps and the engine thread periodically drains and logs.
/// Logging from the callback itself would allocate and take locks, which a real-time thread
/// must not do.
#[derive(Default)]
struct AudioDiagnostics {
    callbacks: AtomicU64,
    /// Callbacks that output silence because the track handle or its consumer was locked.
    lock_misses: AtomicU64,
    /// Times a playing track ran dry and fell back to `Loading`.
    underruns: AtomicU64,
    /// Errors the audio backend reported as buffer under/overruns.
    backend_xruns: AtomicU64,
    max_callback_us: AtomicU64,
    max_callback_frames: AtomicU64,
}

/// A point-in-time copy of `AudioDiagnostics`, with every counter reset by taking it.
#[derive(Debug, PartialEq, Eq)]
struct DiagnosticsSnapshot {
    callbacks: u64,
    lock_misses: u64,
    underruns: u64,
    backend_xruns: u64,
    max_callback_us: u64,
    max_callback_frames: u64,
}

impl AudioDiagnostics {
    fn take(&self) -> DiagnosticsSnapshot {
        DiagnosticsSnapshot {
            callbacks: self.callbacks.swap(0, Ordering::Relaxed),
            lock_misses: self.lock_misses.swap(0, Ordering::Relaxed),
            underruns: self.underruns.swap(0, Ordering::Relaxed),
            backend_xruns: self.backend_xruns.swap(0, Ordering::Relaxed),
            max_callback_us: self.max_callback_us.swap(0, Ordering::Relaxed),
            max_callback_frames: self.max_callback_frames.swap(0, Ordering::Relaxed),
        }
    }
}

impl DiagnosticsSnapshot {
    /// Whether anything audible (or nearly so) happened in the window.
    fn has_glitches(&self) -> bool {
        self.lock_misses > 0 || self.underruns > 0 || self.backend_xruns > 0
    }
}

/// The callback's side of a track's sample pipe. The decoder thread owns the matching
/// `Producer`, so the two sides never share a lock.
struct TrackBuffer {
    /// Locked only by the audio callback, so it is uncontended in practice.
    consumer: Mutex<Consumer<f32>>,
    /// Set by the decoder (with `Release`) after its last sample has been pushed.
    finished: AtomicBool,
    stop_requested: AtomicBool,
}

impl TrackBuffer {
    /// Creates a track buffer holding up to `capacity_frames` frames of `channels` samples each,
    /// returning the callback-side handle and the decoder-side producer.
    fn new(capacity_frames: usize, channels: usize) -> (Arc<Self>, Producer<f32>) {
        let (producer, consumer) = RingBuffer::new(capacity_frames * channels.max(1));
        let buffer = Arc::new(Self {
            consumer: Mutex::new(consumer),
            finished: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
        });
        (buffer, producer)
    }
}

/// Pushes every sample into the ring, waiting for the callback to drain it when full.
/// Returns `false` (dropping the remainder) if the track was asked to stop meanwhile.
fn push_samples(producer: &mut Producer<f32>, buffer: &TrackBuffer, mut samples: &[f32]) -> bool {
    while !samples.is_empty() {
        if buffer.stop_requested.load(Ordering::Acquire) {
            return false;
        }
        let n = producer.slots().min(samples.len());
        if n == 0 {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        let (now, rest) = samples.split_at(n);
        if let Ok(chunk) = producer.write_chunk_uninit(n) {
            chunk.fill_from_iter(now.iter().copied());
        }
        samples = rest;
    }
    true
}

fn handle_command(
    command: AudioCommand,
    shared: &Arc<Mutex<SharedState>>,
    ctx: &Arc<AudioCallbackCtx>,
    app: &AppHandle,
    file_issues: &FileIssueLog,
) -> Result<()> {
    match command {
        AudioCommand::Play(request) => {
            tracing::info!(
                source_id = %request.source_id,
                path = %request.file_path,
                "Beginning streaming track load"
            );
            let file_path = request.file_path.clone();
            if let Err(e) = start_playback(shared, ctx, app, request, 0) {
                file_issues.push_playback_error(file_path, e.to_string());
                return Err(e);
            }
        }
        AudioCommand::Pause => {
            tracing::info!("Pausing playback");
            ctx.status.store(STATUS_PAUSED, Ordering::Release);
        }
        AudioCommand::Resume => {
            tracing::info!("Resuming playback");
            if ctx.current_track.lock().ok().map_or(false, |t| t.is_some()) {
                ctx.status.store(STATUS_LOADING, Ordering::Release);
            }
        }
        AudioCommand::Seek(position_ms) => {
            tracing::info!(position_ms, "Seeking playback");
            let request = {
                let state = shared
                    .lock()
                    .map_err(|_| anyhow!("Audio state lock poisoned"))?;
                let norm_gain = f32::from_bits(ctx.normalization_gain.load(Ordering::Relaxed));
                PlayRequest {
                    source_id: ctx
                        .current_source_id
                        .lock()
                        .ok()
                        .and_then(|g| g.clone())
                        .ok_or_else(|| anyhow!("No active track to seek"))?,
                    file_path: state
                        .current_file_path
                        .clone()
                        .ok_or_else(|| anyhow!("No active track to seek"))?,
                    title: state.current_title.clone(),
                    artist: state.current_artist.clone(),
                    normalization_gain: norm_gain,
                    normalization_source: state.normalization_source.clone(),
                    duration_ms: Some(ctx.track_duration_ms.load(Ordering::Relaxed))
                        .filter(|&d| d > 0),
                }
            };

            start_playback(shared, ctx, app, request, position_ms)?;
        }
        AudioCommand::SetVolume(volume) => {
            let clamped = volume.clamp(0.0, 1.5);
            tracing::info!(volume = clamped, "Updating playback volume");
            ctx.volume.store(clamped.to_bits(), Ordering::Relaxed);
        }
        AudioCommand::SetNormalizationEnabled(enabled) => {
            tracing::info!(enabled, "Toggling loudness normalization");
            ctx.normalization_enabled.store(enabled, Ordering::Relaxed);
        }
        AudioCommand::Stop => {
            tracing::info!("Stopping playback");
            let mut state = shared
                .lock()
                .map_err(|_| anyhow!("Audio state lock poisoned"))?;
            // Send scrobble before clearing if there's an active track.
            if let (Some(artist), Some(track), Some(started_at), Some(ref tx)) = (
                &state.current_artist,
                &state.current_title,
                state.lastfm_track_started_at,
                &state.lastfm_tx,
            ) {
                let position_ms = ctx.position_ms();
                let duration_ms = ctx.track_duration_ms.load(Ordering::Relaxed);
                tracing::info!(
                    artist,
                    track,
                    position_ms,
                    duration_ms,
                    "Sending scrobble on stop"
                );
                if let Err(e) = tx.try_send(LastFmAction::Scrobble {
                    artist: artist.clone(),
                    track: track.clone(),
                    started_at_secs: started_at,
                    played_ms: position_ms,
                    duration_ms,
                }) {
                    tracing::warn!("Failed to send scrobble via channel: {e}");
                }
            } else {
                tracing::info!("No active track metadata for scrobble on stop");
            }
            state.clear_track(ctx);
        }
    }

    Ok(())
}

fn start_playback(
    shared: &Arc<Mutex<SharedState>>,
    ctx: &Arc<AudioCallbackCtx>,
    app: &AppHandle,
    request: PlayRequest,
    start_ms: u64,
) -> Result<()> {
    let (output_rate, output_channels) = (
        ctx.output_sample_rate.load(Ordering::Relaxed),
        ctx.output_channels.load(Ordering::Relaxed),
    );

    let source = LocalFileSource::open(Path::new(&request.file_path))
        .with_context(|| format!("Failed to open {}", request.file_path))?;

    let (buffer, producer) = TrackBuffer::new(MAX_BUFFER_FRAMES, usize::from(output_channels));
    let duration_ms = request.duration_ms.unwrap_or(source.duration_ms);
    let current_output_position = start_ms.saturating_mul(u64::from(output_rate)) / 1000;

    {
        // Stop the previous decoder.
        let state = shared
            .lock()
            .map_err(|_| anyhow!("Audio state lock poisoned"))?;
        state.stop_decoder(ctx);
    }

    // Set up the callback context before spawning the decoder.
    ctx.status.store(STATUS_LOADING, Ordering::Release);
    ctx.normalization_gain
        .store(request.normalization_gain.to_bits(), Ordering::Relaxed);
    {
        let mut guard = ctx.current_track.lock().unwrap();
        *guard = Some(Arc::clone(&buffer));
    }
    ctx.track_duration_ms.store(duration_ms, Ordering::Relaxed);
    {
        let mut guard = ctx.current_source_id.lock().unwrap();
        *guard = Some(request.source_id.clone());
    }
    ctx.output_frame_position
        .store(current_output_position, Ordering::Relaxed);
    ctx.last_position_emit_ms
        .store(start_ms.saturating_sub(250), Ordering::Relaxed);

    // Update SharedState metadata and emit Last.fm now-playing.
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    {
        let mut state = shared.lock().unwrap();
        state.current_title = request.title.clone();
        state.current_artist = request.artist.clone();
        state.current_file_path = Some(request.file_path.clone());
        state.normalization_source = request.normalization_source.clone();
        state.lastfm_track_started_at = Some(started_at);
        if let (Some(artist), Some(track), Some(ref tx)) =
            (&request.artist, &request.title, &state.lastfm_tx)
        {
            let _ = tx.try_send(LastFmAction::NowPlaying {
                source_id: request.source_id.clone(),
                artist: artist.clone(),
                track: track.clone(),
            });
        }
    }

    spawn_decoder_thread(
        source,
        start_ms,
        output_rate,
        output_channels,
        buffer,
        producer,
        app.clone(),
    );

    Ok(())
}

fn spawn_decoder_thread(
    source: LocalFileSource,
    start_ms: u64,
    output_rate: u32,
    output_channels: u16,
    buffer: Arc<TrackBuffer>,
    producer: Producer<f32>,
    app: AppHandle,
) {
    thread::Builder::new()
        .name("audio-decoder".to_string())
        .spawn(move || {
            if let Err(error) = decode_into_buffer(
                source,
                start_ms,
                output_rate,
                output_channels,
                &buffer,
                producer,
            ) {
                buffer.finished.store(true, Ordering::Release);
                emit_error(&app, error.to_string());
            }
        })
        .ok();
}

fn decode_into_buffer(
    mut source: LocalFileSource,
    start_ms: u64,
    output_rate: u32,
    output_channels: u16,
    buffer: &TrackBuffer,
    mut producer: Producer<f32>,
) -> Result<()> {
    tracing::info!(
        start_ms,
        source_rate = source.sample_rate,
        source_channels = source.channels,
        output_rate,
        output_channels,
        "Decoder worker started"
    );

    if start_ms > 0 {
        source.seek_to_ms(start_ms)?;
    }

    let mut resampler = StreamResampler::new(
        source.sample_rate,
        source.channels,
        output_rate,
        output_channels,
        start_ms,
    );

    loop {
        if buffer.stop_requested.load(Ordering::Acquire) {
            tracing::info!("Decoder worker stopping early");
            return Ok(());
        }

        match source.decode_next()? {
            None => break,
            Some(input) => {
                let output = resampler.push(&input);
                if !push_samples(&mut producer, buffer, &output) {
                    return Ok(());
                }
            }
        }
    }

    let tail = resampler.finish();
    if !push_samples(&mut producer, buffer, &tail) {
        return Ok(());
    }
    buffer.finished.store(true, Ordering::Release);
    tracing::info!("Decoder worker finished");
    Ok(())
}

fn report_diagnostics(snapshot: DiagnosticsSnapshot) {
    if snapshot.has_glitches() {
        tracing::warn!(
            callbacks = snapshot.callbacks,
            lock_misses = snapshot.lock_misses,
            underruns = snapshot.underruns,
            backend_xruns = snapshot.backend_xruns,
            max_callback_us = snapshot.max_callback_us,
            max_callback_frames = snapshot.max_callback_frames,
            "Audio callback glitches in the last interval"
        );
    } else if snapshot.callbacks > 0 {
        tracing::debug!(
            callbacks = snapshot.callbacks,
            max_callback_us = snapshot.max_callback_us,
            max_callback_frames = snapshot.max_callback_frames,
            "Audio callback healthy"
        );
    }
}

fn emit_error(app: &AppHandle, message: String) {
    let _ = app.emit(PLAYER_ERROR_EVENT, PlayerErrorEvent { message });
}

fn set_engine_error(shared: &Arc<Mutex<SharedState>>, app: &AppHandle, message: String) {
    tracing::error!(%message, "Audio engine error");
    if let Ok(mut state) = shared.lock() {
        state.engine_error = Some(message.clone());
    }
    emit_error(app, message);
}

fn clear_engine_error(shared: &Arc<Mutex<SharedState>>) {
    if let Ok(mut state) = shared.lock() {
        state.engine_error = None;
    }
}

/// Holds the receiver ends of the callback→engine event channels.
struct EventReceivers {
    position: mpsc::Receiver<u64>,
    state: mpsc::Receiver<PlayerState>,
    track_ended: mpsc::Receiver<TrackEndedEvent>,
}

fn sync_sleep_inhibitor(sleep_inhibitor: &SleepInhibitor, should_inhibit: bool) {
    if let Err(error) = sleep_inhibitor.set_active(should_inhibit) {
        tracing::warn!(
            error = %error,
            should_inhibit,
            "Failed to update desktop sleep inhibitor"
        );
    }
}

/// Drain event channels from the cpal callback and forward them to Tauri.
/// Called periodically from the engine thread.
fn drain_events(
    shared: &Arc<Mutex<SharedState>>,
    ctx: &AudioCallbackCtx,
    events: &EventReceivers,
    app: &AppHandle,
) {
    while let Ok(pos_ms) = events.position.try_recv() {
        let _ = app.emit(PLAYER_POSITION_EVENT, pos_ms);
    }
    while let Ok(event) = events.track_ended.try_recv() {
        // Send Last.fm scrobble before clearing metadata.
        if let Ok(state) = shared.lock() {
            if let (Some(track), Some(artist), Some(started_at), Some(ref tx)) = (
                &state.current_title,
                &state.current_artist,
                state.lastfm_track_started_at,
                &state.lastfm_tx,
            ) {
                let duration_ms = ctx.track_duration_ms.load(Ordering::Relaxed);
                tracing::info!(
                    artist,
                    track,
                    position_ms = event.position_ms,
                    duration_ms,
                    "Sending scrobble on track end"
                );
                if let Err(e) = tx.try_send(LastFmAction::Scrobble {
                    artist: artist.clone(),
                    track: track.clone(),
                    started_at_secs: started_at,
                    played_ms: event.position_ms,
                    duration_ms,
                }) {
                    tracing::warn!("Failed to send scrobble via channel: {e}");
                }
            }
        }
        // Clear SharedState track metadata and release the sleep inhibitor.
        if let Ok(mut state) = shared.lock() {
            state.clear_track(ctx);
            sync_sleep_inhibitor(&state.sleep_inhibitor, false);
        }
        let _ = app.emit(PLAYER_TRACK_ENDED_EVENT, event);
        // Emit the updated player state.
        let _ = app.emit(
            PLAYER_STATE_EVENT,
            PlayerState {
                status: PlaybackStatus::Stopped,
                source_id: None,
                title: None,
                artist: None,
                duration_ms: None,
                position_ms: ctx.position_ms(),
                volume: f32::from_bits(ctx.volume.load(Ordering::Relaxed)),
                normalization_enabled: ctx.normalization_enabled.load(Ordering::Relaxed),
                normalization_gain: f32::from_bits(ctx.normalization_gain.load(Ordering::Relaxed)),
                normalization_source: String::new(),
            },
        );
    }
    while let Ok(state) = events.state.try_recv() {
        let inhibit = should_inhibit_for_status(&state.status);
        if let Ok(s) = shared.lock() {
            sync_sleep_inhibitor(&s.sleep_inhibitor, inhibit);
        }
        let _ = app.emit(PLAYER_STATE_EVENT, state);
    }
}

fn should_inhibit_for_status(status: &PlaybackStatus) -> bool {
    matches!(status, PlaybackStatus::Loading | PlaybackStatus::Playing)
}

fn ensure_output_stream(
    stream: &mut Option<cpal::Stream>,
    shared: &Arc<Mutex<SharedState>>,
    ctx: &Arc<AudioCallbackCtx>,
    app: &AppHandle,
) -> Result<()> {
    // Rebuild the output stream on every Play/Resume so that device changes after
    // system suspend/resume,  idle timeouts, or hotplug events are picked up.  cpal
    // does not expose a "stream is still valid" check, and the error callback on the
    // old stream is fire-and-forget, so caching a single stream for the app lifetime
    // silently fails after the audio sink is invalidated overnight.
    *stream = None;
    // Clear any stale rebuild-request flag from an earlier error callback so it
    // doesn't trigger a redundant rebuild on the next engine loop iteration.
    ctx.stream_rebuild_needed.store(false, Ordering::Relaxed);

    // Try the default host (ALSA on Linux) first.
    let default_host = cpal::default_host();
    match try_build_stream(&default_host, ctx, app.clone()) {
        Ok(output_stream) => {
            *stream = Some(output_stream);
            clear_engine_error(shared);
            return Ok(());
        }
        Err(e) => tracing::warn!(%e, "Default output device unusable"),
    }

    // If the default host fails, try alternative audio backends (JACK, PulseAudio).
    // Each host is queried via its own device enumeration so a broken ALSA/PulseAudio
    // configuration does not prevent e.g. a JACK-only stream from working.
    for host_id in cpal::available_hosts() {
        if let Ok(host) = cpal::host_from_id(host_id) {
            if let Ok(output_stream) = try_build_stream(&host, ctx, app.clone()) {
                *stream = Some(output_stream);
                clear_engine_error(shared);
                return Ok(());
            }
        }
    }

    Err(anyhow!("No usable output audio device is available"))
}

/// Try to select an output device and build a stream on a single cpal host.
fn try_build_stream(
    host: &cpal::Host,
    ctx: &Arc<AudioCallbackCtx>,
    app: AppHandle,
) -> Result<cpal::Stream> {
    let (device, supported_config) = select_output_device(host)?;
    let stream_config = cpal::StreamConfig {
        buffer_size: output_buffer_size(supported_config.buffer_size()),
        ..supported_config.config()
    };
    let device_name = device_name(&device);
    tracing::info!(
        device = %device_name,
        sample_rate = stream_config.sample_rate,
        channels = stream_config.channels,
        format = ?supported_config.sample_format(),
        "Using output device"
    );

    ctx.output_sample_rate
        .store(stream_config.sample_rate, Ordering::Relaxed);
    ctx.output_channels
        .store(stream_config.channels, Ordering::Relaxed);

    let output_stream = build_output_stream(
        &device,
        &stream_config,
        supported_config.sample_format(),
        Arc::clone(ctx),
        app,
    )?;
    output_stream
        .play()
        .context("Failed to start output stream")?;
    Ok(output_stream)
}

/// Whether the stream is dead after a backend error of this kind. Glitch reports and
/// notifications that the stream survived must not tear it down, since rebuilding audibly
/// interrupts playback far longer than the glitch being reported.
fn requires_stream_rebuild(kind: cpal::ErrorKind) -> bool {
    !matches!(
        kind,
        cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied | cpal::ErrorKind::DeviceChanged
    )
}

fn handle_stream_error(ctx: &AudioCallbackCtx, app: &AppHandle, error: &cpal::Error) {
    let kind = error.kind();
    if kind == cpal::ErrorKind::Xrun {
        ctx.diagnostics
            .backend_xruns
            .fetch_add(1, Ordering::Relaxed);
        return;
    }
    if requires_stream_rebuild(kind) {
        ctx.stream_rebuild_needed.store(true, Ordering::Release);
        emit_error(app, format!("Audio stream error: {error}"));
    } else {
        tracing::warn!(%error, "Audio stream reported a non-fatal condition");
    }
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map_or_else(|_| "<unknown>".to_string(), |d| d.name().to_string())
}

/// ALSA's `null` sink accepts any stream and discards it. cpal 0.18 lists it first and it probes
/// as usable, so without this filter the fallback below would pick it and play silence.
fn is_discard_sink(device: &cpal::Device) -> bool {
    device.id().is_ok_and(|id| id.id() == "null")
}

fn select_output_device(host: &cpal::Host) -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    if let Some(device) = host.default_output_device() {
        match device.default_output_config() {
            Ok(config) => return Ok((device, config)),
            Err(default_error) => {
                tracing::warn!("Default output device unusable: {default_error}");
            }
        }
    }

    let devices = host
        .output_devices()
        .context("Failed to enumerate output audio devices")?;

    for device in devices.filter(|device| !is_discard_sink(device)) {
        match device.default_output_config() {
            Ok(config) => return Ok((device, config)),
            Err(error) => {
                let name = device_name(&device);
                tracing::warn!("Skipping output device {name}: {error}");
            }
        }
    }

    Err(anyhow!("No usable output audio device is available"))
}

/// Frames per output callback we ask for. A pause or stop only takes effect at the next callback,
/// so this bounds how long audio keeps playing after the command; some backends (the ALSA JACK
/// plugin) otherwise default to 65536-frame callbacks, i.e. ~1.4 s at 48 kHz.
const TARGET_CALLBACK_FRAMES: cpal::FrameCount = 2_048;

fn output_buffer_size(supported: &cpal::SupportedBufferSize) -> cpal::BufferSize {
    match supported {
        cpal::SupportedBufferSize::Range { min, max } => {
            cpal::BufferSize::Fixed(TARGET_CALLBACK_FRAMES.clamp(*min, *max))
        }
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
    }
}

fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    ctx: Arc<AudioCallbackCtx>,
    app: AppHandle,
) -> Result<cpal::Stream> {
    match sample_format {
        cpal::SampleFormat::F32 => {
            let ctx_ref = Arc::clone(&ctx);
            let err_ctx = Arc::clone(&ctx);
            let app_for_err = app.clone();
            device
                .build_output_stream(
                    *config,
                    move |data: &mut [f32], _| write_output_data_f32(data, &ctx_ref),
                    move |error| handle_stream_error(&err_ctx, &app_for_err, &error),
                    None,
                )
                .context("Failed to build f32 output stream")
        }
        cpal::SampleFormat::I16 => {
            let ctx_ref = Arc::clone(&ctx);
            let err_ctx = Arc::clone(&ctx);
            let app_for_err = app.clone();
            device
                .build_output_stream(
                    *config,
                    move |data: &mut [i16], _| write_output_data_i16(data, &ctx_ref),
                    move |error| handle_stream_error(&err_ctx, &app_for_err, &error),
                    None,
                )
                .context("Failed to build i16 output stream")
        }
        cpal::SampleFormat::U16 => {
            let ctx_ref = Arc::clone(&ctx);
            let err_ctx = Arc::clone(&ctx);
            let app_for_err = app.clone();
            device
                .build_output_stream(
                    *config,
                    move |data: &mut [u16], _| write_output_data_u16(data, &ctx_ref),
                    move |error| handle_stream_error(&err_ctx, &app_for_err, &error),
                    None,
                )
                .context("Failed to build u16 output stream")
        }
        other => Err(anyhow!("Unsupported output sample format: {other:?}")),
    }
}

fn write_output_data_f32(output: &mut [f32], ctx: &Arc<AudioCallbackCtx>) {
    write_output_data(output, ctx, |sample| sample);
}

fn write_output_data_i16(output: &mut [i16], ctx: &Arc<AudioCallbackCtx>) {
    write_output_data(output, ctx, |sample| {
        (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
    });
}

fn write_output_data_u16(output: &mut [u16], ctx: &Arc<AudioCallbackCtx>) {
    write_output_data(output, ctx, |sample| {
        (((sample.clamp(-1.0, 1.0) + 1.0) * 0.5) * u16::MAX as f32) as u16
    });
}

fn write_output_data<T, F>(output: &mut [T], ctx: &AudioCallbackCtx, convert: F)
where
    T: Copy,
    F: Fn(f32) -> T,
{
    let started = Instant::now();
    fill_output(output, ctx, &convert);

    let diagnostics = &ctx.diagnostics;
    let channels = usize::from(ctx.output_channels.load(Ordering::Relaxed)).max(1);
    diagnostics.callbacks.fetch_add(1, Ordering::Relaxed);
    diagnostics.max_callback_us.fetch_max(
        u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    diagnostics.max_callback_frames.fetch_max(
        u64::try_from(output.len() / channels).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

fn fill_output<T, F>(output: &mut [T], ctx: &AudioCallbackCtx, convert: &F)
where
    T: Copy,
    F: Fn(f32) -> T,
{
    let silence = convert(0.0);
    let output_channels = usize::from(ctx.output_channels.load(Ordering::Relaxed));
    if output_channels == 0 {
        output.fill(silence);
        return;
    }

    // Take the track handle and its consumer once per callback rather than once per frame. They
    // are contended only while the engine thread swaps tracks, in which case this whole callback
    // is silence rather than a click-inducing scatter of dropped frames.
    let track = match ctx.current_track.try_lock() {
        Ok(guard) => match guard.as_ref() {
            Some(track) => Arc::clone(track),
            None => {
                output.fill(silence);
                return;
            }
        },
        Err(_) => {
            ctx.diagnostics.lock_misses.fetch_add(1, Ordering::Relaxed);
            output.fill(silence);
            return;
        }
    };
    let mut consumer = match track.consumer.try_lock() {
        Ok(consumer) => consumer,
        Err(_) => {
            ctx.diagnostics.lock_misses.fetch_add(1, Ordering::Relaxed);
            output.fill(silence);
            return;
        }
    };

    let mut emitted_pos = false;
    let mut track_ended = false;

    for frame in output.chunks_mut(output_channels) {
        if track_ended {
            frame.fill(silence);
            continue;
        }

        let status = ctx.status.load(Ordering::Relaxed);
        if status != STATUS_PLAYING && status != STATUS_LOADING {
            frame.fill(silence);
            continue;
        }

        // `finished` must be read before the ring's fill level: the decoder pushes its tail
        // and only then sets `finished`, so seeing both "finished" and "empty" in this order
        // means nothing more can arrive.
        let finished = track.finished.load(Ordering::Acquire);
        let ready_frames = consumer.slots() / output_channels;

        if status == STATUS_LOADING {
            // Transition from Loading → Playing once we have enough data. Until then, play
            // silence and leave the buffered samples (and the position) untouched.
            if ready_frames >= PREBUFFER_FRAMES || (finished && ready_frames > 0) {
                ctx.status.store(STATUS_PLAYING, Ordering::Release);
                let _ = ctx.state_tx.send(PlayerState {
                    status: PlaybackStatus::Playing,
                    source_id: ctx
                        .current_source_id
                        .try_lock()
                        .ok()
                        .and_then(|r| r.clone()),
                    title: None,
                    artist: None,
                    duration_ms: Some(ctx.track_duration_ms.load(Ordering::Relaxed))
                        .filter(|&d| d > 0),
                    position_ms: ctx.position_ms(),
                    volume: f32::from_bits(ctx.volume.load(Ordering::Relaxed)),
                    normalization_enabled: ctx.normalization_enabled.load(Ordering::Relaxed),
                    normalization_gain: f32::from_bits(
                        ctx.normalization_gain.load(Ordering::Relaxed),
                    ),
                    normalization_source: String::new(),
                });
            }
            frame.fill(silence);
            continue;
        }

        if ready_frames == 0 {
            if finished {
                // Track ended naturally – notify engine thread and clear state.
                let ended = TrackEndedEvent {
                    source_id: ctx
                        .current_source_id
                        .try_lock()
                        .ok()
                        .and_then(|g| g.clone())
                        .unwrap_or_default(),
                    position_ms: ctx.position_ms(),
                };
                track.stop_requested.store(true, Ordering::Release);
                ctx.status.store(STATUS_STOPPED, Ordering::Release);
                if let Ok(mut t) = ctx.current_track.try_lock() {
                    *t = None;
                }
                if let Ok(mut id) = ctx.current_source_id.try_lock() {
                    *id = None;
                }
                ctx.track_duration_ms.store(0, Ordering::Relaxed);
                let _ = ctx.track_ended_tx.send(ended);
                track_ended = true;
            } else {
                // Buffer underrun – go back to Loading.
                ctx.diagnostics.underruns.fetch_add(1, Ordering::Relaxed);
                ctx.status.store(STATUS_LOADING, Ordering::Release);
                let _ = ctx.state_tx.send(PlayerState {
                    status: PlaybackStatus::Loading,
                    source_id: None,
                    title: None,
                    artist: None,
                    duration_ms: None,
                    position_ms: ctx.position_ms(),
                    volume: f32::from_bits(ctx.volume.load(Ordering::Relaxed)),
                    normalization_enabled: ctx.normalization_enabled.load(Ordering::Relaxed),
                    normalization_gain: f32::from_bits(
                        ctx.normalization_gain.load(Ordering::Relaxed),
                    ),
                    normalization_source: String::new(),
                });
            }
            frame.fill(silence);
            continue;
        }

        let vol = {
            let user_vol = f32::from_bits(ctx.volume.load(Ordering::Relaxed));
            if ctx.normalization_enabled.load(Ordering::Relaxed) {
                let norm = f32::from_bits(ctx.normalization_gain.load(Ordering::Relaxed));
                user_vol * norm
            } else {
                user_vol
            }
        };
        for s in frame.iter_mut() {
            let raw = consumer.pop().unwrap_or(0.0) * vol;
            *s = convert(raw);
        }

        let prev = ctx.output_frame_position.fetch_add(1, Ordering::Relaxed);
        let new_position_ms = (prev + 1).saturating_mul(1000)
            / u64::from(ctx.output_sample_rate.load(Ordering::Relaxed));
        if !emitted_pos
            && new_position_ms
                >= ctx
                    .last_position_emit_ms
                    .load(Ordering::Relaxed)
                    .saturating_add(250)
        {
            ctx.last_position_emit_ms
                .store(new_position_ms, Ordering::Relaxed);
            let _ = ctx.position_tx.send(new_position_ms);
            emitted_pos = true;
        }
    }
}

enum AudioDecoder {
    Symphonia(Box<dyn symphonia::core::codecs::audio::AudioDecoder>),
    /// Direct libopus decoder used for OGG/Opus files, which symphonia has no codec support for.
    #[cfg(feature = "opus")]
    Opus {
        decoder: OpusDecoder,
        channels: usize,
    },
}

struct LocalFileSource {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: AudioDecoder,
    track_id: u32,
    sample_rate: u32,
    channels: u16,
    duration_ms: u64,
    /// Samples buffered during open() when a packet had to be decoded to discover the audio spec.
    /// Drained by the first call to decode_next().
    pending: Vec<f32>,
}

impl LocalFileSource {
    fn open(path: &Path) -> Result<Self> {
        tracing::info!(path = %path.display(), "Opening local file source");
        let file = File::open(path)?;
        match Self::probe_file(path, file) {
            Ok(source) => Ok(source),
            Err(first_err) => {
                // Some files have malformed ID3v2 headers (e.g. flag bits not cleared) that
                // cause symphonia's probe to fail even though the audio data is fine.  Retry
                // by skipping the ID3v2 block entirely so symphonia sees only raw MP3 frames.
                let msg = format!("{first_err:#}");
                if msg.contains("id3v2") || msg.contains("malformed") {
                    tracing::warn!(
                        path = %path.display(),
                        error = %first_err,
                        "Retrying after skipping malformed ID3v2 header"
                    );
                    let mut file2 = File::open(path)?;
                    if let Some(offset) = id3v2_end_offset(&mut file2) {
                        use std::io::Seek;
                        file2.seek(std::io::SeekFrom::Start(offset))?;
                        return Self::probe_file(path, file2);
                    }
                }
                if let Some(segment) = open_wave_mp3_payload(path)? {
                    tracing::warn!(
                        path = %path.display(),
                        error = %first_err,
                        "Retrying by decoding MP3 payload from RIFF/WAVE wrapper"
                    );
                    return Self::probe_media_source(path, segment, Some("mp3"));
                }
                Err(first_err)
            }
        }
    }

    fn probe_file(path: &Path, file: File) -> Result<Self> {
        Self::probe_media_source(path, file, None)
    }

    fn probe_media_source<M>(
        path: &Path,
        media_source: M,
        force_extension: Option<&str>,
    ) -> Result<Self>
    where
        M: symphonia::core::io::MediaSource + 'static,
    {
        let format = shared_probe_media_source(path, media_source, force_extension)?;
        let (track, codec_params) = default_audio_track(format.as_ref())
            .ok_or_else(|| anyhow!("No supported audio track found"))?;
        let track_id = track.id;
        let sample_rate = codec_params.sample_rate;
        let channels = codec_params.channels.as_ref().map(|c| c.count() as u16);
        let duration_ms = match (track.num_frames, codec_params.sample_rate) {
            (Some(frame_count), Some(rate)) if rate > 0 => {
                frame_count.saturating_mul(1000) / u64::from(rate)
            }
            _ => 0,
        };
        let codec = codec_params.codec;

        // Symphonia has no Opus codec; when the feature is enabled, use libopus directly for
        // packet decoding.  The OGG format reader above still handles container/packet extraction.
        #[cfg(feature = "opus")]
        let decoder = if codec == CODEC_ID_OPUS {
            let n_channels = codec_params
                .channels
                .as_ref()
                .map(|c| c.count())
                .unwrap_or(2);
            let opus_channels = if n_channels == 1 {
                opus::Channels::Mono
            } else {
                opus::Channels::Stereo
            };
            AudioDecoder::Opus {
                decoder: OpusDecoder::new(48_000, opus_channels)
                    .map_err(|e| anyhow!("Failed to create Opus decoder: {e}"))?,
                channels: n_channels,
            }
        } else {
            AudioDecoder::Symphonia(
                make_audio_decoder(codec_params)
                    .map_err(|_| anyhow!("Unsupported audio codec: {}", codec_type_name(codec)))?,
            )
        };

        #[cfg(not(feature = "opus"))]
        let decoder = AudioDecoder::Symphonia(make_audio_decoder(codec_params).map_err(|_| {
            if codec == CODEC_ID_OPUS {
                anyhow!("Opus codec not supported (rebuild with the 'opus' feature and libopus)")
            } else {
                anyhow!("Unsupported audio codec: {}", codec_type_name(codec))
            }
        })?);
        // track borrow of format ends here (NLL)

        // Opus always decodes at 48 kHz; use the libopus channel count rather than whatever
        // the container header says (which is the *input* sample rate, not the output rate).
        let (effective_sample_rate, effective_channels) = match &decoder {
            #[cfg(feature = "opus")]
            AudioDecoder::Opus { channels, .. } => (48_000u32, *channels as u16),
            AudioDecoder::Symphonia(_) => (sample_rate.unwrap_or(0), channels.unwrap_or(0)),
        };

        let mut source = Self {
            format,
            decoder,
            track_id,
            sample_rate: effective_sample_rate,
            channels: effective_channels,
            duration_ms,
            pending: Vec::new(),
        };

        // Always decode one packet to discover the real sample_rate / channels from the
        // decoder output rather than from container metadata.  This is essential for codecs
        // where the container and codec disagree: for example, HE-AAC (SBR) files report
        // the post-SBR rate (e.g. 44100) in the container, but symphonia's AAC decoder
        // only decodes the core at half that rate (e.g. 22050).
        //
        // Non-Symphonia decoders (Opus) set their effective rate/channels explicitly above
        // and don't need priming.
        if matches!(source.decoder, AudioDecoder::Symphonia(_)) {
            source.prime_spec()?;
        }

        Ok(source)
    }

    /// Decode the first decodable packet to discover sample_rate / channels, storing
    /// the resulting samples in `pending` so they aren't lost.
    fn prime_spec(&mut self) -> Result<()> {
        loop {
            let Some(packet) = self.format.next_packet()? else {
                return Err(
                    SymphoniaError::IoError(std::io::ErrorKind::UnexpectedEof.into()).into(),
                );
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match &mut self.decoder {
                AudioDecoder::Symphonia(dec) => match dec.decode(&packet) {
                    Ok(decoded) => {
                        let spec = decoded.spec();
                        // Always use the decoded spec — container metadata may be wrong
                        // (e.g. HE-AAC reports post-SBR rate 44100 but decoder outputs
                        // at the core rate 22050).  Trust what the decoder actually produces.
                        self.sample_rate = spec.rate();
                        self.channels = spec.channels().count() as u16;
                        append_audio_buffer(decoded, &mut self.pending);
                        return Ok(());
                    }
                    Err(SymphoniaError::DecodeError(_)) => continue,
                    Err(e) => return Err(e.into()),
                },
                #[cfg(feature = "opus")]
                AudioDecoder::Opus { decoder, channels } => {
                    let n_ch = *channels;
                    let mut buf = vec![0.0f32; 5760 * n_ch];
                    match decoder.decode_float(&packet.data, &mut buf, false) {
                        Ok(n_frames) => {
                            buf.truncate(n_frames * n_ch);
                            // sample_rate and channels are already set for Opus; just save samples.
                            self.pending.append(&mut buf);
                            return Ok(());
                        }
                        Err(_) => continue,
                    }
                }
            }
        }
    }

    /// Decode the next chunk of interleaved f32 samples.  Returns `None` at end-of-stream.
    /// All format- and codec-specific concerns (packet filtering, decode errors, pending
    /// buffers) are handled here; callers see a uniform stream of sample chunks.
    fn decode_next(&mut self) -> Result<Option<Vec<f32>>> {
        if !self.pending.is_empty() {
            return Ok(Some(std::mem::take(&mut self.pending)));
        }
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(None),
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None);
                }
                Err(SymphoniaError::ResetRequired) => {
                    return Err(anyhow!("Decoder reset required during playback"));
                }
                Err(e) => return Err(e.into()),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match &mut self.decoder {
                AudioDecoder::Symphonia(dec) => match dec.decode(&packet) {
                    Ok(decoded) => {
                        let mut samples = Vec::new();
                        append_audio_buffer(decoded, &mut samples);
                        return Ok(Some(samples));
                    }
                    Err(SymphoniaError::DecodeError(_)) => continue,
                    Err(SymphoniaError::IoError(e))
                        if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                    {
                        return Ok(None);
                    }
                    Err(e) => return Err(e.into()),
                },
                #[cfg(feature = "opus")]
                AudioDecoder::Opus { decoder, channels } => {
                    let n_ch = *channels;
                    let mut buf = vec![0.0f32; 5760 * n_ch];
                    match decoder.decode_float(&packet.data, &mut buf, false) {
                        Ok(n_frames) => {
                            buf.truncate(n_frames * n_ch);
                            return Ok(Some(buf));
                        }
                        Err(_) => continue, // skip header / corrupt packets
                    }
                }
            }
        }
    }

    fn seek_to_ms(&mut self, position_ms: u64) -> Result<()> {
        self.pending.clear();
        let time = Time::from_millis_u64(position_ms);
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map(|_| ())
            .map_err(Into::into)
    }
}

fn append_audio_buffer(decoded: GenericAudioBufferRef<'_>, samples: &mut Vec<f32>) {
    let mut interleaved = Vec::new();
    decoded.copy_to_vec_interleaved(&mut interleaved);
    samples.extend_from_slice(&interleaved);
}

struct StreamResampler {
    source_rate: u32,
    source_channels: usize,
    output_rate: u32,
    output_channels: usize,
    pending_source: Vec<f32>,
    source_frame_offset: u64,
    next_output_frame: u64,
}

impl StreamResampler {
    fn new(
        source_rate: u32,
        source_channels: u16,
        output_rate: u32,
        output_channels: u16,
        start_ms: u64,
    ) -> Self {
        let next_output_frame = start_ms.saturating_mul(u64::from(output_rate)) / 1000;
        let source_frame_offset = start_ms.saturating_mul(u64::from(source_rate)) / 1000;

        Self {
            source_rate,
            source_channels: source_channels as usize,
            output_rate,
            output_channels: output_channels as usize,
            pending_source: Vec::new(),
            source_frame_offset,
            next_output_frame,
        }
    }

    fn push(&mut self, input: &[f32]) -> Vec<f32> {
        self.pending_source.extend_from_slice(input);
        self.produce_available()
    }

    fn finish(&mut self) -> Vec<f32> {
        self.produce_available()
    }

    fn produce_available(&mut self) -> Vec<f32> {
        let pending_frames = self.pending_source.len() / self.source_channels;
        let max_source_frame = self.source_frame_offset + pending_frames as u64;
        let mut output = Vec::new();

        while self.required_source_frame() < max_source_frame {
            let source_frame = self.required_source_frame();
            let local_frame = (source_frame - self.source_frame_offset) as usize;

            for output_channel in 0..self.output_channels {
                let source_channel = if self.source_channels == 1 {
                    0
                } else {
                    output_channel.min(self.source_channels - 1)
                };
                let sample_index = local_frame * self.source_channels + source_channel;
                output.push(*self.pending_source.get(sample_index).unwrap_or(&0.0));
            }

            self.next_output_frame = self.next_output_frame.saturating_add(1);
        }

        let drop_frames = self
            .required_source_frame()
            .saturating_sub(self.source_frame_offset);
        if drop_frames > 0 {
            let drop_samples = (drop_frames as usize) * self.source_channels;
            self.pending_source
                .drain(0..drop_samples.min(self.pending_source.len()));
            self.source_frame_offset = self.source_frame_offset.saturating_add(drop_frames);
        }

        output
    }

    fn required_source_frame(&self) -> u64 {
        self.next_output_frame
            .saturating_mul(u64::from(self.source_rate))
            / u64::from(self.output_rate)
    }
}

fn codec_type_name(codec: AudioCodecId) -> &'static str {
    use symphonia::core::codecs::audio::well_known::*;
    match codec {
        CODEC_ID_OPUS => "Opus",
        CODEC_ID_VORBIS => "Vorbis",
        CODEC_ID_FLAC => "FLAC",
        CODEC_ID_MP3 => "MP3",
        CODEC_ID_AAC => "AAC",
        CODEC_ID_ALAC => "ALAC",
        CODEC_ID_PCM_S16LE | CODEC_ID_PCM_S24LE | CODEC_ID_PCM_S32LE | CODEC_ID_PCM_S16BE
        | CODEC_ID_PCM_S24BE | CODEC_ID_PCM_S32BE | CODEC_ID_PCM_F32LE | CODEC_ID_PCM_F64LE => {
            "PCM"
        }
        _ => "unknown",
    }
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            source_id: None,
            title: None,
            artist: None,
            duration_ms: None,
            position_ms: 0,
            volume: 1.0,
            normalization_enabled: false,
            normalization_gain: 1.0,
            normalization_source: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_callback_ctx_normalization_defaults() {
        let (ptx, _prx) = mpsc::channel();
        let (stx, _srx) = mpsc::channel();
        let (ttx, _trx) = mpsc::channel();
        let ctx = AudioCallbackCtx::new(44100, 2, ptx, stx, ttx);
        assert_eq!(
            f32::from_bits(ctx.normalization_gain.load(Ordering::Relaxed)),
            1.0
        );
        assert!(!ctx.normalization_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn test_shared_state_normalization_source_default() {
        let inhibitor = Arc::new(SleepInhibitor::new("test", "test"));
        let state = SharedState::new(inhibitor);
        assert_eq!(state.normalization_source, "None");
    }

    #[test]
    fn test_shared_state_clear_track_resets_normalization_source() {
        let inhibitor = Arc::new(SleepInhibitor::new("test", "test"));
        let mut state = SharedState::new(inhibitor);
        state.normalization_source = "ReplayGain".into();

        let (ptx, _prx) = mpsc::channel();
        let (stx, _srx) = mpsc::channel();
        let (ttx, _trx) = mpsc::channel();
        let ctx = AudioCallbackCtx::new(44100, 2, ptx, stx, ttx);

        state.clear_track(&ctx);
        assert_eq!(state.normalization_source, "None");
    }

    #[test]
    fn test_player_state_default_normalization_fields() {
        let state = PlayerState::default();
        assert!(!state.normalization_enabled);
        assert_eq!(state.normalization_gain, 1.0);
        assert_eq!(state.normalization_source, "");
    }

    /// Verify that `sample_rate` in the source matches what the decoder actually
    /// produces from the bitstream, not necessarily what the container metadata
    /// reports.  This is a regression test for HE-AAC (SBR), where the container
    /// says 44100 Hz (post-SBR output) but symphonia's AAC decoder only decodes
    /// the core at 22050 Hz.  Using the container rate causes 2× playback speed.
    #[test]
    fn test_source_sample_rate_matches_decoded_spec() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/he-aac.m4a"
        ));
        if !path.exists() {
            eprintln!(
                "skipping: HE-AAC test fixture not found at {}",
                path.display()
            );
            return;
        }

        let mut source = LocalFileSource::open(path).expect("open HE-AAC fixture");
        assert!(
            source.sample_rate > 0,
            "sample_rate must be set from decoded spec"
        );
        assert!(
            source.channels > 0,
            "channels must be set from decoded spec"
        );

        // Decode one more packet to get the decoded spec independently.
        let packet = source
            .format
            .next_packet()
            .expect("read packet from HE-AAC file")
            .expect("HE-AAC file has a second packet");
        let decoded = match &mut source.decoder {
            AudioDecoder::Symphonia(dec) => dec.decode(&packet).expect("decode packet"),
            _ => panic!("HE-AAC file should use Symphonia decoder"),
        };
        let spec = decoded.spec().clone();

        assert_eq!(
            source.sample_rate,
            spec.rate(),
            "source sample_rate must equal decoded spec rate, \
             not container metadata (HE-AAC file has container rate 44100, \
             decoder core rate {})",
            spec.rate(),
        );
        assert_eq!(
            source.channels as u16,
            spec.channels().count() as u16,
            "source channels must equal decoded spec channel count"
        );
    }

    /// Verify that a file with a corrupt WXXX frame in the ID3v2 tag can still
    /// be opened via the retry logic in `LocalFileSource::open()`, which skips
    /// the malformed ID3v2 header and re-probes from the raw MP3 frames.
    ///
    /// Regression test: the retry condition is checked against the full anyhow
    /// error chain (`format!("{err:#}")`), not just the outermost display
    /// message (`to_string()`) which omits the underlying symphonia error.
    #[test]
    fn test_corrupt_id3v2_wxxx_file_opens() {
        let home = option_env!("HOME");
        let path_str = home
            .map(|h| format!("{h}/Music/!Full Albums/Avicii - 2013 - True/True (07) Avicii - Shame On Me.mp3"))
            .unwrap_or_default();
        let path = Path::new(&path_str);
        if !path.exists() {
            eprintln!("skipping: test fixture not found at {}", path.display());
            return;
        }

        let source_result = LocalFileSource::open(path);
        assert!(
            source_result.is_ok(),
            "LocalFileSource::open() should succeed, got: {:#}",
            source_result.err().unwrap()
        );
        let source = source_result.unwrap();
        assert!(source.sample_rate > 0, "sample_rate should be set");
        assert!(source.channels > 0, "channels should be set");
    }

    const TEST_CHANNELS: usize = 2;

    struct CallbackHarness {
        ctx: AudioCallbackCtx,
        track: Arc<TrackBuffer>,
        producer: Producer<f32>,
        track_ended: mpsc::Receiver<TrackEndedEvent>,
    }

    impl CallbackHarness {
        fn new(status: u8) -> Self {
            let (ptx, _prx) = mpsc::channel();
            let (stx, _srx) = mpsc::channel();
            let (ttx, track_ended) = mpsc::channel();
            let ctx = AudioCallbackCtx::new(48_000, 2, ptx, stx, ttx);
            let (track, producer) = TrackBuffer::new(1_000, TEST_CHANNELS);
            *ctx.current_track.lock().unwrap() = Some(Arc::clone(&track));
            ctx.status.store(status, Ordering::Release);
            Self {
                ctx,
                track,
                producer,
                track_ended,
            }
        }

        /// Pushes `frames` frames whose samples count up from `first`.
        fn push_ramp(&mut self, first: usize, frames: usize) {
            let samples: Vec<f32> = (first..first + frames * TEST_CHANNELS)
                .map(|i| i as f32)
                .collect();
            assert!(push_samples(&mut self.producer, &self.track, &samples));
        }

        fn finish(&self) {
            self.track.finished.store(true, Ordering::Release);
        }

        fn render(&self, frames: usize) -> Vec<f32> {
            let mut out = vec![f32::NAN; frames * TEST_CHANNELS];
            write_output_data(&mut out, &self.ctx, |s| s);
            out
        }
    }

    #[test]
    fn playing_track_outputs_pushed_samples_in_order() {
        let mut h = CallbackHarness::new(STATUS_PLAYING);
        h.push_ramp(1, 4);
        let out = h.render(4);
        assert_eq!(out, (1..=8).map(|i| i as f32).collect::<Vec<_>>());
    }

    #[test]
    fn loading_track_with_too_little_data_is_silent_and_loses_nothing() {
        let mut h = CallbackHarness::new(STATUS_LOADING);
        h.push_ramp(1, 4);

        assert!(h.render(8).iter().all(|&s| s == 0.0));
        assert_eq!(h.ctx.status.load(Ordering::Acquire), STATUS_LOADING);

        h.finish();
        let out = h.render(8);
        let audible: Vec<f32> = out.iter().copied().filter(|&s| s != 0.0).collect();
        assert_eq!(audible, (1..=8).map(|i| i as f32).collect::<Vec<_>>());
    }

    #[test]
    fn starved_playing_track_reports_underrun_and_returns_to_loading() {
        let h = CallbackHarness::new(STATUS_PLAYING);
        assert!(h.render(8).iter().all(|&s| s == 0.0));
        assert_eq!(h.ctx.status.load(Ordering::Acquire), STATUS_LOADING);
        assert_eq!(h.ctx.diagnostics.take().underruns, 1);
    }

    #[test]
    fn finished_track_plays_its_tail_then_signals_end_with_silence_after() {
        let mut h = CallbackHarness::new(STATUS_PLAYING);
        h.push_ramp(1, 3);
        h.finish();

        let out = h.render(8);
        assert_eq!(&out[..6], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert!(out[6..].iter().all(|&s| s == 0.0));
        assert_eq!(h.ctx.status.load(Ordering::Acquire), STATUS_STOPPED);
        assert!(h.track_ended.try_recv().is_ok());
        assert!(h.track_ended.try_recv().is_err(), "end is signalled once");
    }

    #[test]
    fn contended_track_handle_yields_silent_callback_and_is_counted() {
        let mut h = CallbackHarness::new(STATUS_PLAYING);
        h.push_ramp(1, 4);
        let held = h.ctx.current_track.lock().unwrap();
        assert!(h.render(4).iter().all(|&s| s == 0.0));
        drop(held);
        assert_eq!(h.ctx.diagnostics.take().lock_misses, 1);

        assert_eq!(h.render(1), vec![1.0, 2.0], "samples were not consumed");
    }

    #[test]
    fn push_samples_waits_for_the_consumer_and_delivers_everything() {
        let (track, mut producer) = TrackBuffer::new(4, 1);
        let expected: Vec<f32> = (0..50).map(|i| i as f32).collect();
        let sent = expected.clone();
        let sender_track = Arc::clone(&track);
        let sender = thread::spawn(move || push_samples(&mut producer, &sender_track, &sent));

        let mut received = Vec::new();
        while received.len() < expected.len() {
            if let Ok(sample) = track.consumer.lock().unwrap().pop() {
                received.push(sample);
            }
        }
        assert!(sender.join().unwrap());
        assert_eq!(received, expected);
    }

    #[test]
    fn push_samples_gives_up_when_stop_is_requested_while_full() {
        let (track, mut producer) = TrackBuffer::new(2, 1);
        track.stop_requested.store(true, Ordering::Release);
        assert!(!push_samples(&mut producer, &track, &[0.0; 10]));
    }

    #[test]
    fn glitch_reports_do_not_force_a_stream_rebuild() {
        assert!(!requires_stream_rebuild(cpal::ErrorKind::Xrun));
        assert!(!requires_stream_rebuild(cpal::ErrorKind::RealtimeDenied));
        assert!(requires_stream_rebuild(cpal::ErrorKind::DeviceNotAvailable));
    }

    #[test]
    fn taking_diagnostics_resets_them() {
        let diagnostics = AudioDiagnostics::default();
        diagnostics.underruns.fetch_add(2, Ordering::Relaxed);
        assert!(diagnostics.take().has_glitches());
        assert!(!diagnostics.take().has_glitches());
    }
}
