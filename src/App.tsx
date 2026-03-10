import { FormEvent, useEffect, useMemo, useRef, useState } from "react";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
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

type QueueItem = RecordingRow;

function formatDuration(durationMs: number | null): string {
  if (!durationMs || durationMs <= 0) {
    return "Unknown";
  }

  const totalSeconds = Math.floor(durationMs / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

function formatLastPlayed(value: string | null): string {
  if (!value) {
    return "Never";
  }

  return value;
}

function sourceUrl(recording: RecordingRow | null): string | null {
  if (!recording?.primary_source_path) {
    return null;
  }

  return convertFileSrc(recording.primary_source_path);
}

function App() {
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const [bootstrap, setBootstrap] = useState<AppBootstrap | null>(null);
  const [recordings, setRecordings] = useState<RecordingRow[]>([]);
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [history, setHistory] = useState<QueueItem[]>([]);
  const [currentTrack, setCurrentTrack] = useState<QueueItem | null>(null);
  const [search, setSearch] = useState("");
  const [wizardPath, setWizardPath] = useState("");
  const [queueHistoryLimitInput, setQueueHistoryLimitInput] = useState("5");
  const [isBootstrapping, setIsBootstrapping] = useState(true);
  const [isSubmittingWizard, setIsSubmittingWizard] = useState(false);
  const [isRefreshingTable, setIsRefreshingTable] = useState(false);
  const [isSavingQueueSettings, setIsSavingQueueSettings] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [isPlaying, setIsPlaying] = useState(false);

  const queueHistoryLimit = bootstrap?.config.queue_history_limit ?? 5;

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
      const result = await invoke<AppBootstrap>("get_app_bootstrap");
      setBootstrap(result);
      setQueueHistoryLimitInput(String(result.config.queue_history_limit));
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
        setBootstrap((current) =>
          current
            ? {
                ...current,
                import_progress: progress,
              }
            : current,
        );

        if (progress.is_running) {
          const rows = await invoke<RecordingRow[]>("list_recordings", {
            limit: 10000,
            offset: 0,
          });
          const summary = await invoke<LibrarySummary>("get_library_summary");
          setRecordings(rows);
          setBootstrap((current) =>
            current
              ? {
                  ...current,
                  import_progress: progress,
                  library_summary: summary,
                }
              : current,
          );
        } else {
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
        }
      } catch (pollError) {
        setError(pollError instanceof Error ? pollError.message : String(pollError));
      }
    }, 2000);

    return () => window.clearInterval(interval);
  }, [bootstrap?.needs_setup]);

  useEffect(() => {
    if (currentTrack || queue.length === 0) {
      return;
    }

    setCurrentTrack(queue[0]);
    setQueue((current) => current.slice(1));
  }, [currentTrack, queue]);

  useEffect(() => {
    const audio = audioRef.current;
    if (!audio) {
      return;
    }

    const nextSource = sourceUrl(currentTrack);
    if (!nextSource) {
      setIsPlaying(false);
      return;
    }

    audio.src = nextSource;
    void audio.play().then(
      () => setIsPlaying(true),
      (playError) =>
        setError(playError instanceof Error ? playError.message : String(playError)),
    );
  }, [currentTrack]);

  async function completeCurrentTrack() {
    const audio = audioRef.current;
    if (currentTrack) {
      try {
        await invoke("record_play_history", {
          input: {
            recording_id: currentTrack.id,
            source_id: currentTrack.primary_source_id,
            duration_played_ms: audio ? Math.floor(audio.currentTime * 1000) : null,
          },
        });
      } catch (recordError) {
        setError(recordError instanceof Error ? recordError.message : String(recordError));
      }

      setHistory((current) => [currentTrack, ...current].slice(0, queueHistoryLimit));
    }

    setCurrentTrack(null);
    setIsPlaying(false);
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

  function addToQueue(recording: RecordingRow) {
    if (!recording.primary_source_path) {
      setError(`No playable local file is available for "${recording.title}".`);
      return;
    }

    setQueue((current) => [...current, recording]);
  }

  function handlePauseResume() {
    const audio = audioRef.current;
    if (!audio) {
      return;
    }

    if (audio.paused) {
      void audio.play().then(() => setIsPlaying(true));
      return;
    }

    audio.pause();
    setIsPlaying(false);
  }

  async function handleSkip() {
    await completeCurrentTrack();
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
      <audio
        onEnded={() => {
          void completeCurrentTrack();
        }}
        onPause={() => setIsPlaying(false)}
        onPlay={() => setIsPlaying(true)}
        ref={audioRef}
      />

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
                    className={recording.primary_source_path ? "playable-row" : "muted-row"}
                    key={recording.id}
                    onDoubleClick={() => addToQueue(recording)}
                    title={
                      recording.primary_source_path
                        ? "Double click to queue"
                        : "No playable local file"
                    }
                  >
                    <td>{recording.artist_credit_name ?? "Unknown Artist"}</td>
                    <td>{recording.title}</td>
                    <td>{recording.release_group_title ?? "Unknown Album"}</td>
                    <td>{recording.genre ?? "Unknown"}</td>
                    <td>{recording.rating ?? "–"}</td>
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
            <p className="panel-label">Now playing</p>
            <h2>{currentTrack?.title ?? "Nothing playing"}</h2>
            <p className="subtle-text">
              {currentTrack?.artist_credit_name ?? "Queue a recording by double clicking a row."}
            </p>
            <div className="player-controls">
              <button
                className="secondary-button"
                disabled={!currentTrack}
                onClick={handlePauseResume}
                type="button"
              >
                {isPlaying ? "Pause" : "Play"}
              </button>
              <button
                className="secondary-button"
                disabled={!currentTrack}
                onClick={() => {
                  void handleSkip();
                }}
                type="button"
              >
                Next
              </button>
            </div>
          </section>

          <section className="queue-settings">
            <p className="panel-label">Queue settings</p>
            <label className="input-label" htmlFor="history-limit">
              Keep last N songs
            </label>
            <div className="settings-row">
              <input
                id="history-limit"
                className="small-input"
                onChange={(event) => setQueueHistoryLimitInput(event.currentTarget.value)}
                value={queueHistoryLimitInput}
              />
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
            <p className="subtle-text">Current limit: {queueHistoryLimit}</p>
          </section>

          <section className="queue-list-panel">
            <div className="queue-header">
              <p className="panel-label">Up next</p>
              <span className="panel-meta">{queue.length}</span>
            </div>
            <ul className="queue-list">
              {queue.map((item, index) => (
                <li key={`${item.id}-${index}`}>
                  <strong>{item.title}</strong>
                  <span>{item.artist_credit_name ?? "Unknown Artist"}</span>
                </li>
              ))}
              {queue.length === 0 ? <li className="empty-item">Queue is empty.</li> : null}
            </ul>
          </section>

          <section className="queue-list-panel">
            <div className="queue-header">
              <p className="panel-label">Recently played</p>
              <span className="panel-meta">{history.length}</span>
            </div>
            <ul className="queue-list">
              {history.map((item) => (
                <li key={`history-${item.id}-${item.primary_source_id ?? "none"}`}>
                  <strong>{item.title}</strong>
                  <span>{item.artist_credit_name ?? "Unknown Artist"}</span>
                </li>
              ))}
              {history.length === 0 ? (
                <li className="empty-item">Playback history is empty.</li>
              ) : null}
            </ul>
          </section>
        </aside>
      </section>
    </main>
  );
}

export default App;
