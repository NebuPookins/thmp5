use crate::audio_probe::{
    id3v2_end_offset, open_wave_mp3_payload, probe_media_source as shared_probe_media_source,
};
use crate::file_issues::FileIssueLog;
use crate::models::{PlaybackStatus, PlayerState};
use crate::sleep_inhibitor::SleepInhibitor;
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
#[cfg(feature = "opus")]
use opus::Decoder as OpusDecoder;
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::File;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::{CodecType, DecoderOptions, CODEC_TYPE_OPUS};
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
    pub recording_id: String,
    pub source_id: String,
    pub file_path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub normalization_gain: f32,
    pub normalization_source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackEndedEvent {
    pub recording_id: String,
    pub source_id: String,
    pub position_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerErrorEvent {
    pub message: String,
}

/// Lock-free state accessible from the real-time cpal audio callback.
/// The engine thread writes hot-path fields through atomics; the callback
/// reads them without acquiring `SharedState`'s mutex.  The `current_track`
/// buffer is accessed via `try_lock` with a silence fallback.
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
    current_track: Mutex<Option<Arc<Mutex<TrackBuffer>>>>,
    /// Metadata snapshot used by the callback to build track-ended events.
    current_recording_id: Mutex<Option<String>>,
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
            current_track: Mutex::new(None),
            current_recording_id: Mutex::new(None),
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
    pub fn new(app: AppHandle, file_issues: FileIssueLog) -> Result<Self> {
        let sleep_inhibitor = Arc::new(SleepInhibitor::new("thmp5", "Music playback in progress"));
        let shared = Arc::new(Mutex::new(SharedState::new(sleep_inhibitor)));
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
                let host = cpal::default_host();
                let mut stream: Option<cpal::Stream> = None;
                let events = Some(events);
                tracing::info!("Audio engine thread started");

                loop {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(command) => {
                            if matches!(command, AudioCommand::Play(_) | AudioCommand::Resume) {
                                if let Err(error) = ensure_output_stream(
                                    &host,
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

                    // Drain event channels from the cpal callback.
                    if let Some(ref ev) = events {
                        drain_events(&command_shared, &command_ctx, ev, &app);
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
        let recording_id = ctx.current_recording_id.lock().ok().and_then(|r| r.clone());
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
            recording_id,
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
        }
    }

    fn stop_decoder(&self, ctx: &AudioCallbackCtx) {
        if let Ok(track) = ctx.current_track.lock() {
            if let Some(buffer) = track.as_ref() {
                if let Ok(buf) = buffer.lock() {
                    buf.stop_requested.store(true, Ordering::Release);
                }
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
        if let Ok(mut id) = ctx.current_recording_id.lock() {
            *id = None;
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
    }
}

struct TrackBuffer {
    samples: VecDeque<f32>,
    finished: bool,
    stop_requested: AtomicBool,
}

impl TrackBuffer {
    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            finished: false,
            stop_requested: AtomicBool::new(false),
        }
    }

    fn buffered_frames(&self, channels: usize) -> usize {
        if channels == 0 {
            return 0;
        }

        self.samples.len() / channels
    }
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
                recording_id = %request.recording_id,
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
                    recording_id: ctx
                        .current_recording_id
                        .lock()
                        .ok()
                        .and_then(|g| g.clone())
                        .ok_or_else(|| anyhow!("No active track to seek"))?,
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

    let buffer = Arc::new(Mutex::new(TrackBuffer::new()));
    let duration_ms = source.duration_ms;
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
        let mut guard = ctx.current_recording_id.lock().unwrap();
        *guard = Some(request.recording_id.clone());
    }
    {
        let mut guard = ctx.current_source_id.lock().unwrap();
        *guard = Some(request.source_id.clone());
    }
    ctx.output_frame_position
        .store(current_output_position, Ordering::Relaxed);
    ctx.last_position_emit_ms
        .store(start_ms.saturating_sub(250), Ordering::Relaxed);

    // Update SharedState metadata.
    {
        let mut state = shared.lock().unwrap();
        state.current_title = request.title.clone();
        state.current_artist = request.artist.clone();
        state.current_file_path = Some(request.file_path.clone());
        state.normalization_source = request.normalization_source.clone();
    }

    spawn_decoder_thread(
        source,
        start_ms,
        output_rate,
        output_channels,
        buffer,
        app.clone(),
    );

    Ok(())
}

fn spawn_decoder_thread(
    source: LocalFileSource,
    start_ms: u64,
    output_rate: u32,
    output_channels: u16,
    buffer: Arc<Mutex<TrackBuffer>>,
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
                Arc::clone(&buffer),
            ) {
                if let Ok(mut state) = buffer.lock() {
                    state.finished = true;
                }
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
    buffer: Arc<Mutex<TrackBuffer>>,
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
        let state = buffer
            .lock()
            .map_err(|_| anyhow!("Track buffer lock poisoned"))?;

        if state.stop_requested.load(Ordering::Acquire) {
            tracing::info!("Decoder worker stopping early");
            return Ok(());
        }

        if state.buffered_frames(output_channels as usize) >= MAX_BUFFER_FRAMES {
            drop(state);
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        drop(state);

        match source.decode_next()? {
            None => break,
            Some(input) => {
                let output = resampler.push(&input);
                if !output.is_empty() {
                    let mut state = buffer
                        .lock()
                        .map_err(|_| anyhow!("Track buffer lock poisoned"))?;
                    if state.stop_requested.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    state.samples.extend(output);
                }
            }
        }
    }

    let tail = resampler.finish();
    if !tail.is_empty() {
        let mut state = buffer
            .lock()
            .map_err(|_| anyhow!("Track buffer lock poisoned"))?;
        state.samples.extend(tail);
    }
    {
        let mut state = buffer
            .lock()
            .map_err(|_| anyhow!("Track buffer lock poisoned"))?;
        state.finished = true;
    }
    tracing::info!("Decoder worker finished");
    Ok(())
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
                recording_id: None,
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
    host: &cpal::Host,
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

    let (device, supported_config) = select_output_device(host)?;
    let stream_config = supported_config.config();
    let device_name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
    tracing::info!(
        device = %device_name,
        sample_rate = stream_config.sample_rate.0,
        channels = stream_config.channels,
        format = ?supported_config.sample_format(),
        "Using output device"
    );

    ctx.output_sample_rate
        .store(stream_config.sample_rate.0, Ordering::Relaxed);
    ctx.output_channels
        .store(stream_config.channels, Ordering::Relaxed);

    let output_stream =
        build_output_stream(&device, &supported_config, Arc::clone(ctx), app.clone())?;
    output_stream
        .play()
        .context("Failed to start output stream")?;
    *stream = Some(output_stream);
    clear_engine_error(shared);
    Ok(())
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

    for device in devices {
        match device.default_output_config() {
            Ok(config) => return Ok((device, config)),
            Err(error) => {
                let name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
                tracing::warn!("Skipping output device {name}: {error}");
            }
        }
    }

    Err(anyhow!("No usable output audio device is available"))
}

fn build_output_stream(
    device: &cpal::Device,
    supported_config: &cpal::SupportedStreamConfig,
    ctx: Arc<AudioCallbackCtx>,
    app: AppHandle,
) -> Result<cpal::Stream> {
    let config = supported_config.config();

    match supported_config.sample_format() {
        cpal::SampleFormat::F32 => {
            let ctx_ref = Arc::clone(&ctx);
            let app_for_err = app.clone();
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [f32], _| write_output_data_f32(data, &ctx_ref),
                    move |error| emit_error(&app_for_err, format!("Audio stream error: {error}")),
                    None,
                )
                .context("Failed to build f32 output stream")
        }
        cpal::SampleFormat::I16 => {
            let ctx_ref = Arc::clone(&ctx);
            let app_for_err = app.clone();
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [i16], _| write_output_data_i16(data, &ctx_ref),
                    move |error| emit_error(&app_for_err, format!("Audio stream error: {error}")),
                    None,
                )
                .context("Failed to build i16 output stream")
        }
        cpal::SampleFormat::U16 => {
            let ctx_ref = Arc::clone(&ctx);
            let app_for_err = app;
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [u16], _| write_output_data_u16(data, &ctx_ref),
                    move |error| emit_error(&app_for_err, format!("Audio stream error: {error}")),
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
    let output_channels = ctx.output_channels.load(Ordering::Relaxed) as usize;
    if output_channels == 0 {
        for s in output.iter_mut() {
            *s = convert(0.0);
        }
        return;
    }

    let mut emitted_pos = false;

    for frame in output.chunks_mut(output_channels) {
        let vol = {
            let user_vol = f32::from_bits(ctx.volume.load(Ordering::Relaxed));
            if ctx.normalization_enabled.load(Ordering::Relaxed) {
                let norm = f32::from_bits(ctx.normalization_gain.load(Ordering::Relaxed));
                user_vol * norm
            } else {
                user_vol
            }
        };
        let status = ctx.status.load(Ordering::Relaxed);
        // Try to acquire the current track buffer without blocking.
        let track_buffer = match ctx.current_track.try_lock() {
            Ok(guard) => match guard.as_ref() {
                Some(buf) => Arc::clone(buf),
                None => {
                    for s in frame.iter_mut() {
                        *s = convert(0.0);
                    }
                    continue;
                }
            },
            Err(_) => {
                // Contended — play silence rather than blocking the audio thread.
                for s in frame.iter_mut() {
                    *s = convert(0.0);
                }
                continue;
            }
        };

        let mut buffer = match track_buffer.try_lock() {
            Ok(buf) => buf,
            Err(_) => {
                for s in frame.iter_mut() {
                    *s = convert(0.0);
                }
                continue;
            }
        };

        if status == STATUS_PAUSED {
            for s in frame.iter_mut() {
                *s = convert(0.0);
            }
            continue;
        }

        let ready_frames = buffer.buffered_frames(output_channels);

        // Transition from Loading → Playing once we have enough data.
        if status == STATUS_LOADING
            && (ready_frames >= PREBUFFER_FRAMES || (buffer.finished && ready_frames > 0))
        {
            drop(buffer);
            ctx.status.store(STATUS_PLAYING, Ordering::Release);
            let _ = ctx.state_tx.send(PlayerState {
                status: PlaybackStatus::Playing,
                recording_id: ctx
                    .current_recording_id
                    .try_lock()
                    .ok()
                    .and_then(|r| r.clone()),
                source_id: ctx
                    .current_source_id
                    .try_lock()
                    .ok()
                    .and_then(|r| r.clone()),
                title: None,
                artist: None,
                duration_ms: Some(ctx.track_duration_ms.load(Ordering::Relaxed)).filter(|&d| d > 0),
                position_ms: ctx.position_ms(),
                volume: f32::from_bits(ctx.volume.load(Ordering::Relaxed)),
                normalization_enabled: ctx.normalization_enabled.load(Ordering::Relaxed),
                normalization_gain: f32::from_bits(ctx.normalization_gain.load(Ordering::Relaxed)),
                normalization_source: String::new(),
            });
            for s in frame.iter_mut() {
                *s = convert(0.0);
            }
            continue;
        }

        if status == STATUS_PLAYING && ready_frames == 0 {
            if buffer.finished {
                // Track ended naturally – notify engine thread and clear state.
                let ended = TrackEndedEvent {
                    recording_id: ctx
                        .current_recording_id
                        .try_lock()
                        .ok()
                        .and_then(|g| g.clone())
                        .unwrap_or_default(),
                    source_id: ctx
                        .current_source_id
                        .try_lock()
                        .ok()
                        .and_then(|g| g.clone())
                        .unwrap_or_default(),
                    position_ms: ctx.position_ms(),
                };
                // We're holding the TrackBuffer lock, but stop_requested is
                // an AtomicBool so the decoder can check it independently.
                buffer.stop_requested.store(true, Ordering::Release);
                drop(buffer);
                // Clear callback-side track state.
                ctx.status.store(STATUS_STOPPED, Ordering::Release);
                if let Ok(mut t) = ctx.current_track.try_lock() {
                    *t = None;
                }
                if let Ok(mut id) = ctx.current_recording_id.try_lock() {
                    *id = None;
                }
                if let Ok(mut id) = ctx.current_source_id.try_lock() {
                    *id = None;
                }
                ctx.track_duration_ms.store(0, Ordering::Relaxed);
                let _ = ctx.track_ended_tx.send(ended);
                for s in frame.iter_mut() {
                    *s = convert(0.0);
                }
                break;
            }

            // Buffer underrun – go back to Loading.
            drop(buffer);
            ctx.status.store(STATUS_LOADING, Ordering::Release);
            let _ = ctx.state_tx.send(PlayerState {
                status: PlaybackStatus::Loading,
                recording_id: None,
                source_id: None,
                title: None,
                artist: None,
                duration_ms: None,
                position_ms: ctx.position_ms(),
                volume: f32::from_bits(ctx.volume.load(Ordering::Relaxed)),
                normalization_enabled: ctx.normalization_enabled.load(Ordering::Relaxed),
                normalization_gain: f32::from_bits(ctx.normalization_gain.load(Ordering::Relaxed)),
                normalization_source: String::new(),
            });
            for s in frame.iter_mut() {
                *s = convert(0.0);
            }
            continue;
        }

        // Read and convert samples.
        for s in frame.iter_mut() {
            let raw = buffer.samples.pop_front().unwrap_or(0.0) * vol;
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
    Symphonia(Box<dyn symphonia::core::codecs::Decoder>),
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
        let probed = shared_probe_media_source(path, media_source, force_extension)?;
        let format = probed.format;
        let track = format
            .default_track()
            .ok_or_else(|| anyhow!("No supported audio track found"))?;
        let track_id = track.id;
        let sample_rate = track.codec_params.sample_rate;
        let channels = track.codec_params.channels.map(|c| c.count() as u16);
        let duration_ms = match (track.codec_params.n_frames, track.codec_params.sample_rate) {
            (Some(frame_count), Some(rate)) if rate > 0 => {
                frame_count.saturating_mul(1000) / u64::from(rate)
            }
            _ => 0,
        };
        let codec = track.codec_params.codec;

        // Symphonia has no Opus codec; when the feature is enabled, use libopus directly for
        // packet decoding.  The OGG format reader above still handles container/packet extraction.
        #[cfg(feature = "opus")]
        let decoder = if codec == CODEC_TYPE_OPUS {
            let n_channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);
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
                symphonia::default::get_codecs()
                    .make(&track.codec_params, &DecoderOptions::default())
                    .map_err(|_| anyhow!("Unsupported audio codec: {}", codec_type_name(codec)))?,
            )
        };

        #[cfg(not(feature = "opus"))]
        let decoder = AudioDecoder::Symphonia(
            symphonia::default::get_codecs()
                .make(&track.codec_params, &DecoderOptions::default())
                .map_err(|_| {
                    if codec == CODEC_TYPE_OPUS {
                        anyhow!(
                            "Opus codec not supported (rebuild with the 'opus' feature and libopus)"
                        )
                    } else {
                        anyhow!("Unsupported audio codec: {}", codec_type_name(codec))
                    }
                })?,
        );
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
            let packet = self.format.next_packet()?;
            if packet.track_id() != self.track_id {
                continue;
            }
            match &mut self.decoder {
                AudioDecoder::Symphonia(dec) => match dec.decode(&packet) {
                    Ok(decoded) => {
                        let spec = *decoded.spec();
                        // Always use the decoded spec — container metadata may be wrong
                        // (e.g. HE-AAC reports post-SBR rate 44100 but decoder outputs
                        // at the core rate 22050).  Trust what the decoder actually produces.
                        self.sample_rate = spec.rate;
                        self.channels = spec.channels.count() as u16;
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
                Ok(p) => p,
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
            if packet.track_id() != self.track_id {
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
        let time = Time::from(position_ms as f64 / 1000.0);
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

fn append_audio_buffer(decoded: AudioBufferRef<'_>, samples: &mut Vec<f32>) {
    let mut interleaved = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
    interleaved.copy_interleaved_ref(decoded);
    samples.extend_from_slice(interleaved.samples());
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

fn codec_type_name(codec: CodecType) -> &'static str {
    use symphonia::core::codecs::*;
    match codec {
        CODEC_TYPE_OPUS => "Opus",
        CODEC_TYPE_VORBIS => "Vorbis",
        CODEC_TYPE_FLAC => "FLAC",
        CODEC_TYPE_MP3 => "MP3",
        CODEC_TYPE_AAC => "AAC",
        CODEC_TYPE_ALAC => "ALAC",
        CODEC_TYPE_PCM_S16LE | CODEC_TYPE_PCM_S24LE | CODEC_TYPE_PCM_S32LE
        | CODEC_TYPE_PCM_S16BE | CODEC_TYPE_PCM_S24BE | CODEC_TYPE_PCM_S32BE
        | CODEC_TYPE_PCM_F32LE | CODEC_TYPE_PCM_F64LE => "PCM",
        _ => "unknown",
    }
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            recording_id: None,
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
            .expect("read packet from HE-AAC file");
        let decoded = match &mut source.decoder {
            AudioDecoder::Symphonia(dec) => dec.decode(&packet).expect("decode packet"),
            _ => panic!("HE-AAC file should use Symphonia decoder"),
        };
        let spec = *decoded.spec();

        assert_eq!(
            source.sample_rate, spec.rate,
            "source sample_rate must equal decoded spec rate, \
             not container metadata (HE-AAC file has container rate 44100, \
             decoder core rate {})",
            spec.rate,
        );
        assert_eq!(
            source.channels as u16,
            spec.channels.count() as u16,
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
}
