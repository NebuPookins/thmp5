import { memo } from "react";

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

export type FileIssueListActions = {
  onContextMenu: (e: React.MouseEvent, filePath: string) => void;
  onClearManual: (filePath: string) => void;
  onRelinkMissing: (issue: FileIssue) => void;
  onRemoveMissing: (issue: FileIssue) => void;
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
};

export const FileIssueList = memo(function FileIssueList({
  issues,
  firstDupIndexByFile,
  deletingBackups,
  actions,
}: Props) {
  const keys = issueRowKeys(issues);
  return (
    <ol className="issue-list">
      {issues.map((issue, i) => {
        const isFirstDupForFile =
          issue.kind === "duplicate_frame" &&
          firstDupIndexByFile.get(issue.file_path) === i;
        const sourceId = issue.source_id;
        const backupPath = issue.backup_path;
        return (
          <li
            key={keys[i]}
            className="issue-item"
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
