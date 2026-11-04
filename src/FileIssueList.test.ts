import { describe, expect, it } from "vitest";
import { issueRowKeys, type FileIssue } from "./FileIssueList";

const issue = (over: Partial<FileIssue> = {}): FileIssue => ({
  file_path: "/a.mp3",
  kind: "manual",
  message: "bad",
  ...over,
});

describe("issueRowKeys", () => {
  it("gives every row a distinct key, even for identical issues", () => {
    const keys = issueRowKeys([issue(), issue(), issue({ message: "other" })]);
    expect(new Set(keys).size).toBe(3);
  });

  it("keeps a row's key when other rows are removed or reordered", () => {
    const a = issue({ file_path: "/a.mp3" });
    const b = issue({ file_path: "/b.mp3" });
    const before = issueRowKeys([a, b]);
    const after = issueRowKeys([b]);
    expect(after[0]).toBe(before[1]);
  });
});
