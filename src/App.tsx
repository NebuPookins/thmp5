import { FormEvent, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

type LibrarySummary = {
  recording_count: number;
  artist_count: number;
  release_group_count: number;
  source_count: number;
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
  onChange: (value: number | null) => void;
  disabled?: boolean;
};

function RatingStars({ value, onChange, disabled = false }: RatingStarsProps) {
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
              onChange(value === star ? null : star);
            }}
            type="button"
          >
            ★
          </button>
        );
      })}
    </div>
  );
}

function App() {
  const [bootstrap, setBootstrap] = useState<AppBootstrap | null>(null);
  const [recordings, setRecordings] = useState<RecordingRow[]>([]);
  const [artists, setArtists] = useState<ArtistRow[]>([]);
  const [releaseGroups, setReleaseGroups] = useState<ReleaseGroupRow[]>([]);
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [history, setHistory] = useState<QueueItem[]>([]);
  const [currentTrack, setCurrentTrack] = useState<QueueItem | null>(null);
  const [playerState, setPlayerState] = useState<PlayerState>(DEFAULT_PLAYER_STATE);
  const [search, setSearch] = useState("");
  const [selectedArtistId, setSelectedArtistId] = useState<string | null>(null);
  const [selectedReleaseGroupId, setSelectedReleaseGroupId] = useState<string | null>(null);
  const [wizardPath, setWizardPath] = useState("");
  const [queueHistoryLimitInput, setQueueHistoryLimitInput] = useState("5");
  const [isBootstrapping, setIsBootstrapping] = useState(true);
  const [isSubmittingWizard, setIsSubmittingWizard] = useState(false);
  const [isRefreshingLibrary, setIsRefreshingLibrary] = useState(false);
  const [isSavingQueueSettings, setIsSavingQueueSettings] = useState(false);
  const [ratingKeyInFlight, setRatingKeyInFlight] = useState<string | null>(null);
  const [playerCoverArt, setPlayerCoverArt] = useState<string | null>(null);
  const [isModalOpen, setIsModalOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selectedTag, setSelectedTag] = useState<string | null>(null);
  const [_allTags, setAllTags] = useState<string[]>([]);

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

        return [recording.artist_credit_name, recording.title, recording.release_group_title, recording.genre]
          .filter(Boolean)
          .some((value) => value!.toLowerCase().includes(needle));
      })
      .sort(trackSort);
  }, [recordings, search, selectedArtist, selectedReleaseGroup, selectedTag]);

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

  async function loadLibraryData(nextArtistId = selectedArtistId, nextSearch = search) {
    setIsRefreshingLibrary(true);
    try {
      await Promise.all([
        loadRecordings(),
        loadArtists(),
        loadReleaseGroups(nextArtistId, nextSearch),
        loadAllTags(),
      ]);
    } catch (loadError) {
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
      setPlayerState(currentPlayerState);
    } catch (loadError) {
      setError(loadError instanceof Error ? loadError.message : String(loadError));
    } finally {
      setIsBootstrapping(false);
    }
  }

  useEffect(() => {
    void loadBootstrap();
  }, []);

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
      void loadReleaseGroups(selectedArtistId, search).catch((loadError) => {
        setError(loadError instanceof Error ? loadError.message : String(loadError));
      });
    }, 120);

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

    const interval = window.setInterval(async () => {
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

        if (progress.is_running) {
          await loadLibraryData(selectedArtistId, search);
        }
      } catch (pollError) {
        setError(pollError instanceof Error ? pollError.message : String(pollError));
      }
    }, 2000);

    return () => window.clearInterval(interval);
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

  async function completeCurrentTrack(positionMs?: number) {
    const finishedTrack = currentTrack;
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

  async function updateRecordingRating(recordingId: string, stars: number | null) {
    const ratingKey = `recording:${recordingId}`;
    const previousRecordings = recordings;
    setRatingKeyInFlight(ratingKey);
    setRecordings((current) =>
      current.map((recording) =>
        recording.id === recordingId ? { ...recording, rating: stars } : recording,
      ),
    );
    setCurrentTrack((current) =>
      current?.id === recordingId ? { ...current, rating: stars } : current,
    );

    try {
      await invoke("set_recording_rating", {
        request: { id: recordingId, stars },
      });
    } catch (ratingError) {
      setRecordings(previousRecordings);
      setCurrentTrack((current) =>
        current?.id === recordingId ? { ...current, rating: recordings.find((r) => r.id === recordingId)?.rating ?? null } : current,
      );
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
                onChange={(stars) => { void updateRecordingRating(currentTrack.id, stars); }}
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

          <div className="browser-grid">
            <section className="browser-column">
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
            </section>

            <section className="browser-column">
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
            </section>

            <section className="track-column">
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
              <div className="table-wrap track-table-wrap">
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
                    </tr>
                  </thead>
                  <tbody>
                    {filteredRecordings.map((recording) => (
                      <tr
                        className={recording.primary_source_id ? "playable-row" : "muted-row"}
                        key={recording.id}
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
                            onChange={(stars) => {
                              void updateRecordingRating(recording.id, stars);
                            }}
                            value={recording.rating}
                          />
                        </td>
                        <td>{formatDuration(recording.duration_ms)}</td>
                        <td>{recording.play_count}</td>
                        <td>{formatLastPlayed(recording.last_played)}</td>
                      </tr>
                    ))}
                    {filteredRecordings.length === 0 ? (
                      <tr>
                        <td className="empty-table-state" colSpan={10}>
                          No tracks match the current artist, album, and search filters.
                        </td>
                      </tr>
                    ) : null}
                  </tbody>
                </table>
              </div>
            </section>
          </div>
        </section>

        <aside className="queue-panel">
          <section className="queue-list-panel">
            <div className="queue-header">
              <div>
                <p className="panel-label">Up next</p>
                <strong>{queue.length} queued</strong>
              </div>
            </div>
            <ol className="queue-list">
              {queue.length === 0 ? (
                <li className="empty-item">Nothing queued.</li>
              ) : (
                queue.map((item) => (
                  <li key={`${item.id}-${item.primary_source_id}`}>
                    <strong>{item.title}</strong>
                    <span>{item.artist_credit_name ?? "Unknown Artist"}</span>
                  </li>
                ))
              )}
            </ol>
          </section>

          <section className="queue-list-panel">
            <div className="queue-header">
              <div>
                <p className="panel-label">Recently played</p>
                <strong>{history.length} tracked</strong>
              </div>
            </div>
            <ol className="queue-list">
              {history.length === 0 ? (
                <li className="empty-item">Playback history will show up here.</li>
              ) : (
                history.map((item) => (
                  <li key={`history-${item.id}-${item.play_count}`}>
                    <strong>{item.title}</strong>
                    <span>{item.artist_credit_name ?? "Unknown Artist"}</span>
                  </li>
                ))
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
              <h2>Options</h2>
              <button
                className="modal-close-btn"
                onClick={() => setIsModalOpen(false)}
                type="button"
              >✕</button>
            </div>

            <div className="modal-section">
              <p className="modal-section-label">Library</p>
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
      </footer>
    </main>
  );
}

export default App;
