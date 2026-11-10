function decomposeDuration(durationMs: number) {
  const totalSeconds = Math.floor(durationMs / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  return { hours, minutes, seconds };
}

export function formatDuration(durationMs: number | null): string {
  if (!durationMs || durationMs <= 0) {
    return "Unknown";
  }

  const { hours, minutes, seconds } = decomposeDuration(durationMs);

  if (hours > 0) {
    return `${hours}:${minutes.toString().padStart(2, "0")}:${seconds
      .toString()
      .padStart(2, "0")}`;
  }

  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

export function formatDurationCompact(durationMs: number): string {
  if (durationMs <= 0) return "0m00s";

  const { hours, minutes, seconds } = decomposeDuration(durationMs);

  if (hours > 0) {
    return `${hours}h${minutes.toString().padStart(2, "0")}m${seconds
      .toString()
      .padStart(2, "0")}s`;
  }
  return `${minutes}m${seconds.toString().padStart(2, "0")}s`;
}
