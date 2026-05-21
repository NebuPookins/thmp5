-- Clear raw_tags_json so the next startup scan re-reads all files with the
-- corrected scanner (which now correctly handles UFID frames that store the
-- MusicBrainz Recording ID, e.g. UFID:http://musicbrainz.org).
UPDATE source SET raw_tags_json = NULL;
