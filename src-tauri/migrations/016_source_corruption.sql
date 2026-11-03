-- When the whole audio stream was last decoded to look for corruption. NULL means it has
-- never been checked (e.g. imported before the check existed); the background integrity
-- backfill works through those rows.
ALTER TABLE source ADD COLUMN integrity_checked_at TEXT;

-- Human-readable description of detected audio stream corruption (frames the decoder
-- rejects, or a stream that dies mid-file). NULL means no known problem. Only meaningful
-- once the file has been checked, which the constraint enforces.
ALTER TABLE source ADD COLUMN corruption TEXT
    CHECK (corruption IS NULL OR integrity_checked_at IS NOT NULL);
