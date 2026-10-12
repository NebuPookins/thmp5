import { parsePlayedAt } from "./autoDj";

const MS_PER_DAY = 24 * 60 * 60 * 1000;

function localDateString(date: Date): string {
  const yyyy = date.getFullYear().toString().padStart(4, "0");
  const mm = (date.getMonth() + 1).toString().padStart(2, "0");
  const dd = date.getDate().toString().padStart(2, "0");
  return `${yyyy}-${mm}-${dd}`;
}

/** Whole local calendar days from `from` to `to` (DST-safe). */
function calendarDaysBetween(from: Date, to: Date): number {
  const fromMidnight = Date.UTC(from.getFullYear(), from.getMonth(), from.getDate());
  const toMidnight = Date.UTC(to.getFullYear(), to.getMonth(), to.getDate());
  return Math.round((toMidnight - fromMidnight) / MS_PER_DAY);
}

function formatAgo(days: number): string {
  if (days <= 0) return "today";
  return days === 1 ? "1 day ago" : `${days} days ago`;
}

/**
 * Describe when a recording was last played, e.g.
 * "last played: 2026-10-10 (1 day ago)". The date is shown in local time.
 */
export function describeLastPlayed(lastPlayed: string | null, now: Date): string {
  if (lastPlayed === null) return "last played: never";
  const playedMs = parsePlayedAt(lastPlayed);
  if (playedMs === null) return "last played: unknown";
  const played = new Date(playedMs);
  return `last played: ${localDateString(played)} (${formatAgo(calendarDaysBetween(played, now))})`;
}
