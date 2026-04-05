-- Drop the UNIQUE constraint on file_hash so the same content can be registered
-- at multiple paths (e.g. when a file has been moved or copied).
-- SQLite does not support DROP CONSTRAINT, so we recreate the table.

PRAGMA foreign_keys = OFF;

CREATE TABLE source_new (
    id                      TEXT PRIMARY KEY,
    recording_id            TEXT NOT NULL REFERENCES recording(id) ON DELETE CASCADE,
    source_type             TEXT NOT NULL CHECK(source_type IN ('local_file', 'youtube', 'http_stream')),
    file_path               TEXT UNIQUE,
    file_hash               TEXT,
    format                  TEXT,
    url                     TEXT,
    duration_ms             INTEGER,
    fingerprint             TEXT,
    last_verified           TEXT,
    replay_gain_track_db    REAL,
    replay_gain_track_peak  REAL,
    replay_gain_album_db    REAL,
    replay_gain_album_peak  REAL,
    lufs                    REAL,
    file_size               INTEGER,
    file_mtime_ms           INTEGER
);

INSERT INTO source_new
    SELECT id, recording_id, source_type, file_path, file_hash, format, url,
           duration_ms, fingerprint, last_verified,
           replay_gain_track_db, replay_gain_track_peak,
           replay_gain_album_db, replay_gain_album_peak,
           lufs, file_size, file_mtime_ms
    FROM source;

DROP TABLE source;
ALTER TABLE source_new RENAME TO source;

CREATE INDEX IF NOT EXISTS idx_source_recording        ON source (recording_id);
CREATE INDEX IF NOT EXISTS idx_source_hash             ON source (file_hash);
CREATE INDEX IF NOT EXISTS idx_source_file_path_identity ON source (file_path, file_size, file_mtime_ms);

PRAGMA foreign_keys = ON;
