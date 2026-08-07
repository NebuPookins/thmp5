/**
 * Shared test helpers used across test files.
 */

import type { RecordingRow } from "./autoDj";

export const FAKE_NOW = 1_000_000_000_000; // a fixed "now" in ms
export const ONE_DAY_MS = 86_400_000;

export function makeRecording(
  overrides: Partial<RecordingRow> & { id: string },
): RecordingRow {
  return {
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
    releases: [],
    ...overrides,
  };
}
