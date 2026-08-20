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
