import { describe, expect, it } from "vitest";
import { endedEventAppliesTo } from "./trackEnded";

describe("endedEventAppliesTo", () => {
  it("applies when the ended source is the current one", () => {
    expect(endedEventAppliesTo("a", ["a", null])).toBe(true);
  });
  it("applies when the ended source is the one the backend started instead (a fallback file)", () => {
    expect(endedEventAppliesTo("fallback", ["queued", "fallback"])).toBe(true);
  });
  it("does not apply when a different track is current", () => {
    expect(endedEventAppliesTo("a", ["b", "b"])).toBe(false);
  });
  it("does not apply when nothing is current but a source id was given", () => {
    expect(endedEventAppliesTo("a", [undefined, null])).toBe(false);
  });
  it("applies when the backend sent no source id", () => {
    expect(endedEventAppliesTo("", ["b"])).toBe(true);
    expect(endedEventAppliesTo("", [undefined, null])).toBe(true);
  });
});
