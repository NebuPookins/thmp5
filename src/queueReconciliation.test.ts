import { describe, expect, it } from "vitest";
import type { RecordingRow, SourceRow } from "./autoDj";
import { refreshItem, repinItem, snapshotOf } from "./queueReconciliation";
import { makeQueueItem, makeRecording } from "./testHelpers";

const noRedirects = new Map<string, string>();

function source(id: string, overrides: Partial<SourceRow> = {}): SourceRow {
  return { id, file_path: `/${id}.mp3`, duration_ms: null, format: null, rating: null, play_count: 0, last_played: null, ...overrides };
}

function recording(id: string, sourceIds: string[], primary: string | null = sourceIds[0] ?? null): RecordingRow {
  return makeRecording({ id, primary_source_id: primary, sources: sourceIds.map((s) => source(s)) });
}

describe("repinItem", () => {
  it("keeps an item on its live source and refreshes its recording data", () => {
    const old = makeQueueItem({ id: "rec", primary_source_id: "s1" });
    const fresh = makeRecording({ id: "rec", title: "Renamed", primary_source_id: "s1", sources: [source("s1", { rating: 4 })] });

    const result = repinItem(old, snapshotOf([fresh]), noRedirects);

    expect(result).toMatchObject({ id: "rec", title: "Renamed", source: { id: "s1", rating: 4 } });
  });

  it("follows a live source that now belongs to a different recording", () => {
    const old = makeQueueItem({ id: "old-rec", primary_source_id: "s1" });

    const result = repinItem(old, snapshotOf([recording("old-rec", ["s2"]), recording("new-rec", ["s1"])]), noRedirects);

    expect(result).toMatchObject({ id: "new-rec", source: { id: "s1" } });
  });

  it("moves an item whose source was deleted to its recording's primary live source", () => {
    const old = makeQueueItem({ id: "rec", primary_source_id: "gone" });

    const result = repinItem(old, snapshotOf([recording("rec", ["other", "live"], "live")]), noRedirects);

    expect(result).toMatchObject({ id: "rec", source: { id: "live" } });
  });

  it("drops an item whose recording has no live source left", () => {
    const old = makeQueueItem({ id: "rec", primary_source_id: "gone" });

    expect(repinItem(old, snapshotOf([recording("unrelated", ["x"])]), noRedirects)).toBeNull();
  });

  it("moves an item to the source its dead source was redirected to, even on another recording", () => {
    const old = makeQueueItem({ id: "rec", primary_source_id: "gone" });
    const snapshot = snapshotOf([recording("rec", ["sibling"]), recording("target-rec", ["target"])]);

    const result = repinItem(old, snapshot, new Map([["gone", "target"]]));

    expect(result).toMatchObject({ id: "target-rec", source: { id: "target" } });
  });

  it("follows a chain of redirects to the first live source", () => {
    const old = makeQueueItem({ id: "rec", primary_source_id: "a" });

    const result = repinItem(old, snapshotOf([recording("c-rec", ["c"])]), new Map([["a", "b"], ["b", "c"]]));

    expect(result).toMatchObject({ id: "c-rec", source: { id: "c" } });
  });

  it("ignores a redirect while the source it redirects from is still live", () => {
    const old = makeQueueItem({ id: "rec", primary_source_id: "s1" });

    const result = repinItem(old, snapshotOf([recording("rec", ["s1"]), recording("t-rec", ["t"])]), new Map([["s1", "t"]]));

    expect(result).toMatchObject({ id: "rec", source: { id: "s1" } });
  });

  it("falls back to the recording's primary source when redirects form a dead cycle", () => {
    const old = makeQueueItem({ id: "rec", primary_source_id: "a" });

    const result = repinItem(old, snapshotOf([recording("rec", ["live"])]), new Map([["a", "b"], ["b", "a"]]));

    expect(result).toMatchObject({ id: "rec", source: { id: "live" } });
  });
});

describe("repinItem after a source deletion changed the recording id", () => {
  it("moves the item to the primary source of the recording now holding its sibling source", () => {
    const old = makeQueueItem({ id: "rec-old", primary_source_id: "gone", sources: [source("gone"), source("sibling")] });

    const result = repinItem(old, snapshotOf([recording("rec-new", ["sibling"])]), noRedirects);

    expect(result).toMatchObject({ id: "rec-new", source: { id: "sibling" } });
  });
});

describe("refreshItem", () => {
  it("refreshes an item's recording data on its live source", () => {
    const old = makeQueueItem({ id: "rec", primary_source_id: "s1" });
    const fresh = makeRecording({ id: "rec", title: "Renamed", primary_source_id: "s1", sources: [source("s1", { rating: 4 })] });

    expect(refreshItem(old, snapshotOf([fresh]))).toMatchObject({ id: "rec", title: "Renamed", source: { id: "s1", rating: 4 } });
  });

  it("keeps an item on its source when that source is gone, refreshing its recording's data", () => {
    const old = makeQueueItem({ id: "rec", primary_source_id: "gone" });
    const fresh = makeRecording({ id: "rec", title: "Renamed", primary_source_id: "live", sources: [source("live")] });

    expect(refreshItem(old, snapshotOf([fresh]))).toMatchObject({ id: "rec", title: "Renamed", source: { id: "gone" } });
  });

  it("keeps an item whose recording is absent from the snapshot unchanged", () => {
    const old = makeQueueItem({ id: "rec", primary_source_id: "s1" });

    expect(refreshItem(old, snapshotOf([]))).toBe(old);
  });

  it("returns the item itself when it is already current", () => {
    const rec = recording("rec", ["s1"]);
    const item = makeQueueItem({ id: "rec", primary_source_id: "s1", sources: rec.sources });

    expect(refreshItem(item, snapshotOf([rec]))).toBe(item);
    expect(repinItem(item, snapshotOf([rec]), noRedirects)).toBe(item);
  });
});
