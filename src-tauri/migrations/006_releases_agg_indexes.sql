-- Support indexes for the releases_agg CTE used in list_recordings /
-- evaluate_smart_playlist.  The CTE joins track→medium→release→release_group;
-- medium.release_id and release.release_group_id have no index in the initial
-- schema, forcing full scans when driving those joins from the child side.
CREATE INDEX IF NOT EXISTS idx_medium_release
    ON medium (release_id);

CREATE INDEX IF NOT EXISTS idx_release_release_group
    ON release (release_group_id);
