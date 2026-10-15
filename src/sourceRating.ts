/**
 * Pure helpers for applying a per-source rating to the rows the UI displays.
 * Ratings live on sources; a recording's rating is the average of its sources'.
 */

import type { QueueItem, RecordingRow, SourceRow } from "./autoDj";

function averageRating(sources: SourceRow[]): number | null {
  const ratings = sources.flatMap((s) => (s.rating === null ? [] : [s.rating]));
  return ratings.length === 0 ? null : ratings.reduce((a, b) => a + b, 0) / ratings.length;
}

/**
 * Set the rating of source `sourceId` on a recording row and re-derive the
 * recording's average. Returns the same reference when the recording does not
 * own that source.
 */
export function applySourceRating<T extends RecordingRow>(
  row: T,
  sourceId: string,
  stars: number | null,
): T {
  if (!row.sources.some((s) => s.id === sourceId)) {
    return row;
  }
  const sources = row.sources.map((s) => (s.id === sourceId ? { ...s, rating: stars } : s));
  return { ...row, sources, rating: averageRating(sources) };
}

/** Like `applySourceRating`, additionally updating the item's pinned source. */
export function applySourceRatingToItem(
  item: QueueItem,
  sourceId: string,
  stars: number | null,
): QueueItem {
  const updated = applySourceRating(item, sourceId, stars);
  return item.source.id === sourceId
    ? { ...updated, source: { ...item.source, rating: stars } }
    : updated;
}
