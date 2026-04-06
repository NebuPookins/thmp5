-- Covering index for the three per-row source subqueries in list_recordings /
-- evaluate_smart_playlist.  Without this, every correlated subquery does a
-- full scan of the source rows for each recording filtered only by recording_id.
-- With (recording_id, source_type, file_path) SQLite can satisfy those
-- subqueries with an index-range scan and return file_path without touching
-- the table at all.
CREATE INDEX IF NOT EXISTS idx_source_type_path
    ON source (recording_id, source_type, file_path);
