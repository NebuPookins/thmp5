import { useMemo, type Dispatch } from "react";
import type { RecordingRow, SourceRow } from "./autoDj";
import { formatDuration } from "./formatDuration";
import {
  canCompareRelink,
  canSubmitRelink,
  type FileDisposal,
  type RelinkKind,
  type RelinkModalAction,
  type RelinkModalState,
  type RelinkSubject,
} from "./modalReducers";

/** Everything that differs between the relink-missing and replace-corrupt flows. */
export const RELINK_KINDS: Record<
  RelinkKind,
  {
    /** The Tauri command that performs the merge. */
    command: string;
    title: string;
    /** The sentence that follows the subject's file path. */
    blurb: (subject: RelinkSubject) => string;
    confirmLabel: string;
    busyLabel: string;
  }
> = {
  relink_missing: {
    command: "relink_missing_source",
    title: "Relink Missing File",
    blurb: () =>
      "no longer exists. Pick the library file it was replaced by: this source's plays, rating, playlist entries and note move onto it, and this source is deleted.",
    confirmLabel: "Relink",
    busyLabel: "Relinking…",
  },
  replace_corrupt: {
    command: "replace_corrupt_source",
    title: "Replace Corrupt File",
    blurb: (subject) =>
      `is corrupt (${subject.message}). Pick the library file that replaces it: this source's plays, rating, playlist entries and note move onto it, this source is deleted, and its file is moved to the trash.`,
    confirmLabel: "Replace",
    busyLabel: "Replacing…",
  },
};

export type NowPlaying = "loading" | "playing" | "paused";

/**
 * Whether (and how) the source `sourceId` is the current track. `currentSourceId` changes as soon
 * as playback is requested; the player state catches up a moment later.
 */
export function nowPlayingOf(
  sourceId: string,
  currentSourceId: string | null,
  playerSourceId: string | null,
  playerStatus: "stopped" | "loading" | "playing" | "paused",
): NowPlaying | null {
  if (currentSourceId !== sourceId) return null;
  if (playerSourceId !== sourceId || playerStatus === "loading") return "loading";
  if (playerStatus === "stopped") return null;
  return playerStatus === "playing" ? "playing" : "paused";
}

const NOW_PLAYING_TEXT: Record<NowPlaying, string> = {
  loading: "Loading…",
  playing: "♪ Now playing",
  paused: "Paused",
};

function findSource(recordings: RecordingRow[], sourceId: string): SourceRow | undefined {
  for (const recording of recordings) {
    const source = recording.sources.find((s) => s.id === sourceId);
    if (source) return source;
  }
  return undefined;
}

type ComparisonSideProps = {
  label: string;
  source: { path: string | null; format: string | null; durationMs: number | null };
  note: string | null;
  nowPlaying: NowPlaying | null;
  disabled: boolean;
  onPlay: () => void;
};

function ComparisonSide({ label, source, note, nowPlaying, disabled, onPlay }: ComparisonSideProps) {
  return (
    <div className={`comparison-side${nowPlaying !== null ? " comparison-side-current" : ""}`}>
      <div>
        <span className="comparison-side-label">{label}</span>
        {nowPlaying !== null && <span className="comparison-now-playing">{NOW_PLAYING_TEXT[nowPlaying]}</span>}
      </div>
      <span className="comparison-side-paths" title={source.path ?? undefined}>{source.path ?? "(no path)"}</span>
      <span className="comparison-side-artist">
        {source.format ?? "unknown format"} · {source.durationMs ? formatDuration(source.durationMs) : "unknown duration"}
      </span>
      {note !== null && <span className="comparison-side-artist">{note}</span>}
      <button
        className="comparison-listen-btn"
        type="button"
        disabled={disabled || source.path === null}
        onClick={onPlay}
      >
        {nowPlaying !== null ? "↺ Restart" : "▶ Play"}
      </button>
    </div>
  );
}

type RelinkModalProps = {
  state: Extract<RelinkModalState, { phase: "open" }>;
  dispatch: Dispatch<RelinkModalAction>;
  recordings: RecordingRow[];
  currentSourceId: string | null;
  playerSourceId: string | null;
  playerStatus: "stopped" | "loading" | "playing" | "paused";
  onConfirm: (disposal: FileDisposal) => void;
  onPlay: (sourceId: string, filePath: string) => void;
};

export function RelinkModal({
  state,
  dispatch,
  recordings,
  currentSourceId,
  playerSourceId,
  playerStatus,
  onConfirm,
  onPlay,
}: RelinkModalProps) {
  const { subject, step, kind } = state;
  const texts = RELINK_KINDS[kind];
  const submitting = state.status === "submitting";
  const close = () => dispatch({ type: "close" });

  const comparing = step === "compare";
  const corruptSource = useMemo(
    () => (comparing ? findSource(recordings, subject.sourceId) : undefined),
    [comparing, recordings, subject.sourceId],
  );
  const targetSource = useMemo(
    () => (comparing && state.selectedSourceId !== null ? findSource(recordings, state.selectedSourceId) : undefined),
    [comparing, recordings, state.selectedSourceId],
  );

  const side = (label: string, source: SourceRow, note: string | null) => (
    <ComparisonSide
      label={label}
      source={{ path: source.file_path, format: source.format, durationMs: source.duration_ms }}
      note={note}
      nowPlaying={nowPlayingOf(source.id, currentSourceId, playerSourceId, playerStatus)}
      disabled={submitting}
      onPlay={() => {
        if (source.file_path !== null) onPlay(source.id, source.file_path);
      }}
    />
  );

  return (
    <div className="modal-overlay" onClick={close}>
      <div className="modal-card" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 640 }}>
        <div className="modal-header">
          <h2>{texts.title}</h2>
          <button className="modal-close-btn" onClick={close} type="button">×</button>
        </div>
        <div className="modal-section">
          <p className="subtle-text">
            <strong title={subject.filePath}>{subject.filePath}</strong> {texts.blurb(subject)}
          </p>
          {comparing ? (
            <div className="comparison-grid">
              {corruptSource ? side("Corrupt", corruptSource, subject.message) : <p className="issue-empty">The corrupt source is no longer in the library.</p>}
              {targetSource ? side("Replacement", targetSource, null) : <p className="issue-empty">The chosen replacement is no longer in the library.</p>}
            </div>
          ) : (
            <>
              <input
                className="relink-search"
                type="search"
                placeholder="Search title, artist, album or path…"
                value={state.query}
                autoFocus
                disabled={submitting}
                onChange={(e) => dispatch({ type: "setQuery", query: e.target.value })}
              />
              {state.status === "loading" ? (
                <p className="issue-empty">Searching…</p>
              ) : state.candidates.length === 0 ? (
                state.status === "ready" && state.error !== null ? null : (
                  <p className="issue-empty">No matching files found.</p>
                )
              ) : (
                <ul className="relink-candidate-list">
                  {state.candidates.map((c) => (
                    <li key={c.source_id}>
                      <button
                        type="button"
                        className={`relink-candidate${state.selectedSourceId === c.source_id ? " relink-candidate-selected" : ""}`}
                        disabled={submitting}
                        onClick={() => dispatch({ type: "select", sourceId: c.source_id })}
                      >
                        <span className="relink-candidate-title">
                          <span>
                            {c.title}
                            {c.artist ? ` — ${c.artist}` : ""}
                            {c.album ? ` (${c.album})` : ""}
                          </span>
                          <span>{c.duration_ms ? formatDuration(c.duration_ms) : null}</span>
                        </span>
                        {c.same_recording && (
                          <span className="relink-candidate-same">Same recording</span>
                        )}
                        <span className="relink-candidate-path" title={c.file_path}>{c.file_path}</span>
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </>
          )}
          {state.status === "ready" && state.error !== null && <div className="error-banner">{state.error.message}</div>}
        </div>
        <div className="comparison-modal-footer">
          {comparing && (
            <button className="comparison-back-btn" disabled={submitting} onClick={() => dispatch({ type: "back" })} type="button">Back</button>
          )}
          <button className="comparison-cancel-btn" onClick={close} type="button">Cancel</button>
          {state.error?.kind === "trash_failed" && (
            <button
              className="comparison-danger-btn"
              disabled={!canSubmitRelink(state)}
              title="Permanently delete the corrupt file instead of moving it to the trash. This can't be undone."
              onClick={() => onConfirm("delete")}
              type="button"
            >
              Delete permanently instead
            </button>
          )}
          {kind === "replace_corrupt" && !comparing ? (
            <button
              className="comparison-confirm-btn"
              disabled={!canCompareRelink(state)}
              onClick={() => dispatch({ type: "compare" })}
              type="button"
            >
              Compare…
            </button>
          ) : (
            <button className="comparison-confirm-btn" disabled={!canSubmitRelink(state)} onClick={() => onConfirm("trash")} type="button">
              {submitting ? texts.busyLabel : texts.confirmLabel}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
