import { describe, expect, it } from "vitest";
import { NO_TRACKS, applyTrackEdits, sameTrack, buildTrackSet, hasTrack, loveToggleAction, withTrack } from "./lastfmLove";

describe("loveToggleAction", () => {
  it("loves an unloved track", () => {
    expect(loveToggleAction(false)).toEqual({
      command: "lastfm_love_track",
      nextLoved: true,
    });
  });

  it("unloves a loved track", () => {
    expect(loveToggleAction(true)).toEqual({
      command: "lastfm_unlove_track",
      nextLoved: false,
    });
  });
});

describe("track set", () => {
  const set = buildTrackSet([{ artist: "Björk", track: "Hyperballad" }]);

  it("finds loved tracks case-insensitively", () => {
    expect(hasTrack(set, "BJÖRK", "hyperballad")).toBe(true);
  });

  it("treats composed and decomposed unicode as the same", () => {
    expect(hasTrack(set, "Bjo\u0308rk", "Hyperballad")).toBe(true);
  });

  it("does not find other tracks or a different artist", () => {
    expect(hasTrack(set, "Björk", "Army of Me")).toBe(false);
    expect(hasTrack(set, "Other", "Hyperballad")).toBe(false);
  });

  it("is never loved without an artist", () => {
    expect(hasTrack(set, null, "Hyperballad")).toBe(false);
  });

  it("is never present when the set is unavailable", () => {
    expect(hasTrack(null, "Björk", "Hyperballad")).toBe(false);
  });

  it("NO_TRACKS contains nothing", () => {
    expect(hasTrack(NO_TRACKS, "Björk", "Hyperballad")).toBe(false);
  });

  it("an empty list loves nothing", () => {
    expect(hasTrack(buildTrackSet([]), "Björk", "Hyperballad")).toBe(false);
  });

  it("withTrack adds without mutating the original", () => {
    const next = withTrack(set, "A", "T", true);
    expect(hasTrack(next, "a", "t")).toBe(true);
    expect(hasTrack(next, "Björk", "Hyperballad")).toBe(true);
    expect(hasTrack(set, "A", "T")).toBe(false);
  });

  it("withTrack changes nothing when the track is already in the requested state", () => {
    expect(hasTrack(withTrack(set, "björk", "hyperballad", true), "Björk", "Hyperballad")).toBe(true);
    expect(hasTrack(withTrack(set, "A", "T", false), "A", "T")).toBe(false);
  });

  it("withTrack removes without mutating the original", () => {
    const next = withTrack(set, "björk", "HYPERBALLAD", false);
    expect(hasTrack(next, "Björk", "Hyperballad")).toBe(false);
    expect(hasTrack(set, "Björk", "Hyperballad")).toBe(true);
  });
});

describe("sameTrack", () => {
  it("matches case-insensitively and across unicode forms", () => {
    expect(sameTrack("Björk", "Hyperballad", "BJÖRK", "hyperballad")).toBe(true);
    expect(sameTrack("Bjo\u0308rk", "Hyperballad", "Björk", "Hyperballad")).toBe(true);
  });

  it("differs on artist or title", () => {
    expect(sameTrack("A", "T", "B", "T")).toBe(false);
    expect(sameTrack("A", "T", "A", "U")).toBe(false);
  });

  it("never matches without an artist", () => {
    expect(sameTrack(null, "T", null, "T")).toBe(false);
    expect(sameTrack(null, "T", "A", "T")).toBe(false);
    expect(sameTrack("A", "T", null, "T")).toBe(false);
  });
});

describe("applyTrackEdits", () => {
  const base = buildTrackSet([{ artist: "A", track: "Old" }]);

  it("applies loves and unloves", () => {
    const out = applyTrackEdits(base, [
      { artist: "B", title: "New", loved: true },
      { artist: "a", title: "old", loved: false },
    ]);
    expect(hasTrack(out, "B", "New")).toBe(true);
    expect(hasTrack(out, "A", "Old")).toBe(false);
  });

  it("lets a later edit for the same track win", () => {
    const loveThenUnlove = applyTrackEdits(base, [
      { artist: "B", title: "T", loved: true },
      { artist: "B", title: "T", loved: false },
    ]);
    expect(hasTrack(loveThenUnlove, "B", "T")).toBe(false);
    const unloveThenLove = applyTrackEdits(base, [
      { artist: "A", title: "Old", loved: false },
      { artist: "A", title: "Old", loved: true },
    ]);
    expect(hasTrack(unloveThenLove, "A", "Old")).toBe(true);
  });

  it("does not mutate the input and handles no edits", () => {
    applyTrackEdits(base, [{ artist: "B", title: "T", loved: true }]);
    expect(hasTrack(base, "B", "T")).toBe(false);
    expect(hasTrack(applyTrackEdits(base, []), "A", "Old")).toBe(true);
  });
});
