import { describe, expect, it } from "vitest";
import { nowPlayingOf } from "./RelinkModal";

describe("nowPlayingOf", () => {
  it("is null for a source that is not the current track", () => {
    expect(nowPlayingOf("a", "b", "a", "playing")).toBeNull();
    expect(nowPlayingOf("a", null, null, "stopped")).toBeNull();
  });

  it("is loading while the player has not caught up with the current track", () => {
    expect(nowPlayingOf("a", "a", "b", "playing")).toBe("loading");
    expect(nowPlayingOf("a", "a", null, "stopped")).toBe("loading");
  });

  it("is loading while the player is loading it", () => {
    expect(nowPlayingOf("a", "a", "a", "loading")).toBe("loading");
  });

  it("is playing or paused once the player has it", () => {
    expect(nowPlayingOf("a", "a", "a", "playing")).toBe("playing");
    expect(nowPlayingOf("a", "a", "a", "paused")).toBe("paused");
  });

  it("is null once the player has stopped it", () => {
    expect(nowPlayingOf("a", "a", "a", "stopped")).toBeNull();
  });
});
