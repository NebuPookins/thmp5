/**
 * Pure queue-manipulation helpers — extracted so they can be unit-tested without
 * React. Each queued item is wrapped in a QueueEntry that carries a stable,
 * unique `key` alongside the recording, so drag/reorder logic can address the
 * exact entry even when duplicate recordings are queued or the queue shifts
 * mid-drag (auto-advance) and is reconciled into fresh objects.
 */

import type { RecordingRow } from "./autoDj";

/** A single upcoming queue item: a stable key plus the recording it references. */
export interface QueueEntry {
  /** Stable, unique-per-enqueue identity (unlike recording id, which may repeat). */
  key: string;
  recording: RecordingRow;
}

// Monotonic source of keys so each enqueue is uniquely addressable even when the
// same recording is queued more than once. Explicit keys can still be supplied
// (e.g. in tests) via the optional second argument. Seeded from the module-load
// time so a Fast-Refresh/HMR reload of this module (which resets the counter)
// can't collide with keys already held in React state.
let keyCounter = Date.now();
function nextKey(): string {
  keyCounter += 1;
  return `queue-${keyCounter}`;
}

export function makeQueueEntry(recording: RecordingRow, key?: string): QueueEntry {
  return { key: key ?? nextKey(), recording };
}

/**
 * Move the item at `fromIndex` to `toIndex`, where `toIndex` is an index into
 * the array *after* `fromIndex` has been removed. An out-of-range `fromIndex`
 * is a no-op; `toIndex` is clamped to the valid range.
 */
export function moveItem<T>(items: T[], fromIndex: number, toIndex: number): T[] {
  if (fromIndex < 0 || fromIndex >= items.length) {
    return items;
  }
  const moved = items[fromIndex];
  const withoutFrom = [...items.slice(0, fromIndex), ...items.slice(fromIndex + 1)];
  const insertAt = Math.min(Math.max(toIndex, 0), withoutFrom.length);
  return [...withoutFrom.slice(0, insertAt), moved, ...withoutFrom.slice(insertAt)];
}

/**
 * Target insert index for a drop in the gap *after* the hovered item, expressed
 * in the array with `fromIndex` already removed. "After the hovered item" is
 * `over` when dragging down (`from < over`) and `over + 1` when dragging up
 * (`from > over`). When `from === over` the drop is the item's own gap — a
 * no-op — so we return null rather than swapping it with its neighbour.
 * `over === -1` means nothing was hovered yet: append at the end.
 */
export function resolveGapDropIndex(
  fromIndex: number,
  overIndex: number,
  length: number,
): number | null {
  if (overIndex === -1) {
    return length - 1;
  }
  if (fromIndex === overIndex) {
    return null;
  }
  return fromIndex < overIndex ? overIndex : overIndex + 1;
}

/**
 * Target insert index for a drop directly *onto* the hovered item: the dragged
 * item takes the hovered item's position. `from === over` is a no-op.
 */
export function resolveItemDropIndex(fromIndex: number, overIndex: number): number | null {
  if (overIndex === -1 || fromIndex === overIndex) {
    return null;
  }
  return overIndex;
}

export type DropKind = "gap" | "item";

/**
 * Resolve a drag-and-drop reorder against `entries` and return the new array.
 *
 * `fromKey`/`overKey` identify entries by their stable key (not recording id,
 * which is ambiguous under duplicates). Both are resolved against the *passed*
 * array, so callers must pass the latest committed queue (e.g. from a
 * `setQueue(current => …)` updater) rather than a render-closure snapshot that
 * can be stale after a mid-drag shift. Returns `entries` unchanged when nothing
 * should move.
 */
export function moveEntryByKey<T extends { key: string }>(
  entries: T[],
  fromKey: string,
  overKey: string | null,
  dropKind: DropKind,
): T[] {
  const from = entries.findIndex((entry) => entry.key === fromKey);
  if (from === -1) {
    return entries;
  }
  const over = overKey !== null ? entries.findIndex((entry) => entry.key === overKey) : -1;
  // A hover target that has been removed mid-drag (e.g. auto-advance promoted it
  // out of the queue) is no longer a valid drop location. Treat it as a no-op
  // rather than conflating it with "nothing hovered" (overKey === null), which
  // for a gap drop means "append to the end".
  if (overKey !== null && over === -1) {
    return entries;
  }
  const target = dropKind === "gap"
    ? resolveGapDropIndex(from, over, entries.length)
    : resolveItemDropIndex(from, over);
  if (target === null || from === target) {
    return entries;
  }
  return moveItem(entries, from, target);
}

/**
 * Apply `fn` to each entry's recording, preserving its stable key. Returns the
 * original array reference when no recording changed, so callers can skip a
 * state update.
 */
export function mapQueueEntryRecording(
  entries: QueueEntry[],
  fn: (recording: RecordingRow) => RecordingRow,
): QueueEntry[] {
  let changed = false;
  const next = entries.map((entry) => {
    const recording = fn(entry.recording);
    if (recording !== entry.recording) {
      changed = true;
      return { ...entry, recording };
    }
    return entry;
  });
  return changed ? next : entries;
}
