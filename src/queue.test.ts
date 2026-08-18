import { describe, it, expect } from "vitest";
import {
  moveItem,
  resolveGapDropIndex,
  resolveItemDropIndex,
  moveEntryByKey,
  makeQueueEntry,
  mapQueueEntryRecording,
  type QueueEntry,
} from "./queue";
import { makeRecording } from "./testHelpers";

function entry(key: string, recordingId: string): QueueEntry {
  return makeQueueEntry(makeRecording({ id: recordingId }), key);
}

// ── moveItem ─────────────────────────────────────────────────────────────────

describe("moveItem", () => {
  it("moves an item from earlier to later index", () => {
    expect(moveItem(["a", "b", "c", "d"], 0, 2)).toEqual(["b", "c", "a", "d"]);
  });

  it("moves an item from later to earlier index", () => {
    expect(moveItem(["a", "b", "c", "d"], 3, 1)).toEqual(["a", "d", "b", "c"]);
  });

  it("returns the original array when fromIndex is out of range", () => {
    const items = ["a", "b"];
    expect(moveItem(items, 5, 0)).toBe(items);
    expect(moveItem(items, -1, 0)).toBe(items);
  });

  it("clamps toIndex to the valid range", () => {
    expect(moveItem(["a", "b", "c"], 0, 99)).toEqual(["b", "c", "a"]);
    expect(moveItem(["a", "b", "c"], 2, -99)).toEqual(["c", "a", "b"]);
  });
});

// ── resolveGapDropIndex ──────────────────────────────────────────────────────

describe("resolveGapDropIndex", () => {
  it("inserts after the hovered item when dragging down", () => {
    expect(resolveGapDropIndex(0, 2, 4)).toBe(2);
  });

  it("inserts after the hovered item when dragging up", () => {
    expect(resolveGapDropIndex(3, 0, 4)).toBe(1);
  });

  it("is a no-op when dropping in the gap right after its own row", () => {
    expect(resolveGapDropIndex(0, 0, 4)).toBeNull();
    expect(resolveGapDropIndex(2, 2, 4)).toBeNull();
  });

  it("appends at the end when nothing was hovered", () => {
    expect(resolveGapDropIndex(0, -1, 4)).toBe(3);
  });
});

// ── resolveItemDropIndex ─────────────────────────────────────────────────────

describe("resolveItemDropIndex", () => {
  it("returns the hovered item's index", () => {
    expect(resolveItemDropIndex(0, 2)).toBe(2);
    expect(resolveItemDropIndex(3, 1)).toBe(1);
  });

  it("is a no-op when dropped onto itself", () => {
    expect(resolveItemDropIndex(1, 1)).toBeNull();
  });

  it("is a no-op when there is no hovered item", () => {
    expect(resolveItemDropIndex(0, -1)).toBeNull();
  });
});

// ── moveEntryByKey ───────────────────────────────────────────────────────────

describe("moveEntryByKey", () => {
  it("moves the dragged entry onto the hovered entry (item drop)", () => {
    const entries = [entry("a", "r1"), entry("b", "r2"), entry("c", "r3"), entry("d", "r4")];
    const result = moveEntryByKey(entries, "a", "c", "item");
    expect(result.map((e) => e.key)).toEqual(["b", "c", "a", "d"]);
  });

  it("moves the dragged entry into the gap after the hovered entry (gap drop)", () => {
    const entries = [entry("a", "r1"), entry("b", "r2"), entry("c", "r3"), entry("d", "r4")];
    const result = moveEntryByKey(entries, "a", "c", "gap");
    expect(result.map((e) => e.key)).toEqual(["b", "c", "a", "d"]);
  });

  it("moves up into the gap after the hovered entry", () => {
    const entries = [entry("a", "r1"), entry("b", "r2"), entry("c", "r3"), entry("d", "r4")];
    const result = moveEntryByKey(entries, "d", "a", "gap");
    expect(result.map((e) => e.key)).toEqual(["a", "d", "b", "c"]);
  });

  it("appends at the end when nothing was hovered (gap drop)", () => {
    const entries = [entry("a", "r1"), entry("b", "r2"), entry("c", "r3")];
    const result = moveEntryByKey(entries, "a", null, "gap");
    expect(result.map((e) => e.key)).toEqual(["b", "c", "a"]);
  });

  it("does not reorder when dropping in the gap after its own row", () => {
    const entries = [entry("a", "r1"), entry("b", "r2"), entry("c", "r3")];
    const result = moveEntryByKey(entries, "b", "b", "gap");
    expect(result).toBe(entries);
    expect(result.map((e) => e.key)).toEqual(["a", "b", "c"]);
  });

  it("does not reorder when dropped onto itself", () => {
    const entries = [entry("a", "r1"), entry("b", "r2"), entry("c", "r3")];
    const result = moveEntryByKey(entries, "b", "b", "item");
    expect(result).toBe(entries);
  });

  it("moves the exact duplicate entry, not its same-id twin", () => {
    // Two copies of the same recording ("dup") with different keys.
    const entries = [
      entry("dup-1", "dup"),
      entry("other", "other"),
      entry("dup-2", "dup"),
    ];
    const result = moveEntryByKey(entries, "dup-2", "other", "item");
    // The *second* copy (dup-2) moves before "other"; dup-1 stays at index 0.
    expect(result.map((e) => e.key)).toEqual(["dup-1", "dup-2", "other"]);
  });

  it("resolves the dragged entry by key after the queue shifts (auto-advance)", () => {
    // Before the shift, the dragged entry "b" is at index 1.
    const beforeShift = [entry("a", "ra"), entry("b", "rb"), entry("c", "rc")];
    // Auto-advance removes the head ("a") -> the latest queue is [b, c].
    const afterShift = beforeShift.slice(1);
    // An index captured before the shift (1) would now point at "c". Resolving by
    // key against the latest array moves "b" instead.
    const result = moveEntryByKey(afterShift, "b", "c", "item");
    expect(result.map((e) => e.key)).toEqual(["c", "b"]);
  });

  it("returns the original array when the dragged entry is no longer present", () => {
    const entries = [entry("a", "r1"), entry("b", "r2")];
    const result = moveEntryByKey(entries, "missing", "a", "item");
    expect(result).toBe(entries);
  });

  it("is a no-op when the hover target was removed mid-drag (gap drop)", () => {
    // The hovered entry was auto-advanced out of the queue, leaving the gap-drop
    // target absent. This must not silently append the dragged item to the end.
    const entries = [entry("a", "r1"), entry("b", "r2"), entry("c", "r3")];
    const result = moveEntryByKey(entries, "a", "gone", "gap");
    expect(result).toBe(entries);
  });
});

// ── mapQueueEntryRecording ───────────────────────────────────────────────────

describe("mapQueueEntryRecording", () => {
  it("applies a mapping to each recording and preserves keys", () => {
    const entries = [entry("a", "r1"), entry("b", "r2")];
    const result = mapQueueEntryRecording(entries, (r) => ({ ...r, title: "x" }));
    expect(result[0].key).toBe("a");
    expect(result[0].recording.title).toBe("x");
    expect(result[1].key).toBe("b");
  });

  it("returns the original array when nothing changed", () => {
    const entries = [entry("a", "r1")];
    const result = mapQueueEntryRecording(entries, (r) => r);
    expect(result).toBe(entries);
  });
});
