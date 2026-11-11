import { fileIssueLabel, type FileIssue } from "./FileIssueList";

/**
 * The small warning marker shown next to songs with file issues. Clicking it asks
 * the app to reveal the first related issue in the File Issues panel.
 */
export function FileIssueIndicator({ paths, issuesByPath, onReveal }: {
  paths: readonly string[];
  issuesByPath: ReadonlyMap<string, readonly FileIssue[]>;
  onReveal: (paths: readonly string[]) => void;
}) {
  const affected = paths.flatMap((path) => (issuesByPath.get(path) ?? []).map((issue) => ({ path, issue })));
  if (affected.length === 0) return null;
  const includePaths = paths.length > 1;
  const tooltip = affected.map(({ path, issue }) => (
    `${includePaths ? `${path}: ` : ""}${fileIssueLabel(issue.kind)} \u2014 ${issue.message}`
  )).join("\n");
  return (
    <button
      type="button"
      className="file-issue-indicator"
      title={`${tooltip}\n(click to show in File Issues)`}
      aria-label={`Show in File Issues: ${tooltip}`}
      onClick={(e) => { e.stopPropagation(); onReveal(paths); }}
      onDoubleClick={(e) => { e.stopPropagation(); }}
    >⚠</button>
  );
}
