/**
 * Bring queued, playing and played items up to date with a catalog snapshot.
 * Each item is pinned to a source, which may since have moved to another
 * recording or gone away (removed, relinked, replaced, rescanned, ...).
 */

import { pinPrimarySource, pinSource, type QueueItem, type RecordingRow } from "./autoDj";

/** Lookups over one catalog snapshot. */
export type CatalogSnapshot = {
  readonly recordingsById: ReadonlyMap<string, RecordingRow>;
  readonly recordingsBySourceId: ReadonlyMap<string, RecordingRow>;
};

/** Dead source id -> the source its data was merged onto (Relink, Replace corrupt). */
export type SourceRedirects = ReadonlyMap<string, string>;

export function snapshotOf(recordings: readonly RecordingRow[]): CatalogSnapshot {
  const recordingsBySourceId = new Map<string, RecordingRow>();
  recordings.forEach((recording) => recording.sources.forEach((source) => recordingsBySourceId.set(source.id, recording)));
  return {
    recordingsById: new Map(recordings.map((recording) => [recording.id, recording] as const)),
    recordingsBySourceId,
  };
}

/** Keep `item` itself when `next` carries exactly the same data, so state updates can settle. */
function sameOr(item: QueueItem, next: QueueItem): QueueItem {
  return next.id === item.id && next.sources === item.sources && next.source === item.source ? item : next;
}

/** `item` pinned to its own source on whichever recording now holds it, or null when the source is gone. */
function onLiveSource(item: QueueItem, snapshot: CatalogSnapshot): QueueItem | null {
  const holder = snapshot.recordingsBySourceId.get(item.source.id);
  const pinned = holder ? pinSource(holder, item.source.id) : null;
  return pinned ? sameOr(item, pinned) : null;
}

/**
 * The recording `item` belongs to now. Without a shared MBID a recording's id
 * derives from its source ids, so after a source is deleted the old id is gone
 * and the recording is found through any of its other sources.
 */
function currentRecording(item: QueueItem, snapshot: CatalogSnapshot): RecordingRow | undefined {
  return (
    snapshot.recordingsById.get(item.id) ??
    item.sources.map((source) => snapshot.recordingsBySourceId.get(source.id)).find((recording) => recording !== undefined)
  );
}

/**
 * Refresh `item`'s data without ever moving it off its source: safe on any
 * snapshot, even one from a partial load. When the source is gone the item
 * keeps it, with its recording's data refreshed if that recording remains.
 */
export function refreshItem(item: QueueItem, snapshot: CatalogSnapshot): QueueItem {
  const recording = currentRecording(item, snapshot);
  return onLiveSource(item, snapshot) ?? (recording ? sameOr(item, { ...recording, source: item.source }) : item);
}

/** The first live source reached by following redirects from `sourceId`. Cycles end the search. */
function redirectTarget(sourceId: string, snapshot: CatalogSnapshot, redirects: SourceRedirects): QueueItem | null {
  const visited = new Set([sourceId]);
  for (let next = redirects.get(sourceId); next !== undefined && !visited.has(next); next = redirects.get(next)) {
    const recording = snapshot.recordingsBySourceId.get(next);
    if (recording) return pinSource(recording, next);
    visited.add(next);
  }
  return null;
}

/**
 * The live version of `item`: its own source if it still exists, else the
 * source it was redirected to, else its recording's primary live source. Null
 * when none is left. Only use with a snapshot from a complete, consistent
 * library load, since an item whose source is absent is moved or dropped.
 */
export function repinItem(item: QueueItem, snapshot: CatalogSnapshot, redirects: SourceRedirects): QueueItem | null {
  const recording = currentRecording(item, snapshot);
  return (
    onLiveSource(item, snapshot) ??
    redirectTarget(item.source.id, snapshot, redirects) ??
    (recording ? pinPrimarySource(recording) : null)
  );
}
