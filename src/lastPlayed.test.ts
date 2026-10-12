import { describe, expect, it } from "vitest";
import { describeLastPlayed } from "./lastPlayed";

// Local-time timestamps so the expectations hold in any test timezone.
const now = new Date(2026, 9, 11, 9, 0, 0);
const iso = (d: Date) => d.toISOString();

describe("describeLastPlayed", () => {
  it("reports never-played recordings", () => {
    expect(describeLastPlayed(null, now)).toBe("last played: never");
  });

  it("reports unparseable timestamps as unknown", () => {
    expect(describeLastPlayed("not-a-date", now)).toBe("last played: unknown");
  });

  it("says today for a play earlier the same day", () => {
    expect(describeLastPlayed(iso(new Date(2026, 9, 11, 0, 30)), now)).toBe("last played: 2026-10-11 (today)");
  });

  it("counts calendar days, so late yesterday is 1 day ago", () => {
    expect(describeLastPlayed(iso(new Date(2026, 9, 10, 23, 30)), now)).toBe("last played: 2026-10-10 (1 day ago)");
  });

  it("pluralises multiple days", () => {
    expect(describeLastPlayed(iso(new Date(2026, 8, 1, 12, 0)), now)).toBe("last played: 2026-09-01 (40 days ago)");
  });

  it("accepts SQLite UTC datetime strings", () => {
    const played = new Date(Date.UTC(2026, 9, 5, 12, 0, 0));
    expect(describeLastPlayed("2026-10-05 12:00:00", now)).toBe(describeLastPlayed(played.toISOString(), now));
  });
});
