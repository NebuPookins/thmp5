import { invoke } from "@tauri-apps/api/core";

export type FrontendLogLevel = "debug" | "info" | "warn" | "error";

/**
 * Write to the backend app log (fire-and-forget) and mirror to the console.
 * Never throws: a logging failure must not break the caller.
 */
export function frontendLog(
  level: FrontendLogLevel,
  message: string,
  context?: Readonly<Record<string, unknown>>,
): void {
  console[level](`[frontend] ${message}`, context ?? "");
  invoke("log_frontend", { level, message, context: context ?? null }).catch((error: unknown) =>
    console.error("[frontend] failed to forward log to backend", error),
  );
}
