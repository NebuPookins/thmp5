ALTER TABLE source ADD COLUMN file_size INTEGER;
ALTER TABLE source ADD COLUMN file_mtime_ms INTEGER;

CREATE INDEX IF NOT EXISTS idx_source_file_path_identity
ON source (file_path, file_size, file_mtime_ms);
