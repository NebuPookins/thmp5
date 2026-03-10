use serde::{Deserialize, Serialize};

// ── IPC response types (serialized to frontend) ──────────────────────────────

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerState {
    pub status: PlaybackStatus,
    pub recording_id: Option<String>,
    pub source_id: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration_ms: Option<u64>,
    pub position_ms: u64,
    pub volume: f32,
}

#[derive(Debug, Deserialize)]
pub struct PlayRequest {
    pub source_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SeekRequest {
    pub position_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct VolumeRequest {
    pub volume: f32,
}

#[derive(Debug, Serialize)]
pub struct ImportStats {
    pub scanned: u32,
    pub imported: u32,
    pub skipped: u32,
    pub errors: u32,
    pub error_messages: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportProgress {
    pub is_running: bool,
    pub root_path: Option<String>,
    pub current_path: Option<String>,
    pub scanned: u32,
    pub imported: u32,
    pub skipped: u32,
    pub errors: u32,
    pub error_messages: Vec<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppConfig {
    pub music_root: Option<String>,
    pub queue_history_limit: i64,
}

#[derive(Debug, Serialize)]
pub struct AppBootstrap {
    pub needs_setup: bool,
    pub config: AppConfig,
    pub import_progress: ImportProgress,
    pub library_summary: LibrarySummary,
}

#[derive(Debug, Serialize)]
pub struct LibrarySummary {
    pub recording_count: i64,
    pub artist_count: i64,
    pub release_group_count: i64,
    pub source_count: i64,
}

#[derive(Debug, Serialize)]
pub struct RecordingRow {
    pub id: String,
    pub title: String,
    pub duration_ms: Option<i64>,
    pub primary_artist_id: Option<String>,
    pub release_group_id: Option<String>,
    pub artist_credit_name: Option<String>,
    pub release_group_title: Option<String>,
    pub genre: Option<String>,
    pub release_date: Option<String>,
    pub track_position: Option<i64>,
    pub disc_position: Option<i64>,
    pub rating: Option<i64>,
    pub play_count: i64,
    pub last_played: Option<String>,
    pub primary_source_id: Option<String>,
    pub primary_source_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ArtistRow {
    pub id: String,
    pub name: String,
    pub sort_name: String,
    pub release_group_count: i64,
    pub recording_count: i64,
}

#[derive(Debug, Serialize)]
pub struct ReleaseGroupRow {
    pub id: String,
    pub title: String,
    pub artist_credit_name: Option<String>,
    pub primary_artist_id: Option<String>,
    pub release_count: i64,
    pub recording_count: i64,
    pub release_date: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InitialSetupRequest {
    pub music_root: String,
}

#[derive(Debug, Deserialize)]
pub struct QueueSettingsUpdate {
    pub queue_history_limit: i64,
}

#[derive(Debug, Deserialize)]
pub struct PlayHistoryInput {
    pub recording_id: String,
    pub source_id: Option<String>,
    pub duration_played_ms: Option<i64>,
}

// ── Internal scanner metadata ─────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<u32>,
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_number: Option<u32>,
    pub duration_ms: u64,
    pub format: String,
    pub genre: Option<String>,
    pub bpm: Option<f64>,
    pub comment: Option<String>,
    pub replay_gain_track_db: Option<f64>,
    pub replay_gain_track_peak: Option<f64>,
    pub replay_gain_album_db: Option<f64>,
    pub replay_gain_album_peak: Option<f64>,
}

// ── Query engine ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SmartPlaylistRequest {
    pub query: String,
}

#[derive(Debug, Serialize)]
pub struct SmartPlaylistResult {
    pub recordings: Vec<RecordingRow>,
    pub total_duration_ms: i64,
    pub sql: String, // for debugging/display
}
