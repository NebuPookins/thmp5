-- Move user rating from recording level to source level.
-- Each source file now carries its own independent rating; a recording's
-- rating is computed at query time as the hierarchical average of its sources.

CREATE TABLE source_rating (
    source_id  TEXT    PRIMARY KEY REFERENCES source(id) ON DELETE CASCADE,
    stars      INTEGER NOT NULL CHECK(stars BETWEEN 1 AND 5),
    updated_at TEXT    NOT NULL DEFAULT (datetime('now'))
);

-- Migrate existing recording-level ratings to the canonical local_file source
-- for each recording (smallest file_path), falling back to any source.
INSERT OR IGNORE INTO source_rating (source_id, stars, updated_at)
SELECT sid, stars, updated_at FROM (
    SELECT
        COALESCE(
            (SELECT id FROM source
             WHERE recording_id = ur.recording_id AND source_type = 'local_file'
             ORDER BY file_path ASC LIMIT 1),
            (SELECT id FROM source
             WHERE recording_id = ur.recording_id
             ORDER BY id ASC LIMIT 1)
        ) AS sid,
        ur.stars,
        ur.updated_at
    FROM user_rating ur
) WHERE sid IS NOT NULL;

DROP TABLE user_rating;
