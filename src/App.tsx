import { FormEvent, memo, useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import EntityDetailView, { type DetailNav } from "./EntityDetailView";
import { pickNextTrack } from "./autoDj";
import { calculatePomodoroBatch, parsePomodoroDuration, formatPomodoroDuration, DEFAULT_TARGET_MS } from "./pomodoro";
import "./App.css";

type LibrarySummary = {
  recording_count: number;
  artist_count: number;
  release_group_count: number;
  source_count: number;
};

type DbPoolLeaseInfo = {
  id: number;
  purpose: string;
  acquired_at_unix_ms: number;
  held_for_ms: number;
};

type DbPoolWaiterInfo = {
  id: number;
  purpose: string;
  requested_at_unix_ms: number;
  waiting_for_ms: number;
};

type DbPoolDebugSnapshot = {
  size: number;
  idle: number;
  active_connection_count: number;
  waiting_request_count: number;
  active_connections: DbPoolLeaseInfo[];
  waiting_requests: DbPoolWaiterInfo[];
};

type ImportProgress = {
  is_running: boolean;
  root_path: string | null;
  current_path: string | null;
  scanned: number;
  imported: number;
  skipped: number;
  errors: number;
  error_messages: string[];
  started_at: string | null;
  fingerprinting_count: number;
  finished_at: string | null;
};

type ExternalCommand = {
  name: string;
  template: string;
};

type AppConfig = {
  music_root: string | null;
  queue_history_limit: number;
  external_commands: ExternalCommand[];
};

type AppBootstrap = {
  needs_setup: boolean;
  config: AppConfig;
  import_progress: ImportProgress;
  library_summary: LibrarySummary;
};

type ReleaseInfo = {
  release_group_id: string;
  release_group_title: string;
  track_position: number | null;
  disc_position: number | null;
  disc_total: number | null;
};

type RecordingRow = {
  id: string;
  title: string;
  duration_ms: number | null;
  primary_artist_id: string | null;
  artist_credit_name: string | null;
  genre: string | null;
  rating: number | null;
  predicted_rating: number | null;
  play_count: number;
  last_played: string | null;
  primary_source_id: string | null;
  primary_source_path: string | null;
  tags: string[];
  artist_ids: string[];
  source_paths: string[];
  releases: ReleaseInfo[];
};

const RECORDINGS_PAGE_SIZE = 5_000;

type ArtistRow = {
  id: string;
  name: string;
  sort_name: string;
  release_group_count: number;
  recording_count: number;
  rating: number | null;
  last_played: string | null;
};

type ReleaseGroupRow = {
  id: string;
  title: string;
  artist_credit_name: string | null;
  primary_artist_id: string | null;
  release_count: number;
  recording_count: number;
  release_date: string | null;
  rating: number | null;
  last_played: string | null;
};

type EntityRatingUpdate = {
  id: string;
  rating: number | null;
};

type RecordingRatingUpdateResult = {
  recording: EntityRatingUpdate;
  release_groups: EntityRatingUpdate[];
  artists: EntityRatingUpdate[];
  affected_recordings: EntityRatingUpdate[];
};

type SmartPlaylistResult = {
  recordings: RecordingRow[];
  total_duration_ms: number;
  sql: string;
};

type PlaylistRow = {
  id: number;
  name: string;
  kind: string;
  query: string | null;
};

type PlaybackStatus = "stopped" | "loading" | "playing" | "paused";

type PlayerState = {
  status: PlaybackStatus;
  source_id: string | null;
  title: string | null;
  artist: string | null;
  duration_ms: number | null;
  position_ms: number;
  volume: number;
  normalization_enabled: boolean;
  normalization_gain: number;
  normalization_source: string;
};

type PlayerErrorEvent = {
  message: string;
};

type CompoundArtistCheck = {
  is_compound: boolean;
  evidence_count: number;
  total_sources_checked: number;
  individual_artist_names: string[];
  source_examples: string[];
};

type FileIssue = {
  file_path: string;
  kind: "import_error" | "playback_error" | "orphan_source" | "duplicate_frame" | "backup_file_exists";
  message: string;
  source_id?: string;
  recording_id?: string;
  frame_id?: string;
  field_name?: string;
  lofty_value?: string;
  corrected_value?: string;
  backup_path?: string;
};

type LastFmStatus = {
  configured: boolean;
  logged_in: boolean;
  username: string | null;
};

type QueueItem = RecordingRow;

// ── Types for split recording modal ──────────────────────────────────────────
type SplitSourceTagInfo = {
  frame_id: string;
  field_name: string;
  value: string;
};

type SplitSourceDetail = {
  id: string;
  source_type: string;
  file_path: string | null;
  duration_ms: number | null;
  tags: SplitSourceTagInfo[];
};

type SplitRecordingDetail = {
  id: string;
  title: string;
  artist_credit_name: string | null;
  primary_artist_id: string | null;
  genre: string | null;
  bpm: number | null;
  sources: SplitSourceDetail[];
};

type ContextMenuState =
  | { kind: "recording"; x: number; y: number; path: string; recording: RecordingRow }
  | { kind: "source"; x: number; y: number; path: string }
  | { kind: "artist"; x: number; y: number; artist_id: string; artist_name: string }
  | { kind: "release_group"; x: number; y: number; release_group_id: string; title: string };

const DEFAULT_PLAYER_STATE: PlayerState = {
  status: "stopped",
  source_id: null,
  title: null,
  artist: null,
  duration_ms: null,
  position_ms: 0,
  volume: 1,
  normalization_enabled: false,
  normalization_gain: 1,
  normalization_source: "",
};

function formatNormGain(gain: number): string {
  if (gain <= 0) return "0 dB";
  const db = 20 * Math.log10(gain);
  return `${db >= 0 ? "+" : ""}${db.toFixed(1)} dB`;
}

// Helper: derive metadata from selected sources' tags (first source wins per field)
function firstTag(sources: SplitSourceDetail[], frameId: string): string | null {
  for (const src of sources) {
    const tag = src.tags.find((t) => t.frame_id === frameId);
    if (tag?.value) return tag.value;
  }
  return null;
}

function allUniqueTags(sources: SplitSourceDetail[], frameId: string): string[] {
  const values = new Set<string>();
  for (const src of sources) {
    for (const t of src.tags) {
      if (t.frame_id === frameId && t.value) values.add(t.value);
    }
  }
  return Array.from(values);
}

function abbreviatePaths(sources: SplitSourceDetail[]): Map<string, string> {
  const paths = sources.map((s) => s.file_path).filter((p): p is string => p !== null);
  const map = new Map<string, string>();

  if (paths.length <= 1) {
    for (const s of sources) {
      map.set(s.id, s.file_path ?? "(no file path)");
    }
    return map;
  }

  // Split all paths into segments
  const segmented = paths.map((p) => p.split("/"));
  const minLen = Math.min(...segmented.map((s) => s.length));

  // Find common prefix segments
  let prefixLen = 0;
  while (prefixLen < minLen) {
    const first = segmented[0][prefixLen];
    if (segmented.every((s) => s[prefixLen] === first)) {
      prefixLen++;
    } else {
      break;
    }
  }

  // Find common suffix segments
  let suffixLen = 0;
  while (suffixLen < minLen - prefixLen) {
    const last = segmented[0][segmented[0].length - 1 - suffixLen];
    if (segmented.every((s) => s[s.length - 1 - suffixLen] === last)) {
      suffixLen++;
    } else {
      break;
    }
  }

  for (const s of sources) {
    if (!s.file_path) {
      map.set(s.id, "(no file path)");
      continue;
    }
    const segs = s.file_path.split("/");
    const middle = segs.slice(prefixLen, suffixLen === 0 ? undefined : segs.length - suffixLen);

    let abbreviated = "";
    if (prefixLen > 0) abbreviated = "…/";
    abbreviated += middle.join("/");
    if (suffixLen > 0 && middle.length > 0) abbreviated += "/…";

    // Fallback: if nothing unique remains, show just the filename
    if (!abbreviated || abbreviated === "…/") {
      abbreviated = "…/" + segs[segs.length - 1];
    }

    map.set(s.id, abbreviated);
  }
  return map;
}

function decomposeDuration(durationMs: number) {
  const totalSeconds = Math.floor(durationMs / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  return { hours, minutes, seconds };
}

function formatDuration(durationMs: number | null): string {
  if (!durationMs || durationMs <= 0) {
    return "Unknown";
  }

  const { hours, minutes, seconds } = decomposeDuration(durationMs);

  if (hours > 0) {
    return `${hours}:${minutes.toString().padStart(2, "0")}:${seconds
      .toString()
      .padStart(2, "0")}`;
  }

  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

function formatDurationCompact(durationMs: number): string {
  if (durationMs <= 0) return "0m00s";

  const { hours, minutes, seconds } = decomposeDuration(durationMs);

  if (hours > 0) {
    return `${hours}h${minutes.toString().padStart(2, "0")}m${seconds
      .toString()
      .padStart(2, "0")}s`;
  }
  return `${minutes}m${seconds.toString().padStart(2, "0")}s`;
}

function formatEtaTime(date: Date): string {
  return date
    .toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
    .replace(/[\s  ]+/g, "");
}

function formatLastPlayed(value: string | null): string {
  return value ?? "Never";
}

function formatYear(value: string | null): string {
  return value?.slice(0, 4) ?? "Unknown year";
}

function formatAlbumRating(value: number | null): string {
  if (value === null) {
    return "Unrated album";
  }

  return `Avg ${value.toFixed(1)}`;
}

type SortColumn = "title" | "artist" | "releases" | "genre" | "rating" | "duration" | "plays" | "last_played";

function compareAggregateRatings(
  aRating: number | null,
  bRating: number | null,
): number {
  if (aRating === null && bRating === null) {
    return 0;
  }

  if (aRating === null) {
    return -1;
  }

  if (bRating === null) {
    return 1;
  }

  return bRating - aRating;
}

function compareAggregateLastPlayed(
  aLastPlayed: string | null,
  bLastPlayed: string | null,
): number {
  if (aLastPlayed === null && bLastPlayed === null) {
    return 0;
  }

  if (aLastPlayed === null) {
    return -1;
  }

  if (bLastPlayed === null) {
    return 1;
  }

  return aLastPlayed.localeCompare(bLastPlayed);
}

function compareRecordings(a: RecordingRow, b: RecordingRow, col: SortColumn, asc: boolean): number {
  let delta = 0;
  switch (col) {
    case "title":
      delta = a.title.localeCompare(b.title);
      break;
    case "artist":
      delta = (a.artist_credit_name ?? "").localeCompare(b.artist_credit_name ?? "");
      if (delta === 0) delta = a.title.localeCompare(b.title);
      break;
    case "releases":
      delta = (a.releases[0]?.release_group_title ?? "").localeCompare(b.releases[0]?.release_group_title ?? "");
      if (delta === 0) delta = (a.releases[0]?.disc_position ?? Infinity) - (b.releases[0]?.disc_position ?? Infinity);
      if (delta === 0) delta = (a.releases[0]?.track_position ?? Infinity) - (b.releases[0]?.track_position ?? Infinity);
      break;
    case "genre":
      delta = (a.genre ?? "").localeCompare(b.genre ?? "");
      break;
    case "rating":
      delta = (a.rating ?? a.predicted_rating ?? 0) - (b.rating ?? b.predicted_rating ?? 0);
      break;
    case "duration":
      delta = (a.duration_ms ?? 0) - (b.duration_ms ?? 0);
      break;
    case "plays":
      delta = a.play_count - b.play_count;
      break;
    case "last_played":
      delta = (a.last_played ?? "").localeCompare(b.last_played ?? "");
      break;
  }
  return asc ? delta : -delta;
}

type RatingStarsProps = {
  value: number | null;
  recordingId: string;
  onRate: (recordingId: string, value: number | null) => void;
  disabled?: boolean;
  isPredicted?: boolean;
};

const RatingStars = memo(function RatingStars({ value, recordingId, onRate, disabled = false, isPredicted = false }: RatingStarsProps) {
  const fillClass = isPredicted ? "rating-star-predicted" : "rating-star-filled";
  return (
    <div className="rating-stars" role="group">
      {[1, 2, 3, 4, 5].map((star) => {
        const filled = value !== null && star <= value;
        return (
          <button
            aria-label={`Set rating to ${star}`}
            className={`rating-star ${filled ? fillClass : ""}`}
            disabled={disabled}
            key={star}
            onClick={(event) => {
              event.preventDefault();
              event.stopPropagation();
              onRate(recordingId, isPredicted ? star : (value === star ? null : star));
            }}
            type="button"
          >
            ★
          </button>
        );
      })}
    </div>
  );
});

async function reportPoolTimeout(context: string, value: unknown) {
  const message = value instanceof Error ? value.message : String(value);
  if (!message.includes("pool timed out while waiting for an open connection")) {
    return;
  }

  try {
    const snapshot = await invoke<DbPoolDebugSnapshot>("get_db_pool_debug_snapshot");
    console.error(`DB pool snapshot after timeout in ${context}`, snapshot);
  } catch (snapshotError) {
    console.error(`Failed to fetch DB pool snapshot after timeout in ${context}`, snapshotError);
  }
}

function applyRatingToRecording(recording: RecordingRow, recordingId: string, stars: number | null): RecordingRow {
  return recording.id === recordingId ? { ...recording, rating: stars } : recording;
}

function reconcileRecording(recording: RecordingRow, recordingsById: Map<string, RecordingRow>): RecordingRow {
  return recordingsById.get(recording.id) ?? recording;
}

function reconcileRecordingList(
  items: RecordingRow[],
  recordingsById: Map<string, RecordingRow>,
): RecordingRow[] {
  let changed = false;
  const nextItems = items.map((item) => {
    const nextItem = reconcileRecording(item, recordingsById);
    if (nextItem !== item) {
      changed = true;
    }
    return nextItem;
  });

  return changed ? nextItems : items;
}

function applyAggregateRatings<T extends { id: string; rating: number | null }>(
  rows: T[],
  updates: EntityRatingUpdate[],
): T[] {
  if (updates.length === 0) {
    return rows;
  }

  const ratingsById = new Map(updates.map((update) => [update.id, update.rating] as const));
  return rows.map((row) => (
    ratingsById.has(row.id)
      ? { ...row, rating: ratingsById.get(row.id) ?? null }
      : row
  ));
}

function applyPredictedRatings<T extends { id: string; predicted_rating: number | null }>(
  rows: T[],
  updates: EntityRatingUpdate[],
): T[] {
  if (updates.length === 0) {
    return rows;
  }

  const predictedById = new Map(updates.map((update) => [update.id, update.rating] as const));
  return rows.map((row) =>
    predictedById.has(row.id)
      ? { ...row, predicted_rating: predictedById.get(row.id) ?? null }
      : row,
  );
}

// ── Detail view navigation stack ──────────────────────────────────────────────

type NavState = { stack: DetailNav[]; index: number };

type NavAction =
  | { type: "navigate"; nav: DetailNav }
  | { type: "back" }
  | { type: "forward" }
  | { type: "close" };

function navReducer(state: NavState | null, action: NavAction): NavState | null {
  switch (action.type) {
    case "navigate":
      if (!state) return { stack: [action.nav], index: 0 };
      return {
        stack: [...state.stack.slice(0, state.index + 1), action.nav],
        index: state.index + 1,
      };
    case "back":
      if (!state || state.index <= 0) return state;
      return { ...state, index: state.index - 1 };
    case "forward":
      if (!state || state.index >= state.stack.length - 1) return state;
      return { ...state, index: state.index + 1 };
    case "close":
      return null;
  }
}

// Quick-fill presets for the Pomodoro buttons in the timeline header.
const POMODORO_PRESETS = [
  { durationMs: DEFAULT_TARGET_MS }, // 25m focus session
  { durationMs: 5 * 60 * 1000 }, // 5m quick break
];

function App() {
  const [bootstrap, setBootstrap] = useState<AppBootstrap | null>(null);
  const [recordings, setRecordings] = useState<RecordingRow[]>([]);
  const [artists, setArtists] = useState<ArtistRow[]>([]);
  const [releaseGroups, setReleaseGroups] = useState<ReleaseGroupRow[]>([]);
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [history, setHistory] = useState<QueueItem[]>([]);
  const [autoDj, setAutoDj] = useState(false);
  const [pomodoroPrompt, setPomodoroPrompt] = useState<{ x: number; y: number; value: string } | null>(null);
  const [currentTrack, setCurrentTrack] = useState<QueueItem | null>(null);
  const currentTrackRef = useRef<QueueItem | null>(null);
  const [playerState, setPlayerState] = useState<PlayerState>(DEFAULT_PLAYER_STATE);
  const [search, setSearch] = useState("");
  const [selectedArtistId, setSelectedArtistId] = useState<string | null>(null);
  const [selectedReleaseGroupId, setSelectedReleaseGroupId] = useState<string | null>(null);
  const [wizardPath, setWizardPath] = useState("");
  const [queueHistoryLimitInput, setQueueHistoryLimitInput] = useState("5");
  const [musicRootInput, setMusicRootInput] = useState("");
  const [isSavingMusicRoot, setIsSavingMusicRoot] = useState(false);
  const [isBootstrapping, setIsBootstrapping] = useState(true);
  const [isSubmittingWizard, setIsSubmittingWizard] = useState(false);
  const [isSavingQueueSettings, setIsSavingQueueSettings] = useState(false);
  const [ratingKeyInFlight, setRatingKeyInFlight] = useState<string | null>(null);
  const [playerCoverArt, setPlayerCoverArt] = useState<string | null>(null);
  const [waveformData, setWaveformData] = useState<number[] | null>(null);
  const waveformCanvasRef = useRef<HTMLCanvasElement>(null);
  const waveformContainerRef = useRef<HTMLDivElement>(null);
  const [isModalOpen, setIsModalOpen] = useState(false);
  const [settingsTab, setSettingsTab] = useState<"options" | "issues">("options");
  const [fileIssues, setFileIssues] = useState<FileIssue[]>([]);
  const [fixingOrphans, setFixingOrphans] = useState<Set<string>>(new Set());
  const [deletingBackups, setDeletingBackups] = useState<Set<string>>(new Set());
  const [resolvingDuplicates, setResolvingDuplicates] = useState<Set<string>>(new Set());
  const [error, setError] = useState<string | null>(null);
  const [selectedTag, setSelectedTag] = useState<string | null>(null);
  const [_allTags, setAllTags] = useState<string[]>([]);
  const [browserLeftTab, setBrowserLeftTab] = useState<"artists" | "albums" | "smartplaylists">("artists");
  const [smartQuery, setSmartQuery] = useState("");
  const [smartResult, setSmartResult] = useState<SmartPlaylistResult | null>(null);
  const [smartError, setSmartError] = useState<string | null>(null);
  const [isRunningQuery, setIsRunningQuery] = useState(false);
  const [playlists, setPlaylists] = useState<PlaylistRow[]>([]);
  const [savePlaylistName, setSavePlaylistName] = useState("");
  const [isSavingPlaylist, setIsSavingPlaylist] = useState(false);
  const [externalCommands, setExternalCommands] = useState<ExternalCommand[]>([]);
  const [newCmdName, setNewCmdName] = useState("");
  const [newCmdTemplate, setNewCmdTemplate] = useState("");
  const [lastfmStatus, setLastfmStatus] = useState<LastFmStatus | null>(null);
  const [lastfmAuthUrl, setLastfmAuthUrl] = useState<string | null>(null);
  const [isLastfmConnecting, setIsLastfmConnecting] = useState(false);
  const [isLastfmCompleting, setIsLastfmCompleting] = useState(false);
  const [lastfmApiKeyInput, setLastfmApiKeyInput] = useState("");
  const [lastfmSharedSecretInput, setLastfmSharedSecretInput] = useState("");
  const [isSavingLastfmCredentials, setIsSavingLastfmCredentials] = useState(false);
  const [currentTrackLoved, setCurrentTrackLoved] = useState(false);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [artistFixModal, setArtistFixModal] = useState<{ artistId: string; artistName: string } | null>(null);
  const [artistFixCheckResult, setArtistFixCheckResult] = useState<CompoundArtistCheck | null>(null);
  const [artistFixChecking, setArtistFixChecking] = useState(false);
  const [artistFixError, setArtistFixError] = useState<string | null>(null);
  const [splitRecordingId, setSplitRecordingId] = useState<string | null>(null);
  const [splitData, setSplitData] = useState<SplitRecordingDetail | null>(null);
  const [splitSelectedSourceIds, setSplitSelectedSourceIds] = useState<Set<string>>(new Set());
  const [splitIsSubmitting, setSplitIsSubmitting] = useState(false);
  const [splitError, setSplitError] = useState<string | null>(null);
  const [sortColumn, setSortColumn] = useState<SortColumn>("artist");
  const [sortAsc, setSortAsc] = useState(true);

  const COLUMN_KEYS = ["title", "artist", "releases", "genre", "rating", "duration", "tags", "plays", "last_played", "sources"] as const;
  const DEFAULT_COL_WIDTHS: Record<string, number> = {
    title: 300, artist: 200, releases: 280, genre: 90, rating: 110,
    duration: 80, tags: 150, plays: 65, last_played: 120, sources: 300,
  };
  const [colWidths, setColWidths] = useState<Record<string, number>>(() => {
    try {
      const saved = localStorage.getItem("colWidths");
      if (saved) return { ...DEFAULT_COL_WIDTHS, ...JSON.parse(saved) };
    } catch { /* ignore */ }
    return DEFAULT_COL_WIDTHS;
  });
  useEffect(() => {
    const timer = setTimeout(() => localStorage.setItem("colWidths", JSON.stringify(colWidths)), 300);
    return () => clearTimeout(timer);
  }, [colWidths]);
  const activeResize = useRef<{ column: string; startX: number; startWidth: number } | null>(null);
  const [navState, dispatchNav] = useReducer(navReducer, null);
  const detailView = navState ? navState.stack[navState.index] : null;
  const canGoBack = navState ? navState.index > 0 : false;
  const canGoForward = navState ? navState.index < navState.stack.length - 1 : false;
  const [pendingJobs, setPendingJobs] = useState(0);
  const [currentJobType, setCurrentJobType] = useState("");
  const releaseGroupSearchInFlightRef = useRef(false);
  const releaseGroupPruneInFlightRef = useRef(false);
  const queuedReleaseGroupSearchRef = useRef<{ artistId: string | null; search: string } | null>(null);

  const queueHistoryLimit = bootstrap?.config.queue_history_limit ?? 5;
  const isPlaying = playerState.status === "playing";
  const activeDurationMs = playerState.duration_ms ?? currentTrack?.duration_ms ?? 0;

  const queuedDurationMs = useMemo(
    () => queue.reduce((sum, item) => sum + (item.duration_ms ?? 0), 0),
    [queue],
  );

  const currentRemaining = currentTrack
    ? Math.max(0, activeDurationMs - playerState.position_ms)
    : 0;
  const totalRemainingMs = currentRemaining + queuedDurationMs;
  const totalCount = queue.length + (currentTrack ? 1 : 0);

  const selectedArtist = useMemo(
    () => artists.find((artist) => artist.id === selectedArtistId) ?? null,
    [artists, selectedArtistId],
  );

  const selectedReleaseGroup = useMemo(
    () => releaseGroups.find((releaseGroup) => releaseGroup.id === selectedReleaseGroupId) ?? null,
    [releaseGroups, selectedReleaseGroupId],
  );

  const visibleArtists = useMemo(() => {
    const needle = search.trim().toLowerCase();
    const filteredArtists = !needle
      ? artists
      : artists.filter((artist) => artist.name.toLowerCase().includes(needle));

    return [...filteredArtists].sort((a, b) => {
      const ratingDelta = compareAggregateRatings(a.rating, b.rating);
      if (ratingDelta !== 0) {
        return ratingDelta;
      }

      const lastPlayedDelta = compareAggregateLastPlayed(a.last_played, b.last_played);
      if (lastPlayedDelta !== 0) {
        return lastPlayedDelta;
      }

      const recordingCountDelta = b.recording_count - a.recording_count;
      if (recordingCountDelta !== 0) {
        return recordingCountDelta;
      }

      return a.sort_name.localeCompare(b.sort_name) || a.name.localeCompare(b.name);
    });
  }, [artists, search]);

  const visibleReleaseGroups = useMemo(() => (
    [...releaseGroups].sort((a, b) => {
      const ratingDelta = compareAggregateRatings(a.rating, b.rating);
      if (ratingDelta !== 0) {
        return ratingDelta;
      }

      const lastPlayedDelta = compareAggregateLastPlayed(a.last_played, b.last_played);
      if (lastPlayedDelta !== 0) {
        return lastPlayedDelta;
      }

      const recordingCountDelta = b.recording_count - a.recording_count;
      if (recordingCountDelta !== 0) {
        return recordingCountDelta;
      }

      return a.title.localeCompare(b.title) || (a.artist_credit_name ?? "").localeCompare(b.artist_credit_name ?? "");
    })
  ), [releaseGroups]);

  const filteredRecordings = useMemo(() => {
    const needle = search.trim().toLowerCase();

    return recordings
      .filter((recording) => {
        if (selectedArtist) {
          if (
            recording.primary_artist_id !== selectedArtist.id &&
            !recording.artist_ids.includes(selectedArtist.id)
          ) {
            return false;
          }
        }

        if (selectedReleaseGroup) {
          if (!recording.releases.some(r => r.release_group_id === selectedReleaseGroup.id)) {
            return false;
          }
        }

        if (selectedTag) {
          if (!recording.tags.includes(selectedTag)) {
            return false;
          }
        }

        if (!needle) {
          return true;
        }

        return (
          [recording.artist_credit_name, recording.title, recording.genre]
            .filter(Boolean)
            .some((value) => value!.toLowerCase().includes(needle)) ||
          recording.releases.some(r => r.release_group_title.toLowerCase().includes(needle)) ||
          (recording.source_paths ?? []).some((p) => p.toLowerCase().includes(needle))
        );
      })
      .sort((a, b) => compareRecordings(a, b, sortColumn, sortAsc));
  }, [recordings, search, selectedArtist, selectedReleaseGroup, selectedTag, sortColumn, sortAsc]);

  const trackTableScrollRef = useRef<HTMLDivElement>(null);
  const rowVirtualizer = useVirtualizer({
    count: filteredRecordings.length,
    getScrollElement: () => trackTableScrollRef.current,
    estimateSize: () => 41,
    overscan: 10,
  });

  async function loadRecordings() {
    const rows: RecordingRow[] = [];
    let offset = 0;

    while (true) {
      const page = await invoke<RecordingRow[]>("list_recordings", {
        limit: RECORDINGS_PAGE_SIZE,
        offset,
      });
      rows.push(...page);

      if (page.length < RECORDINGS_PAGE_SIZE) {
        break;
      }

      offset += page.length;
    }

    setRecordings(rows);
  }

  async function loadAllTags() {
    const tags = await invoke<string[]>("list_all_tags");
    setAllTags(tags);
  }

  async function loadPlaylists() {
    const rows = await invoke<PlaylistRow[]>("list_playlists");
    setPlaylists(rows);
  }

  async function runSmartQuery() {
    const q = smartQuery.trim();
    if (!q) return;
    setIsRunningQuery(true);
    setSmartError(null);
    try {
      const result = await invoke<SmartPlaylistResult>("evaluate_smart_playlist", { query: q });
      setSmartResult(result);
    } catch (e) {
      setSmartError(e instanceof Error ? e.message : String(e));
      setSmartResult(null);
    } finally {
      setIsRunningQuery(false);
    }
  }

  async function saveSmartPlaylist() {
    const name = savePlaylistName.trim();
    const q = smartQuery.trim();
    if (!name || !q) return;
    setIsSavingPlaylist(true);
    setSmartError(null);
    try {
      await invoke<PlaylistRow>("save_smart_playlist", { request: { name, query: q } });
      setSavePlaylistName("");
      await loadPlaylists();
    } catch (e) {
      setSmartError(e instanceof Error ? e.message : String(e));
    } finally {
      setIsSavingPlaylist(false);
    }
  }

  async function deletePlaylist(id: number) {
    try {
      await invoke("delete_playlist", { id });
      await loadPlaylists();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  function handleColumnSort(col: SortColumn) {
    if (sortColumn === col) {
      setSortAsc((prev) => !prev);
    } else {
      setSortColumn(col);
      setSortAsc(true);
    }
  }

  function onColumnResizeStart(e: React.MouseEvent, column: string) {
    e.preventDefault();
    e.stopPropagation();
    const th = (e.currentTarget as HTMLElement).closest("th");
    if (!th) return;
    const rect = th.getBoundingClientRect();
    activeResize.current = { column, startX: e.clientX, startWidth: rect.width };

    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";

    const onMouseMove = (e: MouseEvent) => {
      if (!activeResize.current) return;
      const { column, startX, startWidth } = activeResize.current;
      const newWidth = Math.round(Math.max(40, startWidth + (e.clientX - startX)));
      setColWidths((prev) => ({ ...prev, [column]: newWidth }));
    };

    const onMouseUp = () => {
      activeResize.current = null;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      document.removeEventListener("mousemove", onMouseMove);
      document.removeEventListener("mouseup", onMouseUp);
    };

    document.addEventListener("mousemove", onMouseMove);
    document.addEventListener("mouseup", onMouseUp);
  }

  function enqueueAll(recordings: RecordingRow[]) {
    const playable = recordings.filter((r) => r.primary_source_id !== null);
    if (playable.length === 0) {
      setError("No playable tracks in results.");
      return;
    }
    setError(null);
    if (!currentTrack && playerState.status === "stopped" && playable.length > 0) {
      const [first, ...rest] = playable;
      setCurrentTrack(first);
      setQueue((q) => [...q, ...rest]);
    } else {
      setQueue((q) => [...q, ...playable]);
    }
  }

  function handlePomodoroFill(targetDurationMs: number) {
    setError(null);

    const playable = recordings.filter((r) => r.primary_source_id !== null);
    if (playable.length === 0) {
      setError("No playable tracks available for Pomodoro.");
      return;
    }

    const batch = calculatePomodoroBatch({
      playable,
      queue,
      history,
      currentTrack,
      targetDurationMs,
    });

    if (batch.length === 0) {
      setError("Not enough tracks to fill the Pomodoro duration.");
      return;
    }

    enqueueAll(batch);
  }

  function openPomodoroPrompt(e: React.MouseEvent) {
    e.preventDefault();
    setPomodoroPrompt({ x: e.clientX, y: e.clientY, value: "" });
  }

  function submitPomodoroPrompt() {
    if (!pomodoroPrompt) return;
    const targetDurationMs = parsePomodoroDuration(pomodoroPrompt.value);
    if (targetDurationMs === null) {
      setError("Invalid Pomodoro duration. Try e.g. 25m, 1h, or 1h30m.");
      return;
    }
    setPomodoroPrompt(null);
    handlePomodoroFill(targetDurationMs);
  }

  async function loadArtists() {
    const rows = await invoke<ArtistRow[]>("list_artists");
    setArtists(rows);
    setSelectedArtistId((current) => (
      current && !rows.some((artist) => artist.id === current) ? null : current
    ));
    return rows;
  }

  async function loadReleaseGroups(nextArtistId: string | null, nextSearch: string) {
    const rows = await invoke<ReleaseGroupRow[]>("list_release_groups", {
      artistId: nextArtistId,
      search: nextSearch.trim() || null,
    });
    const visibleRows = rows.filter((releaseGroup) => releaseGroup.recording_count > 0);
    setReleaseGroups(visibleRows);
    setSelectedReleaseGroupId((current) => (
      current && !visibleRows.some((releaseGroup) => releaseGroup.id === current) ? null : current
    ));
    if (visibleRows.length !== rows.length && !releaseGroupPruneInFlightRef.current) {
      releaseGroupPruneInFlightRef.current = true;
      void (async () => {
        try {
          await invoke("prune_empty_library_entities_command");
          const artistRows = await loadArtists();
          const resolvedArtistId = nextArtistId && artistRows.some((artist) => artist.id === nextArtistId)
            ? nextArtistId
            : null;
          await loadReleaseGroups(resolvedArtistId, nextSearch);
        } catch (pruneError) {
          await reportPoolTimeout("empty album cleanup", pruneError);
          setError(pruneError instanceof Error ? pruneError.message : String(pruneError));
        } finally {
          releaseGroupPruneInFlightRef.current = false;
        }
      })();
    }
    return visibleRows;
  }

  function scheduleReleaseGroupSearch(nextArtistId: string | null, nextSearch: string) {
    queuedReleaseGroupSearchRef.current = {
      artistId: nextArtistId,
      search: nextSearch,
    };

    if (releaseGroupSearchInFlightRef.current) {
      return;
    }

    releaseGroupSearchInFlightRef.current = true;
    void (async () => {
      while (queuedReleaseGroupSearchRef.current) {
        const request = queuedReleaseGroupSearchRef.current;
        queuedReleaseGroupSearchRef.current = null;

        try {
          await loadReleaseGroups(request.artistId, request.search);
        } catch (loadError) {
          await reportPoolTimeout("release group search", loadError);
          setError(loadError instanceof Error ? loadError.message : String(loadError));
        }
      }

      releaseGroupSearchInFlightRef.current = false;
      const pendingRequest = queuedReleaseGroupSearchRef.current as { artistId: string | null; search: string } | null;
      if (pendingRequest) {
        scheduleReleaseGroupSearch(pendingRequest.artistId, pendingRequest.search);
      }
    })();
  }

  async function loadLibraryData(nextArtistId = selectedArtistId, nextSearch = search) {
    try {
      await loadRecordings();
      const artistRows = await loadArtists();
      const resolvedArtistId = nextArtistId && artistRows.some((artist) => artist.id === nextArtistId)
        ? nextArtistId
        : null;
      await loadReleaseGroups(resolvedArtistId, nextSearch);
      await loadAllTags();
      await loadPlaylists();
    } catch (loadError) {
      await reportPoolTimeout("loadLibraryData", loadError);
      setError(loadError instanceof Error ? loadError.message : String(loadError));
    }
  }

  async function loadBootstrap() {
    try {
      const [bootstrapResult, currentPlayerState, lastfmStatusResult] = await Promise.all([
        invoke<AppBootstrap>("get_app_bootstrap"),
        invoke<PlayerState>("get_player_state"),
        invoke<LastFmStatus>("get_lastfm_status"),
      ]);
      setBootstrap(bootstrapResult);
      setQueueHistoryLimitInput(String(bootstrapResult.config.queue_history_limit));
      setMusicRootInput(bootstrapResult.config.music_root ?? "");
      setExternalCommands(bootstrapResult.config.external_commands ?? []);
      setPlayerState(currentPlayerState);
      setLastfmStatus(lastfmStatusResult);
    } catch (loadError) {
      await reportPoolTimeout("loadBootstrap", loadError);
      setError(loadError instanceof Error ? loadError.message : String(loadError));
    } finally {
      setIsBootstrapping(false);
    }
  }

  useEffect(() => {
    void loadBootstrap();
  }, []);

  // After bootstrap, run the orphan-source check and refresh issue list.
  // Also re-check any time the issues tab opens.
  useEffect(() => {
    if (!bootstrap) return;
    void (async () => {
      try {
        await invoke("find_orphan_sources");
      } catch { /* best-effort */ }
      try {
        const issues = await invoke<FileIssue[]>("get_file_issues");
        setFileIssues(issues);
      } catch { /* best-effort */ }
    })();
  }, [bootstrap]);

  // Reload the file issue list whenever the issues tab becomes visible.
  useEffect(() => {
    if (!isModalOpen || settingsTab !== "issues") return;
    invoke<FileIssue[]>("get_file_issues")
      .then(setFileIssues)
      .catch(() => {});
  }, [isModalOpen, settingsTab]);

  useEffect(() => {
    if (!bootstrap || bootstrap.needs_setup) {
      return;
    }

    void loadLibraryData();
  }, [bootstrap?.needs_setup]);

  useEffect(() => {
    if (!bootstrap || bootstrap.needs_setup) {
      return;
    }

    const timeoutId = window.setTimeout(() => {
      scheduleReleaseGroupSearch(selectedArtistId, search);
    }, 250);

    return () => window.clearTimeout(timeoutId);
  }, [bootstrap?.needs_setup, search, selectedArtistId]);

  // Fetch recording detail when split modal opens
  useEffect(() => {
    if (!splitRecordingId) {
      setSplitData(null);
      setSplitSelectedSourceIds(new Set());
      setSplitError(null);
      return;
    }
    invoke<SplitRecordingDetail>("get_recording_detail", { id: splitRecordingId })
      .then((detail) => {
        setSplitData(detail);
        // Pre-select all sources except the last one as a reasonable default
        const ids = detail.sources.slice(0, -1).map((s) => s.id);
        setSplitSelectedSourceIds(new Set(ids));
      })
      .catch((e) => setSplitError(e instanceof Error ? e.message : String(e)));
  }, [splitRecordingId]);

  useEffect(() => {
    if (!selectedReleaseGroupId) {
      return;
    }

    const stillExists = releaseGroups.some((releaseGroup) => releaseGroup.id === selectedReleaseGroupId);
    if (!stillExists) {
      setSelectedReleaseGroupId(null);
    }
  }, [releaseGroups, selectedReleaseGroupId]);

  useEffect(() => {
    if (!bootstrap || bootstrap.needs_setup) {
      return;
    }

    const isRunning = bootstrap.import_progress.is_running;
    let wasRunning = isRunning;
    let cancelled = false;
    let timeoutId: number | null = null;

    async function poll() {
      try {
        const progress = await invoke<ImportProgress>("get_import_progress");
        const summary = await invoke<LibrarySummary>("get_library_summary");
        setBootstrap((current) =>
          current
            ? {
                ...current,
                import_progress: progress,
                library_summary: summary,
              }
            : current,
        );

        const shouldReloadLibrary = wasRunning && !progress.is_running;
        wasRunning = progress.is_running;
        if (shouldReloadLibrary) {
          await loadLibraryData(selectedArtistId, search);
        }

        if (!cancelled) {
          const interval = progress.is_running ? 2000 : 60000;
          timeoutId = window.setTimeout(() => {
            void poll();
          }, interval);
        }
      } catch (pollError) {
        await reportPoolTimeout("import progress poll", pollError);
        setError(pollError instanceof Error ? pollError.message : String(pollError));
        if (!cancelled) {
          timeoutId = window.setTimeout(() => {
            void poll();
          }, 60000);
        }
      }
    }

    const initialInterval = isRunning ? 2000 : 60000;
    timeoutId = window.setTimeout(() => {
      void poll();
    }, initialInterval);

    return () => {
      cancelled = true;
      if (timeoutId !== null) {
        window.clearTimeout(timeoutId);
      }
    };
  }, [bootstrap?.needs_setup, bootstrap?.import_progress.is_running, search, selectedArtistId]);

  useEffect(() => {
    if (recordings.length === 0) {
      return;
    }

    const recordingsById = new Map(recordings.map((recording) => [recording.id, recording] as const));
    setHistory((current) => reconcileRecordingList(current, recordingsById));
    setQueue((current) => reconcileRecordingList(current, recordingsById));
    setCurrentTrack((current) => (current ? reconcileRecording(current, recordingsById) : current));
    setSmartResult((current) => (
      current
        ? {
            ...current,
            recordings: reconcileRecordingList(current.recordings, recordingsById),
          }
        : current
    ));
    setContextMenu((current) => (
      current && current.kind === "recording"
        ? {
            ...current,
            recording: reconcileRecording(current.recording, recordingsById),
          }
        : current
    ));
  }, [recordings]);

  useEffect(() => {
    let isMounted = true;

    async function subscribe() {
      const unlistenState = await listen<PlayerState>("player-state", (event) => {
        if (isMounted) {
          setPlayerState(event.payload);
        }
      });
      const unlistenPosition = await listen<number>("player-position", (event) => {
        if (!isMounted) {
          return;
        }

        setPlayerState((current) => ({
          ...current,
          position_ms: event.payload,
        }));
	      });
	      const unlistenEnded = await listen<{
        source_id: string;
        position_ms: number;
      }>("player-track-ended", (event) => {
        if (isMounted) {
          void completeCurrentTrack(event.payload.position_ms);
        }
      });
      const unlistenError = await listen<PlayerErrorEvent>("player-error", (event) => {
        if (isMounted) {
          setError(event.payload.message);
        }
      });
      const unlistenJobUpdate = await listen<{remaining: number; job_type: string}>("job-update", (event) => {
        if (isMounted) {
          setPendingJobs(Math.max(0, event.payload.remaining));
          setCurrentJobType(event.payload.job_type);
          if (event.payload.remaining <= 0 && !bootstrap?.import_progress.is_running) {
            void loadLibraryData(selectedArtistId, search);
          }
        }
      });
      const unlistenLoved = await listen<boolean>("lastfm-loved-status", (event) => {
        if (isMounted) {
          setCurrentTrackLoved(event.payload);
        }
      });

      return () => {
        unlistenState();
        unlistenPosition();
        unlistenEnded();
        unlistenError();
        unlistenJobUpdate();
        unlistenLoved();
      };
    }

    const unsubscribePromise = subscribe();

    return () => {
      isMounted = false;
      void unsubscribePromise.then((unsubscribe) => unsubscribe?.());
    };
  }, [queueHistoryLimit]);

  useEffect(() => {
    if (currentTrack || queue.length === 0 || playerState.status === "loading") {
      return;
    }

    const [nextTrack, ...rest] = queue;
    setCurrentTrack(nextTrack);
    setQueue(rest);
  }, [currentTrack, playerState.status, queue]);

  useEffect(() => {
    if (!autoDj || queue.length > 0) {
      return;
    }
    const playable = recordings.filter((r) => r.primary_source_id !== null);
    if (playable.length === 0) {
      return;
    }

    const pick = pickNextTrack({
      playable,
      queue,
      history,
      currentTrack,
    });

    if (pick) {
      setQueue((current) => [...current, pick]);
    }
  }, [autoDj, queue, recordings, history, currentTrack]);

  useEffect(() => {
    if (!currentTrack?.primary_source_id) {
      return;
    }

    setError(null);
    void invoke<PlayerState>("play", {
      request: { source_id: currentTrack.primary_source_id },
    }).catch((playError) => {
      setError(playError instanceof Error ? playError.message : String(playError));
      setCurrentTrack(null);
    });
  }, [currentTrack?.id, currentTrack?.primary_source_id]);

  useEffect(() => {
    if (!playerState.source_id) {
      setPlayerCoverArt(null);
      setWaveformData(null);
      return;
    }
    void invoke<string | null>("get_cover_art", { sourceId: playerState.source_id }).then(
      (art) => setPlayerCoverArt(art),
      () => setPlayerCoverArt(null),
    );
    void invoke<number[]>("get_waveform", { sourceId: playerState.source_id }).then(
      setWaveformData,
      () => setWaveformData(null),
    );
  }, [playerState.source_id]);

  // Reset loved status on track change; backend emits lastfm-loved-status with the actual value.
  useEffect(() => {
    if (!lastfmStatus?.logged_in) return;
    setCurrentTrackLoved(false);
  }, [playerState.source_id, lastfmStatus?.logged_in]);

  // Draw waveform on canvas whenever data, position, or container size changes.
  useEffect(() => {
    const canvas = waveformCanvasRef.current;
    const container = waveformContainerRef.current;
    if (!canvas || !container || !waveformData || waveformData.length === 0) {
      return;
    }

    let animFrameId: number;

    function draw() {
      const dpr = window.devicePixelRatio || 1;
      const canvas = waveformCanvasRef.current;
      const container = waveformContainerRef.current;
      const data = waveformData;
      if (!canvas || !container || !data || data.length === 0) return;
      const rect = container.getBoundingClientRect();
      const w = rect.width;
      const h = rect.height;
      if (w <= 0 || h <= 0) return;

      // Resize canvas backing store only when needed (avoids flicker)
      if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
        canvas.width = Math.round(w * dpr);
        canvas.height = Math.round(h * dpr);
        canvas.style.width = `${w}px`;
        canvas.style.height = `${h}px`;
      }

      const ctx = canvas.getContext("2d");
      if (!ctx) return;

      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);

      // Calculate the split point for played vs unplayed
      const playRatio = activeDurationMs > 0
        ? Math.min(playerState.position_ms / activeDurationMs, 1)
        : 0;
      const splitX = w * playRatio;

      const barWidth = w / data.length;
      const midY = h / 2;
      const maxBarHeight = Math.max(h * 0.85, 2);

      // Draw dim (unplayed) portion first — full waveform in dim color
      ctx.fillStyle = "rgba(143, 165, 194, 0.25)";
      for (let i = 0; i < data.length; i++) {
        const barHeight = Math.max(data[i] * maxBarHeight, 1);
        const x = i * barWidth;
        ctx.fillRect(x, midY - barHeight / 2, Math.max(1, barWidth - 1), barHeight);
      }

      // Draw bright (played) portion on top — clipped to the left of splitX
      ctx.save();
      ctx.beginPath();
      ctx.rect(0, 0, splitX, h);
      ctx.clip();

      ctx.fillStyle = "#efb15a";
      for (let i = 0; i < data.length; i++) {
        const barHeight = Math.max(data[i] * maxBarHeight, 1);
        const x = i * barWidth;
        ctx.fillRect(x, midY - barHeight / 2, Math.max(1, barWidth - 1), barHeight);
      }

      ctx.restore();
    }

    const resizeObserver = new ResizeObserver(() => {
      cancelAnimationFrame(animFrameId);
      animFrameId = requestAnimationFrame(draw);
    });
    resizeObserver.observe(container);

    // Initial draw
    animFrameId = requestAnimationFrame(draw);

    return () => {
      cancelAnimationFrame(animFrameId);
      resizeObserver.disconnect();
    };
  }, [waveformData, playerState.position_ms, activeDurationMs]);

  // Keep ref in sync so stale-closure callbacks (e.g. player-track-ended listener)
  // always see the latest value.
  currentTrackRef.current = currentTrack;

  async function completeCurrentTrack(positionMs?: number) {
    const finishedTrack = currentTrackRef.current;
    if (finishedTrack) {
      try {
        await invoke("record_play_history", {
          input: {
            source_id: finishedTrack.primary_source_id!,
            duration_played_ms: positionMs ?? playerState.position_ms,
          },
        });
      } catch (recordError) {
        setError(recordError instanceof Error ? recordError.message : String(recordError));
      }
      void loadRecordings();
      void (async () => {
        try {
          const artistRows = await loadArtists();
          const resolvedArtistId = selectedArtistId && artistRows.some((artist) => artist.id === selectedArtistId)
            ? selectedArtistId
            : null;
          await loadReleaseGroups(resolvedArtistId, search);
        } catch (reloadError) {
          setError(reloadError instanceof Error ? reloadError.message : String(reloadError));
        }
      })();

      setHistory((current) => [finishedTrack, ...current].slice(0, queueHistoryLimit));
    }

    setCurrentTrack(null);
    setPlayerState((current) => ({
      ...current,
      position_ms: 0,
      duration_ms: null,
      status: "stopped",
      recording_id: null,
      source_id: null,
    }));
  }

  async function handleWizardSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setIsSubmittingWizard(true);
    setError(null);

    try {
      const config = await invoke<AppConfig>("complete_initial_setup", {
        request: { music_root: wizardPath },
      });
      const summary = await invoke<LibrarySummary>("get_library_summary");
      const progress = await invoke<ImportProgress>("get_import_progress");
      setBootstrap({
        needs_setup: false,
        config,
        import_progress: progress,
        library_summary: summary,
      });
      setQueueHistoryLimitInput(String(config.queue_history_limit));
      await loadLibraryData();
    } catch (submitError) {
      setError(submitError instanceof Error ? submitError.message : String(submitError));
    } finally {
      setIsSubmittingWizard(false);
    }
  }

  async function handleRescan() {
    setError(null);
    try {
      const progress = await invoke<ImportProgress>("trigger_library_scan");
      setBootstrap((current) =>
        current
          ? {
              ...current,
              import_progress: progress,
            }
          : current,
      );
    } catch (scanError) {
      setError(scanError instanceof Error ? scanError.message : String(scanError));
    }
  }

  async function handleRescanAll() {
    setError(null);
    try {
      await invoke("rescan_all_sources");
    } catch (scanError) {
      setError(scanError instanceof Error ? scanError.message : String(scanError));
    }
  }

  async function saveQueueSettings() {
    const parsed = Number.parseInt(queueHistoryLimitInput, 10);
    if (!Number.isFinite(parsed) || parsed < 1) {
      setError("Queue history limit must be at least 1.");
      return;
    }

    setIsSavingQueueSettings(true);
    setError(null);

    try {
      const config = await invoke<AppConfig>("update_queue_settings", {
        update: { queue_history_limit: parsed },
      });
      setBootstrap((current) =>
        current
          ? {
              ...current,
              config,
            }
          : current,
      );
      setHistory((current) => current.slice(0, config.queue_history_limit));
    } catch (saveError) {
      setError(saveError instanceof Error ? saveError.message : String(saveError));
    } finally {
      setIsSavingQueueSettings(false);
    }
  }

  async function saveMusicRoot() {
    setIsSavingMusicRoot(true);
    setError(null);
    try {
      const config = await invoke<AppConfig>("set_music_root", { path: musicRootInput });
      setBootstrap((current) => current ? { ...current, config } : current);
    } catch (saveError) {
      setError(saveError instanceof Error ? saveError.message : String(saveError));
    } finally {
      setIsSavingMusicRoot(false);
    }
  }

  async function saveExternalCommands(cmds: ExternalCommand[]) {
    try {
      const config = await invoke<AppConfig>("save_external_commands", { commands: cmds });
      setExternalCommands(config.external_commands ?? []);
      setBootstrap((current) => current ? { ...current, config } : current);
    } catch (saveError) {
      setError(saveError instanceof Error ? saveError.message : String(saveError));
    }
  }

  async function handleSpawnExternalCommand(template: string, filePath: string) {
    try {
      await invoke("spawn_external_command", { template, filePath });
    } catch (spawnError) {
      setError(spawnError instanceof Error ? spawnError.message : String(spawnError));
    }
  }

  async function handleRescanSource(path: string) {
    try {
      setError(null);
      await invoke("rescan_source", { path });
    } catch (rescanError) {
      await reportPoolTimeout("source rescan", rescanError);
      setError(rescanError instanceof Error ? rescanError.message : String(rescanError));
    } finally {
      await loadLibraryData(selectedArtistId, search);
    }
  }

  async function handleRescanSources(paths: string[]) {
    try {
      setError(null);
      await invoke("rescan_sources", { paths });
    } catch (rescanError) {
      await reportPoolTimeout("source rescan", rescanError);
      setError(rescanError instanceof Error ? rescanError.message : String(rescanError));
    } finally {
      await loadLibraryData(selectedArtistId, search);
    }
  }

  async function handleRescanArtist(artistId: string) {
    try {
      setError(null);
      await invoke("rescan_sources_for_artist", { artistId });
    } catch (rescanError) {
      await reportPoolTimeout("source rescan", rescanError);
      setError(rescanError instanceof Error ? rescanError.message : String(rescanError));
    } finally {
      await loadLibraryData(selectedArtistId, search);
    }
  }

  async function handleRescanRecording(recordingId: string) {
    try {
      setError(null);
      await invoke("rescan_sources_for_recording", { recordingId });
    } catch (rescanError) {
      await reportPoolTimeout("source rescan", rescanError);
      setError(rescanError instanceof Error ? rescanError.message : String(rescanError));
    } finally {
      await loadLibraryData(selectedArtistId, search);
    }
  }

  async function handleRescanReleaseGroup(releaseGroupId: string) {
    try {
      setError(null);
      await invoke("rescan_sources_for_release_group", { releaseGroupId });
    } catch (rescanError) {
      await reportPoolTimeout("source rescan", rescanError);
      setError(rescanError instanceof Error ? rescanError.message : String(rescanError));
    } finally {
      await loadLibraryData(selectedArtistId, search);
    }
  }

  function openRecordingContextMenu(e: React.MouseEvent, recording: RecordingRow) {
    e.preventDefault();
    const path = recording.primary_source_path ?? recording.source_paths[0];
    setContextMenu({ kind: "recording", x: e.clientX, y: e.clientY, path: path ?? "", recording });
  }

  function openEntitySourceContextMenu(e: React.MouseEvent, filePath: string) {
    e.preventDefault();
    setContextMenu({ kind: "source", x: e.clientX, y: e.clientY, path: filePath });
  }

  function enqueueRecording(recording: RecordingRow) {
    if (!recording.primary_source_id) {
      setError(`No playable local file is available for "${recording.title}".`);
      return;
    }

    setError(null);
    if (!currentTrack && playerState.status === "stopped") {
      setCurrentTrack(recording);
      return;
    }

    setQueue((current) => [...current, recording]);
  }

  function removeFromQueue(index: number) {
    setQueue((current) => current.filter((_, i) => i !== index));
  }

  function moveQueueItem(fromIndex: number, toIndex: number) {
    setQueue((current) => {
      if (fromIndex < 0 || fromIndex >= current.length) {
        return current;
      }
      const moved = current[fromIndex];
      const withoutFrom = [...current.slice(0, fromIndex), ...current.slice(fromIndex + 1)];
      const insertAt = Math.min(Math.max(toIndex, 0), withoutFrom.length);
      return [...withoutFrom.slice(0, insertAt), moved, ...withoutFrom.slice(insertAt)];
    });
  }

  // Recording id of the item being dragged, or null when no drag is active. An id
  // (not an index or object reference) survives the queue shifting and its items
  // being reconciled mid-drag.
  const dragRef = useRef<string | null>(null);
  const [dragOverId, setDragOverId] = useState<string | null>(null);
  const queueListRef = useRef<HTMLOListElement>(null);

  // Current index of the dragged recording, or null if no drag is active or the
  // recording is no longer in the queue. Resolved at drop time (not drag start)
  // because the queue can shift (auto-advance) and be reconciled (fresh objects)
  // mid-drag — neither index nor object reference stays valid across both.
  function resolveDraggedIndex(): number | null {
    const draggedId = dragRef.current;
    if (draggedId === null) return null;
    const from = queue.findIndex((item) => item.id === draggedId);
    return from === -1 ? null : from;
  }

  async function handlePauseResume() {
    if (!currentTrack) {
      if (queue.length > 0) {
        const [nextTrack, ...rest] = queue;
        setCurrentTrack(nextTrack);
        setQueue(rest);
      }
      return;
    }

    try {
      if (playerState.status === "playing") {
        setPlayerState((current) => ({ ...current, status: "paused" }));
        await invoke<PlayerState>("pause");
      } else {
        setPlayerState((current) => ({ ...current, status: "loading" }));
        await invoke<PlayerState>("resume");
      }
    } catch (playbackError) {
      setError(playbackError instanceof Error ? playbackError.message : String(playbackError));
    }
  }

  async function handleSkipBack() {
    // If more than 3 s in, just restart the current track
    if (playerState.position_ms >= 3000 || history.length === 0) {
      void handleSeek(0);
      return;
    }
    const [prev, ...restHistory] = history;
    setHistory(restHistory);
    if (currentTrack) {
      setQueue((current) => [currentTrack, ...current]);
    }
    try {
      await invoke<PlayerState>("stop");
    } catch { /* engine may already be stopped */ }
    setCurrentTrack(prev);
  }

  async function handleSkip() {
    try {
      await invoke<PlayerState>("stop");
    } catch (stopError) {
      setError(stopError instanceof Error ? stopError.message : String(stopError));
    }
    await completeCurrentTrack(playerState.position_ms);
  }

  async function handleSeek(nextPositionMs: number) {
    try {
      setPlayerState((current) => ({
        ...current,
        position_ms: nextPositionMs,
      }));
      await invoke<PlayerState>("seek", {
        request: { position_ms: nextPositionMs },
      });
    } catch (seekError) {
      setError(seekError instanceof Error ? seekError.message : String(seekError));
    }
  }

  async function handleSplitConfirm() {
    if (!splitData || splitSelectedSourceIds.size === 0) return;
    const sourceIdsToMove = Array.from(splitSelectedSourceIds);
    setSplitIsSubmitting(true);
    setSplitError(null);
    try {
      const newId = await invoke<string>("split_recording", {
        recordingId: splitData.id,
        sourceIdsToMove,
      });
      setSplitRecordingId(null);
      setSplitSelectedSourceIds(new Set());
      await loadLibraryData(selectedArtistId, search);
      dispatchNav({ type: "navigate", nav: { type: "recording", id: newId } });
    } catch (e) {
      setSplitError(e instanceof Error ? e.message : String(e));
    } finally {
      setSplitIsSubmitting(false);
    }
  }

  async function handleVolumeChange(nextVolume: number) {
    try {
      setPlayerState((current) => ({
        ...current,
        volume: nextVolume,
      }));
      await invoke<PlayerState>("set_volume", {
        request: { volume: nextVolume },
      });
    } catch (volumeError) {
      setError(volumeError instanceof Error ? volumeError.message : String(volumeError));
    }
  }

  async function handleNormalizationToggle() {
    const next = !playerState.normalization_enabled;
    try {
      setPlayerState((current) => ({
        ...current,
        normalization_enabled: next,
      }));
      await invoke<PlayerState>("set_normalization_enabled", { enabled: next });
    } catch (normError) {
      setError(normError instanceof Error ? normError.message : String(normError));
    }
  }

  const handleRate = useCallback((recordingId: string, stars: number | null) => {
    void updateRecordingRating(recordingId, stars);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recordings]);

  async function handleLoveTrack() {
    if (!currentTrack?.artist_credit_name || !currentTrack?.title || !lastfmStatus?.logged_in) return;
    try {
      await invoke("lastfm_love_track", {
        request: {
          artist: currentTrack.artist_credit_name,
          track: currentTrack.title,
        },
      });
      setCurrentTrackLoved(true);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  async function handleSaveLastfmCredentials() {
    if (isSavingLastfmCredentials) return;
    setIsSavingLastfmCredentials(true);
    try {
      await invoke("save_lastfm_credentials", {
        apiKey: lastfmApiKeyInput.trim(),
        sharedSecret: lastfmSharedSecretInput.trim(),
      });
      setLastfmStatus((prev) => prev ? { ...prev, configured: true } : { configured: true, logged_in: false, username: null });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setIsSavingLastfmCredentials(false);
    }
  }

  async function handleLastfmConnect() {
    if (isLastfmConnecting) return;
    setIsLastfmConnecting(true);
    try {
      const { url } = await invoke<{ url: string }>("lastfm_get_auth_url");
      setLastfmAuthUrl(url);
      await openUrl(url);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setIsLastfmConnecting(false);
    }
  }

  async function handleLastfmCompleteAuth() {
    if (isLastfmCompleting) return;
    setIsLastfmCompleting(true);
    try {
      const username = await invoke<string>("lastfm_complete_auth");
      setLastfmStatus({ configured: true, logged_in: true, username });
      setLastfmAuthUrl(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setIsLastfmCompleting(false);
    }
  }

  async function handleLastfmDisconnect() {
    try {
      await invoke("lastfm_disconnect");
      setLastfmStatus({ configured: false, logged_in: false, username: null });
      setLastfmAuthUrl(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  async function updateRecordingRating(recordingId: string, stars: number | null) {
    const ratingKey = `recording:${recordingId}`;
    const previousRecordings = recordings;
    const previousHistory = history;
    const previousQueue = queue;
    const previousCurrentTrack = currentTrack;
    const previousSmartResult = smartResult;
    setRatingKeyInFlight(ratingKey);
    setRecordings((current) =>
      current.map((recording) => applyRatingToRecording(recording, recordingId, stars)),
    );
    setHistory((current) =>
      current.map((recording) => applyRatingToRecording(recording, recordingId, stars)),
    );
    setQueue((current) =>
      current.map((recording) => applyRatingToRecording(recording, recordingId, stars)),
    );
    setCurrentTrack((current) =>
      current ? applyRatingToRecording(current, recordingId, stars) : current,
    );
    setSmartResult((current) =>
      current
        ? {
            ...current,
            recordings: current.recordings.map((recording) =>
              applyRatingToRecording(recording, recordingId, stars),
            ),
          }
        : current,
    );

    try {
      const rec = recordings.find((r) => r.id === recordingId);
      const sourceId = rec?.primary_source_id;
      if (!sourceId) throw new Error("No source available to rate");
      const updateResult = await invoke<RecordingRatingUpdateResult>("set_source_rating", {
        request: { source_id: sourceId, stars },
      });
      // Reconcile the recording's displayed rating with the server's computed average
      setRecordings((current) => applyAggregateRatings(current, [updateResult.recording]));
      setHistory((current) => applyAggregateRatings(current, [updateResult.recording]));
      setQueue((current) => applyAggregateRatings(current, [updateResult.recording]));
      setCurrentTrack((current) =>
        current ? (applyAggregateRatings([current], [updateResult.recording])[0] ?? current) : current,
      );
      setArtists((current) => applyAggregateRatings(current, updateResult.artists));
      setReleaseGroups((current) => applyAggregateRatings(current, updateResult.release_groups));
      // Refresh predicted ratings for recordings sharing an artist or album
      // with the rated track (e.g. other tracks on the same album).
      if (updateResult.affected_recordings.length > 0) {
        setRecordings((current) => applyPredictedRatings(current, updateResult.affected_recordings));
        setHistory((current) => applyPredictedRatings(current, updateResult.affected_recordings));
        setQueue((current) => applyPredictedRatings(current, updateResult.affected_recordings));
        setCurrentTrack((current) =>
          current
            ? (applyPredictedRatings([current], updateResult.affected_recordings)[0] ?? current)
            : current,
        );
        setSmartResult((current) =>
          current
            ? {
                ...current,
                recordings: applyPredictedRatings(current.recordings, updateResult.affected_recordings),
              }
            : current,
        );
      }
    } catch (ratingError) {
      setRecordings(previousRecordings);
      setHistory(previousHistory);
      setQueue(previousQueue);
      setCurrentTrack(previousCurrentTrack);
      setSmartResult(previousSmartResult);
      setError(ratingError instanceof Error ? ratingError.message : String(ratingError));
    } finally {
      setRatingKeyInFlight((current) => (current === ratingKey ? null : current));
    }
  }

  if (isBootstrapping) {
    return <main className="loading-screen">Loading thmp5…</main>;
  }

  if (bootstrap?.needs_setup) {
    return (
      <main className="wizard-shell">
        <section className="wizard-card">
          <p className="eyebrow">First-time setup</p>
          <h1>Point thmp5 at your music library</h1>
          <p className="wizard-copy">
            Enter the root folder of your library. Once you submit it, the app
            switches straight to the main UI and starts scanning that folder in
            the background.
          </p>

          {error ? <div className="error-banner">{error}</div> : null}

          <form className="wizard-form" onSubmit={handleWizardSubmit}>
            <label className="input-label" htmlFor="music-root">
              Music library root
            </label>
            <input
              id="music-root"
              className="path-input"
              onChange={(event) => setWizardPath(event.currentTarget.value)}
              placeholder="/home/user/Music"
              value={wizardPath}
            />

            <button className="primary-button" disabled={isSubmittingWizard} type="submit">
              {isSubmittingWizard ? "Starting scan..." : "Start importing"}
            </button>
          </form>
        </section>
      </main>
    );
  }

  const fingerprintCores = Math.min(6, Math.max(2, navigator.hardwareConcurrency ?? 4));
  const contextMenuPath = contextMenu && (contextMenu.kind === "recording" || contextMenu.kind === "source")
    ? contextMenu.path
    : null;

  return (
    <main className="app-shell">
      <section className="topbar">
        {playerCoverArt ? (
          <img alt="Album art" className="topbar-art" src={playerCoverArt} />
        ) : (
          <div className="topbar-art topbar-art-placeholder" />
        )}

        <div className="transport-controls">
          <button
            className="transport-btn"
            disabled={history.length === 0 && playerState.position_ms < 3000}
            onClick={() => { void handleSkipBack(); }}
            title="Previous"
            type="button"
          >⏮</button>
          <button
            className="transport-btn transport-btn-play"
            onClick={handlePauseResume}
            title={isPlaying ? "Pause" : "Play"}
            type="button"
          >
            {playerState.status === "loading" ? "…" : isPlaying ? "⏸" : "▶"}
          </button>
          <button
            className="transport-btn"
            disabled={!currentTrack}
            onClick={() => { void handleSkip(); }}
            title="Skip"
            type="button"
          >⏭</button>
        </div>

        <div className="now-playing-block" ref={waveformContainerRef}>
          <canvas className="waveform-canvas" ref={waveformCanvasRef} />
          <div className="now-playing-meta">
            <div className="now-playing-text">
              <div className="now-playing-title">
                {currentTrack?.title ?? "Nothing playing"}
              </div>
              <div className="now-playing-sub">
                {currentTrack
                  ? [currentTrack.artist_credit_name, currentTrack.releases[0]?.release_group_title ?? null]
                      .filter(Boolean)
                      .join(" · ")
                  : "Queue a track from the library"}
              </div>
            </div>
            <div style={{ flex: 1 }} />
            {currentTrack ? (
              <span className="now-playing-rating">
                <RatingStars
                disabled={ratingKeyInFlight === `recording:${currentTrack.id}`}
                onRate={handleRate}
                recordingId={currentTrack.id}
                value={currentTrack.rating !== null ? currentTrack.rating : currentTrack.predicted_rating}
                isPredicted={currentTrack.rating === null}
              />
              {lastfmStatus?.logged_in ? (
                <button
                  className={"love-btn" + (currentTrackLoved ? " love-btn-loved" : "")}
                  onClick={() => { void handleLoveTrack(); }}
                  title="Love on Last.fm"
                  type="button"
                >{currentTrackLoved ? "♥" : "♡"}</button>
              ) : null}
              </span>
            ) : null}
          </div>
          <div className="topbar-scrubber">
            <span>{formatDuration(playerState.position_ms)}</span>
            <div className="scrubber-waveform-container">
              <input
                className="slider-input"
                disabled={!currentTrack || activeDurationMs <= 0}
                max={activeDurationMs || 0}
                min={0}
                onChange={(event) => { void handleSeek(Number(event.currentTarget.value)); }}
                type="range"
                value={Math.min(playerState.position_ms, activeDurationMs || 0)}
              />
            </div>
            <span>{formatDuration(activeDurationMs)}</span>
          </div>
        </div>

        <div className="topbar-volume">
          <span>🔊</span>
          <input
            className="slider-input"
            max={1.5}
            min={0}
            onChange={(event) => { void handleVolumeChange(Number(event.currentTarget.value)); }}
            step={0.01}
            type="range"
            value={playerState.volume}
          />
          <span>{Math.round(playerState.volume * 100)}%</span>
        </div>

        <div className="topbar-norm" title={playerState.normalization_source ? `Normalization: ${formatNormGain(playerState.normalization_gain)} from ${playerState.normalization_source}` : ""}>
          <label className="toggle-switch">
            <input
              type="checkbox"
              checked={playerState.normalization_enabled}
              onChange={() => { void handleNormalizationToggle(); }}
            />
            <span className="toggle-slider"></span>
          </label>
          {playerState.normalization_enabled ? (
            <span className="norm-info">
              {formatNormGain(playerState.normalization_gain)}
              {playerState.normalization_source ? (
                <span className="norm-source">{playerState.normalization_source}</span>
              ) : null}
            </span>
          ) : null}
        </div>

        <button
          className="settings-btn"
          onClick={() => setIsModalOpen(true)}
          title="Options"
          type="button"
        >⋯</button>
      </section>

      {error ? <section className="error-banner">{error}</section> : null}


      <section className="layout-grid">
        <section className="table-panel">
          {browserLeftTab !== "smartplaylists" ? (
            <div className="table-toolbar browser-toolbar">
              <input
                className="search-input"
                onChange={(event) => setSearch(event.currentTarget.value)}
                placeholder="Filter artists, albums, tracks, genre"
                value={search}
              />
              <span className="panel-meta">
                {filteredRecordings.length} tracks · {releaseGroups.length} albums
              </span>
            </div>
          ) : null}

          <div className="browser-grid">
            <section className="browser-left-panel">
              <div className="tab-bar">
                <button
                  className={`tab-btn ${browserLeftTab === "artists" ? "tab-btn-active" : ""}`}
                  onClick={() => setBrowserLeftTab("artists")}
                  type="button"
                >Artists</button>
                <button
                  className={`tab-btn ${browserLeftTab === "albums" ? "tab-btn-active" : ""}`}
                  onClick={() => setBrowserLeftTab("albums")}
                  type="button"
                >Albums</button>
                <button
                  className={`tab-btn ${browserLeftTab === "smartplaylists" ? "tab-btn-active" : ""}`}
                  onClick={() => setBrowserLeftTab("smartplaylists")}
                  type="button"
                >Smart Playlists</button>
              </div>

              {browserLeftTab === "artists" ? (
                <div className="browser-left-content">
                  <div className="browser-column-header">
                    <div>
                      <p className="panel-label">Artists</p>
                      <strong>{visibleArtists.length}</strong>
                    </div>
                    <button
                      className={`filter-chip ${selectedArtistId ? "" : "filter-chip-active"}`}
                      onClick={() => setSelectedArtistId(null)}
                      type="button"
                    >
                      All
                    </button>
                  </div>
                  <div className="browser-list">
                    {visibleArtists.length === 0 ? (
                      <p className="empty-browser-state">No artists match this filter.</p>
                    ) : (
                      visibleArtists.map((artist) => (
                        <button
                          className={`browser-item ${artist.id === selectedArtistId ? "browser-item-active" : ""}`}
                          key={artist.id}
                          onClick={() =>
                            setSelectedArtistId((current) => (current === artist.id ? null : artist.id))
                          }
                          onContextMenu={(e) => {
                            e.preventDefault();
                            setContextMenu({ kind: "artist", x: e.clientX, y: e.clientY, artist_id: artist.id, artist_name: artist.name });
                          }}
                          type="button"
                        >
                          <strong>{artist.name}</strong>
                          <span>
                            {artist.release_group_count} albums · {artist.recording_count} tracks
                          </span>
                          <span className="rating-summary">{formatAlbumRating(artist.rating)}</span>
                        </button>
                      ))
                    )}
                  </div>
                </div>
              ) : browserLeftTab === "albums" ? (
                <div className="browser-left-content">
                  <div className="browser-column-header">
                    <div>
                      <p className="panel-label">Albums</p>
                      <strong>{selectedArtist?.name ?? "All artists"}</strong>
                    </div>
                    <button
                      className={`filter-chip ${selectedReleaseGroupId ? "" : "filter-chip-active"}`}
                      onClick={() => setSelectedReleaseGroupId(null)}
                      type="button"
                    >
                      All
                    </button>
                  </div>
                  <div className="browser-list">
                    {visibleReleaseGroups.length === 0 ? (
                      <p className="empty-browser-state">No albums available for this view.</p>
                    ) : (
                      visibleReleaseGroups.map((releaseGroup) => (
                        <div
                          className={`browser-item ${releaseGroup.id === selectedReleaseGroupId ? "browser-item-active" : ""}`}
                          key={releaseGroup.id}
                          onClick={() =>
                            setSelectedReleaseGroupId((current) =>
                              current === releaseGroup.id ? null : releaseGroup.id,
                            )
                          }
                          onContextMenu={(e) => {
                            e.preventDefault();
                            setContextMenu({ kind: "release_group", x: e.clientX, y: e.clientY, release_group_id: releaseGroup.id, title: releaseGroup.title });
                          }}
                          onKeyDown={(event) => {
                            if (event.key === "Enter" || event.key === " ") {
                              event.preventDefault();
                              setSelectedReleaseGroupId((current) =>
                                current === releaseGroup.id ? null : releaseGroup.id,
                              );
                            }
                          }}
                          role="button"
                          tabIndex={0}
                        >
                          <strong>{releaseGroup.title}</strong>
                          <span>
                            {releaseGroup.artist_credit_name ?? "Unknown Artist"} ·{" "}
                            {formatYear(releaseGroup.release_date)}
                          </span>
                          <span className="rating-summary">{formatAlbumRating(releaseGroup.rating)}</span>
                        </div>
                      ))
                    )}
                  </div>
                </div>
              ) : (
                <div className="browser-left-content smart-pl-left">
                  <div className="smart-pl-saved-section">
                    <p className="panel-label">Saved playlists</p>
                    {playlists.filter((p) => p.kind === "smart").length === 0 ? (
                      <p className="empty-browser-state">No saved playlists yet.</p>
                    ) : (
                      <ul className="saved-playlist-list">
                        {playlists
                          .filter((p) => p.kind === "smart")
                          .map((pl) => (
                            <li className="saved-playlist-item" key={pl.id}>
                              <button
                                className="saved-playlist-name"
                                onClick={() => {
                                  setSmartQuery(pl.query ?? "");
                                  setSavePlaylistName(pl.name);
                                }}
                                type="button"
                                title={pl.query ?? ""}
                              >{pl.name}</button>
                              <button
                                className="saved-playlist-delete"
                                onClick={() => { void deletePlaylist(pl.id); }}
                                title="Delete"
                                type="button"
                              >✕</button>
                            </li>
                          ))}
                      </ul>
                    )}
                  </div>

                  <div className="smart-pl-editor">
                    <div className="smart-pl-query-area">
                      <textarea
                        className="smart-pl-textarea"
                        onChange={(e) => setSmartQuery(e.currentTarget.value)}
                        onKeyDown={(e) => {
                          if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
                            e.preventDefault();
                            void runSmartQuery();
                          }
                        }}
                        placeholder={
                          "Rating >= 4 AND LastPlayed NotInLast 8 Months\n" +
                          "HasTag chill AND PlayCount >= 3\n" +
                          "Artist contains \"Radiohead\" LIMIT 30 minutes"
                        }
                        rows={4}
                        spellCheck={false}
                        value={smartQuery}
                      />
                      <div className="smart-pl-actions">
                        <button
                          className="primary-button"
                          disabled={isRunningQuery || !smartQuery.trim()}
                          onClick={() => { void runSmartQuery(); }}
                          type="button"
                        >
                          {isRunningQuery ? "Running…" : "Run (Ctrl+Enter)"}
                        </button>
                        <div className="smart-pl-save-row">
                          <input
                            className="small-input"
                            onChange={(e) => setSavePlaylistName(e.currentTarget.value)}
                            placeholder="Playlist name"
                            value={savePlaylistName}
                          />
                          <button
                            className="secondary-button"
                            disabled={isSavingPlaylist || !savePlaylistName.trim() || !smartQuery.trim()}
                            onClick={() => { void saveSmartPlaylist(); }}
                            type="button"
                          >
                            {isSavingPlaylist ? "Saving…" : "Save"}
                          </button>
                        </div>
                      </div>
                    </div>

                    {smartError ? (
                      <div className="error-banner smart-pl-error">{smartError}</div>
                    ) : null}
                  </div>
                </div>
              )}
            </section>

            <section className="track-column">
              {detailView ? (
                <EntityDetailView
                  nav={detailView}
                  canGoBack={canGoBack}
                  canGoForward={canGoForward}
                  onNavigate={(nav) => dispatchNav({ type: "navigate", nav })}
                  onBack={() => dispatchNav({ type: "back" })}
                  onForward={() => dispatchNav({ type: "forward" })}
                  onClose={() => dispatchNav({ type: "close" })}
                  onSourceContextMenu={openEntitySourceContextMenu}
                  onEnqueueTrack={(track) => {
                    if (!track.has_source || !track.primary_source_id) {
                      setError(`No playable source available for "${track.recording_title}".`);
                      return;
                    }
                    setError(null);
                    const item: RecordingRow = {
                      id: track.recording_id,
                      title: track.recording_title,
                      duration_ms: track.duration_ms,
                      primary_artist_id: track.primary_artist_id,
                      artist_credit_name: track.artist_credit_name,
                      primary_source_id: track.primary_source_id,
                      primary_source_path: null,
                      genre: null,
                      rating: null,
                      predicted_rating: null,
                      play_count: 0,
                      last_played: null,
                      tags: [],
                      artist_ids: [],
                      source_paths: [],
                      releases: [],
                    };
                    if (!currentTrack && playerState.status === "stopped") {
                      setCurrentTrack(item);
                    } else {
                      setQueue((q) => [...q, item]);
                    }
                  }}
                  onRescanRecording={(recordingId) => { void handleRescanRecording(recordingId); }}
                  onRescanReleaseGroup={(releaseGroupId) => { void handleRescanReleaseGroup(releaseGroupId); }}
                  onRefreshLibrary={() => { void loadLibraryData(selectedArtistId, search); }}
                  onSplitRecording={(recordingId) => { setSplitRecordingId(recordingId); }}
                />
              ) : browserLeftTab === "smartplaylists" ? (
                <div className="smart-pl-results-panel">
                  {smartResult ? (
                    <>
                      <div className="browser-column-header">
                        <span className="panel-meta">
                          {smartResult.recordings.length} tracks · {formatDuration(smartResult.total_duration_ms)}
                        </span>
                        <button
                          className="secondary-button"
                          disabled={smartResult.recordings.length === 0}
                          onClick={() => enqueueAll(smartResult.recordings)}
                          type="button"
                        >Queue all</button>
                      </div>
                      <div className="table-wrap">
                        <table className="recordings-table">
                          <thead>
                            <tr>
                              <th>Title</th>
                              <th>Artist</th>
                              <th>Releases</th>
                              <th>Rating</th>
                              <th>Duration</th>
                              <th>Plays</th>
                              <th>Last played</th>
                            </tr>
                          </thead>
                          <tbody>
                            {smartResult.recordings.map((recording) => (
                              <tr
                                className={recording.primary_source_id ? "playable-row" : "muted-row"}
                                key={recording.id}
                                onDoubleClick={() => enqueueRecording(recording)}
                                title={recording.primary_source_id ? "Double click to queue" : "No playable file"}
                              >
                                <td>{recording.title}</td>
                                <td>{recording.artist_credit_name ?? "Unknown Artist"}</td>
                                <td>
                                  {recording.releases.length === 0
                                    ? "—"
                                    : recording.releases.map((rel, i) => (
                                        <div key={i}>{rel.release_group_title}</div>
                                      ))
                                  }
                                </td>
                                <td>
                                  <RatingStars
                                    disabled={ratingKeyInFlight === `recording:${recording.id}`}
                                    onRate={handleRate}
                                    recordingId={recording.id}
                                    value={recording.rating !== null ? recording.rating : recording.predicted_rating}
                                    isPredicted={recording.rating === null}
                                  />
                                </td>
                                <td>{formatDuration(recording.duration_ms)}</td>
                                <td>{recording.play_count}</td>
                                <td>{formatLastPlayed(recording.last_played)}</td>
                              </tr>
                            ))}
                            {smartResult.recordings.length === 0 ? (
                              <tr>
                                <td className="empty-table-state" colSpan={7}>
                                  No tracks match this query.
                                </td>
                              </tr>
                            ) : null}
                          </tbody>
                        </table>
                      </div>
                    </>
                  ) : (
                    <p className="empty-browser-state">Run a query to see results here.</p>
                  )}
                </div>
              ) : (
                <>
                  <div className="browser-column-header">
                    <div>
                      <p className="panel-label">Tracks</p>
                      <strong>{selectedReleaseGroup?.title ?? "Library view"}</strong>
                    </div>
                    <span className="panel-meta">
                      {selectedArtist?.name ?? "All artists"}
                    </span>
                  </div>
                  {selectedTag ? (
                    <div className="active-tag-filter">
                      Filtered by tag: <strong>{selectedTag}</strong>
                      <button onClick={() => setSelectedTag(null)} type="button" className="modal-close-btn">×</button>
                    </div>
                  ) : null}
                  <div className="table-wrap track-table-wrap" ref={trackTableScrollRef}>
                    <table className="recordings-table">
                      <colgroup>
                        {COLUMN_KEYS.map((key) => (
                          <col key={key} style={{ width: colWidths[key] }} />
                        ))}
                      </colgroup>
                      <thead>
                        <tr>
                          {(["title", "artist", "releases", "genre"] as SortColumn[]).map((col) => (
                            <th
                              key={col}
                              className="sortable-th"
                              onClick={() => handleColumnSort(col)}
                            >
                              {col.charAt(0).toUpperCase() + col.slice(1)}
                              {sortColumn === col ? (sortAsc ? " ↑" : " ↓") : ""}
                              <div className="column-resize-handle" onMouseDown={(e) => onColumnResizeStart(e, col)} />
                            </th>
                          ))}
                          {(["rating", "duration"] as SortColumn[]).map((col) => (
                            <th
                              key={col}
                              className="sortable-th"
                              onClick={() => handleColumnSort(col)}
                            >
                              {col.charAt(0).toUpperCase() + col.slice(1)}
                              {sortColumn === col ? (sortAsc ? " ↑" : " ↓") : ""}
                              <div className="column-resize-handle" onMouseDown={(e) => onColumnResizeStart(e, col)} />
                            </th>
                          ))}
                          <th>
                            Tags
                            <div className="column-resize-handle" onMouseDown={(e) => onColumnResizeStart(e, "tags")} />
                          </th>
                          {(["plays", "last_played"] as SortColumn[]).map((col) => (
                            <th
                              key={col}
                              className="sortable-th"
                              onClick={() => handleColumnSort(col)}
                            >
                              {col === "last_played" ? "Last played" : col.charAt(0).toUpperCase() + col.slice(1)}
                              {sortColumn === col ? (sortAsc ? " ↑" : " ↓") : ""}
                              <div className="column-resize-handle" onMouseDown={(e) => onColumnResizeStart(e, col)} />
                            </th>
                          ))}
                          <th>
                            Sources
                            <div className="column-resize-handle" onMouseDown={(e) => onColumnResizeStart(e, "sources")} />
                          </th>
                        </tr>
                      </thead>
                      <tbody>
                        {filteredRecordings.length === 0 ? (
                          <tr>
                            <td className="empty-table-state" colSpan={10}>
                              No tracks match the current artist, album, and search filters.
                            </td>
                          </tr>
                        ) : (
                          <>
                            {rowVirtualizer.getVirtualItems()[0]?.start > 0 && (
                              <tr style={{ height: rowVirtualizer.getVirtualItems()[0].start }}>
                                <td colSpan={10} />
                              </tr>
                            )}
                            {rowVirtualizer.getVirtualItems().map((virtualRow) => {
                              const recording = filteredRecordings[virtualRow.index];
                              return (
                                <tr
                                  className={recording.primary_source_id ? "playable-row" : "muted-row"}
                                  key={recording.id}
                                  data-index={virtualRow.index}
                                  ref={rowVirtualizer.measureElement}
                                  onDoubleClick={() => enqueueRecording(recording)}
                                  onContextMenu={(e) => openRecordingContextMenu(e, recording)}
                                  title={
                                    recording.primary_source_id
                                      ? "Double click to play or queue"
                                      : "No playable local file"
                                  }
                                >
                                  <td>{recording.title}</td>
                                  <td>{recording.artist_credit_name ?? "Unknown Artist"}</td>
                                  <td className="releases-cell">
                                    {recording.releases.length === 0
                                      ? "—"
                                      : recording.releases.map((rel, i) => {
                                          const pos = rel.disc_total && rel.disc_total > 1 && rel.disc_position
                                            ? `${rel.disc_position}.${rel.track_position ?? "—"}`
                                            : rel.track_position != null
                                              ? String(rel.track_position)
                                              : null;
                                          return (
                                            <div key={i} className="release-entry">
                                              {rel.release_group_title}{pos !== null ? ` (#${pos})` : ""}
                                            </div>
                                          );
                                        })
                                    }
                                  </td>
                                  <td>{recording.genre ?? "—"}</td>
                                  <td>
                                    <RatingStars
                                      disabled={ratingKeyInFlight === `recording:${recording.id}`}
                                      onRate={handleRate}
                                      recordingId={recording.id}
                                      value={recording.rating !== null ? recording.rating : recording.predicted_rating}
                                      isPredicted={recording.rating === null}
                                    />
                                  </td>
                                  <td>{formatDuration(recording.duration_ms)}</td>
                                  <td
                                    onClick={(e) => e.stopPropagation()}
                                    onDoubleClick={(e) => e.stopPropagation()}
                                  >
                                    <div className="tag-chips">
                                      {recording.tags.map((tag) => (
                                        <span
                                          className="tag-chip"
                                          key={tag}
                                          onClick={(e) => {
                                            e.stopPropagation();
                                            setSelectedTag(tag);
                                          }}
                                          title={`Filter by tag "${tag}"`}
                                        >
                                          {tag}
                                        </span>
                                      ))}
                                    </div>
                                  </td>
                                  <td>{recording.play_count}</td>
                                  <td>{formatLastPlayed(recording.last_played)}</td>
                                  <td className="source-paths-cell">
                                    {recording.source_paths.map((p) => (
                                      <div
                                        key={p}
                                        title={p}
                                        className="source-path"
                                        onContextMenu={(e) => {
                                          e.preventDefault();
                                          e.stopPropagation();
                                          setContextMenu({ kind: "source", x: e.clientX, y: e.clientY, path: p });
                                        }}
                                      >
                                        {p}
                                      </div>
                                    ))}
                                  </td>
                                </tr>
                              );
                            })}
                            {(() => {
                              const items = rowVirtualizer.getVirtualItems();
                              const last = items[items.length - 1];
                              const remaining = last
                                ? rowVirtualizer.getTotalSize() - last.end
                                : 0;
                              return remaining > 0 ? (
                                <tr style={{ height: remaining }}>
                                  <td colSpan={10} />
                                </tr>
                              ) : null;
                            })()}
                          </>
                        )}
                      </tbody>
                    </table>
                  </div>
                </>
              )}
            </section>
          </div>
        </section>

        <aside className="queue-panel">
          <section className="queue-list-panel">
            <div className="queue-header">
              <div>
                <p className="panel-label">
                  Timeline |
                  {history.length > 0 || totalCount > 0
                    ? `${history.length} Played - ${totalCount} Queued` +
                      (totalRemainingMs > 0
                        ? ` (${formatDurationCompact(totalRemainingMs)} ETA ${formatEtaTime(new Date(Date.now() + totalRemainingMs))})`
                        : "")
                    : "Nothing playing"}
                </p>
              </div>
              <div className="queue-header-actions">
                {POMODORO_PRESETS.map((preset) => {
                  const label = formatPomodoroDuration(preset.durationMs);
                  return (
                    <button
                      key={preset.durationMs}
                      className="pomodoro-btn"
                      onClick={() => handlePomodoroFill(preset.durationMs)}
                      onContextMenu={openPomodoroPrompt}
                      title={`Queue ~${label} of music. Right-click for a custom duration.`}
                      type="button"
                    >
                      {label} 🍅
                    </button>
                  );
                })}
                <button
                  className={`auto-dj-btn ${autoDj ? "auto-dj-btn-on" : ""}`}
                  onClick={() => setAutoDj((v) => !v)}
                  title={autoDj ? "Auto DJ on — click to disable" : "Auto DJ off — click to enable"}
                  type="button"
                >
                  Auto DJ
                </button>
              </div>
            </div>
            <ol
              className="queue-list"
              ref={queueListRef}
              onDragOver={(e) => {
                e.preventDefault();
                e.dataTransfer.dropEffect = "move";
              }}
              onDrop={() => {
                const from = resolveDraggedIndex();
                if (from === null) return;
                // Drop landed on a grid gap — insert after the last-hovered item,
                // or at the end if nothing was hovered yet. Resolve the hovered
                // item's current index at drop time too, since the queue can shift
                // mid-drag. moveQueueItem inserts into the queue with `from` already
                // removed, so "after the hovered item" is `over` (not +1) when
                // dragging down from above it.
                const over =
                  dragOverId !== null ? queue.findIndex((item) => item.id === dragOverId) : -1;
                const targetIndex =
                  over !== -1
                    ? from < over
                      ? over
                      : over + 1
                    : queue.length - 1;
                if (from !== targetIndex) {
                  moveQueueItem(from, targetIndex);
                }
              }}
            >
              {history.length === 0 && !currentTrack && queue.length === 0 ? (
                <li className="empty-item">Double-click a track to start playing.</li>
              ) : (
                <>
                  {[...history].reverse().map((item, i) => (
                    <li
                      className="queue-history-item"
                      key={`history-${i}-${item.id}`}
                      onContextMenu={(e) => openRecordingContextMenu(e, item)}
                    >
                      <div className="queue-row">
                        <strong>{item.title}</strong>
                        <span className="queue-duration">{formatDuration(item.duration_ms)}</span>
                      </div>
                      <div className="queue-row">
                        <span>{item.artist_credit_name ?? "Unknown Artist"}</span>
                        <RatingStars
                          disabled={ratingKeyInFlight === `recording:${item.id}`}
                          onRate={handleRate}
                          recordingId={item.id}
                          value={item.rating !== null ? item.rating : item.predicted_rating}
                          isPredicted={item.rating === null}
                        />
                      </div>
                    </li>
                  ))}
                  {currentTrack ? (
                    <li className="queue-now-playing" onContextMenu={(e) => openRecordingContextMenu(e, currentTrack)}>
                      <div className="queue-row">
                        <strong>{currentTrack.title}</strong>
                        <span className="queue-duration">{formatDuration(currentTrack.duration_ms)}</span>
                      </div>
                      <div className="queue-row">
                        <span>{currentTrack.artist_credit_name ?? "Unknown Artist"}</span>
                        <RatingStars
                          disabled={ratingKeyInFlight === `recording:${currentTrack.id}`}
                          onRate={handleRate}
                          recordingId={currentTrack.id}
                          value={currentTrack.rating !== null ? currentTrack.rating : currentTrack.predicted_rating}
                          isPredicted={currentTrack.rating === null}
                        />
                      </div>
                    </li>
                  ) : null}
                  {queue.map((item, index) => (
                    <li
                      className={`queue-upcoming-item${dragOverId === item.id ? " drag-over" : ""}`}
                      key={`${index}-${item.id}-${item.primary_source_id}`}
                      draggable
                      onDragStart={(e) => {
                        dragRef.current = item.id;
                        queueListRef.current?.classList.add("is-dragging");
                        // WebKitGTK requires setData for the drag to be a valid drop source.
                        e.dataTransfer.effectAllowed = "move";
                        e.dataTransfer.setData("text/plain", item.id);
                      }}
                      onDragEnter={(e) => e.preventDefault()}
                      onDragOver={(e) => {
                        e.preventDefault();
                        e.dataTransfer.dropEffect = "move";
                        if (dragOverId !== item.id) setDragOverId(item.id);
                      }}
                      onDragEnd={() => {
                        dragRef.current = null;
                        setDragOverId(null);
                        queueListRef.current?.classList.remove("is-dragging");
                      }}
                      onDrop={(e) => {
                        e.preventDefault();
                        e.stopPropagation();
                        const from = resolveDraggedIndex();
                        if (from !== null && from !== index) {
                          moveQueueItem(from, index);
                        }
                      }}
                      onContextMenu={(e) => openRecordingContextMenu(e, item)}
                    >
                      <div className="queue-row">
                        <strong>{item.title}</strong>
                        <span className="queue-duration">{formatDuration(item.duration_ms)}</span>
                        <button
                          className="queue-remove-btn"
                          onClick={() => removeFromQueue(index)}
                          title="Remove from queue"
                          type="button"
                          aria-label="Remove from queue"
                        >
                          ×
                        </button>
                      </div>
                      <div className="queue-row">
                        <span>{item.artist_credit_name ?? "Unknown Artist"}</span>
                        <RatingStars
                          disabled={ratingKeyInFlight === `recording:${item.id}`}
                          onRate={handleRate}
                          recordingId={item.id}
                          value={item.rating !== null ? item.rating : item.predicted_rating}
                          isPredicted={item.rating === null}
                        />
                      </div>
                    </li>
                  ))}
                </>
              )}
            </ol>
          </section>
        </aside>
      </section>

      {isModalOpen ? (
        <div
          className="modal-overlay"
          onClick={() => setIsModalOpen(false)}
          role="dialog"
          aria-modal="true"
        >
          <div className="modal-card" onClick={(e) => e.stopPropagation()}>
            <div className="modal-header">
              <div className="modal-tabs">
                <button
                  className={`modal-tab${settingsTab === "options" ? " modal-tab-active" : ""}`}
                  onClick={() => setSettingsTab("options")}
                  type="button"
                >
                  Options
                </button>
                <button
                  className={`modal-tab${settingsTab === "issues" ? " modal-tab-active" : ""}`}
                  onClick={() => setSettingsTab("issues")}
                  type="button"
                >
                  File Issues
                  {fileIssues.length > 0 && (
                    <span className="modal-tab-badge">{fileIssues.length}</span>
                  )}
                </button>
              </div>
              <button
                className="modal-close-btn"
                onClick={() => setIsModalOpen(false)}
                type="button"
              >✕</button>
            </div>

            {settingsTab === "options" ? (
              <>
                <div className="modal-section">
                  <p className="modal-section-label">Library</p>
                  <div className="settings-row">
                    <div>
                      <label className="input-label" htmlFor="modal-music-root">
                        Music root
                      </label>
                      <input
                        id="modal-music-root"
                        className="small-input music-root-input"
                        type="text"
                        onChange={(event) => setMusicRootInput(event.currentTarget.value)}
                        value={musicRootInput}
                        placeholder="/home/user/Music"
                      />
                    </div>
                    <button
                      className="secondary-button"
                      disabled={isSavingMusicRoot}
                      onClick={() => { void saveMusicRoot(); }}
                      type="button"
                    >
                      {isSavingMusicRoot ? "Saving…" : "Save"}
                    </button>
                  </div>
                  <div className="modal-actions">
                    <button
                      className="secondary-button"
                      onClick={handleRescan}
                      type="button"
                    >
                      Scan for new files
                    </button>
                    <button
                      className="secondary-button"
                      onClick={handleRescanAll}
                      type="button"
                    >
                      Rescan all files
                    </button>
                  </div>
                </div>

                <div className="modal-section">
                  <p className="modal-section-label">Queue</p>
                  <div className="settings-row">
                    <div>
                      <label className="input-label" htmlFor="modal-queue-history-limit">
                        History limit
                      </label>
                      <input
                        id="modal-queue-history-limit"
                        className="small-input"
                        inputMode="numeric"
                        onChange={(event) => setQueueHistoryLimitInput(event.currentTarget.value)}
                        value={queueHistoryLimitInput}
                      />
                    </div>
                    <button
                      className="secondary-button"
                      disabled={isSavingQueueSettings}
                      onClick={() => { void saveQueueSettings(); }}
                      type="button"
                    >
                      {isSavingQueueSettings ? "Saving…" : "Save"}
                    </button>
                  </div>
                </div>

                <div className="modal-section">
                  <p className="modal-section-label">Last.fm</p>
                  {!lastfmStatus?.configured ? (
                    <>
                      <p className="lastfm-hint">Enter your Last.fm API credentials (get them at <span className="lastfm-link" onClick={() => { void openUrl("https://www.last.fm/api"); }} role="link" tabIndex={0}>last.fm/api</span>).</p>
                      <div className="settings-row">
                        <div>
                          <label className="input-label" htmlFor="lastfm-api-key">API Key</label>
                          <input
                            id="lastfm-api-key"
                            className="small-input"
                            type="text"
                            value={lastfmApiKeyInput}
                            onChange={(e) => setLastfmApiKeyInput(e.currentTarget.value)}
                            placeholder="your_api_key"
                          />
                        </div>
                      </div>
                      <div className="settings-row">
                        <div>
                          <label className="input-label" htmlFor="lastfm-shared-secret">Shared Secret</label>
                          <input
                            id="lastfm-shared-secret"
                            className="small-input"
                            type="text"
                            value={lastfmSharedSecretInput}
                            onChange={(e) => setLastfmSharedSecretInput(e.currentTarget.value)}
                            placeholder="your_shared_secret"
                          />
                        </div>
                      </div>
                      <div className="settings-row">
                        <button
                          className="secondary-button"
                          disabled={isSavingLastfmCredentials || !lastfmApiKeyInput.trim() || !lastfmSharedSecretInput.trim()}
                          onClick={() => { void handleSaveLastfmCredentials(); }}
                          type="button"
                        >
                          {isSavingLastfmCredentials ? "Saving…" : "Save Credentials"}
                        </button>
                      </div>
                    </>
                  ) : lastfmStatus.logged_in ? (
                    <div className="settings-row">
                      <span className="lastfm-connected">Connected as <strong>{lastfmStatus.username}</strong></span>
                      <button className="secondary-button" onClick={() => { void handleLastfmDisconnect(); }} type="button">Disconnect</button>
                    </div>
                  ) : (
                    <div className="settings-row">
                      <button className="secondary-button" disabled={isLastfmConnecting} onClick={() => { void handleLastfmConnect(); }} type="button">
                        {isLastfmConnecting ? "Connecting…" : "Connect to Last.fm"}
                      </button>
                      {lastfmAuthUrl ? (
                        <button className="secondary-button" disabled={isLastfmCompleting} onClick={() => { void handleLastfmCompleteAuth(); }} type="button">
                          {isLastfmCompleting ? "Completing…" : "Complete Login"}
                        </button>
                      ) : null}
                    </div>
                  )}
                </div>

                <div className="modal-section">
                  <p className="modal-section-label">External Commands</p>
                  <p className="ext-cmd-hint">Use <code>%%</code> as a placeholder for the file path.</p>
                  {externalCommands.map((cmd, i) => (
                    <div key={i} className="settings-row ext-cmd-row">
                      <span className="ext-cmd-name">{cmd.name}</span>
                      <span className="ext-cmd-template">{cmd.template}</span>
                      <button
                        className="secondary-button"
                        type="button"
                        onClick={() => {
                          const updated = externalCommands.filter((_, j) => j !== i);
                          void saveExternalCommands(updated);
                        }}
                      >
                        Remove
                      </button>
                    </div>
                  ))}
                  <div className="settings-row ext-cmd-row">
                    <input
                      className="small-input ext-cmd-name-input"
                      type="text"
                      placeholder="Name"
                      value={newCmdName}
                      onChange={(e) => setNewCmdName(e.currentTarget.value)}
                    />
                    <input
                      className="small-input ext-cmd-template-input"
                      type="text"
                      placeholder="Command (e.g. picard %%)"
                      value={newCmdTemplate}
                      onChange={(e) => setNewCmdTemplate(e.currentTarget.value)}
                    />
                    <button
                      className="secondary-button"
                      type="button"
                      disabled={!newCmdName.trim() || !newCmdTemplate.trim()}
                      onClick={() => {
                        const updated = [...externalCommands, { name: newCmdName.trim(), template: newCmdTemplate.trim() }];
                        void saveExternalCommands(updated);
                        setNewCmdName("");
                        setNewCmdTemplate("");
                      }}
                    >
                      Add
                    </button>
                  </div>
                </div>
              </>
            ) : (
              <div className="modal-section">
                {fileIssues.length === 0 ? (
                  <p className="issue-empty">No file issues recorded this session.</p>
                ) : (
                  <ol className="issue-list">
                    {fileIssues.map((issue, i) => (
                      <li
                        key={i}
                        className="issue-item"
                        onContextMenu={(e) => openEntitySourceContextMenu(e, issue.file_path)}
                      >
                        <div className="issue-item-header">
                          <span className={`issue-kind issue-kind-${issue.kind}`}>
                            {issue.kind === "import_error" ? "Import" : issue.kind === "orphan_source" ? "Orphan Source" : issue.kind === "duplicate_frame" ? "Duplicate Tag" : issue.kind === "backup_file_exists" ? "Backup" : "Playback"}
                          </span>
                          {issue.kind === "orphan_source" ? (
                            <button
                              className="fix-orphan-btn"
                              type="button"
                              disabled={fixingOrphans.has(issue.source_id!)}
                              onClick={() => {
                                const sid = issue.source_id!;
                                setFixingOrphans(prev => new Set(prev).add(sid));
                                void (async () => {
                                  await invoke("fix_orphan_source", { sourceId: sid });
                                  setFileIssues(prev => prev.filter(fi => fi.source_id !== sid));
                                  setFixingOrphans(prev => { const n = new Set(prev); n.delete(sid); return n; });
                                })();
                              }}
                            >
                              {fixingOrphans.has(issue.source_id!) ? "Fixing…" : "Fix"}
                            </button>
                          ) : issue.kind === "duplicate_frame" ? (
                            <div className="duplicate-frame-choices">
                              <button
                                className="use-value-btn"
                                type="button"
                                disabled={resolvingDuplicates.has(issue.file_path + issue.frame_id)}
                                onClick={() => {
                                  const key = issue.file_path + issue.frame_id;
                                  setResolvingDuplicates(prev => new Set(prev).add(key));
                                  void (async () => {
                                    await invoke("resolve_duplicate_frame", {
                                      filePath: issue.file_path,
                                      frameId: issue.frame_id,
                                      chosenValue: issue.corrected_value,
                                    });
                                    setFileIssues(prev => prev.filter(
                                      fi => !(fi.kind === "duplicate_frame" && fi.file_path === issue.file_path && fi.frame_id === issue.frame_id)
                                    ));
                                    setResolvingDuplicates(prev => { const n = new Set(prev); n.delete(key); return n; });
                                  })();
                                }}
                              >
                                {resolvingDuplicates.has(issue.file_path + issue.frame_id) ? "Applying…" : `Use: ${issue.corrected_value?.substring(0, 60)}`}
                              </button>
                              <button
                                className="use-value-btn use-alt-value"
                                type="button"
                                disabled={resolvingDuplicates.has(issue.file_path + issue.frame_id)}
                                onClick={() => {
                                  const key = issue.file_path + issue.frame_id;
                                  setResolvingDuplicates(prev => new Set(prev).add(key));
                                  void (async () => {
                                    await invoke("resolve_duplicate_frame", {
                                      filePath: issue.file_path,
                                      frameId: issue.frame_id,
                                      chosenValue: issue.lofty_value,
                                    });
                                    setFileIssues(prev => prev.filter(
                                      fi => !(fi.kind === "duplicate_frame" && fi.file_path === issue.file_path && fi.frame_id === issue.frame_id)
                                    ));
                                    setResolvingDuplicates(prev => { const n = new Set(prev); n.delete(key); return n; });
                                  })();
                                }}
                              >
                                {resolvingDuplicates.has(issue.file_path + issue.frame_id) ? "Applying…" : `Use: ${issue.lofty_value?.substring(0, 60)}`}
                              </button>
                            </div>
                          ) : issue.kind === "backup_file_exists" ? (
                            <button
                              className="fix-orphan-btn"
                              type="button"
                              disabled={deletingBackups.has(issue.backup_path ?? "")}
                              onClick={() => {
                                const bp = issue.backup_path!;
                                setDeletingBackups(prev => new Set(prev).add(bp));
                                void (async () => {
                                  await invoke("delete_backup_file", { backupPath: bp });
                                  setFileIssues(prev => prev.filter(fi => fi.backup_path !== bp));
                                  setDeletingBackups(prev => { const n = new Set(prev); n.delete(bp); return n; });
                                })();
                              }}
                            >
                              {deletingBackups.has(issue.backup_path ?? "") ? "Deleting…" : "Delete backup"}
                            </button>
                          ) : null}
                        </div>
                        <span className="issue-path" title={issue.file_path}>
                          {issue.file_path}
                        </span>
                        <span className="issue-message">{issue.message}</span>
                      </li>
                    ))}
                  </ol>
                )}
              </div>
            )}
          </div>
        </div>
      ) : null}


      {splitRecordingId && splitData && (
        <div className="modal-overlay" onClick={() => setSplitRecordingId(null)}>
          <div className="modal-card" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 560 }}>
            <div className="modal-header">
              <h2>Split Recording</h2>
              <button className="modal-close-btn" onClick={() => setSplitRecordingId(null)} type="button">×</button>
            </div>
            <div className="modal-section">
              <p className="subtle-text">
                Splitting: <strong>{splitData.title}</strong>
              </p>
              <p className="subtle-text">Select sources to move to a new recording.</p>

              {(() => {
                const abbrevPaths = abbreviatePaths(splitData.sources);
                return (
                  <div className="split-source-list">
                    {splitData.sources.map((source) => (
                      <label
                        key={source.id}
                        className={`split-source-item${splitSelectedSourceIds.has(source.id) ? " split-source-item-selected" : ""}`}
                      >
                        <input
                          type="checkbox"
                          checked={splitSelectedSourceIds.has(source.id)}
                          onChange={() => {
                            setSplitSelectedSourceIds((prev) => {
                              const next = new Set(prev);
                              next.has(source.id) ? next.delete(source.id) : next.add(source.id);
                              return next;
                            });
                          }}
                        />
                        <span className="entity-detail-badge">{source.source_type}</span>
                        <span className="split-source-path" title={source.file_path ?? undefined}>{abbrevPaths.get(source.id) ?? source.file_path ?? "(no file path)"}</span>
                        {source.duration_ms != null && (
                          <span className="subtle-text">{formatDuration(source.duration_ms)}</span>
                        )}
                      </label>
                    ))}
                  </div>
                );
              })()}

              {/* Metadata preview */}
              <div className="split-preview-section">
                <h3 className="entity-detail-section-title">Metadata preview</h3>
                {(() => {
                  const selectedSources = splitData.sources.filter((s) => splitSelectedSourceIds.has(s.id));
                  const derivedTitle = firstTag(selectedSources, "TIT2");
                  const derivedArtists = allUniqueTags(selectedSources, "TPE1");
                  const derivedTags = allUniqueTags(selectedSources, "TCON");

                  const releaseStrs: string[] = [];
                  const releaseKeys = new Set<string>();
                  for (const src of selectedSources) {
                    const album = firstTag([src], "TALB");
                    if (!album) continue;
                    const discNum = src.tags.find((t) => t.frame_id === "TPOS" && t.field_name === "disc_number")?.value;
                    const discTot = src.tags.find((t) => t.frame_id === "TPOS" && t.field_name === "disc_total")?.value;
                    const trackNum = src.tags.find((t) => t.frame_id === "TRCK" && t.field_name === "track_number")?.value;
                    const trackTot = src.tags.find((t) => t.frame_id === "TRCK" && t.field_name === "track_total")?.value;
                    let r = album;
                    const parts: string[] = [];
                    if (discNum) parts.push(discTot ? `Disc ${discNum}/${discTot}` : `Disc ${discNum}`);
                    if (trackNum) parts.push(trackTot ? `Track ${trackNum}/${trackTot}` : `Track ${trackNum}`);
                    if (parts.length) r += ` (${parts.join(", ")})`;
                    if (!releaseKeys.has(r)) {
                      releaseKeys.add(r);
                      releaseStrs.push(r);
                    }
                  }

                  return (
                    <table className="entity-detail-meta-table">
                      <tbody>
                        <tr>
                          <td className="split-preview-label">Title</td>
                          <td className="split-preview-value">{derivedTitle || "—"}</td>
                        </tr>
                        <tr>
                          <td className="split-preview-label">Artist(s)</td>
                          <td className="split-preview-value">{derivedArtists.length ? derivedArtists.join(", ") : "—"}</td>
                        </tr>
                        <tr>
                          <td className="split-preview-label">Release(s)</td>
                          <td className="split-preview-value">{releaseStrs.length ? releaseStrs.join("; ") : "—"}</td>
                        </tr>
                        <tr>
                          <td className="split-preview-label">Tag(s)</td>
                          <td className="split-preview-value">{derivedTags.length ? derivedTags.join(", ") : "—"}</td>
                        </tr>
                      </tbody>
                    </table>
                  );
                })()}
              </div>

              <p className="split-summary">
                {splitData.sources.length - splitSelectedSourceIds.size} source(s) remain on original,
                {" "}{splitSelectedSourceIds.size} moved to new recording.
              </p>

              {splitError && <div className="error-banner">{splitError}</div>}
            </div>
            <div className="comparison-modal-footer">
              <button className="comparison-cancel-btn" onClick={() => setSplitRecordingId(null)} type="button">Cancel</button>
              <button
                className="comparison-confirm-btn"
                disabled={
                  splitIsSubmitting ||
                  splitSelectedSourceIds.size === 0 ||
                  splitSelectedSourceIds.size === splitData.sources.length
                }
                onClick={() => { void handleSplitConfirm(); }}
                type="button"
              >
                {splitIsSubmitting ? "Splitting..." : "Split Recording"}
              </button>
            </div>
          </div>
        </div>
      )}

      {artistFixModal && (() => {
        const { artistId, artistName } = artistFixModal;
        function closeModal() {
          setArtistFixModal(null);
          setArtistFixCheckResult(null);
          setArtistFixError(null);
        }
        async function runChecks() {
          setArtistFixChecking(true);
          setArtistFixCheckResult(null);
          setArtistFixError(null);
          try {
            const result = await invoke<CompoundArtistCheck>("check_artist_compound", { artistId });
            setArtistFixCheckResult(result);
          } catch (e) {
            setArtistFixError(String(e));
          } finally {
            setArtistFixChecking(false);
          }
        }
        return (
          <div
            className="modal-overlay"
            onClick={() => closeModal()}
            role="dialog"
            aria-modal="true"
          >
            <div className="modal-card" onClick={(e) => e.stopPropagation()}>
              <div className="modal-header">
                <h2>Fix Issues: {artistName}</h2>
                <button className="modal-close-btn" onClick={() => closeModal()} type="button">×</button>
              </div>
              <div className="modal-section">
                {/* Error */}
                {artistFixError && (
                  <p className="check-error">Error: {artistFixError}</p>
                )}

                {/* Initial state — no checks run yet */}
                {!artistFixChecking && !artistFixCheckResult && !artistFixError && (
                  <>
                    <p className="check-empty">
                      Run checks to identify potential issues with this artist.
                    </p>
                    <div className="check-actions">
                      <button className="check-run-btn" type="button" onClick={() => void runChecks()}>
                        Run Checks
                      </button>
                    </div>
                  </>
                )}

                {/* Checking in progress */}
                {artistFixChecking && (
                  <p className="check-running">Running checks…</p>
                )}

                {/* Check complete: not compound */}
                {artistFixCheckResult && !artistFixCheckResult.is_compound && (
                  <div className="check-status check-pass-banner">
                    ✓ All checks passed — no issues found
                  </div>
                )}

                {/* Check complete: compound artist detected */}
                {artistFixCheckResult && artistFixCheckResult.is_compound && (
                  <>
                    <div className="check-status check-fail-banner">
                      ✗ Artist appears to be a collaboration
                    </div>
                    <p className="check-meta">
                      Checked {artistFixCheckResult.total_sources_checked} source file{artistFixCheckResult.total_sources_checked !== 1 ? "s" : ""}.
                      Found evidence in {artistFixCheckResult.evidence_count} file{artistFixCheckResult.evidence_count !== 1 ? "s" : ""}.
                    </p>
                    <div className="check-section">
                      <span className="modal-section-label">Individual artists detected</span>
                      <ul className="check-detail-list">
                        {artistFixCheckResult.individual_artist_names.map((name) => (
                          <li key={name} className="check-detail-item">{name}</li>
                        ))}
                      </ul>
                    </div>
                  </>
                )}
              </div>
            </div>
          </div>
        );
      })()}

      {contextMenu && (
        <>
          <div
            className="ctx-menu-backdrop"
            onClick={() => setContextMenu(null)}
            onContextMenu={(e) => { e.preventDefault(); setContextMenu(null); }}
          />
          <ul
            className="ctx-menu"
            style={{ left: contextMenu.x, top: contextMenu.y }}
          >
            {contextMenu.kind === "recording" && (
              <>
                <li>
                  <button
                    className="ctx-menu-item"
                    type="button"
                    onClick={() => {
                      dispatchNav({ type: "navigate", nav: { type: "recording", id: contextMenu.recording.id } });
                      setContextMenu(null);
                    }}
                  >
                    View details
                  </button>
                </li>
                <li className="ctx-menu-divider" />
                {contextMenu.recording.source_paths.length === 1 ? (
                  <li>
                    <button
                      className="ctx-menu-item"
                      type="button"
                      onClick={() => {
                        void handleRescanSources(contextMenu.recording.source_paths);
                        setContextMenu(null);
                      }}
                    >
                      Rescan source
                    </button>
                  </li>
                ) : contextMenu.recording.source_paths.length > 1 ? (
                  <li>
                    <button
                      className="ctx-menu-item"
                      type="button"
                      onClick={() => {
                        void handleRescanSources(contextMenu.recording.source_paths);
                        setContextMenu(null);
                      }}
                    >
                      Rescan all sources
                    </button>
                  </li>
                ) : null}
                {contextMenu.recording.source_paths.length > 1 && (
                  <li>
                    <button
                      className="ctx-menu-item"
                      type="button"
                      onClick={() => {
                        setSplitRecordingId(contextMenu.recording.id);
                        setContextMenu(null);
                      }}
                    >
                      Split recording...
                    </button>
                  </li>
                )}
              </>
            )}
            {contextMenu.kind === "source" && (
              <li>
                <button
                  className="ctx-menu-item"
                  type="button"
                  onClick={() => {
                    void handleRescanSource(contextMenu.path);
                    setContextMenu(null);
                  }}
                >
                  Rescan source
                </button>
              </li>
            )}
            {contextMenu.kind === "artist" && (
              <>
                <li>
                  <button
                    className="ctx-menu-item"
                    type="button"
                    onClick={() => {
                      dispatchNav({ type: "navigate", nav: { type: "artist", id: contextMenu.artist_id } });
                      setContextMenu(null);
                    }}
                  >
                    View details
                  </button>
                </li>
                <li>
                  <button
                    className="ctx-menu-item"
                    type="button"
                    onClick={() => {
                      setArtistFixModal({ artistId: contextMenu.artist_id, artistName: contextMenu.artist_name });
                      setContextMenu(null);
                    }}
                  >
                    Fix issues with artist…
                  </button>
                </li>
                <li>
                  <button
                    className="ctx-menu-item"
                    type="button"
                    onClick={() => {
                      void handleRescanArtist(contextMenu.artist_id);
                      setContextMenu(null);
                    }}
                  >
                    Rescan sources
                  </button>
                </li>
              </>
            )}
            {contextMenu.kind === "release_group" && (
              <>
                <li>
                  <button
                    className="ctx-menu-item"
                    type="button"
                    onClick={() => {
                      dispatchNav({ type: "navigate", nav: { type: "release_group", id: contextMenu.release_group_id } });
                      setContextMenu(null);
                    }}
                  >
                    View details
                  </button>
                </li>
                <li>
                  <button
                    className="ctx-menu-item"
                    type="button"
                    onClick={() => {
                      void handleRescanReleaseGroup(contextMenu.release_group_id);
                      setContextMenu(null);
                    }}
                  >
                    Rescan sources
                  </button>
                </li>
              </>
            )}
            {externalCommands.length > 0 && contextMenuPath && (
              <li className="ctx-menu-divider" />
            )}
            {externalCommands.map((cmd) => (
              contextMenuPath ? (
                <li key={cmd.name}>
                  <button
                    className="ctx-menu-item"
                    type="button"
                    onClick={() => {
                      void handleSpawnExternalCommand(cmd.template, contextMenuPath);
                      setContextMenu(null);
                    }}
                  >
                    {cmd.name}
                  </button>
                </li>
              ) : null
            ))}
          </ul>
        </>
      )}

      {pomodoroPrompt && (
        <>
          <div
            className="ctx-menu-backdrop"
            onClick={() => setPomodoroPrompt(null)}
            onContextMenu={(e) => { e.preventDefault(); setPomodoroPrompt(null); }}
          />
          <div
            className="pomodoro-prompt"
            style={{ left: pomodoroPrompt.x, top: pomodoroPrompt.y }}
          >
            <input
              autoFocus
              className="pomodoro-duration-input"
              type="text"
              value={pomodoroPrompt.value}
              onChange={(e) => setPomodoroPrompt({ ...pomodoroPrompt, value: e.target.value })}
              onFocus={(e) => e.target.select()}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  submitPomodoroPrompt();
                } else if (e.key === "Escape") {
                  setPomodoroPrompt(null);
                }
              }}
              placeholder="e.g. 55m"
              aria-label="Custom Pomodoro duration"
              title="Pomodoro duration (e.g., 25m, 1h, 1h30m)"
            />
          </div>
        </>
      )}

      <footer className="status-bar">
        <span className={`status-bar-indicator ${bootstrap?.import_progress.is_running || pendingJobs > 0 ? "status-bar-indicator-active" : ""}`} />
        <span>Scanned {bootstrap?.import_progress.scanned ?? 0}</span>
        <span className="status-bar-sep">·</span>
        <span>Imported {bootstrap?.import_progress.imported ?? 0}</span>
        <span className="status-bar-sep">·</span>
        <span className={bootstrap?.import_progress.errors ? "status-bar-errors" : ""}>
          Errors {bootstrap?.import_progress.errors ?? 0}
        </span>
        {bootstrap?.import_progress.current_path ? (
          <>
            <span className="status-bar-sep">·</span>
            <span className="status-bar-path" title={bootstrap.import_progress.current_path}>
              {bootstrap.import_progress.current_path}
            </span>
          </>
        ) : null}
        {(bootstrap?.import_progress.fingerprinting_count ?? 0) > 0 ? (
          <>
            <span className="status-bar-sep">·</span>
            <span>
              Fingerprinting {bootstrap!.import_progress.fingerprinting_count} song{bootstrap!.import_progress.fingerprinting_count !== 1 ? "s" : ""} using {fingerprintCores} CPU core{fingerprintCores !== 1 ? "s" : ""}
            </span>
          </>
        ) : null}
        {pendingJobs > 0 ? (
          <>
            <span className="status-bar-sep">·</span>
            <span>{currentJobType === "delete" ? "Deleting" : "Processing"} {pendingJobs} job{pendingJobs !== 1 ? "s" : ""}</span>
          </>
        ) : null}
        {fileIssues.length > 0 ? (
          <>
            <span className="status-bar-sep">·</span>
            <button
              className="status-bar-warning"
              onClick={() => { setSettingsTab("issues"); setIsModalOpen(true); }}
              type="button"
              title={`${fileIssues.length} file issue${fileIssues.length !== 1 ? "s" : ""}`}
            >
              ⚠ {fileIssues.length}
            </button>
          </>
        ) : null}
      </footer>
    </main>
  );
}

export default App;
