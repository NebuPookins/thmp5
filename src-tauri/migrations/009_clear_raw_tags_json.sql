-- Clear raw_tags_json so the next startup scan re-reads all files with the
-- corrected scanner (which now correctly handles ID3v2.3 tags in files that
-- have large embedded artwork -- the previous scanner used big-endian instead
-- of synchsafe for the tag header size, causing read_exact to fail and
-- producing an empty [] for any ID3v2.3 file with a tag larger than ~128 bytes).
UPDATE source SET raw_tags_json = NULL;
