/**
 * Shared test helpers used across test files.
 */

import { pinPrimarySource, type QueueItem, type RecordingRow, type SourceRow } from "./autoDj";

export const FAKE_NOW = 1_000_000_000_000; // a fixed "now" in ms
export const ONE_DAY_MS = 86_400_000;

export function makeRecording(
  overrides: Partial<RecordingRow> & { id: string },
): RecordingRow {
  const base: RecordingRow = {
    title: "test",
    duration_ms: null,
    primary_artist_id: null,
    artist_credit_name: null,
    genre: null,
    rating: null,
    predicted_rating: null,
    play_count: 0,
    last_played: null,
    primary_source_id: "src-" + overrides.id,
    primary_source_path: null,
    tags: [],
    artist_ids: [],
    source_paths: [],
    sources: [],
    releases: [],
    ...overrides,
  };
  // Keep `sources` consistent with the primary source unless the test set it.
  if (overrides.sources !== undefined || base.primary_source_id === null) {
    return base;
  }
  const source: SourceRow = {
    id: base.primary_source_id,
    file_path: base.primary_source_path,
    duration_ms: base.duration_ms,
    rating: base.rating,
    play_count: base.play_count,
    last_played: base.last_played,
  };
  return { ...base, sources: [source] };
}

export function makeQueueItem(overrides: Partial<RecordingRow> & { id: string }): QueueItem {
  const item = pinPrimarySource(makeRecording(overrides));
  if (!item) {
    throw new Error("makeQueueItem needs a primary_source_id or sources containing it");
  }
  return item;
}
