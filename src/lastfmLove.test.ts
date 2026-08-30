import { describe, expect, it } from "vitest";
import { loveToggleAction } from "./lastfmLove";

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
