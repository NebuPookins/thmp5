/**
 * Pure AutoDJ selection logic — extracted so it can be unit-tested without React.
 */

export interface RecordingRow {
  id: string;
  title: string;
  duration_ms: number | null;
  primary_artist_id: string | null;
  artist_credit_name: string | null;
  genre: string | null;
  rating: number | null;
  play_count: number;
  last_played: string | null;
  primary_source_id: string | null;
  primary_source_path: string | null;
  tags: string[];
  artist_ids: string[];
  source_paths: string[];
  releases: ReleaseInfo[];
}

export interface ReleaseInfo {
  release_group_id: string;
  release_group_title: string;
  track_position: number | null;
  disc_position: number | null;
  disc_total: number | null;
}

export type QueueItem = RecordingRow;

export interface PickOptions {
  /** All playable recordings (those with primary_source_id !== null). */
  playable: RecordingRow[];
  /** Currently enqueued (upcoming) items. */
  queue: QueueItem[];
  /** Play history (most recent first). */
  history: QueueItem[];
  /** The currently-playing track (may be null when stopped). */
  currentTrack: QueueItem | null;
  /**
   * A source of the current wall-clock time.
   * Defaults to `Date.now()` — override in tests to control "last 24h".
   */
  now?: number;
}

/**
 * Return the subset of `items` that were last played more than 24h ago
 * (or have never been played / have no timestamp).
 */
export function excludeRecentlyPlayed(
  items: RecordingRow[],
  now: number,
): RecordingRow[] {
  const cutoff = now - 86_400_000; // 24 hours in ms
  return items.filter((r) => {
    if (!r.last_played) return true; // never played → fine
    const playedAt = new Date(r.last_played).getTime();
    return !isNaN(playedAt) && playedAt < cutoff;
  });
}

/**
 * Return the subset of `items` whose IDs are NOT in the given set of
 * IDs to exclude.
 */
export function excludeIds(
  items: RecordingRow[],
  exclude: Set<string>,
): RecordingRow[] {
  return items.filter((r) => !exclude.has(r.id));
}

/**
 * Apply the standard rating-based pick strategy to a filtered pool.
 *
 * 1. Prefer based on average rating of history (same logic as was inlined).
 * 2. If no pickable items pass the filter, fall back to the full unfiltered
 *    `playable` list (minus excluded IDs + recently played — the caller
 *    decides whether to allow those as final fallback).
 */
export function pickByRatingStrategy(
  history: QueueItem[],
  filteredCandidates: RecordingRow[],
): RecordingRow | undefined {
  if (filteredCandidates.length === 0) return undefined;

  const pickRandom = (items: RecordingRow[]): RecordingRow | undefined => {
    if (items.length === 0) return undefined;
    return items[Math.floor(Math.random() * items.length)];
  };

  const ratedHistory = history.filter((r) => r.rating !== null);

  if (ratedHistory.length === 0) {
    return (
      pickRandom(filteredCandidates.filter((r) => r.rating === null)) ??
      pickRandom(filteredCandidates)
    );
  }

  const avgRating =
    ratedHistory.reduce((sum, r) => sum + r.rating!, 0) /
    ratedHistory.length;

  if (avgRating >= 2.5 && avgRating <= 3.5) {
    return (
      pickRandom(
        filteredCandidates.filter(
          (r) => r.rating !== null && r.rating >= 4,
        ),
      ) ?? pickRandom(filteredCandidates)
    );
  }

  if (avgRating < 2.5) {
    return (
      pickRandom(
        filteredCandidates.filter((r) => r.rating !== null && r.rating >= 5),
      ) ??
        pickRandom(
          filteredCandidates.filter(
            (r) => r.rating !== null && r.rating >= 4,
          ),
        ) ??
      pickRandom(filteredCandidates)
    );
  }

  // avgRating > 3.5
  return (
    pickRandom(filteredCandidates.filter((r) => r.rating === null)) ??
    pickRandom(filteredCandidates)
  );
}

/**
 * Main entry point: pick the next AutoDJ track.
 *
 * The algorithm:
 * 1. Build the set of IDs to exclude (currently playing + any queued item).
 * 2. Filter `playable` to remove recently-played (last 24h) and excluded IDs.
 * 3. Apply rating-based strategy on the filtered pool.
 * 4. If nothing survives, fall back to the original behaviour — pick from
 *    the full playable set minus only the excluded IDs (ignore 24h).
 */
export function pickNextTrack(options: PickOptions): RecordingRow | undefined {
  const { playable, queue, history, currentTrack, now } = options;
  const wallClock = now ?? Date.now();

  // IDs that must be excluded (currently playing + queued).
  const excludeIdSet = new Set<string>();
  if (currentTrack) excludeIdSet.add(currentTrack.id);
  for (const item of queue) excludeIdSet.add(item.id);
  // Exclude recently-completed tracks from history to prevent re-picking
  // when the queue empties before backend playback history refreshes.
  // We exclude the most recent entries (up to 5) since history is most-recent-first.
  for (let i = 0; i < Math.min(history.length, 5); i++) {
    excludeIdSet.add(history[i].id);
  }

  // First pass: strict — exclude recently-played AND in-queue/current.
  let candidates = excludeRecentlyPlayed(playable, wallClock);
  candidates = excludeIds(candidates, excludeIdSet);

  const pick = pickByRatingStrategy(history, candidates);
  if (pick) return pick;

  // Second pass: relaxed — only exclude in-queue/current/recent-history, allow recently-played.
  candidates = excludeIds(playable, excludeIdSet);
  return pickByRatingStrategy(history, candidates);
}
