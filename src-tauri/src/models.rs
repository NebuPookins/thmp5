use nonempty::NonEmpty;
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

#[derive(Debug, Serialize)]
pub struct FixMergedRecordingsStats {
    pub recordings_checked: u32,
    pub recordings_split: u32,
    pub sources_reimported: u32,
    pub errors: Vec<String>,
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
    /// Number of files currently being fingerprinted (CPU-bound).
    pub fingerprinting_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalCommand {
    pub name: String,
    pub template: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppConfig {
    pub music_root: Option<String>,
    pub queue_history_limit: i64,
    pub external_commands: Vec<ExternalCommand>,
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

#[derive(Debug, Clone, Serialize)]
pub struct DbPoolLeaseInfo {
    pub id: u64,
    pub purpose: String,
    pub acquired_at_unix_ms: i64,
    pub held_for_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct DbPoolWaiterInfo {
    pub id: u64,
    pub purpose: String,
    pub requested_at_unix_ms: i64,
    pub waiting_for_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct DbPoolDebugSnapshot {
    pub size: u32,
    pub idle: usize,
    pub active_connection_count: usize,
    pub waiting_request_count: usize,
    pub active_connections: Vec<DbPoolLeaseInfo>,
    pub waiting_requests: Vec<DbPoolWaiterInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseInfo {
    pub release_group_id: String,
    pub release_group_title: String,
    pub track_position: Option<i64>,
    pub disc_position: Option<i64>,
    pub disc_total: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingRow {
    pub id: String,
    pub title: String,
    pub duration_ms: Option<i64>,
    pub primary_artist_id: Option<String>,
    pub artist_credit_name: Option<String>,
    pub genre: Option<String>,
    pub rating: Option<i64>,
    pub play_count: i64,
    pub last_played: Option<String>,
    pub primary_source_id: Option<String>,
    pub primary_source_path: Option<String>,
    pub tags: Vec<String>,
    pub artist_ids: Vec<String>,
    pub source_paths: Vec<String>,
    pub releases: Vec<ReleaseInfo>,
}

#[derive(Debug, Serialize)]
pub struct ArtistRow {
    pub id: String,
    pub name: String,
    pub sort_name: String,
    pub release_group_count: i64,
    pub recording_count: i64,
    pub rating: Option<f64>,
    pub last_played: Option<String>,
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
    pub rating: Option<f64>,
    pub last_played: Option<String>,
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

#[derive(Debug, Deserialize)]
pub struct RatingUpdateRequest {
    pub id: String,
    pub stars: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityRatingUpdate {
    pub id: String,
    pub rating: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingRatingUpdateResult {
    pub release_groups: Vec<EntityRatingUpdate>,
    pub artists: Vec<EntityRatingUpdate>,
}

#[derive(Debug, Deserialize)]
pub struct Id3FrameDebugRequest {
    pub path: String,
    pub frame_id: String,
}

#[derive(Debug, Serialize)]
pub struct Id3FrameDebugInfo {
    pub frame_id: String,
    pub field_name: String,
    pub version_major: u8,
    pub declared_encoding_byte: u8,
    pub declared_encoding_name: String,
    pub payload_len: usize,
    pub raw_payload_hex_preview: String,
    pub utf8: Option<String>,
    pub latin1: Option<String>,
    pub utf16_with_bom: Option<String>,
    pub utf16be_without_bom: Option<String>,
    pub utf16le_without_bom: Option<String>,
}

// ── Internal scanner metadata ─────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
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
    /// TXXX=ARTISTS — semicolon-separated list of additional artists.
    pub artists: Option<String>,
}

#[derive(Debug)]
pub struct MetadataReadResult {
    pub meta: TrackMetadata,
    pub warning: Option<String>,
    pub all_tags: Vec<TagProperty>,
}

#[derive(Debug, Deserialize)]
pub struct TaglibHelperResponse {
    pub meta: TrackMetadata,
    pub warning: Option<String>,
    #[serde(default)]
    pub all_tags: Vec<TagProperty>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagProperty {
    pub key: String,
    pub value: String,
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

#[derive(Debug, Serialize)]
pub struct PlaylistRow {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub query: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SaveSmartPlaylistRequest {
    pub name: String,
    pub query: String,
}

// ── Entity detail view types ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ArtistDetail {
    pub id: String,
    pub name: String,
    pub sort_name: String,
    pub mbid: Option<String>,
    pub rating: Option<f64>,
    pub last_played: Option<String>,
    pub recording_count: i64,
    pub release_group_count: i64,
    pub release_groups: Vec<ArtistReleaseGroup>,
    pub guest_appearances: Vec<GuestAppearanceReleaseGroup>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtistReleaseGroup {
    pub id: String,
    pub title: String,
    pub rg_type: Option<String>,
    pub release_date: Option<String>,
    pub recording_count: i64,
    pub rating: Option<f64>,
    pub primary_artist_id: Option<String>,
    pub artist_credit_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuestAppearanceTrack {
    pub recording_id: String,
    pub recording_title: String,
    pub track_position: Option<i64>,
    pub disc_position: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuestAppearanceReleaseGroup {
    pub id: String,
    pub title: String,
    pub rg_type: Option<String>,
    pub release_date: Option<String>,
    pub primary_artist_id: Option<String>,
    pub artist_credit_name: Option<String>,
    pub tracks: Vec<GuestAppearanceTrack>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseGroupDetail {
    pub id: String,
    pub title: String,
    pub rg_type: Option<String>,
    pub artist_credit_name: Option<String>,
    pub primary_artist_id: Option<String>,
    pub rating: Option<f64>,
    pub last_played: Option<String>,
    pub release_date: Option<String>,
    pub releases: Vec<ReleaseDetail>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReleaseCompleteness {
    Complete,
    Incomplete {
        missing_tracks: NonEmpty<MissingTrackDetail>,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseDetail {
    pub id: String,
    pub title: String,
    pub release_date: Option<String>,
    pub country: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub mediums: Vec<MediumDetail>,
    pub completeness: ReleaseCompleteness,
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingTrackDetail {
    pub disc_position: i64,
    pub track_position: i64,
    pub title: String,
    pub recording_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MediumDetail {
    pub id: String,
    pub position: i64,
    pub format: Option<String>,
    pub tracks: Vec<TrackDetail>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackDetail {
    pub id: String,
    pub position: i64,
    pub title: Option<String>,
    pub duration_ms: Option<i64>,
    pub recording_id: String,
    pub recording_title: String,
    pub artist_credit_name: Option<String>,
    pub primary_artist_id: Option<String>,
    pub has_source: bool,
    pub primary_source_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingDetail {
    pub id: String,
    pub title: String,
    pub duration_ms: Option<i64>,
    pub genre: Option<String>,
    pub bpm: Option<f64>,
    pub comment: Option<String>,
    pub artist_credit_name: Option<String>,
    pub primary_artist_id: Option<String>,
    pub artist_credit_text: Option<String>,
    pub mbid: Option<String>,
    pub acoustid: Option<String>,
    pub rating: Option<i64>,
    pub play_count: i64,
    pub last_played: Option<String>,
    pub artists: Vec<RecordingArtistInfo>,
    pub releases: Vec<ReleaseInfo>,
    pub sources: Vec<SourceDetail>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingArtistInfo {
    pub artist_id: String,
    pub name: String,
    pub position: i64,
    pub role: String,
    pub credited_as: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceDetail {
    pub id: String,
    pub source_type: String,
    pub file_path: Option<String>,
    pub format: Option<String>,
    pub duration_ms: Option<i64>,
    pub replay_gain_track_db: Option<f64>,
    pub replay_gain_track_peak: Option<f64>,
    pub tags: Vec<SourceTagInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceTagInfo {
    pub frame_id: String,
    pub field_name: String,
    pub value: String,
}
