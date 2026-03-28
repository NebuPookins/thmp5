use crate::file_issues::FileIssueLog;
use crate::models::{PlaybackStatus, PlayerState};
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;
use tauri::{AppHandle, Emitter};

pub const PLAYER_STATE_EVENT: &str = "player-state";
pub const PLAYER_POSITION_EVENT: &str = "player-position";
pub const PLAYER_TRACK_ENDED_EVENT: &str = "player-track-ended";
pub const PLAYER_ERROR_EVENT: &str = "player-error";

const PREBUFFER_FRAMES: usize = 8_192;
const MAX_BUFFER_FRAMES: usize = 96_000;

#[derive(Debug, Clone)]
pub struct PlayRequest {
    pub recording_id: String,
    pub source_id: String,
    pub file_path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
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

enum AudioCommand {
    Play(PlayRequest),
    Pause,
    Resume,
    Seek(u64),
    SetVolume(f32),
    Stop,
}

#[derive(Clone)]
pub struct AudioEngineHandle {
    tx: Sender<AudioCommand>,
    shared: Arc<Mutex<SharedState>>,
}

impl AudioEngineHandle {
    pub fn new(app: AppHandle, file_issues: FileIssueLog) -> Result<Self> {
        let shared = Arc::new(Mutex::new(SharedState::new(48_000, 2)));
        let (tx, rx) = mpsc::channel();
        let command_shared = Arc::clone(&shared);

        thread::Builder::new()
            .name("audio-engine".to_string())
            .spawn(move || {
                let host = cpal::default_host();
                let mut stream: Option<cpal::Stream> = None;
                tracing::info!("Audio engine thread started");

                while let Ok(command) = rx.recv() {
                    if matches!(command, AudioCommand::Play(_) | AudioCommand::Resume) {
                        if let Err(error) =
                            ensure_output_stream(&host, &mut stream, &command_shared, &app)
                        {
                            set_engine_error(&command_shared, &app, error.to_string());
                            continue;
                        }
                    }

                    if let Err(error) = handle_command(command, &command_shared, &app, &file_issues)
                    {
                        set_engine_error(&command_shared, &app, format!("{error:#}"));
                    }
                }
            })
            .context("Failed to start audio engine thread")?;

        Ok(Self { tx, shared })
    }

    pub fn play(&self, request: PlayRequest) -> Result<()> {
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

    pub fn stop(&self) -> Result<()> {
        self.send_command(AudioCommand::Stop)
    }

    pub fn snapshot(&self) -> PlayerState {
        self.shared
            .lock()
            .map(|shared| shared.player_state())
            .unwrap_or_default()
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
    output_sample_rate: u32,
    output_channels: u16,
    status: PlaybackStatus,
    volume: f32,
    engine_error: Option<String>,
    current_track: Option<StreamingTrack>,
    current_recording_id: Option<String>,
    current_source_id: Option<String>,
    current_title: Option<String>,
    current_artist: Option<String>,
    current_file_path: Option<String>,
    output_frame_position: u64,
    last_position_emit_ms: u64,
}

impl SharedState {
    fn new(output_sample_rate: u32, output_channels: u16) -> Self {
        Self {
            output_sample_rate,
            output_channels,
            status: PlaybackStatus::Stopped,
            volume: 1.0,
            engine_error: None,
            current_track: None,
            current_recording_id: None,
            current_source_id: None,
            current_title: None,
            current_artist: None,
            current_file_path: None,
            output_frame_position: 0,
            last_position_emit_ms: 0,
        }
    }

    fn player_state(&self) -> PlayerState {
        PlayerState {
            status: self.status.clone(),
            recording_id: self.current_recording_id.clone(),
            source_id: self.current_source_id.clone(),
            title: self.current_title.clone(),
            artist: self.current_artist.clone(),
            duration_ms: self.current_track.as_ref().map(|track| track.duration_ms),
            position_ms: self.position_ms(),
            volume: self.volume,
        }
    }

    fn position_ms(&self) -> u64 {
        if self.output_sample_rate == 0 {
            return 0;
        }

        self.output_frame_position.saturating_mul(1000) / u64::from(self.output_sample_rate)
    }

    fn stop_decoder(&mut self) {
        if let Some(track) = &self.current_track {
            if let Ok(mut buffer) = track.buffer.lock() {
                buffer.stop_requested = true;
            }
        }
    }

    fn clear_track(&mut self) {
        self.stop_decoder();
        self.status = PlaybackStatus::Stopped;
        self.current_track = None;
        self.current_recording_id = None;
        self.current_source_id = None;
        self.current_title = None;
        self.current_artist = None;
        self.current_file_path = None;
        self.output_frame_position = 0;
        self.last_position_emit_ms = 0;
    }
}

#[derive(Clone)]
struct StreamingTrack {
    buffer: Arc<Mutex<TrackBuffer>>,
    duration_ms: u64,
}

struct TrackBuffer {
    samples: VecDeque<f32>,
    finished: bool,
    stop_requested: bool,
}

impl TrackBuffer {
    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            finished: false,
            stop_requested: false,
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
            if let Err(e) = start_playback(shared, app, request, 0) {
                file_issues.push_playback_error(file_path, e.to_string());
                return Err(e);
            }
        }
        AudioCommand::Pause => {
            tracing::info!("Pausing playback");
            update_state(shared, app, |state| {
                if state.current_track.is_some() {
                    state.status = PlaybackStatus::Paused;
                }
            });
        }
        AudioCommand::Resume => {
            tracing::info!("Resuming playback");
            update_state(shared, app, |state| {
                if state.current_track.is_some() {
                    state.status = PlaybackStatus::Loading;
                }
            });
        }
        AudioCommand::Seek(position_ms) => {
            tracing::info!(position_ms, "Seeking playback");
            let request = {
                let state = shared
                    .lock()
                    .map_err(|_| anyhow!("Audio state lock poisoned"))?;
                PlayRequest {
                    recording_id: state
                        .current_recording_id
                        .clone()
                        .ok_or_else(|| anyhow!("No active track to seek"))?,
                    source_id: state
                        .current_source_id
                        .clone()
                        .ok_or_else(|| anyhow!("No active track to seek"))?,
                    file_path: state
                        .current_file_path
                        .clone()
                        .ok_or_else(|| anyhow!("No active track to seek"))?,
                    title: state.current_title.clone(),
                    artist: state.current_artist.clone(),
                }
            };

            start_playback(shared, app, request, position_ms)?;
        }
        AudioCommand::SetVolume(volume) => {
            tracing::info!(volume, "Updating playback volume");
            update_state(shared, app, |state| {
                state.volume = volume.clamp(0.0, 1.5);
            });
        }
        AudioCommand::Stop => {
            tracing::info!("Stopping playback");
            update_state(shared, app, |state| {
                state.clear_track();
            });
        }
    }

    Ok(())
}

fn start_playback(
    shared: &Arc<Mutex<SharedState>>,
    app: &AppHandle,
    request: PlayRequest,
    start_ms: u64,
) -> Result<()> {
    let (output_rate, output_channels) = {
        let state = shared
            .lock()
            .map_err(|_| anyhow!("Audio state lock poisoned"))?;
        (state.output_sample_rate, state.output_channels)
    };

    let source = LocalFileSource::open(Path::new(&request.file_path))
        .with_context(|| format!("Failed to open {}", request.file_path))?;

    let buffer = Arc::new(Mutex::new(TrackBuffer::new()));
    let duration_ms = source.duration_ms;
    let current_output_position = start_ms.saturating_mul(u64::from(output_rate)) / 1000;

    update_state(shared, app, |state| {
        state.stop_decoder();
        state.status = PlaybackStatus::Loading;
        state.current_track = Some(StreamingTrack {
            buffer: Arc::clone(&buffer),
            duration_ms,
        });
        state.current_recording_id = Some(request.recording_id.clone());
        state.current_source_id = Some(request.source_id.clone());
        state.current_title = request.title.clone();
        state.current_artist = request.artist.clone();
        state.current_file_path = Some(request.file_path.clone());
        state.output_frame_position = current_output_position;
        state.last_position_emit_ms = start_ms.saturating_sub(250);
    });

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
        {
            let state = buffer
                .lock()
                .map_err(|_| anyhow!("Track buffer lock poisoned"))?;
            if state.stop_requested {
                tracing::info!("Decoder worker stopping early");
                return Ok(());
            }
            if state.buffered_frames(output_channels as usize) >= MAX_BUFFER_FRAMES {
                drop(state);
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        }

        match source.decode_next()? {
            None => break,
            Some(input) => {
                let output = resampler.push(&input);
                if !output.is_empty() {
                    let mut state = buffer
                        .lock()
                        .map_err(|_| anyhow!("Track buffer lock poisoned"))?;
                    if state.stop_requested {
                        return Ok(());
                    }
                    state.samples.extend(output);
                }
            }
        }
    }

    let tail = resampler.finish();
    let mut state = buffer
        .lock()
        .map_err(|_| anyhow!("Track buffer lock poisoned"))?;
    if !tail.is_empty() {
        state.samples.extend(tail);
    }
    state.finished = true;
    tracing::info!("Decoder worker finished");
    Ok(())
}

fn update_state<F>(shared: &Arc<Mutex<SharedState>>, app: &AppHandle, f: F)
where
    F: FnOnce(&mut SharedState),
{
    let snapshot = {
        let mut state = match shared.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        f(&mut state);
        state.player_state()
    };

    let _ = app.emit(PLAYER_STATE_EVENT, snapshot);
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

fn ensure_output_stream(
    host: &cpal::Host,
    stream: &mut Option<cpal::Stream>,
    shared: &Arc<Mutex<SharedState>>,
    app: &AppHandle,
) -> Result<()> {
    if stream.is_some() {
        return Ok(());
    }

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

    if let Ok(mut state) = shared.lock() {
        state.output_sample_rate = stream_config.sample_rate.0;
        state.output_channels = stream_config.channels;
    }

    let output_stream =
        build_output_stream(&device, &supported_config, Arc::clone(shared), app.clone())?;
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
    shared: Arc<Mutex<SharedState>>,
    app: AppHandle,
) -> Result<cpal::Stream> {
    let config = supported_config.config();

    match supported_config.sample_format() {
        cpal::SampleFormat::F32 => {
            let shared = Arc::clone(&shared);
            let app_for_data = app.clone();
            let app_for_err = app.clone();
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [f32], _| write_output_data_f32(data, &shared, &app_for_data),
                    move |error| emit_error(&app_for_err, format!("Audio stream error: {error}")),
                    None,
                )
                .context("Failed to build f32 output stream")
        }
        cpal::SampleFormat::I16 => {
            let shared = Arc::clone(&shared);
            let app_for_data = app.clone();
            let app_for_err = app.clone();
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [i16], _| write_output_data_i16(data, &shared, &app_for_data),
                    move |error| emit_error(&app_for_err, format!("Audio stream error: {error}")),
                    None,
                )
                .context("Failed to build i16 output stream")
        }
        cpal::SampleFormat::U16 => {
            let shared = Arc::clone(&shared);
            let app_for_data = app.clone();
            let app_for_err = app;
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [u16], _| write_output_data_u16(data, &shared, &app_for_data),
                    move |error| emit_error(&app_for_err, format!("Audio stream error: {error}")),
                    None,
                )
                .context("Failed to build u16 output stream")
        }
        other => Err(anyhow!("Unsupported output sample format: {other:?}")),
    }
}

fn write_output_data_f32(output: &mut [f32], shared: &Arc<Mutex<SharedState>>, app: &AppHandle) {
    write_output_data(output, shared, app, |sample| sample);
}

fn write_output_data_i16(output: &mut [i16], shared: &Arc<Mutex<SharedState>>, app: &AppHandle) {
    write_output_data(output, shared, app, |sample| {
        (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
    });
}

fn write_output_data_u16(output: &mut [u16], shared: &Arc<Mutex<SharedState>>, app: &AppHandle) {
    write_output_data(output, shared, app, |sample| {
        (((sample.clamp(-1.0, 1.0) + 1.0) * 0.5) * u16::MAX as f32) as u16
    });
}

fn write_output_data<T, F>(
    output: &mut [T],
    shared: &Arc<Mutex<SharedState>>,
    app: &AppHandle,
    convert: F,
) where
    T: Copy,
    F: Fn(f32) -> T,
{
    let mut ended_event = None;
    let mut snapshot = None;
    let mut position_event = None;

    {
        let mut state = match shared.lock() {
            Ok(state) => state,
            Err(_) => {
                for sample in output.iter_mut() {
                    *sample = convert(0.0);
                }
                return;
            }
        };

        let output_channels = state.output_channels as usize;
        for frame in output.chunks_mut(output_channels) {
            let Some((track_buffer, track_duration_ms)) = state
                .current_track
                .as_ref()
                .map(|track| (Arc::clone(&track.buffer), track.duration_ms))
            else {
                for sample in frame.iter_mut() {
                    *sample = convert(0.0);
                }
                continue;
            };

            let mut buffer = match track_buffer.lock() {
                Ok(buffer) => buffer,
                Err(_) => {
                    for sample in frame.iter_mut() {
                        *sample = convert(0.0);
                    }
                    continue;
                }
            };

            if state.status == PlaybackStatus::Paused {
                for sample in frame.iter_mut() {
                    *sample = convert(0.0);
                }
                continue;
            }

            let ready_frames = buffer.buffered_frames(output_channels);
            if state.status == PlaybackStatus::Loading
                && (ready_frames >= PREBUFFER_FRAMES || (buffer.finished && ready_frames > 0))
            {
                state.status = PlaybackStatus::Playing;
                snapshot = Some(state.player_state());
            }

            if state.status != PlaybackStatus::Playing {
                for sample in frame.iter_mut() {
                    *sample = convert(0.0);
                }
                continue;
            }

            if ready_frames == 0 {
                if buffer.finished {
                    ended_event = match (
                        state.current_recording_id.clone(),
                        state.current_source_id.clone(),
                    ) {
                        (Some(recording_id), Some(source_id)) => Some(TrackEndedEvent {
                            recording_id,
                            source_id,
                            position_ms: track_duration_ms,
                        }),
                        _ => None,
                    };
                    // Drop the TrackBuffer guard before calling clear_track, which
                    // internally calls stop_decoder → TrackBuffer::lock().  Holding
                    // the guard here while re-entering the same lock causes a
                    // self-deadlock on the cpal output thread.
                    drop(buffer);
                    state.clear_track();
                    snapshot = Some(state.player_state());
                    for sample in frame.iter_mut() {
                        *sample = convert(0.0);
                    }
                    break;
                }

                state.status = PlaybackStatus::Loading;
                snapshot = Some(state.player_state());
                for sample in frame.iter_mut() {
                    *sample = convert(0.0);
                }
                continue;
            }

            for sample in frame.iter_mut() {
                let raw_sample = buffer.samples.pop_front().unwrap_or(0.0) * state.volume;
                *sample = convert(raw_sample);
            }

            state.output_frame_position = state.output_frame_position.saturating_add(1);
            let new_position_ms = state.position_ms();
            if new_position_ms >= state.last_position_emit_ms.saturating_add(250) {
                state.last_position_emit_ms = new_position_ms;
                position_event = Some(new_position_ms);
            }
        }
    }

    if let Some(position_ms) = position_event {
        let _ = app.emit(PLAYER_POSITION_EVENT, position_ms);
    }
    if let Some(event) = ended_event {
        let _ = app.emit(PLAYER_TRACK_ENDED_EVENT, event);
    }
    if let Some(player_state) = snapshot {
        let _ = app.emit(PLAYER_STATE_EVENT, player_state);
    }
}

/// Reads the 10-byte ID3v2 header from `file` and returns the byte offset of the
/// first byte after the entire tag (i.e. where the actual audio data begins).
/// Returns `None` if there is no ID3v2 header at the current file position.
fn id3v2_end_offset(file: &mut File) -> Option<u64> {
    use std::io::Read;
    let mut header = [0u8; 10];
    file.read_exact(&mut header).ok()?;
    if &header[0..3] != b"ID3" {
        return None;
    }
    // Bytes 6–9 encode the tag body size as a syncsafe integer (7 bits per byte).
    let size = ((header[6] as u64) << 21)
        | ((header[7] as u64) << 14)
        | ((header[8] as u64) << 7)
        | (header[9] as u64);
    Some(10 + size)
}

struct LocalFileSource {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
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
                let msg = first_err.to_string();
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
                Err(first_err)
            }
        }
    }

    fn probe_file(path: &Path, file: File) -> Result<Self> {
        let media_source = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
            hint.with_extension(extension);
        }

        let probed = symphonia::default::get_probe().format(
            &hint,
            media_source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;
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
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())?;
        // track borrow of format ends here (NLL)

        let mut source = Self {
            format,
            decoder,
            track_id,
            sample_rate: sample_rate.unwrap_or(0),
            channels: channels.unwrap_or(0),
            duration_ms,
            pending: Vec::new(),
        };

        // M4A/AAC and some other formats don't expose sample_rate/channels in the container
        // metadata — those values live inside the codec's private data and only become known
        // after the first packet is decoded.  Prime the source by decoding one packet so that
        // sample_rate and channels are always valid by the time open() returns.
        if source.sample_rate == 0 || source.channels == 0 {
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
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    if self.sample_rate == 0 {
                        self.sample_rate = spec.rate;
                    }
                    if self.channels == 0 {
                        self.channels = spec.channels.count() as u16;
                    }
                    append_audio_buffer(decoded, &mut self.pending);
                    return Ok(());
                }
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(e) => return Err(e.into()),
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
            match self.decoder.decode(&packet) {
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
        }
    }
}
