import { describe, it, expect } from "vitest";
import {
  scoreTrack,
  calculatePomodoroBatch,
  parsePomodoroDuration,
  formatPomodoroDuration,
  type RecordingRow,
} from "./pomodoro";
import { FAKE_NOW, ONE_DAY_MS, makeRecording } from "./testHelpers";

// ── Helpers ──────────────────────────────────────────────────────────────────

/** Parse a last_played ISO string to epoch ms, matching the internal helper. */
function lpEpoch(lastPlayed: string | null): number | null {
  if (lastPlayed === null) return null;
  const t = new Date(lastPlayed).getTime();
  return isNaN(t) ? null : t;
}

function mk(
  overrides: Partial<RecordingRow> & { id: string },
): RecordingRow {
  return makeRecording({ duration_ms: 240_000, ...overrides } as Partial<RecordingRow> & { id: string });
}

/**
 * Run fill multiple times and check that the predicate holds every time.
 * The algorithm uses random selection, so we need to verify properties
 * hold across many runs, not just one.
 */
function assertConsistently(runs: number, fn: () => void): void {
  for (let i = 0; i < runs; i++) {
    fn();
  }
}

function manyTracks(
  count: number,
  overrides: (i: number) => Partial<RecordingRow>,
): RecordingRow[] {
  return Array.from({ length: count }, (_, i) =>
    makeRecording({ id: String(i), duration_ms: 240_000, ...overrides(i) } as Partial<RecordingRow> & { id: string }),
  );
}

// ── parsePomodoroDuration ────────────────────────────────────────────────────

describe("parsePomodoroDuration", () => {
  it('parses "25m" as 25 minutes', () => {
    expect(parsePomodoroDuration("25m")).toBe(25 * 60 * 1000);
  });

  it('parses "1h" as 1 hour', () => {
    expect(parsePomodoroDuration("1h")).toBe(60 * 60 * 1000);
  });

  it('parses "1h30m" as 1 hour 30 minutes', () => {
    expect(parsePomodoroDuration("1h30m")).toBe(90 * 60 * 1000);
  });

  it('parses "90" (bare number) as 90 minutes', () => {
    expect(parsePomodoroDuration("90")).toBe(90 * 60 * 1000);
  });

  it('parses "1.5h" as 90 minutes', () => {
    expect(parsePomodoroDuration("1.5h")).toBe(90 * 60 * 1000);
  });

  it('parses "30min" as 30 minutes', () => {
    expect(parsePomodoroDuration("30min")).toBe(30 * 60 * 1000);
  });

  it("returns null for empty string", () => {
    expect(parsePomodoroDuration("")).toBeNull();
  });

  it("returns null for whitespace-only string", () => {
    expect(parsePomodoroDuration("   ")).toBeNull();
  });

  it("returns null for garbage input", () => {
    expect(parsePomodoroDuration("not a duration")).toBeNull();
  });
});

// ── formatPomodoroDuration ───────────────────────────────────────────────────

describe("formatPomodoroDuration", () => {
  it("formats 25 minutes", () => {
    expect(formatPomodoroDuration(25 * 60 * 1000)).toBe("25m");
  });

  it("formats 1 hour", () => {
    expect(formatPomodoroDuration(60 * 60 * 1000)).toBe("1h");
  });

  it("formats 1 hour 30 minutes", () => {
    expect(formatPomodoroDuration(90 * 60 * 1000)).toBe("1h30m");
  });

  it("rounds to nearest minute", () => {
    expect(formatPomodoroDuration(25 * 60 * 1000 + 500)).toBe("25m");
  });

  it("returns 0m for zero", () => {
    expect(formatPomodoroDuration(0)).toBe("0m");
  });
});

// ── scoreTrack ───────────────────────────────────────────────────────────────

describe("scoreTrack", () => {
  it("returns max score for 5-rated never-played track", () => {
    const track = mk({ id: "a", rating: 5, last_played: null });
    const score = scoreTrack(track, null, FAKE_NOW);
    // ratingScore = 1.0, recencyScore = 1.0 → combined = 1.0
    expect(score).toBeCloseTo(1.0, 5);
  });

  it("returns low score for 1-rated just-played track", () => {
    const track = mk({ id: "a", rating: 1, last_played: new Date(FAKE_NOW).toISOString() });
    const score = scoreTrack(track, FAKE_NOW, FAKE_NOW);
    // ratingScore = 0.0, recencyScore ≈ 0.193 → combined ≈ 0.077
    expect(score).toBeLessThan(0.1);
  });

  it("uses predicted_rating as fallback when rating is null", () => {
    const a = mk({ id: "a", rating: 5, predicted_rating: null, last_played: null });
    const b = mk({ id: "b", rating: null, predicted_rating: 5, last_played: null });
    expect(scoreTrack(a, null, FAKE_NOW)).toBeCloseTo(scoreTrack(b, null, FAKE_NOW), 5);
  });

  it("returns neutral rating score when both rating and predicted_rating are null", () => {
    const track = mk({ id: "a", rating: null, predicted_rating: null, last_played: null });
    // ratingScore = 0.5, recencyScore = 1.0 → combined = 0.7
    expect(scoreTrack(track, null, FAKE_NOW)).toBeCloseTo(0.7, 5);
  });

  it("gives higher score to never-played track than recently-played (same rating)", () => {
    const old = mk({ id: "a", rating: 4, last_played: null });
    const justPlayed = mk({ id: "b", rating: 4, last_played: new Date(FAKE_NOW).toISOString() });
    expect(scoreTrack(old, null, FAKE_NOW)).toBeGreaterThan(
      scoreTrack(justPlayed, FAKE_NOW, FAKE_NOW),
    );
  });

  it("recency sigmoid: ~0.25 at 7 days, ~0.5 at 30 days, ~0.81 at 60 days", () => {
    const r = (days: number) =>
      mk({ id: "x", rating: 3, last_played: new Date(FAKE_NOW - days * ONE_DAY_MS).toISOString() });

    // ratingScore = 0.5 for all; combined depends on recency
    expect(scoreTrack(r(7), FAKE_NOW - 7 * ONE_DAY_MS, FAKE_NOW)).toBeCloseTo(0.4, 1);
    expect(scoreTrack(r(30), FAKE_NOW - 30 * ONE_DAY_MS, FAKE_NOW)).toBeCloseTo(0.5, 1);
    expect(scoreTrack(r(60), FAKE_NOW - 60 * ONE_DAY_MS, FAKE_NOW)).toBeCloseTo(0.623, 1);
  });

  it("handles malformed last_played date gracefully", () => {
    // Passing a malformed date through lpEpoch returns null,
    // which means scoreTrack treats it as "never played" (recency = 1.0).
    const track = mk({ id: "a", rating: 3, last_played: "not-a-date" });
    const lpMs = lpEpoch(track.last_played);
    // lpEpoch returns null for malformed dates, so recency = 1.0
    expect(lpMs).toBeNull();
    expect(scoreTrack(track, null, FAKE_NOW)).toBe(0.7); // 0.6*0.5 + 0.4*1.0
  });
});

// ── calculatePomodoroBatch ───────────────────────────────────────────────────

describe("calculatePomodoroBatch", () => {
  it("fills to within tolerance range", () => {
    assertConsistently(20, () => {
      const tracks = manyTracks(30, (i) => ({
        duration_ms: (3 + (i % 3)) * 60 * 1000,
        rating: 4 + (i % 2),
        last_played: null,
      }));
      const batch = calculatePomodoroBatch({
        playable: tracks,
        queue: [],
        history: [],
        currentTrack: null,
        targetDurationMs: 25 * 60 * 1000,
        toleranceMs: 3 * 60 * 1000,
      });
      const total = batch.reduce((sum, r) => sum + r.duration_ms!, 0);
      expect(total).toBeGreaterThanOrEqual(22 * 60 * 1000);
      expect(total).toBeLessThanOrEqual(28 * 60 * 1000);
      expect(batch.length).toBeGreaterThanOrEqual(5);
    });
  });

  it("respects tolerance upper bound consistently", () => {
    assertConsistently(20, () => {
      const tracks = manyTracks(30, (i) => ({
        duration_ms: (3 + (i % 3)) * 60 * 1000,
        rating: 4,
        last_played: null,
      }));
      const batch = calculatePomodoroBatch({
        playable: tracks,
        queue: [],
        history: [],
        currentTrack: null,
        targetDurationMs: 25 * 60 * 1000,
        toleranceMs: 3 * 60 * 1000,
      });
      const total = batch.reduce((sum, r) => sum + r.duration_ms!, 0);
      expect(total).toBeLessThanOrEqual(28 * 60 * 1000);
    });
  });

  it("excludes currentTrack", () => {
    const base = manyTracks(20, () => ({
      duration_ms: 4 * 60 * 1000,
      rating: 4,
      last_played: null,
    }));
    const tracks = [
      makeRecording({ id: "current", duration_ms: 5 * 60 * 1000, rating: 5 } as Partial<RecordingRow> & { id: string }),
      ...base,
    ];
    const batch = calculatePomodoroBatch({
      playable: tracks,
      queue: [],
      history: [],
      currentTrack: { id: "current" } as RecordingRow,
      targetDurationMs: 25 * 60 * 1000,
    });
    expect(batch.map((r) => r.id)).not.toContain("current");
  });

  it("excludes already-queued tracks", () => {
    const base = manyTracks(20, () => ({
      duration_ms: 4 * 60 * 1000,
      rating: 4,
      last_played: null,
    }));
    const tracks = [
      makeRecording({ id: "queued", duration_ms: 5 * 60 * 1000, rating: 5 } as Partial<RecordingRow> & { id: string }),
      ...base,
    ];
    const batch = calculatePomodoroBatch({
      playable: tracks,
      queue: [{ id: "queued" } as RecordingRow],
      history: [],
      currentTrack: null,
      targetDurationMs: 25 * 60 * 1000,
    });
    expect(batch.map((r) => r.id)).not.toContain("queued");
  });

  it("excludes recent history entries (up to 5)", () => {
    const tracks = manyTracks(20, () => ({
      duration_ms: 3 * 60 * 1000,
      rating: 4,
      last_played: null,
    }));
    const history = [
      { id: "0" } as RecordingRow,
      { id: "1" } as RecordingRow,
      { id: "2" } as RecordingRow,
      { id: "3" } as RecordingRow,
      { id: "4" } as RecordingRow,
    ];
    const batch = calculatePomodoroBatch({
      playable: tracks,
      queue: [],
      history,
      currentTrack: null,
      targetDurationMs: 25 * 60 * 1000,
    });
    const ids = batch.map((r) => r.id);
    expect(ids).not.toContain("0");
    expect(ids).not.toContain("1");
    expect(ids).not.toContain("2");
    expect(ids).not.toContain("3");
    expect(ids).not.toContain("4");
  });

  it("strict pass prefers non-recent tracks over recently-played ones", () => {
    const oneHourAgo = new Date(FAKE_NOW - 60 * 60 * 1000).toISOString();

    const recent = manyTracks(15, (i) => ({
      id: `recent-${i}`,
      duration_ms: 4 * 60 * 1000,
      rating: 5,
      last_played: oneHourAgo,
    }));
    const fresh = manyTracks(15, (i) => ({
      id: `fresh-${i}`,
      duration_ms: 4 * 60 * 1000,
      rating: 3,
      last_played: null,
    }));

    assertConsistently(10, () => {
      const batch = calculatePomodoroBatch({
        playable: [...recent, ...fresh],
        queue: [],
        history: [],
        currentTrack: null,
        targetDurationMs: 25 * 60 * 1000,
        toleranceMs: 3 * 60 * 1000,
        now: FAKE_NOW,
      });
      const ids = batch.map((r) => r.id);
      const freshPicked = ids.filter((id) => id.startsWith("fresh-"));
      // Fresh tracks (even with lower rating) should be preferred in strict pass
      expect(freshPicked.length).toBeGreaterThan(0);
    });
  });

  it("falls back to relaxed pass when strict pass has too few tracks", () => {
    const oneHourAgo = new Date(FAKE_NOW - 60 * 60 * 1000).toISOString();
    const tracks = manyTracks(25, () => ({
      duration_ms: 4 * 60 * 1000,
      rating: 4,
      last_played: oneHourAgo,
    }));
    const batch = calculatePomodoroBatch({
      playable: tracks,
      queue: [],
      history: [],
      currentTrack: null,
      targetDurationMs: 25 * 60 * 1000,
      now: FAKE_NOW,
    });
    expect(batch.length).toBeGreaterThan(0);
  });

  it("returns empty array for empty playable", () => {
    const batch = calculatePomodoroBatch({
      playable: [],
      queue: [],
      history: [],
      currentTrack: null,
      targetDurationMs: 25 * 60 * 1000,
    });
    expect(batch).toEqual([]);
  });

  it("excludes tracks with null duration_ms", () => {
    const base = manyTracks(15, () => ({
      duration_ms: 4 * 60 * 1000,
      rating: 4,
      last_played: null,
    }));
    const tracks = [
      makeRecording({ id: "nodur", duration_ms: null, rating: 5 } as Partial<RecordingRow> & { id: string }),
      ...base,
    ];
    const batch = calculatePomodoroBatch({
      playable: tracks,
      queue: [],
      history: [],
      currentTrack: null,
      targetDurationMs: 25 * 60 * 1000,
    });
    expect(batch.map((r) => r.id)).not.toContain("nodur");
  });

  it("returns empty when all tracks are too long", () => {
    const tracks = [
      makeRecording({ id: "long", duration_ms: 40 * 60 * 1000, rating: 5 } as Partial<RecordingRow> & { id: string }),
    ];
    const batch = calculatePomodoroBatch({
      playable: tracks,
      queue: [],
      history: [],
      currentTrack: null,
      targetDurationMs: 25 * 60 * 1000,
      toleranceMs: 3 * 60 * 1000,
    });
    expect(batch).toEqual([]);
  });

  it("returns at least one track even for very short targets (≤ tolerance)", () => {
    // Regression: when target ≤ tolerance, lowerBound ≤ 0, which used to
    // cause the while loop to skip entirely.
    const tracks = [
      makeRecording({ id: "short", duration_ms: 4 * 60 * 1000, rating: 5 } as Partial<RecordingRow> & { id: string }),
    ];
    const batch = calculatePomodoroBatch({
      playable: tracks,
      queue: [],
      history: [],
      currentTrack: null,
      targetDurationMs: 3 * 60 * 1000, // 3 min target
      toleranceMs: 3 * 60 * 1000, // 3 min tolerance → lowerBound = 0
    });
    expect(batch.length).toBeGreaterThanOrEqual(1);
    expect(batch[0].id).toBe("short");
  });

  it("returns best effort even if under target", () => {
    const tracks = [
      makeRecording({ id: "only", duration_ms: 10 * 60 * 1000, rating: 5 } as Partial<RecordingRow> & { id: string }),
    ];
    const batch = calculatePomodoroBatch({
      playable: tracks,
      queue: [],
      history: [],
      currentTrack: null,
      targetDurationMs: 25 * 60 * 1000,
    });
    expect(batch.length).toBe(1);
    expect(batch[0].id).toBe("only");
  });

  it("produces varied selections across runs", () => {
    const tracks = manyTracks(40, () => ({
      duration_ms: 3 * 60 * 1000,
      rating: 4,
      last_played: null,
    }));

    const seenIds = new Set<string>();
    for (let i = 0; i < 15; i++) {
      const batch = calculatePomodoroBatch({
        playable: tracks,
        queue: [],
        history: [],
        currentTrack: null,
        targetDurationMs: 25 * 60 * 1000,
        toleranceMs: 3 * 60 * 1000,
      });
      for (const r of batch) seenIds.add(r.id);
    }

    // With 40 equally-good tracks and 15 runs picking ~8-9 each,
    // we should see significant variety.
    expect(seenIds.size).toBeGreaterThan(15);
  });
});
