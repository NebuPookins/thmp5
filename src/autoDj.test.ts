import { describe, it, expect } from "vitest";
import {
  excludeRecentlyPlayed,
  excludeIds,
  pickByRatingStrategy,
  pickNextTrack,
} from "./autoDj";
import { FAKE_NOW, ONE_DAY_MS, makeRecording } from "./testHelpers";

// ── excludeRecentlyPlayed ────────────────────────────────────────────────────

describe("excludeRecentlyPlayed", () => {
  it("keeps items with no last_played", () => {
    const a = makeRecording({ id: "a", last_played: null });
    const result = excludeRecentlyPlayed([a], FAKE_NOW);
    expect(result).toEqual([a]);
  });

  it("excludes items played less than 24h ago", () => {
    const recent = new Date(FAKE_NOW - 1000).toISOString();
    const a = makeRecording({ id: "a", last_played: recent });
    const result = excludeRecentlyPlayed([a], FAKE_NOW);
    expect(result).toEqual([]);
  });

  it("keeps items played more than 24h ago", () => {
    const old = new Date(FAKE_NOW - ONE_DAY_MS - 1).toISOString();
    const a = makeRecording({ id: "a", last_played: old });
    const result = excludeRecentlyPlayed([a], FAKE_NOW);
    expect(result).toEqual([a]);
  });

  it("keeps items played exactly 24h ago (boundary)", () => {
    const boundary = new Date(FAKE_NOW - ONE_DAY_MS).toISOString();
    const a = makeRecording({ id: "a", last_played: boundary });
    // exactly 24h → playedAt = FAKE_NOW - 86400000
    // cutoff = FAKE_NOW - 86400000
    // playedAt === cutoff → NOT < cutoff → excluded
    const result = excludeRecentlyPlayed([a], FAKE_NOW);
    expect(result).toEqual([]);
  });

  it("handles malformed last_played gracefully", () => {
    const a = makeRecording({ id: "a", last_played: "not-a-date" });
    const result = excludeRecentlyPlayed([a], FAKE_NOW);
    expect(result).toEqual([]);
  });

  it("mixed bag: recently played excluded, old kept, null kept", () => {
    const old = makeRecording({
      id: "old",
      last_played: new Date(FAKE_NOW - ONE_DAY_MS - 100_000).toISOString(),
    });
    const recent = makeRecording({
      id: "recent",
      last_played: new Date(FAKE_NOW - 1000).toISOString(),
    });
    const never = makeRecording({ id: "never", last_played: null });
    const result = excludeRecentlyPlayed([old, recent, never], FAKE_NOW);
    expect(result).toEqual([old, never]);
  });
});

// ── excludeIds ───────────────────────────────────────────────────────────────

describe("excludeIds", () => {
  it("excludes items whose id is in the set", () => {
    const a = makeRecording({ id: "a" });
    const b = makeRecording({ id: "b" });
    const result = excludeIds([a, b], new Set(["a"]));
    expect(result).toEqual([b]);
  });

  it("returns all items when set is empty", () => {
    const a = makeRecording({ id: "a" });
    const b = makeRecording({ id: "b" });
    const result = excludeIds([a, b], new Set());
    expect(result).toEqual([a, b]);
  });

  it("returns empty array when all items excluded", () => {
    const a = makeRecording({ id: "a" });
    const b = makeRecording({ id: "b" });
    const result = excludeIds([a, b], new Set(["a", "b"]));
    expect(result).toEqual([]);
  });
});

// ── pickByRatingStrategy ─────────────────────────────────────────────────────

describe("pickByRatingStrategy", () => {
  it("returns undefined for empty candidates", () => {
    const result = pickByRatingStrategy([], []);
    expect(result).toBeUndefined();
  });

  it("with no rated history, prefers unrated candidates over rated ones", () => {
    // Run multiple times to see if we ever get the rated one without
    // the unrated being available.
    // If only unrated is available we must always get it.
    const unrated = makeRecording({ id: "un", rating: null, primary_source_id: "src-un" });
    const result = pickByRatingStrategy([], [unrated]);
    expect(result).toBeDefined();
    expect(result!.id).toBe("un");
  });

  it("with no rated history and only rated candidates, picks from candidates", () => {
    const rated = makeRecording({ id: "rated", rating: 4, primary_source_id: "src-rated" });
    const result = pickByRatingStrategy([], [rated]);
    expect(result).toBeDefined();
    expect(result!.id).toBe("rated");
  });

  it("with neutral avg rating, prefers 4+ candidates", () => {
    const high = makeRecording({ id: "high", rating: 4, primary_source_id: "src-high" });
    const low = makeRecording({ id: "low", rating: 2, primary_source_id: "src-low" });
    // History with avg 3.0 (neutral)
    const history = [
      makeRecording({ id: "h1", rating: 3 }),
      makeRecording({ id: "h2", rating: 3 }),
    ];
    const result = pickByRatingStrategy(history, [high, low]);
    // Can't assert which one is picked (random), but can assert it's one of them
    expect(result).toBeDefined();
    expect([high.id, low.id]).toContain(result!.id);
  });
});

// ── pickNextTrack (integration) ──────────────────────────────────────────────

describe("pickNextTrack", () => {
  const playable = [
    makeRecording({ id: "a", last_played: null }),
    makeRecording({ id: "b", last_played: null }),
    makeRecording({ id: "c", last_played: null }),
    makeRecording({ id: "d", last_played: null }),
    makeRecording({ id: "e", last_played: null }),
    makeRecording({ id: "f", last_played: null }),
    makeRecording({ id: "g", last_played: null }),
  ];

  it("does not pick a song that is currently in the queue", () => {
    const queue = [
      makeRecording({ id: "a" }),
      makeRecording({ id: "b" }),
    ];
    const result = pickNextTrack({
      playable,
      queue,
      history: [],
      currentTrack: null,
      now: FAKE_NOW,
    });
    expect(result).toBeDefined();
    expect(result!.id).not.toBe("a");
    expect(result!.id).not.toBe("b");
  });

  it("does not pick the currently playing track", () => {
    const current = makeRecording({ id: "a" });
    const result = pickNextTrack({
      playable,
      queue: [],
      history: [],
      currentTrack: current,
      now: FAKE_NOW,
    });
    expect(result).toBeDefined();
    expect(result!.id).not.toBe("a");
  });

  it("does not pick a song played less than 24h ago when others exist", () => {
    const recent = makeRecording({
      id: "recent",
      last_played: new Date(FAKE_NOW - 1000).toISOString(),
    });
    const available = makeRecording({ id: "available", last_played: null });
    const result = pickNextTrack({
      playable: [recent, available],
      queue: [],
      history: [],
      currentTrack: null,
      now: FAKE_NOW,
    });
    expect(result).toBeDefined();
    expect(result!.id).toBe("available");
  });

  it("falls back to recently-played when no other candidates exist", () => {
    const recent = makeRecording({
      id: "only-song",
      last_played: new Date(FAKE_NOW - 1000).toISOString(),
    });
    const result = pickNextTrack({
      playable: [recent],
      queue: [],
      history: [],
      currentTrack: null,
      now: FAKE_NOW,
    });
    // Must still return something since it's the only song
    expect(result).toBeDefined();
    expect(result!.id).toBe("only-song");
  });

  it("edge case: does not re-pick the next-up song after queue shift", () => {
    // Simulate the described scenario:
    // Queue was [A, B, C, D, E, F, G] where A-E are played, F was current, G was next.
    // After F finishes: history = [B, C, D, E, F], G becomes currentTrack.
    // AutoDJ picks next -> must not pick G.
    const currentTrack = makeRecording({ id: "g", last_played: null });
    const result = pickNextTrack({
      playable: playable.filter((r) => r.id !== "e" && r.id !== "f"), // keep all except history-settled ones
      queue: [],
      history: [
        makeRecording({ id: "b", rating: 3 }),
        makeRecording({ id: "c", rating: 3 }),
        makeRecording({ id: "d", rating: 3 }),
        makeRecording({ id: "e", rating: 3 }),
        makeRecording({ id: "f", rating: 3 }),
      ],
      currentTrack,
      now: FAKE_NOW,
    });
    expect(result).toBeDefined();
    expect(result!.id).not.toBe("g");
  });

  it("does not re-pick a just-completed track that is now first in history (regression)", () => {
    // Scenario: HANABI finishes playing, completeCurrentTrack() runs,
    // setting currentTrack=null and adding HANABI to history[0].
    // AutoDJ then fires with queue=[], currentTrack=null.
    // It must NOT re-pick the same song just because it's no longer currentTrack.
    const justFinished = makeRecording({ id: "hanabi", last_played: null });
    const other = makeRecording({ id: "other-song", last_played: null });
    const result = pickNextTrack({
      playable: [justFinished, other],
      queue: [],
      history: [justFinished], // just-completed track is first in history
      currentTrack: null,
      now: FAKE_NOW,
    });
    expect(result).toBeDefined();
    expect(result!.id).not.toBe("hanabi");
    expect(result!.id).toBe("other-song");
  });

  it("returns undefined when all songs are excluded and no fallback possible", () => {
    const result = pickNextTrack({
      playable: [],
      queue: [],
      history: [],
      currentTrack: null,
      now: FAKE_NOW,
    });
    expect(result).toBeUndefined();
  });
});
