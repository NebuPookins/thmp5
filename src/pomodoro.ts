/**
 * Pure Pomodoro session logic — calculates a batch of tracks whose total
 * duration approximates a user-specified target.
 *
 * Reuses filtering primitives from autoDj.ts and follows the same
 * pure-function, testable-module pattern.
 */

import {
  buildExclusionSet,
  type RecordingRow,
  type QueueItem,
} from "./autoDj";

// Re-export for consumers
export type { RecordingRow, QueueItem };

// ── Constants ────────────────────────────────────────────────────────────────

/** Default tolerance: ±3 minutes in ms. */
export const DEFAULT_TOLERANCE_MS = 3 * 60 * 1000; // 180_000

/** Default target duration: 25 minutes in ms. */
export const DEFAULT_TARGET_MS = 25 * 60 * 1000; // 1_500_000

/** Weight of rating score vs recency score in the combined score. */
const RATING_WEIGHT = 0.6;

/**
 * Sigmoid parameters for recency scoring.
 * Using a logistic curve so recency differentiates across weeks/months
 * rather than treating everything ≥24h as equally fresh.
 *
 *   recencyScore = 1 / (1 + exp(-k * (daysSincePlayed - midpoint)))
 *
 *   days=0   → ~0.19    days=7  → ~0.25
 *   days=30  →  0.50    days=60 → ~0.81
 *   days=90+ → ~0.95    never   →  1.0
 */
const RECENCY_K = Math.log(3) / 23; // ≈ 0.04777
const RECENCY_MIDPOINT_DAYS = 30;
const MS_PER_DAY = 24 * 60 * 60 * 1000;

/** 24-hour recency cutoff for strict pass (same as autoDj.ts). */
const RECENT_CUTOFF_MS = 24 * 60 * 60 * 1000;

/**
 * Score thresholds to try when selecting tracks, from most selective to least.
 * The algorithm starts at the highest threshold and relaxes until it finds
 * at least one eligible track. Picking randomly within the eligible band
 * (rather than always the top-scored track) ensures variety across sessions.
 */
const SCORE_THRESHOLDS = [0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0];

// ── Exported types ───────────────────────────────────────────────────────────

export interface PomodoroOptions {
  /** All playable recordings (those with primary_source_id !== null). */
  playable: RecordingRow[];
  /** Currently enqueued (upcoming) items. */
  queue: QueueItem[];
  /** Play history (most recent first). */
  history: QueueItem[];
  /** The currently-playing track (may be null when stopped). */
  currentTrack: QueueItem | null;
  /** Target total duration in milliseconds. */
  targetDurationMs: number;
  /** Allowed deviation from target in ms. Defaults to 3 minutes. */
  toleranceMs?: number;
  /** Wall-clock time in ms for recency checks (overridable in tests). */
  now?: number;
}

// ── Helpers ──────────────────────────────────────────────────────────────────

// ── Scoring ──────────────────────────────────────────────────────────────────

/**
 * Score a track on a [0, 1] scale combining rating quality and recency variety.
 * Higher score = more desirable for selection.
 *
 * @param track           The recording to score.
 * @param lastPlayedMs    Pre-parsed last_played epoch (null = never played).
 * @param now             Current wall-clock time in ms.
 * @returns A number in [0, 1].
 */
export function scoreTrack(
  track: RecordingRow,
  lastPlayedMs: number | null,
  now: number,
): number {
  // Rating score (0 to 1)
  let ratingScore: number;
  const effectiveRating = track.rating ?? track.predicted_rating;
  if (effectiveRating !== null) {
    ratingScore = (effectiveRating - 1) / 4; // 1→0, 5→1
  } else {
    ratingScore = 0.5; // neutral
  }

  // Recency score (0 to 1) — sigmoid over days since last play
  let recencyScore: number;
  if (lastPlayedMs === null) {
    recencyScore = 1.0; // never played
  } else {
    const daysSincePlayed = (now - lastPlayedMs) / MS_PER_DAY;
    recencyScore =
      1 / (1 + Math.exp(-RECENCY_K * (daysSincePlayed - RECENCY_MIDPOINT_DAYS)));
  }

  return RATING_WEIGHT * ratingScore + (1 - RATING_WEIGHT) * recencyScore;
}

// ── Internal types ───────────────────────────────────────────────────────────

interface ScoredTrack {
  track: RecordingRow;
  score: number;
  /** Is this track ineligible because of the 24h recency filter? */
  isRecent: boolean;
  /** Is this track excluded by ID (current, queue, history)? */
  isExcluded: boolean;
}

// ── Threshold-based random fill ──────────────────────────────────────────────

/**
 * Find which threshold band a score falls into. Returns the index into
 * SCORE_THRESHOLDS (lower index = higher threshold = better quality).
 */
function thresholdBand(score: number): number {
  for (let i = 0; i < SCORE_THRESHOLDS.length; i++) {
    if (score >= SCORE_THRESHOLDS[i]) return i;
  }
  return SCORE_THRESHOLDS.length - 1; // last band is always 0
}

/**
 * Fill the target duration from a pool of scored candidates using
 * threshold-based random selection.
 *
 * For each pick: bucket unused candidates that fit the remaining budget
 * by their score threshold band, then pick randomly from the highest
 * non-empty band. Single pass per pick — O(candidates) instead of
 * O(candidates × thresholds).
 */
function thresholdFill(
  pool: ScoredTrack[],
  upperBound: number,
  lowerBound: number,
): RecordingRow[] {
  const selected: RecordingRow[] = [];
  let totalMs = 0;
  const usedIds = new Set<string>();

  while (totalMs < lowerBound || selected.length === 0) {
    const remainingBudget = upperBound - totalMs;

    // Bucket: one array per threshold band
    const buckets: ScoredTrack[][] = SCORE_THRESHOLDS.map(() => []);

    for (const s of pool) {
      if (usedIds.has(s.track.id)) continue;
      if (s.track.duration_ms! > remainingBudget) continue;
      buckets[thresholdBand(s.score)].push(s);
    }

    // Pick from the highest (lowest-index) non-empty band
    let picked = false;
    for (let band = 0; band < buckets.length; band++) {
      if (buckets[band].length > 0) {
        const pick = buckets[band][Math.floor(Math.random() * buckets[band].length)];
        selected.push(pick.track);
        usedIds.add(pick.track.id);
        totalMs += pick.track.duration_ms!;
        picked = true;
        break;
      }
    }

    if (!picked) break; // exhausted
  }

  return selected;
}

// ── Main entry point ─────────────────────────────────────────────────────────

/**
 * Calculate a batch of tracks whose total duration approximates the target.
 *
 * Algorithm:
 * 1. Score the entire playable pool once (parsing last_played dates once).
 * 2. Build exclusion set from currentTrack, queue, and recent history.
 * 3. Pass 1 (strict):  exclude recently-played (24h) + excluded IDs →
 *    threshold-based random fill.
 * 4. Pass 2 (relaxed): if still below target, drop the 24h recency filter
 *    (keep ID exclusion), fill remaining budget from the relaxed pool.
 *
 * Tracks with null duration_ms are excluded entirely.
 *
 * @returns The selected track sequence (in selection order).
 */
export function calculatePomodoroBatch(
  options: PomodoroOptions,
): RecordingRow[] {
  const toleranceMs = options.toleranceMs ?? DEFAULT_TOLERANCE_MS;
  const targetMs = options.targetDurationMs;
  const wallClock = options.now ?? Date.now();

  const upperBound = targetMs + toleranceMs;
  const lowerBound = targetMs - toleranceMs;

  // Build exclusion set once (shared with autoDj.ts)
  const excludeIdSet = buildExclusionSet(
    options.currentTrack,
    options.queue,
    options.history,
  );

  // Score the entire pool once — parse last_played dates exactly once per track
  const allScored: ScoredTrack[] = [];
  for (const r of options.playable) {
    if (r.duration_ms === null || r.duration_ms <= 0) continue;

    // Parse last_played once; handle never-played, malformed, and valid dates
    let lpMs: number | null = null;
    let isRecent = false;
    if (r.last_played !== null) {
      const t = new Date(r.last_played).getTime();
      if (isNaN(t)) {
        // Malformed date: treat as recent (matches excludeRecentlyPlayed in autoDj.ts,
        // which filters out malformed dates from the "keep" set).
        isRecent = true;
      } else {
        lpMs = t;
        // ≤ 24h matches autoDj's excludeRecentlyPlayed boundary: at exactly 24h,
        // autoDj excludes the track (playedAt < cutoff is false), so isRecent = true.
        isRecent = wallClock - t <= RECENT_CUTOFF_MS;
      }
    }

    allScored.push({
      track: r,
      score: scoreTrack(r, lpMs, wallClock),
      isRecent,
      isExcluded: excludeIdSet.has(r.id),
    });
  }

  // Pass 1: Strict — exclude ID-excluded AND recently-played
  const strictPool = allScored.filter((s) => !s.isExcluded && !s.isRecent);
  const result1 = thresholdFill(strictPool, upperBound, lowerBound);

  const total1 = result1.reduce((sum, r) => sum + r.duration_ms!, 0);
  if (total1 >= lowerBound && result1.length > 0) {
    return result1;
  }

  // Pass 2: Relaxed — only exclude by ID, allow recently-played
  const pass1Ids = new Set(result1.map((r) => r.id));
  const relaxedPool = allScored.filter(
    (s) => !s.isExcluded && !pass1Ids.has(s.track.id),
  );
  const result2 = thresholdFill(
    relaxedPool,
    upperBound - total1,
    Math.max(0, lowerBound - total1),
  );

  return [...result1, ...result2];
}

// ── Duration parsing ─────────────────────────────────────────────────────────

/**
 * Parse a human-readable duration string into milliseconds.
 *
 * Supported formats:
 *   - "25m"    → 25 minutes
 *   - "1h"     → 1 hour
 *   - "1h30m"  → 1 hour 30 minutes
 *   - "90"     → 90 minutes (bare number = minutes)
 *   - "1.5h"   → 1.5 hours
 *
 * @returns Duration in ms, or null if the input could not be parsed.
 */
export function parsePomodoroDuration(input: string): number | null {
  const trimmed = input.trim().toLowerCase();
  if (!trimmed) return null;

  // "1h" or "1.5h" or "1h30m"
  const hourMatch = trimmed.match(/^(\d+(?:\.\d+)?)\s*h(?:\s*(\d+)\s*m?)?$/);
  if (hourMatch) {
    const totalMinutes =
      parseFloat(hourMatch[1]) * 60 + (hourMatch[2] ? parseInt(hourMatch[2], 10) : 0);
    if (totalMinutes <= 0) return null;
    return Math.round(totalMinutes * 60 * 1000);
  }

  // "30m" or "30min"
  const minMatch = trimmed.match(/^(\d+(?:\.\d+)?)\s*m(?:in)?$/);
  if (minMatch) {
    const totalMinutes = parseFloat(minMatch[1]);
    if (totalMinutes <= 0) return null;
    return Math.round(totalMinutes * 60 * 1000);
  }

  // Bare number — treat as minutes
  const num = parseFloat(trimmed);
  if (!isNaN(num) && num > 0) {
    return Math.round(num * 60 * 1000);
  }

  return null;
}

/**
 * Format a duration in ms to a compact human-readable string.
 * Inverse of parsePomodoroDuration.
 *
 *   e.g., 1_500_000 → "25m"
 *         3_600_000 → "1h"
 *         5_400_000 → "1h30m"
 */
export function formatPomodoroDuration(ms: number): string {
  const totalMinutes = Math.max(0, Math.round(ms / 60_000));
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  if (hours > 0 && minutes > 0) return `${hours}h${minutes}m`;
  if (hours > 0) return `${hours}h`;
  return `${minutes}m`;
}
