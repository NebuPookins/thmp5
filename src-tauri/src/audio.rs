use crate::models::{PlaybackStatus, PlayerState};
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use std::fs::File;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tauri::{AppHandle, Emitter};

pub const PLAYER_STATE_EVENT: &str = "player-state";
pub const PLAYER_POSITION_EVENT: &str = "player-position";
pub const PLAYER_TRACK_ENDED_EVENT: &str = "player-track-ended";
pub const PLAYER_ERROR_EVENT: &str = "player-error";

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
    pub fn new(app: AppHandle) -> Result<Self> {
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

                    if let Err(error) = handle_command(command, &command_shared, &app) {
                        emit_error(&app, error.to_string());
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
    current_track: Option<LoadedTrack>,
    current_recording_id: Option<String>,
    current_source_id: Option<String>,
    current_title: Option<String>,
    current_artist: Option<String>,
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

    fn clear_track(&mut self) {
        self.status = PlaybackStatus::Stopped;
        self.current_track = None;
        self.current_recording_id = None;
        self.current_source_id = None;
        self.current_title = None;
        self.current_artist = None;
        self.output_frame_position = 0;
        self.last_position_emit_ms = 0;
    }
}

struct LoadedTrack {
    samples: Vec<f32>,
    source_sample_rate: u32,
    source_channels: u16,
    duration_ms: u64,
    source_frame_len: usize,
}

impl LoadedTrack {
    fn sample_for_output(
        &self,
        output_frame_index: u64,
        output_rate: u32,
        output_channel: usize,
    ) -> f32 {
        let source_frame = ((output_frame_index as u128) * (self.source_sample_rate as u128)
            / (output_rate as u128)) as usize;

        if source_frame >= self.source_frame_len {
            return 0.0;
        }

        let source_channel = if self.source_channels == 1 {
            0
        } else {
            output_channel.min(self.source_channels as usize - 1)
        };

        let sample_index = source_frame * self.source_channels as usize + source_channel;
        self.samples.get(sample_index).copied().unwrap_or(0.0)
    }
}

fn handle_command(
    command: AudioCommand,
    shared: &Arc<Mutex<SharedState>>,
    app: &AppHandle,
) -> Result<()> {
    match command {
        AudioCommand::Play(request) => {
            tracing::info!(
                recording_id = %request.recording_id,
                source_id = %request.source_id,
                path = %request.file_path,
                "Beginning track load"
            );
            update_state(shared, app, |state| {
                state.status = PlaybackStatus::Loading;
                state.current_recording_id = Some(request.recording_id.clone());
                state.current_source_id = Some(request.source_id.clone());
                state.current_title = request.title.clone();
                state.current_artist = request.artist.clone();
                state.current_track = None;
                state.output_frame_position = 0;
                state.last_position_emit_ms = 0;
            });

            let track = LocalFileSource::open(Path::new(&request.file_path))
                .with_context(|| format!("Failed to open {}", request.file_path))?
                .decode_all()
                .with_context(|| format!("Failed to decode {}", request.file_path))?;

            tracing::info!(
                recording_id = %request.recording_id,
                source_id = %request.source_id,
                duration_ms = track.duration_ms,
                frames = track.source_frame_len,
                sample_rate = track.source_sample_rate,
                channels = track.source_channels,
                "Track decoded"
            );

            update_state(shared, app, |state| {
                state.current_track = Some(track);
                state.status = PlaybackStatus::Playing;
                state.output_frame_position = 0;
                state.last_position_emit_ms = 0;
            });
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
                    state.status = PlaybackStatus::Playing;
                }
            });
        }
        AudioCommand::Seek(position_ms) => {
            tracing::info!(position_ms, "Seeking playback");
            update_state(shared, app, |state| {
                let duration_ms = state.current_track.as_ref().map(|track| track.duration_ms);
                let clamped_ms = duration_ms
                    .map(|duration| position_ms.min(duration))
                    .unwrap_or(position_ms);
                state.output_frame_position =
                    clamped_ms.saturating_mul(u64::from(state.output_sample_rate)) / 1000;
                state.last_position_emit_ms = clamped_ms.saturating_sub(250);
            });
            emit_position(shared, app);
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

fn emit_position(shared: &Arc<Mutex<SharedState>>, app: &AppHandle) {
    let position = {
        let state = match shared.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        state.position_ms()
    };

    let _ = app.emit(PLAYER_POSITION_EVENT, position);
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

    tracing::info!("Initializing output stream");
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
    tracing::info!("Output stream ready");
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
            let Some(track) = state.current_track.as_ref() else {
                for sample in frame.iter_mut() {
                    *sample = convert(0.0);
                }
                continue;
            };

            if state.status != PlaybackStatus::Playing {
                for sample in frame.iter_mut() {
                    *sample = convert(0.0);
                }
                continue;
            }

            let current_ms = state.position_ms();
            if current_ms >= track.duration_ms {
                for sample in frame.iter_mut() {
                    *sample = convert(0.0);
                }

                ended_event = match (
                    state.current_recording_id.clone(),
                    state.current_source_id.clone(),
                ) {
                    (Some(recording_id), Some(source_id)) => Some(TrackEndedEvent {
                        recording_id,
                        source_id,
                        position_ms: track.duration_ms,
                    }),
                    _ => None,
                };
                state.clear_track();
                snapshot = Some(state.player_state());
                break;
            }

            for (channel_index, sample) in frame.iter_mut().enumerate() {
                let raw_sample = track.sample_for_output(
                    state.output_frame_position,
                    state.output_sample_rate,
                    channel_index,
                ) * state.volume;
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

struct LocalFileSource {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
}

impl LocalFileSource {
    fn open(path: &Path) -> Result<Self> {
        tracing::info!(path = %path.display(), "Opening local file source");
        let file = File::open(path)?;
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
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())?;

        Ok(Self {
            format,
            decoder,
            track_id,
        })
    }

    fn decode_all(mut self) -> Result<LoadedTrack> {
        tracing::info!("Starting full track decode");
        let mut samples = Vec::new();
        let mut source_sample_rate = None;
        let mut source_channels = None;
        let mut packet_count: u64 = 0;

        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::ResetRequired) => {
                    return Err(anyhow!("Decoder reset required"));
                }
                Err(SymphoniaError::IoError(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(error) => return Err(error.into()),
            };

            if packet.track_id() != self.track_id {
                continue;
            }
            packet_count = packet_count.saturating_add(1);
            if packet_count % 500 == 0 {
                tracing::debug!(packet_count, "Decode progress");
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(SymphoniaError::IoError(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(error) => return Err(error.into()),
            };

            source_sample_rate.get_or_insert(decoded.spec().rate);
            source_channels.get_or_insert(decoded.spec().channels.count() as u16);
            append_audio_buffer(decoded, &mut samples);
        }

        let source_sample_rate =
            source_sample_rate.ok_or_else(|| anyhow!("Audio stream had no decodable frames"))?;
        let source_channels =
            source_channels.ok_or_else(|| anyhow!("Audio stream had no channel data"))?;
        let source_frame_len = samples.len() / source_channels as usize;
        let duration_ms =
            (source_frame_len as u64).saturating_mul(1000) / u64::from(source_sample_rate);

        tracing::info!(
            packet_count,
            sample_rate = source_sample_rate,
            channels = source_channels,
            duration_ms,
            "Finished full track decode"
        );

        Ok(LoadedTrack {
            samples,
            source_sample_rate,
            source_channels,
            duration_ms,
            source_frame_len,
        })
    }
}

fn append_audio_buffer(decoded: AudioBufferRef<'_>, samples: &mut Vec<f32>) {
    let mut interleaved = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
    interleaved.copy_interleaved_ref(decoded);
    samples.extend_from_slice(interleaved.samples());
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
