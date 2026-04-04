import { FormEvent, memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
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

type AppConfig = {
  music_root: string | null;
  queue_history_limit: number;
};

type AppBootstrap = {
  needs_setup: boolean;
  config: AppConfig;
  import_progress: ImportProgress;
  library_summary: LibrarySummary;
};

type RecordingRow = {
  id: string;
  title: string;
  duration_ms: number | null;
  primary_artist_id: string | null;
  release_group_id: string | null;
  artist_credit_name: string | null;
  release_group_title: string | null;
  genre: string | null;
  release_date: string | null;
  track_position: number | null;
  disc_position: number | null;
  rating: number | null;
  play_count: number;
  last_played: string | null;
  primary_source_id: string | null;
  primary_source_path: string | null;
  tags: string[];
  source_paths: string[];
};

type ArtistRow = {
  id: string;
  name: string;
  sort_name: string;
  release_group_count: number;
  recording_count: number;
  rating: number | null;
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
  recording_id: string | null;
  source_id: string | null;
  title: string | null;
  artist: string | null;
  duration_ms: number | null;
  position_ms: number;
  volume: number;
};

type PlayerErrorEvent = {
  message: string;
};

type FileIssue = {
  file_path: string;
  kind: "import_error" | "playback_error";
  message: string;
};

type QueueItem = RecordingRow;

const DEFAULT_PLAYER_STATE: PlayerState = {
  status: "stopped",
  recording_id: null,
  source_id: null,
  title: null,
  artist: null,
  duration_ms: null,
  position_ms: 0,
  volume: 1,
};

function formatDuration(durationMs: number | null): string {
  if (!durationMs || durationMs <= 0) {
    return "Unknown";
  }

  const totalSeconds = Math.floor(durationMs / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;

  if (hours > 0) {
    return `${hours}:${minutes.toString().padStart(2, "0")}:${seconds
      .toString()
      .padStart(2, "0")}`;
  }

  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
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

function trackSort(a: RecordingRow, b: RecordingRow): number {
  const discDelta = (a.disc_position ?? Number.MAX_SAFE_INTEGER) - (b.disc_position ?? Number.MAX_SAFE_INTEGER);
  if (discDelta !== 0) {
    return discDelta;
  }

  const trackDelta =
    (a.track_position ?? Number.MAX_SAFE_INTEGER) - (b.track_position ?? Number.MAX_SAFE_INTEGER);
  if (trackDelta !== 0) {
    return trackDelta;
  }

  return a.title.localeCompare(b.title);
}

type RatingStarsProps = {
  value: number | null;
  recordingId: string;
  onRate: (recordingId: string, value: number | null) => void;
  disabled?: boolean;
};

const RatingStars = memo(function RatingStars({ value, recordingId, onRate, disabled = false }: RatingStarsProps) {
  return (
    <div className="rating-stars" role="group">
      {[1, 2, 3, 4, 5].map((star) => {
        const filled = value !== null && star <= value;
        return (
          <button
            aria-label={`Set rating to ${star}`}
            className={`rating-star ${filled ? "rating-star-filled" : ""}`}
            disabled={disabled}
            key={star}
            onClick={(event) => {
              event.preventDefault();
              event.stopPropagation();
              onRate(recordingId, value === star ? null : star);
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

function App() {
  const [bootstrap, setBootstrap] = useState<AppBootstrap | null>(null);
  const [recordings, setRecordings] = useState<RecordingRow[]>([]);
  const [artists, setArtists] = useState<ArtistRow[]>([]);
  const [releaseGroups, setReleaseGroups] = useState<ReleaseGroupRow[]>([]);
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [history, setHistory] = useState<QueueItem[]>([]);
  const [autoDj, setAutoDj] = useState(false);
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
  const [isRefreshingLibrary, setIsRefreshingLibrary] = useState(false);
  const [isSavingQueueSettings, setIsSavingQueueSettings] = useState(false);
  const [ratingKeyInFlight, setRatingKeyInFlight] = useState<string | null>(null);
  const [playerCoverArt, setPlayerCoverArt] = useState<string | null>(null);
  const [isModalOpen, setIsModalOpen] = useState(false);
  const [settingsTab, setSettingsTab] = useState<"options" | "issues">("options");
  const [fileIssues, setFileIssues] = useState<FileIssue[]>([]);
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
  const releaseGroupSearchInFlightRef = useRef(false);
  const queuedReleaseGroupSearchRef = useRef<{ artistId: string | null; search: string } | null>(null);

  const queueHistoryLimit = bootstrap?.config.queue_history_limit ?? 5;
  const isPlaying = playerState.status === "playing";
  const activeDurationMs = playerState.duration_ms ?? currentTrack?.duration_ms ?? 0;

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
    if (!needle) {
      return artists;
    }

    return artists.filter((artist) => artist.name.toLowerCase().includes(needle));
  }, [artists, search]);

  const filteredRecordings = useMemo(() => {
    const needle = search.trim().toLowerCase();

    return recordings
      .filter((recording) => {
        if (selectedArtist) {
          if (recording.primary_artist_id !== selectedArtist.id) {
            return false;
          }
        }

        if (selectedReleaseGroup) {
          if (recording.release_group_id !== selectedReleaseGroup.id) {
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
          [recording.artist_credit_name, recording.title, recording.release_group_title, recording.genre]
            .filter(Boolean)
            .some((value) => value!.toLowerCase().includes(needle)) ||
          (recording.source_paths ?? []).some((p) => p.toLowerCase().includes(needle))
        );
      })
      .sort(trackSort);
  }, [recordings, search, selectedArtist, selectedReleaseGroup, selectedTag]);

  const trackTableScrollRef = useRef<HTMLDivElement>(null);
  const rowVirtualizer = useVirtualizer({
    count: filteredRecordings.length,
    getScrollElement: () => trackTableScrollRef.current,
    estimateSize: () => 41,
    overscan: 10,
  });

  async function loadRecordings() {
    const rows = await invoke<RecordingRow[]>("list_recordings", {
      limit: 10000,
      offset: 0,
    });
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

  async function loadArtists() {
    const rows = await invoke<ArtistRow[]>("list_artists");
    setArtists(rows);
  }

  async function loadReleaseGroups(nextArtistId: string | null, nextSearch: string) {
    const rows = await invoke<ReleaseGroupRow[]>("list_release_groups", {
      artistId: nextArtistId,
      search: nextSearch.trim() || null,
    });
    setReleaseGroups(rows);
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
    setIsRefreshingLibrary(true);
    try {
      await loadRecordings();
      await loadArtists();
      await loadReleaseGroups(nextArtistId, nextSearch);
      await loadAllTags();
      await loadPlaylists();
    } catch (loadError) {
      await reportPoolTimeout("loadLibraryData", loadError);
      setError(loadError instanceof Error ? loadError.message : String(loadError));
    } finally {
      setIsRefreshingLibrary(false);
    }
  }

  async function loadBootstrap() {
    try {
      const [bootstrapResult, currentPlayerState] = await Promise.all([
        invoke<AppBootstrap>("get_app_bootstrap"),
        invoke<PlayerState>("get_player_state"),
      ]);
      setBootstrap(bootstrapResult);
      setQueueHistoryLimitInput(String(bootstrapResult.config.queue_history_limit));
      setMusicRootInput(bootstrapResult.config.music_root ?? "");
      setPlayerState(currentPlayerState);
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

  // Reload the file issue list whenever the issues tab is visible.
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

    let wasRunning = bootstrap.import_progress.is_running;
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
      } catch (pollError) {
        await reportPoolTimeout("import progress poll", pollError);
        setError(pollError instanceof Error ? pollError.message : String(pollError));
      } finally {
        if (!cancelled) {
          timeoutId = window.setTimeout(() => {
            void poll();
          }, 60000);
        }
      }
    }

    timeoutId = window.setTimeout(() => {
      void poll();
    }, 60000);

    return () => {
      cancelled = true;
      if (timeoutId !== null) {
        window.clearTimeout(timeoutId);
      }
    };
  }, [bootstrap?.needs_setup, search, selectedArtistId]);

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
        recording_id: string;
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

      return () => {
        unlistenState();
        unlistenPosition();
        unlistenEnded();
        unlistenError();
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
    const pick = playable[Math.floor(Math.random() * playable.length)];
    setQueue((current) => [...current, pick]);
  }, [autoDj, queue, recordings]);

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
    if (!playerState.recording_id) {
      setPlayerCoverArt(null);
      return;
    }
    void invoke<string | null>("get_cover_art", { recordingId: playerState.recording_id }).then(
      (art) => setPlayerCoverArt(art),
      () => setPlayerCoverArt(null),
    );
  }, [playerState.recording_id]);

  // Keep ref in sync so stale-closure callbacks (e.g. player-track-ended listener)
  // always see the latest value.
  currentTrackRef.current = currentTrack;

  async function completeCurrentTrack(positionMs?: number) {
    const finishedTrack = currentTrackRef.current;
    if (finishedTrack) {
      try {
        await invoke("record_play_history", {
          input: {
            recording_id: finishedTrack.id,
            source_id: finishedTrack.primary_source_id,
            duration_played_ms: positionMs ?? playerState.position_ms,
          },
        });
      } catch (recordError) {
        setError(recordError instanceof Error ? recordError.message : String(recordError));
      }
      void loadRecordings();

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

  const handleRate = useCallback((recordingId: string, stars: number | null) => {
    void updateRecordingRating(recordingId, stars);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recordings]);

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
      await invoke("set_recording_rating", {
        request: { id: recordingId, stars },
      });
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
              placeholder="/home/nebu/Music"
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

        <div className="now-playing-block">
          <div className="now-playing-meta">
            <div className="now-playing-text">
              <div className="now-playing-title">
                {currentTrack?.title ?? "Nothing playing"}
              </div>
              <div className="now-playing-sub">
                {currentTrack
                  ? [currentTrack.artist_credit_name, currentTrack.release_group_title]
                      .filter(Boolean)
                      .join(" · ")
                  : "Queue a track from the library"}
              </div>
            </div>
            {currentTrack ? (
              <RatingStars
                disabled={ratingKeyInFlight === `recording:${currentTrack.id}`}
                onRate={handleRate}
                recordingId={currentTrack.id}
                value={currentTrack.rating}
              />
            ) : null}
          </div>
          <div className="topbar-scrubber">
            <span>{formatDuration(playerState.position_ms)}</span>
            <input
              className="slider-input"
              disabled={!currentTrack || activeDurationMs <= 0}
              max={activeDurationMs || 0}
              min={0}
              onChange={(event) => { void handleSeek(Number(event.currentTarget.value)); }}
              type="range"
              value={Math.min(playerState.position_ms, activeDurationMs || 0)}
            />
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
                    {releaseGroups.length === 0 ? (
                      <p className="empty-browser-state">No albums available for this view.</p>
                    ) : (
                      releaseGroups.map((releaseGroup) => (
                        <div
                          className={`browser-item ${releaseGroup.id === selectedReleaseGroupId ? "browser-item-active" : ""}`}
                          key={releaseGroup.id}
                          onClick={() =>
                            setSelectedReleaseGroupId((current) =>
                              current === releaseGroup.id ? null : releaseGroup.id,
                            )
                          }
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
              {browserLeftTab === "smartplaylists" ? (
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
                              <th>Album</th>
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
                                <td>{recording.release_group_title ?? "Unknown Album"}</td>
                                <td>
                                  <RatingStars
                                    disabled={ratingKeyInFlight === `recording:${recording.id}`}
                                    onRate={handleRate}
                                    recordingId={recording.id}
                                    value={recording.rating}
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
                      <thead>
                        <tr>
                          <th>#</th>
                          <th>Title</th>
                          <th>Artist</th>
                          <th>Album</th>
                          <th>Genre</th>
                          <th>Tags</th>
                          <th>Rating</th>
                          <th>Duration</th>
                          <th>Plays</th>
                          <th>Last played</th>
                          <th>Sources</th>
                        </tr>
                      </thead>
                      <tbody>
                        {filteredRecordings.length === 0 ? (
                          <tr>
                            <td className="empty-table-state" colSpan={11}>
                              No tracks match the current artist, album, and search filters.
                            </td>
                          </tr>
                        ) : (
                          <>
                            {rowVirtualizer.getVirtualItems()[0]?.start > 0 && (
                              <tr style={{ height: rowVirtualizer.getVirtualItems()[0].start }}>
                                <td colSpan={11} />
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
                                  title={
                                    recording.primary_source_id
                                      ? "Double click to play or queue"
                                      : "No playable local file"
                                  }
                                >
                                  <td>
                                    {recording.disc_position && recording.disc_position > 1
                                      ? `${recording.disc_position}.${recording.track_position ?? "—"}`
                                      : recording.track_position ?? "—"}
                                  </td>
                                  <td>{recording.title}</td>
                                  <td>{recording.artist_credit_name ?? "Unknown Artist"}</td>
                                  <td>{recording.release_group_title ?? "Unknown Album"}</td>
                                  <td>{recording.genre ?? "—"}</td>
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
                                  <td>
                                    <RatingStars
                                      disabled={ratingKeyInFlight === `recording:${recording.id}`}
                                      onRate={handleRate}
                                      recordingId={recording.id}
                                      value={recording.rating}
                                    />
                                  </td>
                                  <td>{formatDuration(recording.duration_ms)}</td>
                                  <td>{recording.play_count}</td>
                                  <td>{formatLastPlayed(recording.last_played)}</td>
                                  <td className="source-paths-cell">
                                    {recording.source_paths.map((p) => (
                                      <div key={p} title={p} className="source-path">
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
                                  <td colSpan={11} />
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
                  {history.length > 0 || currentTrack || queue.length > 0
                    ? `${history.length} played · ${queue.length} queued`
                    : "Nothing playing"}
                </p>
              </div>
              <button
                className={`auto-dj-btn ${autoDj ? "auto-dj-btn-on" : ""}`}
                onClick={() => setAutoDj((v) => !v)}
                title={autoDj ? "Auto DJ on — click to disable" : "Auto DJ off — click to enable"}
                type="button"
              >
                Auto DJ
              </button>
            </div>
            <ol className="queue-list">
              {history.length === 0 && !currentTrack && queue.length === 0 ? (
                <li className="empty-item">Double-click a track to start playing.</li>
              ) : (
                <>
                  {[...history].reverse().map((item, i) => (
                    <li className="queue-history-item" key={`history-${i}-${item.id}`}>
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
                          value={item.rating}
                        />
                      </div>
                    </li>
                  ))}
                  {currentTrack ? (
                    <li className="queue-now-playing">
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
                          value={currentTrack.rating}
                        />
                      </div>
                    </li>
                  ) : null}
                  {queue.map((item) => (
                    <li key={`${item.id}-${item.primary_source_id}`}>
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
                          value={item.rating}
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
                      disabled={isRefreshingLibrary}
                      onClick={() => { void loadLibraryData(); }}
                      type="button"
                    >
                      {isRefreshingLibrary ? "Refreshing…" : "Refresh library"}
                    </button>
                    <button
                      className="secondary-button"
                      onClick={handleRescan}
                      type="button"
                    >
                      Rescan library
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
              </>
            ) : (
              <div className="modal-section">
                {fileIssues.length === 0 ? (
                  <p className="issue-empty">No file issues recorded this session.</p>
                ) : (
                  <ol className="issue-list">
                    {fileIssues.map((issue, i) => (
                      <li key={i} className="issue-item">
                        <div className="issue-item-header">
                          <span className={`issue-kind issue-kind-${issue.kind}`}>
                            {issue.kind === "import_error" ? "Import" : "Playback"}
                          </span>
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

      <footer className="status-bar">
        <span className={`status-bar-indicator ${bootstrap?.import_progress.is_running ? "status-bar-indicator-active" : ""}`} />
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
      </footer>
    </main>
  );
}

export default App;
