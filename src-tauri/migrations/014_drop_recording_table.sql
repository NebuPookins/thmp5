-- Drop the recording table. Recordings become a fully in-memory derived concept:
-- the MemoryCatalog groups sources by MBID (from raw_tags_json), file_hash, and
-- fingerprint at load time using a union-find algorithm.
--
-- The source table stores only directly-observable facts (raw ID3 tag assertions,
-- computed hashes, file properties) — no interpreted/derived grouping data.

-- 1. Migrate play_history: drop recording_id, make source_id NOT NULL.
--    For rows where source_id was NULL, pick the canonical source for that recording.
PRAGMA foreign_keys = OFF;

CREATE TABLE play_history_new (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id          TEXT    NOT NULL REFERENCES source(id) ON DELETE CASCADE,
    played_at          TEXT    NOT NULL DEFAULT (datetime('now')),
    duration_played_ms INTEGER
);

INSERT INTO play_history_new (id, source_id, played_at, duration_played_ms)
    SELECT ph.id,
        COALESCE(ph.source_id,
            (SELECT id FROM source
             WHERE recording_id = ph.recording_id
             ORDER BY source_type = 'local_file' DESC, file_path ASC LIMIT 1)
        ),
        ph.played_at, ph.duration_played_ms
    FROM play_history ph;

DROP TABLE play_history;
ALTER TABLE play_history_new RENAME TO play_history;
CREATE INDEX idx_play_history_source ON play_history (source_id);
CREATE INDEX idx_play_history_played_at ON play_history (played_at);

-- 2. Migrate playlist_track: switch from recording_id to source_id.
CREATE TABLE playlist_track_new (
    playlist_id  INTEGER NOT NULL REFERENCES playlist(id) ON DELETE CASCADE,
    source_id    TEXT    NOT NULL REFERENCES source(id) ON DELETE CASCADE,
    position     INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, position)
);

INSERT INTO playlist_track_new (playlist_id, source_id, position)
    SELECT pt.playlist_id,
        COALESCE(
            (SELECT id FROM source
             WHERE recording_id = pt.recording_id AND source_type = 'local_file'
             ORDER BY file_path ASC LIMIT 1),
            (SELECT id FROM source
             WHERE recording_id = pt.recording_id LIMIT 1)
        ),
        pt.position
    FROM playlist_track pt;

DROP TABLE playlist_track;
ALTER TABLE playlist_track_new RENAME TO playlist_track;

-- 3. Drop recording_tag (tags derived from source raw_tags_json at catalog load).
DROP TABLE IF EXISTS recording_tag;

-- 4. Recreate source without recording_id FK column.
CREATE TABLE source_new (
    id                      TEXT PRIMARY KEY,
    source_type             TEXT NOT NULL CHECK(source_type IN ('local_file','youtube','http_stream')),
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
    file_mtime_ms           INTEGER,
    track_total             INTEGER,
    raw_tags_json           TEXT
);

INSERT INTO source_new (id, source_type, file_path, file_hash, format, url, duration_ms,
    fingerprint, last_verified, replay_gain_track_db, replay_gain_track_peak,
    replay_gain_album_db, replay_gain_album_peak, lufs, file_size, file_mtime_ms,
    track_total, raw_tags_json)
SELECT id, source_type, file_path, file_hash, format, url, duration_ms,
    fingerprint, last_verified, replay_gain_track_db, replay_gain_track_peak,
    replay_gain_album_db, replay_gain_album_peak, lufs, file_size, file_mtime_ms,
    track_total, raw_tags_json
FROM source;

DROP TABLE source;
ALTER TABLE source_new RENAME TO source;

CREATE INDEX idx_source_hash ON source (file_hash);
CREATE INDEX idx_source_file_path_identity ON source (file_path, file_size, file_mtime_ms);

-- 5. Drop recording table (no more FK references to it).
DROP TABLE IF EXISTS recording;

PRAGMA foreign_keys = ON;
