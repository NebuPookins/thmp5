import { describe, expect, it } from "vitest";
import {
  artistFixReducer,
  mergeModalReducer,
  splitModalReducer,
  type ArtistFixState,
  type CompoundArtistCheck,
  type MergeModalState,
  type SplitModalState,
  type SplitRecordingDetail,
} from "./modalReducers";

function detail(id: string, sourceIds: string[]): SplitRecordingDetail {
  return {
    id,
    title: `Recording ${id}`,
    artist_credit_name: null,
    primary_artist_id: null,
    genre: null,
    bpm: null,
    sources: sourceIds.map((sourceId) => ({
      id: sourceId,
      source_type: "local",
      file_path: null,
      duration_ms: null,
      tags: [],
    })),
  };
}

function check(isCompound: boolean): CompoundArtistCheck {
  return {
    is_compound: isCompound,
    evidence_count: 0,
    total_sources_checked: 0,
    individual_artist_names: [],
    source_examples: [],
  };
}

describe("splitModalReducer", () => {
  it("drops a stale loadOk for a recording that was reopened before the first fetch resolved", () => {
    let state: SplitModalState = { phase: "closed" };
    state = splitModalReducer(state, { type: "open", requestId: 1 });
    state = splitModalReducer(state, { type: "open", requestId: 2 });

    // The fetch started for A resolves after B has already been opened.
    state = splitModalReducer(state, { type: "loadOk", requestId: 1, data: detail("A", ["s1", "s2"]) });
    expect(state).toEqual({ phase: "loading", requestId: 2 });

    // B's own fetch resolving still applies normally.
    state = splitModalReducer(state, { type: "loadOk", requestId: 2, data: detail("B", ["s3", "s4"]) });
    expect(state.phase).toBe("ready");
    expect(state.phase === "ready" && state.data.id).toBe("B");
  });

  it("drops a stale loadErr the same way", () => {
    let state: SplitModalState = { phase: "closed" };
    state = splitModalReducer(state, { type: "open", requestId: 1 });
    state = splitModalReducer(state, { type: "open", requestId: 2 });
    state = splitModalReducer(state, { type: "loadErr", requestId: 1, error: "boom" });
    expect(state).toEqual({ phase: "loading", requestId: 2 });
  });

  it("pre-selects all sources except the last one on load", () => {
    let state: SplitModalState = { phase: "closed" };
    state = splitModalReducer(state, { type: "open", requestId: 1 });
    state = splitModalReducer(state, { type: "loadOk", requestId: 1, data: detail("A", ["s1", "s2", "s3"]) });
    expect(state.phase === "ready" && [...state.selectedSourceIds]).toEqual(["s1", "s2"]);
  });

  it("drops a stale submitOk/submitErr from a superseded submission", () => {
    let state: SplitModalState = { phase: "closed" };
    state = splitModalReducer(state, { type: "open", requestId: 1 });
    state = splitModalReducer(state, { type: "loadOk", requestId: 1, data: detail("A", ["s1", "s2"]) });
    state = splitModalReducer(state, { type: "submit" });
    expect(state.phase).toBe("submitting");

    // A completely unrelated requestId (e.g. from a since-closed-and-reopened
    // modal) must not close or mutate this submission.
    state = splitModalReducer(state, { type: "submitOk", requestId: 99 });
    expect(state.phase).toBe("submitting");

    state = splitModalReducer(state, { type: "submitOk", requestId: 1 });
    expect(state).toEqual({ phase: "closed" });
  });
});

describe("artistFixReducer", () => {
  it("drops a stale checkOk after the modal was closed and reopened for a different artist", () => {
    let state: ArtistFixState = { phase: "closed" };
    state = artistFixReducer(state, { type: "open", artistId: "artist-A", artistName: "A" });
    state = artistFixReducer(state, { type: "runChecks", requestId: 1 });
    expect(state.phase).toBe("checking");

    // User closes the modal before the check for A returns, then opens it
    // for a different artist B.
    state = artistFixReducer(state, { type: "close" });
    state = artistFixReducer(state, { type: "open", artistId: "artist-B", artistName: "B" });
    expect(state).toEqual({ phase: "idle", artistId: "artist-B", artistName: "B" });

    // A's check resolves late; it must not appear under B's modal.
    state = artistFixReducer(state, { type: "checkOk", requestId: 1, result: check(true) });
    expect(state).toEqual({ phase: "idle", artistId: "artist-B", artistName: "B" });
  });

  it("drops a stale checkOk from a superseded run without closing in between", () => {
    let state: ArtistFixState = { phase: "closed" };
    state = artistFixReducer(state, { type: "open", artistId: "artist-A", artistName: "A" });
    state = artistFixReducer(state, { type: "runChecks", requestId: 1 });
    // (Re-running checks for the same artist bumps the requestId.)
    state = artistFixReducer(state, { type: "checkErr", requestId: 1, error: "boom" });
    state = artistFixReducer(state, { type: "runChecks", requestId: 2 });

    state = artistFixReducer(state, { type: "checkOk", requestId: 1, result: check(true) });
    expect(state.phase).toBe("checking");
    expect(state.phase === "checking" && state.requestId).toBe(2);

    state = artistFixReducer(state, { type: "checkOk", requestId: 2, result: check(false) });
    expect(state).toEqual({
      phase: "done",
      artistId: "artist-A",
      artistName: "A",
      outcome: { kind: "ok", result: check(false) },
    });
  });

  it("an error outcome is distinct from an ok outcome", () => {
    let state: ArtistFixState = { phase: "closed" };
    state = artistFixReducer(state, { type: "open", artistId: "artist-A", artistName: "A" });
    state = artistFixReducer(state, { type: "runChecks", requestId: 1 });
    state = artistFixReducer(state, { type: "checkErr", requestId: 1, error: "boom" });
    expect(state).toEqual({
      phase: "done",
      artistId: "artist-A",
      artistName: "A",
      outcome: { kind: "err", error: "boom" },
    });
  });
});

describe("mergeModalReducer", () => {
  it("drops a stale preview for a file that was reopened before the preview resolved", () => {
    let state: MergeModalState = { phase: "closed" };
    state = mergeModalReducer(state, { type: "open", filePath: "a.mp3", requestId: 1 });
    state = mergeModalReducer(state, { type: "open", filePath: "b.mp3", requestId: 2 });

    state = mergeModalReducer(state, { type: "previewOk", requestId: 1, conflicts: [] });
    expect(state).toEqual({ phase: "loading", filePath: "b.mp3", requestId: 2 });

    state = mergeModalReducer(state, { type: "previewOk", requestId: 2, conflicts: [] });
    expect(state.phase).toBe("ready");
    expect(state.phase === "ready" && state.filePath).toBe("b.mp3");
  });

  it("defaults each conflict to its first value", () => {
    let state: MergeModalState = { phase: "closed" };
    state = mergeModalReducer(state, { type: "open", filePath: "a.mp3", requestId: 1 });
    state = mergeModalReducer(state, {
      type: "previewOk",
      requestId: 1,
      conflicts: [{ frame_id: "TIT2", field_name: "Title", values: ["One", "Two"] }],
    });
    expect(state.phase === "ready" && state.choices).toEqual({ TIT2: "One" });
  });
});
