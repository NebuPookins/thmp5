// Wire-model types matching the Rust models returned by the detail
// `#[tauri::command]`s. Kept separate from `entityDetailReducer.ts` so the
// data model and the fetch state machine don't live in one file.

export type EntityType = "artist" | "release_group" | "recording";

export type DetailNav = {
  type: EntityType;
  id: string;
};

export type SourceTagInfo = {
  frame_id: string;
  field_name: string;
  value: string;
};

export type SourceDetail = {
  id: string;
  source_type: string;
  file_path: string | null;
  format: string | null;
  duration_ms: number | null;
  replay_gain_track_db: number | null;
  replay_gain_track_peak: number | null;
  tags: SourceTagInfo[];
};

export type ReleaseInfo = {
  release_group_id: string;
  release_group_title: string;
  track_position: number | null;
  disc_position: number | null;
  disc_total: number | null;
};

// ── Artist detail types ─────────────────────────────────────────────────────

export type ArtistReleaseGroup = {
  id: string;
  title: string;
  rg_type: string | null;
  release_date: string | null;
  recording_count: number;
  rating: number | null;
  primary_artist_id: string | null;
  artist_credit_name: string | null;
};

export type GuestAppearanceTrack = {
  recording_id: string;
  recording_title: string;
  track_position: number | null;
  disc_position: number | null;
};

export type GuestAppearanceReleaseGroup = {
  id: string;
  title: string;
  rg_type: string | null;
  release_date: string | null;
  primary_artist_id: string | null;
  artist_credit_name: string | null;
  tracks: GuestAppearanceTrack[];
};

export type ArtistDetail = {
  id: string;
  name: string;
  sort_name: string;
  mbid: string | null;
  rating: number | null;
  last_played: string | null;
  recording_count: number;
  release_group_count: number;
  release_groups: ArtistReleaseGroup[];
  guest_appearances: GuestAppearanceReleaseGroup[];
};

// ── Release group detail types ──────────────────────────────────────────────

export type TrackDetail = {
  id: string;
  position: number;
  title: string | null;
  duration_ms: number | null;
  recording_id: string;
  recording_title: string;
  artist_credit_name: string | null;
  primary_artist_id: string | null;
  has_source: boolean;
  primary_source_id: string | null;
};

export type MediumDetail = {
  id: string;
  position: number;
  format: string | null;
  tracks: TrackDetail[];
};

export type ReleaseCompleteness =
  | { type: "complete" }
  | { type: "incomplete"; missing_tracks: MissingTrackDetail[] }
  | { type: "unknown"; reason: string; disagreement_groups: SourceDisagreementGroup[] };

export type SourceDisagreementGroup = {
  description: string;
  source_paths: string[];
};

export type ReleaseDetail = {
  id: string;
  title: string;
  release_date: string | null;
  country: string | null;
  label: string | null;
  catalog_number: string | null;
  mediums: MediumDetail[];
  completeness: ReleaseCompleteness;
};

export type MissingTrackDetail = {
  disc_position: number;
  track_position: number;
  title: string;
  recording_id: string | null;
};

export type ReleaseGroupDetail = {
  id: string;
  title: string;
  rg_type: string | null;
  artist_credit_name: string | null;
  primary_artist_id: string | null;
  rating: number | null;
  last_played: string | null;
  release_date: string | null;
  releases: ReleaseDetail[];
};

// ── Recording detail types ──────────────────────────────────────────────────

export type RecordingArtistInfo = {
  artist_id: string;
  name: string;
  position: number;
  role: string;
  credited_as: string | null;
};

export type RecordingDetail = {
  id: string;
  title: string;
  duration_ms: number | null;
  genre: string | null;
  bpm: number | null;
  comment: string | null;
  artist_credit_name: string | null;
  primary_artist_id: string | null;
  artist_credit_text: string | null;
  mbid: string | null;
  acoustid: string | null;
  rating: number | null;
  play_count: number;
  last_played: string | null;
  artists: RecordingArtistInfo[];
  releases: ReleaseInfo[];
  sources: SourceDetail[];
};
