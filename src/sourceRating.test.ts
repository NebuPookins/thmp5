import { describe, expect, it } from "vitest";
import type { SourceRow } from "./autoDj";
import { makeQueueItem, makeRecording } from "./testHelpers";
import { applySourceRating, applySourceRatingToItem } from "./sourceRating";

const source = (id: string, rating: number | null): SourceRow => ({
  id,
  file_path: null,
  duration_ms: null,
  rating,
  play_count: 0,
  last_played: null,
});

describe("applySourceRating", () => {
  it("makes the recording rating the average of its source ratings", () => {
    const rec = makeRecording({ id: "r", sources: [source("a", 4), source("b", null)], rating: 4 });
    const result = applySourceRating(rec, "b", 2);
    expect(result.rating).toBe(3);
    expect(result.sources.map((s) => s.rating)).toEqual([4, 2]);
  });

  it("clears the recording rating when its only rated source is cleared", () => {
    const rec = makeRecording({ id: "r", sources: [source("a", 5)], rating: 5 });
    expect(applySourceRating(rec, "a", null).rating).toBeNull();
  });

  it("leaves recordings that do not own the source untouched", () => {
    const rec = makeRecording({ id: "r", sources: [source("a", 5)], rating: 5 });
    expect(applySourceRating(rec, "other", 1)).toBe(rec);
  });
});

describe("applySourceRatingToItem", () => {
  it("updates the pinned source's own rating", () => {
    const pinned = makeQueueItem({ id: "r", primary_source_id: "a", sources: [source("a", null), source("b", 4)] });
    const result = applySourceRatingToItem(pinned, "a", 2);
    expect(result.source.rating).toBe(2);
    expect(result.rating).toBe(3);
  });

  it("keeps the pinned source when a sibling source is rated", () => {
    const pinned = makeQueueItem({ id: "r", primary_source_id: "a", sources: [source("a", null), source("b", null)] });
    const result = applySourceRatingToItem(pinned, "b", 5);
    expect(result.source.rating).toBeNull();
    expect(result.rating).toBe(5);
  });
});
