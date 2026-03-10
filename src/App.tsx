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

function App() {
  const [bootstrap, setBootstrap] = useState<AppBootstrap | null>(null);
  const [recordings, setRecordings] = useState<RecordingRow[]>([]);
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [history, setHistory] = useState<QueueItem[]>([]);
  const [currentTrack, setCurrentTrack] = useState<QueueItem | null>(null);
  const [playerState, setPlayerState] = useState<PlayerState>(DEFAULT_PLAYER_STATE);
  const [search, setSearch] = useState("");
  const [wizardPath, setWizardPath] = useState("");
  const [queueHistoryLimitInput, setQueueHistoryLimitInput] = useState("5");
  const [isBootstrapping, setIsBootstrapping] = useState(true);
  const [isSubmittingWizard, setIsSubmittingWizard] = useState(false);
  const [isRefreshingTable, setIsRefreshingTable] = useState(false);
  const [isSavingQueueSettings, setIsSavingQueueSettings] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const queueHistoryLimit = bootstrap?.config.queue_history_limit ?? 5;
  const isPlaying = playerState.status === "playing";
  const activeDurationMs = playerState.duration_ms ?? currentTrack?.duration_ms ?? 0;

  async function loadRecordings() {
    setIsRefreshingTable(true);
    try {
      const rows = await invoke<RecordingRow[]>("list_recordings", {
        limit: 10000,
        offset: 0,
      });
      setRecordings(rows);
    } catch (loadError) {
      setError(loadError instanceof Error ? loadError.message : String(loadError));
    } finally {
      setIsRefreshingTable(false);
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

    void loadRecordings();
  }, [bootstrap?.needs_setup]);

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
          const rows = await invoke<RecordingRow[]>("list_recordings", {
            limit: 10000,
            offset: 0,
          });
          setRecordings(rows);
        }
      } catch (pollError) {
        setError(pollError instanceof Error ? pollError.message : String(pollError));
      }
    }, 2000);

    return () => window.clearInterval(interval);
  }, [bootstrap?.needs_setup]);

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
    })
      .then((nextState) => setPlayerState(nextState))
      .catch((playError) => {
        setError(playError instanceof Error ? playError.message : String(playError));
        setCurrentTrack(null);
      });
  }, [currentTrack?.id, currentTrack?.primary_source_id]);

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
      await loadRecordings();
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
        const nextState = await invoke<PlayerState>("pause");
        setPlayerState(nextState);
      } else {
        const nextState = await invoke<PlayerState>("resume");
        setPlayerState(nextState);
      }
    } catch (playbackError) {
      setError(playbackError instanceof Error ? playbackError.message : String(playbackError));
    }
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
      const nextState = await invoke<PlayerState>("seek", {
        request: { position_ms: nextPositionMs },
      });
      setPlayerState(nextState);
    } catch (seekError) {
      setError(seekError instanceof Error ? seekError.message : String(seekError));
    }
  }

  async function handleVolumeChange(nextVolume: number) {
    try {
      const nextState = await invoke<PlayerState>("set_volume", {
        request: { volume: nextVolume },
      });
      setPlayerState(nextState);
    } catch (volumeError) {
      setError(volumeError instanceof Error ? volumeError.message : String(volumeError));
    }
  }

  const filteredRecordings = useMemo(() => {
    const needle = search.trim().toLowerCase();
    if (!needle) {
      return recordings;
    }

    return recordings.filter((recording) =>
      [
        recording.artist_credit_name,
        recording.title,
        recording.release_group_title,
        recording.genre,
      ]
        .filter(Boolean)
        .some((value) => value!.toLowerCase().includes(needle)),
    );
  }, [recordings, search]);

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
        <div>
          <p className="eyebrow">thmp5</p>
          <h1>Library</h1>
        </div>
        <div className="topbar-actions">
          <button
            className="secondary-button"
            disabled={isRefreshingTable}
            onClick={() => {
              void loadRecordings();
            }}
            type="button"
          >
            {isRefreshingTable ? "Refreshing..." : "Refresh table"}
          </button>
          <button className="secondary-button" onClick={handleRescan} type="button">
            Rescan library
          </button>
        </div>
      </section>

      {error ? <section className="error-banner">{error}</section> : null}

      <section className="summary-grid">
        <article className="summary-card">
          <span className="summary-value">
            {bootstrap?.library_summary.recording_count ?? 0}
          </span>
          <span className="summary-label">Recordings</span>
        </article>
        <article className="summary-card">
          <span className="summary-value">
            {bootstrap?.library_summary.artist_count ?? 0}
          </span>
          <span className="summary-label">Artists</span>
        </article>
        <article className="summary-card">
          <span className="summary-value">
            {bootstrap?.library_summary.source_count ?? 0}
          </span>
          <span className="summary-label">Sources</span>
        </article>
      </section>

      <section className="import-banner">
        <div>
          <p className="panel-label">Library scan</p>
          <strong>
            {bootstrap?.import_progress.is_running ? "Scanning in background" : "Idle"}
          </strong>
          <p className="subtle-text">
            Root: {bootstrap?.config.music_root ?? "Not configured"}
          </p>
        </div>
        <div className="import-stats">
          <span>Scanned {bootstrap?.import_progress.scanned ?? 0}</span>
          <span>Imported {bootstrap?.import_progress.imported ?? 0}</span>
          <span>Skipped {bootstrap?.import_progress.skipped ?? 0}</span>
          <span>Errors {bootstrap?.import_progress.errors ?? 0}</span>
        </div>
        {bootstrap?.import_progress.current_path ? (
          <p className="current-path">{bootstrap.import_progress.current_path}</p>
        ) : null}
      </section>

      <section className="layout-grid">
        <section className="table-panel">
          <div className="table-toolbar">
            <input
              className="search-input"
              onChange={(event) => setSearch(event.currentTarget.value)}
              placeholder="Filter by artist, title, album, genre"
              value={search}
            />
            <span className="panel-meta">{filteredRecordings.length} rows</span>
          </div>

          <div className="table-wrap">
            <table className="recordings-table">
              <thead>
                <tr>
                  <th>Artist</th>
                  <th>Title</th>
                  <th>Album</th>
                  <th>Genre</th>
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
                    <td>{recording.artist_credit_name ?? "Unknown Artist"}</td>
                    <td>{recording.title}</td>
                    <td>{recording.release_group_title ?? "Unknown Album"}</td>
                    <td>{recording.genre ?? "—"}</td>
                    <td>{recording.rating ?? "—"}</td>
                    <td>{formatDuration(recording.duration_ms)}</td>
                    <td>{recording.play_count}</td>
                    <td>{formatLastPlayed(recording.last_played)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>

        <aside className="queue-panel">
          <section className="player-panel">
            <p className="panel-label">Player</p>
            <h2>{currentTrack?.title ?? "Nothing queued"}</h2>
            <p className="subtle-text">
              {currentTrack?.artist_credit_name ?? "Queue a track from the library"}
            </p>
            <div className="player-controls">
              <button className="primary-button" onClick={handlePauseResume} type="button">
                {playerState.status === "loading"
                  ? "Loading..."
                  : isPlaying
                    ? "Pause"
                    : "Play"}
              </button>
              <button
                className="secondary-button"
                disabled={!currentTrack}
                onClick={() => {
                  void handleSkip();
                }}
                type="button"
              >
                Skip
              </button>
            </div>

            <div className="player-scrubber">
              <span>{formatDuration(playerState.position_ms)}</span>
              <input
                className="slider-input"
                disabled={!currentTrack || activeDurationMs <= 0}
                max={activeDurationMs || 0}
                min={0}
                onChange={(event) => {
                  void handleSeek(Number(event.currentTarget.value));
                }}
                type="range"
                value={Math.min(playerState.position_ms, activeDurationMs || 0)}
              />
              <span>{formatDuration(activeDurationMs)}</span>
            </div>

            <label className="volume-row">
              <span className="panel-meta">Volume</span>
              <input
                className="slider-input"
                max={1.5}
                min={0}
                onChange={(event) => {
                  void handleVolumeChange(Number(event.currentTarget.value));
                }}
                step={0.01}
                type="range"
                value={playerState.volume}
              />
              <strong>{Math.round(playerState.volume * 100)}%</strong>
            </label>
          </section>

          <section className="queue-settings">
            <p className="panel-label">Queue settings</p>
            <div className="settings-row">
              <div>
                <label className="input-label" htmlFor="queue-history-limit">
                  History limit
                </label>
                <input
                  id="queue-history-limit"
                  className="small-input"
                  inputMode="numeric"
                  onChange={(event) => setQueueHistoryLimitInput(event.currentTarget.value)}
                  value={queueHistoryLimitInput}
                />
              </div>
              <button
                className="secondary-button"
                disabled={isSavingQueueSettings}
                onClick={() => {
                  void saveQueueSettings();
                }}
                type="button"
              >
                {isSavingQueueSettings ? "Saving..." : "Save"}
              </button>
            </div>
          </section>

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
    </main>
  );
}

export default App;
