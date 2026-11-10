import { describe, expect, it } from "vitest";
import {
  artistFixReducer,
  mergeModalReducer,
  canCompareRelink,
  canSubmitRelink,
  relinkModalReducer,
  splitModalReducer,
  type ArtistFixState,
  type CompoundArtistCheck,
  type MergeModalState,
  type RelinkCandidate,
  type RelinkModalState,
  type RelinkSubject,
  type RelinkKind,
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

// ── relinkModalReducer ──────────────────────────────────────────────────────

const subject: RelinkSubject = { sourceId: "dead", filePath: "/gone.mp3", message: "File not found" };

function candidate(sourceId: string): RelinkCandidate {
  return {
    source_id: sourceId,
    file_path: `/${sourceId}.mp3`,
    recording_id: `rec-${sourceId}`,
    title: sourceId,
    artist: null,
    album: null,
    duration_ms: null,
    rating: null,
    same_recording: false,
  };
}

function readyWith(ids: string[], selected: string | null, kind: RelinkKind = "relink_missing"): RelinkModalState {
  const opened = relinkModalReducer({ phase: "closed" }, { type: "open", kind, subject, requestId: 1 });
  const ready = relinkModalReducer(opened, { type: "searchOk", requestId: 1, candidates: ids.map(candidate) });
  return selected === null ? ready : relinkModalReducer(ready, { type: "select", sourceId: selected });
}

describe("relinkModalReducer", () => {
  it("shows the results of the latest search", () => {
    const state = relinkModalReducer(readyWith(["a"], null), { type: "search", query: "x", requestId: 2 });
    const done = relinkModalReducer(state, { type: "searchOk", requestId: 2, candidates: [candidate("b")] });
    expect(done).toMatchObject({ phase: "open", status: "ready", query: "x", candidates: [{ source_id: "b" }] });
  });

  it("drops the response of a superseded search", () => {
    const first = relinkModalReducer(readyWith(["a"], null), { type: "search", query: "x", requestId: 2 });
    const second = relinkModalReducer(first, { type: "search", query: "xy", requestId: 3 });
    const afterStale = relinkModalReducer(second, { type: "searchOk", requestId: 2, candidates: [candidate("stale")] });
    expect(afterStale).toBe(second);
  });

  it("drops a search response that arrives after the modal was closed", () => {
    const opened = relinkModalReducer({ phase: "closed" }, { type: "open", kind: "relink_missing", subject, requestId: 1 });
    const closed = relinkModalReducer(opened, { type: "close" });
    expect(relinkModalReducer(closed, { type: "searchOk", requestId: 1, candidates: [candidate("a")] })).toEqual({ phase: "closed" });
  });

  it("keeps the selection across a search only while it is still listed", () => {
    const searching = relinkModalReducer(readyWith(["a", "b"], "a"), { type: "search", query: "q", requestId: 2 });
    const kept = relinkModalReducer(searching, { type: "searchOk", requestId: 2, candidates: [candidate("a")] });
    expect(kept).toMatchObject({ phase: "open", status: "ready", selectedSourceId: "a" });

    const searchingAgain = relinkModalReducer(kept, { type: "search", query: "r", requestId: 3 });
    const lost = relinkModalReducer(searchingAgain, { type: "searchOk", requestId: 3, candidates: [candidate("z")] });
    expect(lost).toMatchObject({ phase: "open", status: "ready", selectedSourceId: null });
  });

  it("cannot submit without a selection", () => {
    const state = readyWith(["a"], null);
    expect(relinkModalReducer(state, { type: "submit" })).toBe(state);
  });

  it("cannot select a source that is not listed", () => {
    const state = readyWith(["a"], null);
    expect(relinkModalReducer(state, { type: "select", sourceId: "nope" })).toBe(state);
  });

  it("closes when the submission succeeds", () => {
    const submitting = relinkModalReducer(readyWith(["a"], "a"), { type: "submit" });
    expect(submitting).toMatchObject({ phase: "open", status: "submitting" });
    expect(relinkModalReducer(submitting, { type: "submitOk", requestId: 1 })).toEqual({ phase: "closed" });
  });

  it("returns to the results with the error when the submission fails", () => {
    const submitting = relinkModalReducer(readyWith(["a"], "a"), { type: "submit" });
    const failed = relinkModalReducer(submitting, { type: "submitErr", requestId: 1, error: "boom" });
    expect(failed).toMatchObject({ phase: "open", status: "ready", error: { kind: "plain", message: "boom" }, selectedSourceId: "a" });
  });

  it("ignores searches while submitting", () => {
    const submitting = relinkModalReducer(readyWith(["a"], "a"), { type: "submit" });
    expect(relinkModalReducer(submitting, { type: "search", query: "x", requestId: 9 })).toBe(submitting);
  });

  it("keeps results and selection while the query is edited, and records the issued query on search", () => {
    const typed = relinkModalReducer(readyWith(["a"], "a"), { type: "setQuery", query: "abc" });
    expect(typed).toMatchObject({ query: "abc", issuedQuery: "", status: "ready", selectedSourceId: "a", candidates: [{ source_id: "a" }] });
    const searching = relinkModalReducer(typed, { type: "search", query: "abc", requestId: 2 });
    expect(searching).toMatchObject({ query: "abc", issuedQuery: "abc" });
  });

  it("ignores query edits and close while submitting", () => {
    const submitting = relinkModalReducer(readyWith(["a"], "a"), { type: "submit" });
    expect(relinkModalReducer(submitting, { type: "setQuery", query: "x" })).toBe(submitting);
    expect(relinkModalReducer(submitting, { type: "close" })).toBe(submitting);
  });
});

// ── relinkModalReducer: replace_corrupt ─────────────────────────────────────

const corruptReady = (selected: string | null) => readyWith(["a", "b"], selected, "replace_corrupt");

/** A replace-corrupt modal with "a" selected and the comparison showing. */
function comparedWithA(): RelinkModalState {
  return relinkModalReducer(corruptReady("a"), { type: "compare" });
}

describe("relinkModalReducer (replace corrupt)", () => {
  it("cannot be confirmed straight from the pick step", () => {
    const state = corruptReady("a");
    expect(canSubmitRelink(state)).toBe(false);
    expect(relinkModalReducer(state, { type: "submit" })).toBe(state);
  });

  it("can be confirmed after comparing the selected candidate", () => {
    const compared = comparedWithA();
    expect(canSubmitRelink(compared)).toBe(true);
    expect(relinkModalReducer(compared, { type: "submit" })).toMatchObject({ status: "submitting", selectedSourceId: "a" });
  });

  it("offers a comparison only with a selection, and only while picking", () => {
    expect(canCompareRelink(corruptReady("a"))).toBe(true);
    expect(canCompareRelink(comparedWithA())).toBe(false);
  });

  it("does not compare without a selection", () => {
    const none = corruptReady(null);
    expect(canCompareRelink(none)).toBe(false);
    expect(relinkModalReducer(none, { type: "compare" })).toBe(none);
  });

  it("returns to picking with the selection intact", () => {
    expect(relinkModalReducer(comparedWithA(), { type: "back" })).toMatchObject({ selectedSourceId: "a", status: "ready" });
    expect(canSubmitRelink(relinkModalReducer(comparedWithA(), { type: "back" }))).toBe(false);
  });

  it("ignores selection changes and searches while comparing", () => {
    const compared = comparedWithA();
    expect(relinkModalReducer(compared, { type: "select", sourceId: "b" })).toBe(compared);
    expect(relinkModalReducer(compared, { type: "setQuery", query: "zzz" })).toBe(compared);
    expect(relinkModalReducer(compared, { type: "search", query: "zzz", requestId: 9 })).toBe(compared);
  });

  it("never offers a comparison for a relink of a missing file", () => {
    const state = readyWith(["a"], "a");
    expect(relinkModalReducer(state, { type: "compare" })).toBe(state);
  });

  it("a missing file can be relinked straight from the pick step", () => {
    expect(canSubmitRelink(readyWith(["a"], "a"))).toBe(true);
    expect(canSubmitRelink(readyWith(["a"], null))).toBe(false);
    expect(canSubmitRelink({ phase: "closed" })).toBe(false);
  });

  it("closes after a successful replacement", () => {
    const submitting = relinkModalReducer(comparedWithA(), { type: "submit" });
    expect(relinkModalReducer(submitting, { type: "submitOk", requestId: 1 })).toEqual({ phase: "closed" });
  });

  it("reports a trash failure as a distinct error that leaves the selection intact", () => {
    const submitting = relinkModalReducer(comparedWithA(), { type: "submit" });
    const failed = relinkModalReducer(submitting, { type: "trashFailed", requestId: 1, message: "no trash" });
    expect(failed).toMatchObject({
      phase: "open",
      status: "ready",
      selectedSourceId: "a",
      step: "compare",
      error: { kind: "trash_failed", message: "no trash" },
    });
    expect(canSubmitRelink(failed)).toBe(true);
  });

  it("ignores a trash failure for a stale request or when not submitting", () => {
    const submitting = relinkModalReducer(comparedWithA(), { type: "submit" });
    expect(relinkModalReducer(submitting, { type: "trashFailed", requestId: 99, message: "x" })).toBe(submitting);
    const compared = comparedWithA();
    expect(relinkModalReducer(compared, { type: "trashFailed", requestId: 1, message: "x" })).toBe(compared);
  });

  it("forgets a trash failure when going back or submitting again", () => {
    const submitting = relinkModalReducer(comparedWithA(), { type: "submit" });
    const failed = relinkModalReducer(submitting, { type: "trashFailed", requestId: 1, message: "no trash" });
    expect(relinkModalReducer(failed, { type: "back" })).toMatchObject({ error: null });
    expect(relinkModalReducer(failed, { type: "submit" })).toMatchObject({ status: "submitting", error: null });
  });
});
