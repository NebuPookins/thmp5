import { memo, useEffect, useMemo, useRef, useState } from "react";
import { frontendLog } from "./frontendLog";

export type FileIssue = {
  file_path: string;
  kind: "manual" | "import_error" | "playback_error" | "duplicate_frame" | "backup_file_exists" | "duration_mismatch" | "corrupt_audio" | "missing_file";
  message: string;
  source_id?: string;
  recording_id?: string;
  frame_id?: string;
  field_name?: string;
  lofty_value?: string;
  corrected_value?: string;
  backup_path?: string;
};

export const fileIssueLabel = (kind: FileIssue["kind"]): string => ({
  manual: "File issue",
  import_error: "Import error",
  playback_error: "Playback error",
  duplicate_frame: "Duplicate tag",
  backup_file_exists: "Backup file",
  duration_mismatch: "Duration mismatch",
  corrupt_audio: "Corrupt audio",
  missing_file: "Missing file",
}[kind]);

/**
 * One React key per issue, derived from the issue's content so it survives
 * reordering and removal of other rows. Fully identical issues are told apart
 * by their occurrence number.
 */
export function issueRowKeys(issues: readonly FileIssue[]): string[] {
  const seen = new Map<string, number>();
  return issues.map((issue) => {
    const base = JSON.stringify(issue);
    const occurrence = seen.get(base) ?? 0;
    seen.set(base, occurrence + 1);
    return `${base}#${occurrence}`;
  });
}

/** Index of the first issue whose file is in `filePaths`, or undefined when there is none. */
export function findFirstIssueIndex(
  issues: readonly FileIssue[],
  filePaths: readonly string[],
): number | undefined {
  const wanted = new Set(filePaths);
  const index = issues.findIndex((issue) => wanted.has(issue.file_path));
  return index === -1 ? undefined : index;
}

export type FileIssueListActions = {
  onContextMenu: (e: React.MouseEvent, filePath: string) => void;
  onClearManual: (filePath: string) => void;
  onRelinkMissing: (issue: FileIssue) => void;
  onRemoveMissing: (issue: FileIssue) => void;
  onReplaceCorrupt: (issue: FileIssue) => void;
  onOpenMerge: (filePath: string) => void;
  onPlayFile: (issue: FileIssue) => void;
  onDeleteBackup: (backupPath: string) => void;
};

type Props = {
  issues: readonly FileIssue[];
  firstDupIndexByFile: ReadonlyMap<string, number>;
  deletingBackups: ReadonlySet<string>;
  /** Must be referentially stable, or memoization is defeated. */
  actions: FileIssueListActions;
  /**
   * Files whose first issue should be scrolled to and flashed. The list calls
   * `onRevealHandled` once it has acted on them; the owner then clears this.
   */
  revealPaths: readonly string[] | null;
  /** Must be referentially stable, or memoization is defeated. */
  onRevealHandled: () => void;
};

export const FileIssueList = memo(function FileIssueList({
  issues,
  firstDupIndexByFile,
  deletingBackups,
  actions,
  revealPaths,
  onRevealHandled,
}: Props) {
  const keys = useMemo(() => issueRowKeys(issues), [issues]);
  const listRef = useRef<HTMLOListElement>(null);
  const [highlightedKey, setHighlightedKey] = useState<string | null>(null);

  useEffect(() => {
    if (revealPaths === null) return;
    onRevealHandled();
    const index = findFirstIssueIndex(issues, revealPaths);
    const key = index === undefined ? undefined : keys[index];
    const row = key === undefined
      ? undefined
      : Array.from(listRef.current?.children ?? []).find(
        (el) => el instanceof HTMLElement && el.dataset.issueKey === key,
      );
    if (key === undefined || row === undefined) {
      frontendLog("warn", "File Issues: no issue row found to reveal", {
        filePaths: revealPaths,
        issueCount: issues.length,
      });
      return;
    }
    row.scrollIntoView({ block: "nearest", behavior: "smooth" });
    setHighlightedKey(key);
  }, [revealPaths, onRevealHandled, issues, keys]);

  return (
    <ol className="issue-list" ref={listRef}>
      {issues.map((issue, i) => {
        const isFirstDupForFile =
          issue.kind === "duplicate_frame" &&
          firstDupIndexByFile.get(issue.file_path) === i;
        const sourceId = issue.source_id;
        const backupPath = issue.backup_path;
        return (
          <li
            key={keys[i]}
            data-issue-key={keys[i]}
            className={keys[i] === highlightedKey ? "issue-item issue-item-highlight" : "issue-item"}
            onAnimationEnd={(e) => { if (e.target === e.currentTarget) setHighlightedKey(null); }}
            onContextMenu={(e) => actions.onContextMenu(e, issue.file_path)}
          >
            <div className="issue-item-header">
              <span className={`issue-kind issue-kind-${issue.kind}`}>
                {fileIssueLabel(issue.kind)}
              </span>
              {issue.kind === "manual" ? (
                <button
                  className="issue-action-btn"
                  type="button"
                  onClick={() => actions.onClearManual(issue.file_path)}
                >
                  Clear
                </button>
              ) : issue.kind === "missing_file" && sourceId !== undefined ? (
                <>
                  <button
                    className="issue-action-btn"
                    type="button"
                    title="Move this source's plays, rating and playlist entries onto another library file"
                    onClick={() => actions.onRelinkMissing(issue)}
                  >
                    Relink…
                  </button>
                  <button
                    className="issue-action-btn"
                    type="button"
                    title="Delete this source and its play history, rating and playlist entries"
                    onClick={() => actions.onRemoveMissing(issue)}
                  >
                    Remove
                  </button>
                </>
              ) : issue.kind === "corrupt_audio" && sourceId !== undefined ? (
                <button
                  className="issue-action-btn"
                  type="button"
                  title="Replace this corrupt file with another library source"
                  onClick={() => actions.onReplaceCorrupt(issue)}
                >
                  Replace…
                </button>
              ) : issue.kind === "duplicate_frame" ? (
                isFirstDupForFile ? (
                  <button
                    className="issue-action-btn"
                    type="button"
                    onClick={() => actions.onOpenMerge(issue.file_path)}
                  >
                    Resolve merge…
                  </button>
                ) : null
              ) : issue.kind === "backup_file_exists" ? (
                <>
                  <button
                    className="issue-action-btn"
                    type="button"
                    title="Play the current file to confirm it's still valid before deleting the backup"
                    onClick={() => actions.onPlayFile(issue)}
                  >
                    Play file
                  </button>
                  {backupPath !== undefined && (
                    <button
                      className="issue-action-btn"
                      type="button"
                      disabled={deletingBackups.has(backupPath)}
                      onClick={() => actions.onDeleteBackup(backupPath)}
                    >
                      {deletingBackups.has(backupPath) ? "Deleting…" : "Delete backup"}
                    </button>
                  )}
                </>
              ) : null}
            </div>
            <span className="issue-path" title={issue.file_path}>
              {issue.file_path}
            </span>
            <span className="issue-message">{issue.message}</span>
          </li>
        );
      })}
    </ol>
  );
});
