// Pure reducers for the merge, split-recording, and artist-fix modals.
//
// Each had the same bug: the "which entity is this modal open for" state and
// the "result of the in-flight async call" state were separate useState
// calls. Closing and reopening the modal for a different entity while a
// request was still in flight let the stale response land under the new
// entity. A requestId tied to the open/action that started the request makes
// a stale response unrepresentable — the reducer just drops it.

import type { SourceTagInfo } from "./entityTypes";

// ── Merge modal (duplicate-tag resolution) ──────────────────────────────────

export type MergeConflict = {
  frame_id: string;
  field_name: string;
  values: string[];
};

export type MergeModalState =
  | { phase: "closed" }
  | { phase: "loading"; filePath: string; requestId: number }
  | { phase: "ready"; filePath: string; requestId: number; conflicts: MergeConflict[]; choices: Record<string, string>; error: string | null }
  | { phase: "submitting"; filePath: string; requestId: number; conflicts: MergeConflict[]; choices: Record<string, string> };

export type MergeModalAction =
  | { type: "open"; filePath: string; requestId: number }
  | { type: "previewOk"; requestId: number; conflicts: MergeConflict[] }
  | { type: "previewErr"; requestId: number; error: string }
  | { type: "choose"; frameId: string; value: string }
  | { type: "submit" }
  | { type: "submitOk"; requestId: number }
  | { type: "submitErr"; requestId: number; error: string }
  | { type: "close" };

export function mergeModalReducer(state: MergeModalState, action: MergeModalAction): MergeModalState {
  switch (action.type) {
    case "open":
      return { phase: "loading", filePath: action.filePath, requestId: action.requestId };
    case "previewOk":
      if (state.phase === "loading" && state.requestId === action.requestId) {
        const choices: Record<string, string> = {};
        for (const c of action.conflicts) choices[c.frame_id] = c.values[0];
        return { phase: "ready", filePath: state.filePath, requestId: state.requestId, conflicts: action.conflicts, choices, error: null };
      }
      return state;
    case "previewErr":
      if (state.phase === "loading" && state.requestId === action.requestId) {
        return { phase: "ready", filePath: state.filePath, requestId: state.requestId, conflicts: [], choices: {}, error: action.error };
      }
      return state;
    case "choose":
      if (state.phase === "ready") {
        return { ...state, choices: { ...state.choices, [action.frameId]: action.value } };
      }
      return state;
    case "submit":
      if (state.phase === "ready") {
        return { phase: "submitting", filePath: state.filePath, requestId: state.requestId, conflicts: state.conflicts, choices: state.choices };
      }
      return state;
    case "submitOk":
      // Only close if we're still submitting the same file; a stale completion
      // must not close a modal that has since moved to another file.
      if (state.phase === "submitting" && state.requestId === action.requestId) {
        return { phase: "closed" };
      }
      return state;
    case "submitErr":
      if (state.phase === "submitting" && state.requestId === action.requestId) {
        return { phase: "ready", filePath: state.filePath, requestId: state.requestId, conflicts: state.conflicts, choices: state.choices, error: action.error };
      }
      return state;
    case "close":
      return { phase: "closed" };
  }
}

// ── Artist-fix modal (compound-artist detection) ────────────────────────────

export type CompoundArtistCheck = {
  is_compound: boolean;
  evidence_count: number;
  total_sources_checked: number;
  individual_artist_names: string[];
  source_examples: string[];
};

export type ArtistFixOutcome =
  | { kind: "ok"; result: CompoundArtistCheck }
  | { kind: "err"; error: string };

export type ArtistFixState =
  | { phase: "closed" }
  | { phase: "idle"; artistId: string; artistName: string }
  | { phase: "checking"; artistId: string; artistName: string; requestId: number }
  | { phase: "done"; artistId: string; artistName: string; outcome: ArtistFixOutcome };

export type ArtistFixAction =
  | { type: "open"; artistId: string; artistName: string }
  | { type: "runChecks"; requestId: number }
  | { type: "checkOk"; requestId: number; result: CompoundArtistCheck }
  | { type: "checkErr"; requestId: number; error: string }
  | { type: "close" };

export function artistFixReducer(state: ArtistFixState, action: ArtistFixAction): ArtistFixState {
  switch (action.type) {
    case "open":
      return { phase: "idle", artistId: action.artistId, artistName: action.artistName };
    case "runChecks":
      if (state.phase === "idle" || state.phase === "done") {
        return { phase: "checking", artistId: state.artistId, artistName: state.artistName, requestId: action.requestId };
      }
      return state;
    case "checkOk":
      if (state.phase === "checking" && state.requestId === action.requestId) {
        return { phase: "done", artistId: state.artistId, artistName: state.artistName, outcome: { kind: "ok", result: action.result } };
      }
      return state;
    case "checkErr":
      if (state.phase === "checking" && state.requestId === action.requestId) {
        return { phase: "done", artistId: state.artistId, artistName: state.artistName, outcome: { kind: "err", error: action.error } };
      }
      return state;
    case "close":
      return { phase: "closed" };
  }
}

// ── Split-recording modal ───────────────────────────────────────────────────

export type SplitSourceDetail = {
  id: string;
  source_type: string;
  file_path: string | null;
  duration_ms: number | null;
  tags: SourceTagInfo[];
};

export type SplitRecordingDetail = {
  id: string;
  title: string;
  artist_credit_name: string | null;
  primary_artist_id: string | null;
  genre: string | null;
  bpm: number | null;
  sources: SplitSourceDetail[];
};

export type SplitModalState =
  | { phase: "closed" }
  | { phase: "loading"; requestId: number }
  | { phase: "loadError"; error: string }
  | { phase: "ready"; requestId: number; data: SplitRecordingDetail; selectedSourceIds: Set<string>; error: string | null }
  | { phase: "submitting"; requestId: number; data: SplitRecordingDetail; selectedSourceIds: Set<string> };

export type SplitModalAction =
  | { type: "open"; requestId: number }
  | { type: "loadOk"; requestId: number; data: SplitRecordingDetail }
  | { type: "loadErr"; requestId: number; error: string }
  | { type: "toggleSource"; sourceId: string }
  | { type: "submit" }
  | { type: "submitOk"; requestId: number }
  | { type: "submitErr"; requestId: number; error: string }
  | { type: "close" };

export function splitModalReducer(state: SplitModalState, action: SplitModalAction): SplitModalState {
  switch (action.type) {
    case "open":
      return { phase: "loading", requestId: action.requestId };
    case "loadOk":
      if (state.phase === "loading" && state.requestId === action.requestId) {
        // Pre-select all sources except the last one as a reasonable default.
        const ids = action.data.sources.slice(0, -1).map((s) => s.id);
        return { phase: "ready", requestId: state.requestId, data: action.data, selectedSourceIds: new Set(ids), error: null };
      }
      return state;
    case "loadErr":
      if (state.phase === "loading" && state.requestId === action.requestId) {
        return { phase: "loadError", error: action.error };
      }
      return state;
    case "toggleSource":
      if (state.phase === "ready") {
        const next = new Set(state.selectedSourceIds);
        next.has(action.sourceId) ? next.delete(action.sourceId) : next.add(action.sourceId);
        return { ...state, selectedSourceIds: next };
      }
      return state;
    case "submit":
      if (state.phase === "ready") {
        return { phase: "submitting", requestId: state.requestId, data: state.data, selectedSourceIds: state.selectedSourceIds };
      }
      return state;
    case "submitOk":
      if (state.phase === "submitting" && state.requestId === action.requestId) {
        return { phase: "closed" };
      }
      return state;
    case "submitErr":
      if (state.phase === "submitting" && state.requestId === action.requestId) {
        return { phase: "ready", requestId: state.requestId, data: state.data, selectedSourceIds: state.selectedSourceIds, error: action.error };
      }
      return state;
    case "close":
      return { phase: "closed" };
  }
}

// ── Relink-missing / replace-corrupt source modal ───────────────────────────

/** How the corrupt file is disposed of when replaced; mirrors Rust `FileDisposal`. */
export type FileDisposal = "trash" | "delete";

/** Result of `replace_corrupt_source`; mirrors Rust `ReplaceCorruptOutcome`. */
export type ReplaceCorruptOutcome = { outcome: "replaced" } | { outcome: "trash_failed"; message: string };

/** What is wrong with the source: its file is gone (relink), or its audio is damaged (replace). */
export type RelinkKind = "relink_missing" | "replace_corrupt";

/** The source whose data is moved onto another source. */
export type RelinkSubject = {
  sourceId: string;
  filePath: string;
  /** The issue message, e.g. "File not found (3 plays, rated 4★)". */
  message: string;
};

/** A live library source the missing source's data can be moved onto. */
export type RelinkCandidate = {
  source_id: string;
  file_path: string;
  recording_id: string;
  title: string;
  artist: string | null;
  album: string | null;
  duration_ms: number | null;
  rating: number | null;
  same_recording: boolean;
};

/**
 * Where the flow is: picking a replacement, or comparing the subject with the selected one
 * before confirming. Relink-missing flows are always in "pick".
 */
export type RelinkStep = "pick" | "compare";

/**
 * Why the last submission or search failed. `trash_failed` means the corrupt file could not be
 * moved to the trash and nothing was changed, so a permanent delete may be offered.
 */
export type RelinkError = { kind: "plain"; message: string } | { kind: "trash_failed"; message: string };

export type RelinkModalState =
  | { phase: "closed" }
  | {
      phase: "open";
      kind: RelinkKind;
      subject: RelinkSubject;
      /** What the user has typed in the search box. */
      query: string;
      /** The query of the most recently issued search; differs from `query` while typing. */
      issuedQuery: string;
      requestId: number;
      step: RelinkStep;
      candidates: RelinkCandidate[];
      selectedSourceId: string | null;
      status: "loading" | "ready" | "submitting";
      error: RelinkError | null;
    };

export type RelinkModalAction =
  | { type: "open"; kind: RelinkKind; subject: RelinkSubject; requestId: number }
  | { type: "setQuery"; query: string }
  | { type: "search"; query: string; requestId: number }
  | { type: "searchOk"; requestId: number; candidates: RelinkCandidate[] }
  | { type: "searchErr"; requestId: number; error: string }
  | { type: "select"; sourceId: string }
  | { type: "compare" }
  | { type: "back" }
  | { type: "submit" }
  | { type: "submitOk"; requestId: number }
  | { type: "submitErr"; requestId: number; error: string }
  | { type: "trashFailed"; requestId: number; message: string }
  | { type: "close" };

/**
 * Whether the modal's confirm action may run: a selection is made and nothing is in flight. A
 * corrupt file is only replaced from the comparison step; a missing file is relinked straight
 * from the list.
 */
/** Whether a replace-corrupt modal can move from picking a replacement to comparing it. */
export function canCompareRelink(state: RelinkModalState): boolean {
  return (
    state.phase === "open" &&
    state.kind === "replace_corrupt" &&
    state.step === "pick" &&
    state.status === "ready" &&
    state.selectedSourceId !== null
  );
}

export function canSubmitRelink(state: RelinkModalState): boolean {
  return (
    state.phase === "open" &&
    state.status === "ready" &&
    state.selectedSourceId !== null &&
    (state.kind === "relink_missing" || state.step === "compare")
  );
}

export function relinkModalReducer(state: RelinkModalState, action: RelinkModalAction): RelinkModalState {
  if (action.type === "open") {
    return {
      phase: "open",
      kind: action.kind,
      subject: action.subject,
      query: "",
      issuedQuery: "",
      requestId: action.requestId,
      step: "pick",
      candidates: [],
      selectedSourceId: null,
      status: "loading",
      error: null,
    };
  }
  if (state.phase === "closed") return state;
  // The search box and the candidate list are only live while picking.
  const picking = state.step === "pick";
  switch (action.type) {
    case "setQuery":
      return state.status === "submitting" || !picking ? state : { ...state, query: action.query };
    case "search":
      return state.status === "submitting" || !picking
        ? state
        : { ...state, query: action.query, issuedQuery: action.query, requestId: action.requestId, status: "loading", error: null };
    case "searchOk":
      if (state.status === "loading" && state.requestId === action.requestId) {
        // Keep the selection only if the chosen source is still among the results.
        const selectedSourceId = action.candidates.some((c) => c.source_id === state.selectedSourceId)
          ? state.selectedSourceId
          : null;
        return { ...state, status: "ready", candidates: action.candidates, selectedSourceId, error: null };
      }
      return state;
    case "searchErr":
      if (state.status === "loading" && state.requestId === action.requestId) {
        return { ...state, status: "ready", candidates: [], selectedSourceId: null, error: { kind: "plain", message: action.error } };
      }
      return state;
    case "select":
      if (picking && state.status === "ready" && state.candidates.some((c) => c.source_id === action.sourceId)) {
        return { ...state, selectedSourceId: action.sourceId, error: null };
      }
      return state;
    case "compare":
      return canCompareRelink(state)
        ? { ...state, step: "compare", error: null }
        : state;
    case "back":
      return state.status === "ready" && !picking ? { ...state, step: "pick", error: null } : state;
    case "submit":
      return canSubmitRelink(state) ? { ...state, status: "submitting", error: null } : state;
    case "submitOk":
      return state.status === "submitting" && state.requestId === action.requestId ? { phase: "closed" } : state;
    case "submitErr":
      if (state.status === "submitting" && state.requestId === action.requestId) {
        return { ...state, status: "ready", error: { kind: "plain", message: action.error } };
      }
      return state;
    case "trashFailed":
      if (state.status === "submitting" && state.requestId === action.requestId) {
        return { ...state, status: "ready", error: { kind: "trash_failed", message: action.message } };
      }
      return state;
    case "close":
      // An in-flight write can't be cancelled.
      return state.status === "submitting" ? state : { phase: "closed" };
  }
}
