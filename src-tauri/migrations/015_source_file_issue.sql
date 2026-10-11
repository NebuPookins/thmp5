-- A user may attach one persistent, free-form note to a source file.
CREATE TABLE source_file_issue (
    source_id TEXT PRIMARY KEY REFERENCES source(id) ON DELETE CASCADE,
    note      TEXT NOT NULL CHECK(length(trim(note)) > 0),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
