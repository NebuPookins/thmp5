-- Drop all tables whose data is now derived at runtime by MemoryCatalog from
-- source.raw_tags_json.  The recording table is kept as a minimal ID registry
-- because source.recording_id FK-references it; all other columns it carries
-- are dead weight but removing them requires a table-recreation dance in SQLite
-- that can wait for a future migration.
--
-- Drop order: children before parents to satisfy FK constraints.

DROP VIEW  IF EXISTS smart_playlist_view;

DROP TABLE IF EXISTS track;               -- recording_id -> recording, medium_id -> medium
DROP TABLE IF EXISTS medium;              -- release_id -> release
DROP TABLE IF EXISTS release;             -- release_group_id -> release_group
DROP TABLE IF EXISTS release_group_artist;-- release_group_id, artist_id
DROP TABLE IF EXISTS recording_artist;    -- recording_id -> recording, artist_id -> artist
DROP TABLE IF EXISTS release_group_rating;-- release_group_id -> release_group
DROP TABLE IF EXISTS release_group;
DROP TABLE IF EXISTS artist;
DROP TABLE IF EXISTS work;
