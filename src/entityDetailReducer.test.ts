import { describe, expect, it } from "vitest";
import { detailReducer, type DetailState } from "./entityDetailReducer";
import type { ArtistDetail, RecordingDetail } from "./entityTypes";

function artist(id: string): ArtistDetail {
  return {
    id,
    name: `Artist ${id}`,
    sort_name: `Artist ${id}`,
    mbid: null,
    rating: null,
    last_played: null,
    recording_count: 0,
    release_group_count: 0,
    release_groups: [],
    guest_appearances: [],
  };
}

function recording(id: string): RecordingDetail {
  return {
    id,
    title: `Recording ${id}`,
    duration_ms: null,
    genre: null,
    bpm: null,
    comment: null,
    artist_credit_name: null,
    primary_artist_id: null,
    artist_credit_text: null,
    mbid: null,
    acoustid: null,
    rating: null,
    play_count: 0,
    last_played: null,
    artists: [],
    releases: [],
    sources: [],
  };
}

const loading = (): DetailState => ({ phase: "loading", requestId: 0 });

describe("detailReducer", () => {
  it("drops a stale loadOk for a nav that was navigated away from before the fetch resolved", () => {
    let state = loading();
    state = detailReducer(state, { type: "load", requestId: 1 });
    state = detailReducer(state, { type: "load", requestId: 2 });

    // The artist fetch for A resolves after the user has already navigated to recording B.
    state = detailReducer(state, { type: "loadOk", requestId: 1, entity: { type: "artist", data: artist("A") } });
    expect(state).toEqual({ phase: "loading", requestId: 2 });

    // B's own fetch resolving still applies normally, and only recording is populated.
    state = detailReducer(state, { type: "loadOk", requestId: 2, entity: { type: "recording", data: recording("B") } });
    expect(state.phase).toBe("ready");
    expect(state.phase === "ready" && state.entity).toEqual({ type: "recording", data: recording("B") });
  });

  it("drops a stale loadErr the same way", () => {
    let state = loading();
    state = detailReducer(state, { type: "load", requestId: 1 });
    state = detailReducer(state, { type: "load", requestId: 2 });
    state = detailReducer(state, { type: "loadErr", requestId: 1, error: "boom" });
    expect(state).toEqual({ phase: "loading", requestId: 2 });
  });

  it("a fresh load for the same nav (e.g. a refresh after a tag edit) still supersedes the prior request", () => {
    let state = loading();
    state = detailReducer(state, { type: "load", requestId: 1 });
    state = detailReducer(state, { type: "loadOk", requestId: 1, entity: { type: "recording", data: recording("A") } });

    // Refresh in place after e.g. a tag write.
    state = detailReducer(state, { type: "load", requestId: 2 });
    expect(state.phase).toBe("loading");

    // A stale response from the first load must not resurrect old data.
    state = detailReducer(state, { type: "loadOk", requestId: 1, entity: { type: "recording", data: recording("stale") } });
    expect(state.phase).toBe("loading");

    state = detailReducer(state, { type: "loadOk", requestId: 2, entity: { type: "recording", data: recording("fresh") } });
    expect(state.phase === "ready" && state.entity.type === "recording" && state.entity.data.id).toBe("fresh");
  });
});
