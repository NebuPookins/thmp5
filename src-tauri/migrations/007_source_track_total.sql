-- Store each source file's reported track_total tag value so we can
-- determine release completeness by checking consensus across sources.
ALTER TABLE source ADD COLUMN track_total INTEGER;
