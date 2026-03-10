-- thmp5 initial schema
-- MusicBrainz-inspired model

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- ─────────────────────────────────────────────────────────────
-- Core musical entities
-- ─────────────────────────────────────────────────────────────

-- An artist is generally a musician (or musician persona), group of musicians, or other music professional (like a producer or engineer). Occasionally, it can also be a non-musical person (like a photographer, an illustrator, or a poet whose writings are set to music), or even a fictional character. Several special artists should exist in the database to deal with unusual cases.
-- [anonymous] - The track artist is both unknown and unknowable. If you are not sure whether to use [anonymous] or [unknown], use [unknown].
-- [dialogue] - Used for tracks which consist solely of dialogue, and for which a better credit can't be found (as such, this can be seen as a more specific "unknown/anonymous"). If a better credit can be found, use that if it's at all sensible. For example, the "Royale With Cheese" discussion from Pulp Fiction would be best assigned to "Samuel L. Jackson & John Travolta", not to [dialogue], since the actors talking are known even if not directly credited on the tracklist. If it seems against common sense to list all speakers in the artist credit, such as for a recording including several dozen speakers, then using [dialogue] is always an acceptable option. In any case, the speakers' performances within the track should always be indicated by a vocals relationship (generally spoken vocals), if at all possible.
-- [no artist] - There is no artist. (Recorded silence or noise, if it is attributed to any artist, should be assigned to that artist and not to [no artist].)
-- [traditional] - Used for writer/composer (and related) credits when the work in question pre-dates any written or recorded version(s), having originally been passed down from musician to musician. Rather than being attributable to one [anonymous] or [unknown] artist/collaboration, this work is better described as having emerged from a musical tradition.
-- [unknown] - Used when a particular artist is unknown, but is potentially discoverable with further research. Please do some research before using this artist; this artist should not be used simply as a "lazy easy solution" to an artist you don't happen to know offhand.
-- Various Artists - Used only for compilation-type releases or release groups containing tracks by multiple different artists. This artist shouldn't generally be used for recordings, tracks or works.
CREATE TABLE IF NOT EXISTS artist (
    id        TEXT PRIMARY KEY,
    name      TEXT NOT NULL, -- The official name of an artist, be it a person or a band.
    sort_name TEXT NOT NULL,
    mbid      TEXT UNIQUE  -- MusicBrainz artist ID, if known
);

CREATE INDEX IF NOT EXISTS idx_artist_name_lower ON artist (lower(name));

-- Musical composition (the abstract work). A work is a distinct intellectual or artistic creation, which can be expressed in the form of one or more audio recordings. While a work in is usually musical in nature, it is not necessarily so. For example, a work could be a novel, play, poem or essay, later recorded as an oratory or audiobook.
CREATE TABLE IF NOT EXISTS work (
    id   TEXT PRIMARY KEY,
    title TEXT NOT NULL, -- The canonical title of the work, expressed in the language it was originally written.
    mbid TEXT UNIQUE
);

-- A specific performance/master recording of a work (or standalone)
-- This is the primary entity for user data: ratings, play history.
CREATE TABLE IF NOT EXISTS recording (
    id                  TEXT    PRIMARY KEY,
    title               TEXT    NOT NULL,
    work_id             TEXT    REFERENCES work(id),
    -- Duration is stored on source too, but we cache a canonical value here.
    duration_ms         INTEGER,
    mbid                TEXT    UNIQUE,
    acoustid            TEXT    UNIQUE,   -- AcoustID fingerprint ID
    -- Tag fields queryable in smart playlists
    genre               TEXT,
    bpm                 REAL,
    comment             TEXT,
    -- Override for the full rendered credit string (e.g. "Jay-Z feat. Beyoncé")
    -- when programmatic reconstruction doesn't match the printed credit.
    artist_credit_text  TEXT,
    created_at          TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_recording_title_lower ON recording (lower(title));

-- Direct junction: which artists are credited on a recording and in what role
CREATE TABLE IF NOT EXISTS recording_artist (
    recording_id TEXT    NOT NULL REFERENCES recording(id) ON DELETE CASCADE,
    artist_id    TEXT    NOT NULL REFERENCES artist(id),
    position     INTEGER NOT NULL,          -- display order (0 = primary)
    role         TEXT    NOT NULL DEFAULT 'main',  -- main | featuring | with | vs | producer | remixer
    credited_as  TEXT,                      -- NULL = use artist.name
    PRIMARY KEY (recording_id, position)
);

-- An album/EP/single as a concept. A release group is used to group releases into a single logical entity. Every release belongs to one, and only one, release group. Both release groups and releases are "albums" in a general sense. But there is an important difference: a release is something you can buy, such as a CD or a digital download, while a release group embraces the overall concept of an album. When an artist says "We've released our new album", they're talking about a release group. When their publisher says "This new album gets released next week in Japan, and next month in Europe", they're referring to two different releases that belong in the aforementioned release group.
CREATE TABLE IF NOT EXISTS release_group (
    id                 TEXT PRIMARY KEY,
    title              TEXT NOT NULL,
    rg_type            TEXT,   -- album | single | ep | compilation | other
    mbid               TEXT UNIQUE,
    -- Override for the full rendered credit string
    artist_credit_text TEXT
);

CREATE INDEX IF NOT EXISTS idx_release_group_title_lower ON release_group (lower(title));

-- Direct junction: which artists are credited on a release group and in what role
CREATE TABLE IF NOT EXISTS release_group_artist (
    release_group_id TEXT    NOT NULL REFERENCES release_group(id) ON DELETE CASCADE,
    artist_id        TEXT    NOT NULL REFERENCES artist(id),
    position         INTEGER NOT NULL,
    role             TEXT    NOT NULL DEFAULT 'main',
    credited_as      TEXT,
    PRIMARY KEY (release_group_id, position)
);

-- A specific edition/pressing of a release group. If you walk into (or digitally browse) a store and see the standard edition of an album next to a deluxe edition of that same album, you're looking at two different releases, which are grouped as part of the same release group.
CREATE TABLE IF NOT EXISTS release (
    id               TEXT PRIMARY KEY,
    release_group_id TEXT NOT NULL REFERENCES release_group(id),
    title            TEXT NOT NULL,   -- may differ from release group title
    release_date     TEXT,            -- YYYY, YYYY-MM, or YYYY-MM-DD
    country          TEXT,
    label            TEXT,
    catalog_number   TEXT,
    mbid             TEXT UNIQUE
);

-- A disc within a release (most releases have exactly one)
CREATE TABLE IF NOT EXISTS medium (
    id         TEXT    PRIMARY KEY,
    release_id TEXT    NOT NULL REFERENCES release(id) ON DELETE CASCADE,
    position   INTEGER NOT NULL DEFAULT 1,  -- disc number
    format     TEXT    -- CD | Vinyl | Digital | …
);

-- A recording's appearance on a medium at a specific position
CREATE TABLE IF NOT EXISTS track (
    id           TEXT    PRIMARY KEY,
    medium_id    TEXT    NOT NULL REFERENCES medium(id) ON DELETE CASCADE,
    recording_id TEXT    NOT NULL REFERENCES recording(id),
    position     INTEGER NOT NULL,
    title        TEXT,            -- override; NULL = use recording.title
    duration_ms  INTEGER          -- override; NULL = use recording.duration_ms
);

CREATE INDEX IF NOT EXISTS idx_track_recording ON track (recording_id);
CREATE INDEX IF NOT EXISTS idx_track_medium     ON track (medium_id);

-- ─────────────────────────────────────────────────────────────
-- Sources: concrete audio providers for recordings
-- ─────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS source (
    id                      TEXT PRIMARY KEY,
    recording_id            TEXT NOT NULL REFERENCES recording(id) ON DELETE CASCADE,
    source_type             TEXT NOT NULL CHECK(source_type IN ('local_file', 'youtube', 'http_stream')),
    -- local_file
    file_path               TEXT UNIQUE,
    file_hash               TEXT UNIQUE,    -- SHA-256 hex; used for duplicate detection
    format                  TEXT,           -- mp3 | ogg | flac | wav | aac | opus | …
    -- youtube / stream
    url                     TEXT,
    -- shared
    duration_ms             INTEGER,
    fingerprint             TEXT,           -- raw Chromaprint fingerprint string
    last_verified           TEXT,           -- datetime we last confirmed source is accessible
    -- Perceptual loudness (populated from tags on import; LUFS from future analysis pass)
    replay_gain_track_db    REAL,           -- dB adjustment (negative = attenuate)
    replay_gain_track_peak  REAL,           -- peak amplitude 0.0–1.0
    replay_gain_album_db    REAL,
    replay_gain_album_peak  REAL,
    lufs                    REAL            -- NULL until analysis pass runs
);

CREATE INDEX IF NOT EXISTS idx_source_recording ON source (recording_id);
CREATE INDEX IF NOT EXISTS idx_source_hash      ON source (file_hash);

-- ─────────────────────────────────────────────────────────────
-- User data
-- ─────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS user_rating (
    recording_id TEXT    PRIMARY KEY REFERENCES recording(id) ON DELETE CASCADE,
    stars        INTEGER NOT NULL CHECK(stars BETWEEN 1 AND 5),
    updated_at   TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS release_group_rating (
    release_group_id TEXT    PRIMARY KEY REFERENCES release_group(id) ON DELETE CASCADE,
    stars            INTEGER NOT NULL CHECK(stars BETWEEN 1 AND 5),
    updated_at       TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS play_history (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    recording_id       TEXT    NOT NULL REFERENCES recording(id) ON DELETE CASCADE,
    source_id          TEXT    REFERENCES source(id) ON DELETE SET NULL,
    played_at          TEXT    NOT NULL DEFAULT (datetime('now')),  -- UTC ISO-8601
    duration_played_ms INTEGER   -- how long actually played; < recording duration → skip
);

CREATE INDEX IF NOT EXISTS idx_play_history_recording ON play_history (recording_id);
CREATE INDEX IF NOT EXISTS idx_play_history_played_at ON play_history (played_at);

-- ─────────────────────────────────────────────────────────────
-- Playlists
-- ─────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS playlist (
    id    INTEGER PRIMARY KEY AUTOINCREMENT,
    name  TEXT    NOT NULL UNIQUE,
    kind  TEXT    NOT NULL CHECK(kind IN ('static', 'smart')),
    query TEXT    -- non-null and used for smart playlists
);

CREATE TABLE IF NOT EXISTS playlist_track (
    playlist_id  INTEGER NOT NULL REFERENCES playlist(id) ON DELETE CASCADE,
    recording_id TEXT    NOT NULL REFERENCES recording(id) ON DELETE CASCADE,
    position     INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, position)
);

-- ─────────────────────────────────────────────────────────────
-- Tags
-- ─────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS recording_tag (
    recording_id TEXT NOT NULL REFERENCES recording(id) ON DELETE CASCADE,
    tag          TEXT NOT NULL,
    PRIMARY KEY (recording_id, tag)
);

CREATE INDEX IF NOT EXISTS idx_recording_tag_tag ON recording_tag (tag);

-- ─────────────────────────────────────────────────────────────
-- Smart playlist view (used by query engine)
-- ─────────────────────────────────────────────────────────────

CREATE VIEW IF NOT EXISTS smart_playlist_view AS
SELECT
    r.id,
    r.title,
    r.duration_ms,
    r.genre,
    r.bpm,
    r.comment,
    ur.stars                               AS rating,
    rgr.stars                              AS album_rating,
    MAX(ph.played_at)                      AS last_played,
    COUNT(ph.id)                           AS play_count
FROM recording r
LEFT JOIN user_rating ur          ON ur.recording_id = r.id
LEFT JOIN track t                 ON t.recording_id  = r.id
LEFT JOIN medium m                ON m.id            = t.medium_id
LEFT JOIN release rel             ON rel.id          = m.release_id
LEFT JOIN release_group_rating rgr ON rgr.release_group_id = rel.release_group_id
LEFT JOIN play_history ph         ON ph.recording_id = r.id
GROUP BY r.id;
