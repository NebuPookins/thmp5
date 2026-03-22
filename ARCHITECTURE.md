# thmp5 Architecture

## Project Name
**thmp5** — a personal music player.

---

## Platform Choice: Tauri 2 (Rust + TypeScript/React)

Tauri gives us:
- Rust backend for audio processing, fingerprinting, database access, subprocess management
- TypeScript/React frontend for rich UI with hot reload
- Native filesystem and process access (no browser sandbox)
- Much smaller binary than Electron (~3 MB overhead vs ~150 MB)
- Cross-platform: Linux, macOS, Windows

---

## High-Level Architecture

```
┌──────────────────────────────────────────────────┐
│  React/TypeScript UI  (Tauri webview)            │
│                                                  │
│  Library browser · Now Playing · Smart Playlist  │
│  editor · Query input · Settings                 │
└──────────────────┬───────────────────────────────┘
                   │  Tauri IPC  (typed commands + events)
┌──────────────────▼───────────────────────────────┐
│  Rust Core  (src-tauri/src/)                     │
│                                                  │
│  AudioEngine          LibraryManager             │
│  (symphonia + cpal)   (sqlx + SQLite)            │
│                                                  │
│  SourceManager        QueryEngine                │
│  (files, yt-dlp,      (pest parser →             │
│   HTTP streams)        SQL + duration post-filter)│
│                                                  │
│  FingerprintService   MetadataScanner            │
│  (rusty-chromaprint   (ID3v2, Vorbis comments,   │
│   + AcoustID HTTP)     FLAC, MP4 tags)           │
└──────────────────┬───────────────────────────────┘
                   │
            SQLite  (sqlx migrations)
```

---

## Data Model  (MusicBrainz-inspired)

### Core musical entities

```
Work
  A musical composition — the abstract "song" (e.g., Yesterday by Lennon/McCartney).
  Mostly populated from AcoustID/MusicBrainz lookups; may be null for unknown tracks.

Recording
  A specific performance/master recording of a Work (or a standalone recording).
  This is the primary entity for user data: ratings, play history, skip flags.
  Multiple Sources (files, URLs) resolve to the same Recording via fingerprinting.

Source
  A concrete audio provider for a Recording:
    - LocalFile: a path on disk, format (mp3/ogg/flac/wav/…), file hash
    - YouTube: a youtube.com/watch?v=… URL  (audio fetched via yt-dlp)
    - HttpStream: a direct audio stream URL
  A Recording may have many Sources (e.g., a local mp3 + a YouTube backup).

Track
  A Recording's appearance on a Medium, at a specific position, with an optional
  title override (e.g., "Yesterday (Live)" on a live release).

Medium
  A disc within a Release (most releases have one Medium).

Release
  A specific edition of a ReleaseGroup (e.g., 2011 remaster, Japanese pressing).
  Has label, catalog number, date, country, barcode.

ReleaseGroup
  An album/EP/single as a concept (e.g., Abbey Road).
  Type: album | single | EP | compilation | …

Artist
  A musician or band.

ArtistCredit
  A named credit on a Recording or Release, supporting:
    "The Beatles"  →  single credit
    "Jay-Z feat. Beyoncé"  →  two credits with join phrases
```

### User data entities

```
UserRating
  Per-Recording star rating (1–5).  Null = unrated.

ReleaseGroupRating
  Per-ReleaseGroup star rating (1–5), used as AlbumRating in smart playlists.

PlayHistory
  One row per play event: recording_id, played_at (UTC), source_id, duration_played_ms.
  Used to compute LastPlayed, PlayCount.

Playlist
  Named playlists.  Two kinds:
    Static:  an ordered list of recording_ids (PlaylistTrack rows)
    Smart:   a stored query string, evaluated at play time

PlaylistTrack
  recording_id + position for static playlists.

Tag / RecordingTag
  Free-form user tags on recordings.
```

### Full SQLite schema (abbreviated)

```sql
-- Artists
CREATE TABLE artist (
    id          TEXT PRIMARY KEY,   -- MBID or local UUID
    name        TEXT NOT NULL,
    sort_name   TEXT NOT NULL,
    mbid        TEXT UNIQUE         -- MusicBrainz ID if known
);

-- Works (compositions)
CREATE TABLE work (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    mbid        TEXT UNIQUE
);

-- Recordings
CREATE TABLE recording (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    work_id         TEXT REFERENCES work(id),
    duration_ms     INTEGER,
    mbid            TEXT UNIQUE,
    acoustid        TEXT UNIQUE,    -- AcoustID fingerprint ID
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Sources
CREATE TABLE source (
    id              TEXT PRIMARY KEY,
    recording_id    TEXT NOT NULL REFERENCES recording(id),
    source_type     TEXT NOT NULL CHECK(source_type IN ('local_file','youtube','http_stream')),
    -- local_file fields
    file_path       TEXT UNIQUE,
    file_hash       TEXT,           -- SHA-256 of file content
    format          TEXT,           -- mp3 | ogg | flac | wav | aac | …
    -- youtube / stream fields
    url             TEXT,
    -- shared
    duration_ms     INTEGER,
    fingerprint     TEXT,           -- raw Chromaprint fingerprint string
    last_verified   TEXT            -- datetime; when we last confirmed source is accessible
);

-- ReleaseGroups
CREATE TABLE release_group (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    rg_type     TEXT,               -- album | single | ep | compilation | …
    mbid        TEXT UNIQUE
);

-- Releases
CREATE TABLE release (
    id              TEXT PRIMARY KEY,
    release_group_id TEXT NOT NULL REFERENCES release_group(id),
    title           TEXT NOT NULL,  -- may differ from ReleaseGroup title (e.g., "Deluxe Edition")
    date            TEXT,           -- YYYY or YYYY-MM or YYYY-MM-DD
    country         TEXT,
    label           TEXT,
    catalog_number  TEXT,
    mbid            TEXT UNIQUE
);

-- Mediums (discs)
CREATE TABLE medium (
    id          TEXT PRIMARY KEY,
    release_id  TEXT NOT NULL REFERENCES release(id),
    position    INTEGER NOT NULL,   -- disc number
    format      TEXT                -- CD | Vinyl | Digital | …
);

-- Tracks
CREATE TABLE track (
    id              TEXT PRIMARY KEY,
    medium_id       TEXT NOT NULL REFERENCES medium(id),
    recording_id    TEXT NOT NULL REFERENCES recording(id),
    position        INTEGER NOT NULL,
    title           TEXT,           -- override; NULL = use recording.title
    duration_ms     INTEGER         -- override; NULL = use recording.duration_ms
);

-- Artist credits
CREATE TABLE artist_credit (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL       -- full rendered credit string
);
CREATE TABLE artist_credit_name (
    artist_credit_id    TEXT NOT NULL REFERENCES artist_credit(id),
    position            INTEGER NOT NULL,
    artist_id           TEXT NOT NULL REFERENCES artist(id),
    name                TEXT NOT NULL,      -- name as credited
    join_phrase         TEXT NOT NULL DEFAULT ''   -- " feat. ", " & ", ""
);

-- Link artist credits to recordings / releases
CREATE TABLE recording_artist_credit (
    recording_id        TEXT NOT NULL REFERENCES recording(id),
    artist_credit_id    TEXT NOT NULL REFERENCES artist_credit(id)
);
CREATE TABLE release_artist_credit (
    release_id          TEXT NOT NULL REFERENCES release(id),
    artist_credit_id    TEXT NOT NULL REFERENCES artist_credit(id)
);

-- User data
CREATE TABLE user_rating (
    recording_id    TEXT PRIMARY KEY REFERENCES recording(id),
    stars           INTEGER NOT NULL CHECK(stars BETWEEN 1 AND 5)
);
CREATE TABLE release_group_rating (
    release_group_id TEXT PRIMARY KEY REFERENCES release_group(id),
    stars            INTEGER NOT NULL CHECK(stars BETWEEN 1 AND 5)
);
CREATE TABLE play_history (
    id                  INTEGER PRIMARY KEY,
    recording_id        TEXT NOT NULL REFERENCES recording(id),
    source_id           TEXT REFERENCES source(id),
    played_at           TEXT NOT NULL DEFAULT (datetime('now')),  -- UTC ISO-8601
    duration_played_ms  INTEGER     -- how long we actually played (for skip detection)
);

-- Playlists
CREATE TABLE playlist (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    kind        TEXT NOT NULL CHECK(kind IN ('static','smart')),
    query       TEXT                -- non-null for smart playlists
);
CREATE TABLE playlist_track (
    playlist_id     INTEGER NOT NULL REFERENCES playlist(id),
    recording_id    TEXT NOT NULL REFERENCES recording(id),
    position        INTEGER NOT NULL
);
```

---

## Import Pipeline

When the user adds a file or directory:

1. **Scan** — enumerate files by extension (mp3, ogg, flac, wav, aac, …)
2. **Hash** — SHA-256 of file content → check `source.file_hash` for exact duplicate
3. **Decode sample** — use `symphonia` to decode the first ~2 min of audio
4. **Fingerprint** — `rusty-chromaprint` generates a Chromaprint fingerprint
5. **AcoustID lookup** — HTTP request to `api.acoustid.org` with fingerprint + duration
   - On hit: get Recording MBID → look up or import full MusicBrainz metadata
   - On miss: create a new Recording from embedded tags only
6. **Tag extraction** — ID3v2 / Vorbis comments / FLAC / MP4 tags via `lofty` crate
7. **Deduplication** — if AcoustID matches an existing Recording, add a new Source row
   rather than creating a new Recording
8. **Store** — write all entities to SQLite in a transaction
9. **Comment → tag sync** — parse the `comment` field into `recording_tag` rows (runs on
   every import, including rescans, so editing a file's comment and rescanning updates tags):
   - `#token` runs (e.g. `#chill`, `#TS:5/8`, `#drumdiff:7`) are stored verbatim
   - Remaining text split on `,` `;` `\n` yields plain tags
   - Parameterized form `#key:value` is first-class; Phase 4 query language will support
     `HasTag "#TS" != "4/4"` and `HasTag "#drumdiff" < 5` style predicates

---

## Source Abstraction (Rust trait)

```rust
pub trait AudioSource: Send {
    /// Returns the next decoded audio frame (interleaved f32 samples).
    fn next_frame(&mut self) -> Option<AudioFrame>;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn duration_ms(&self) -> Option<u64>;
    fn seek(&mut self, position_ms: u64) -> Result<()>;
}

pub struct LocalFileSource { /* symphonia decoder */ }
pub struct YouTubeSource  { /* yt-dlp child process → pipe → symphonia */ }
pub struct HttpStreamSource { /* reqwest streaming → symphonia */ }
```

The `AudioEngine` holds a `Box<dyn AudioSource>` and feeds samples to `cpal` output — it never knows which source type it's using.

---

## Smart Playlist Query Language

### Grammar (pest PEG)

```pest
query    = { expr ~ limit? ~ EOI }
expr     = { or_expr }
or_expr  = { and_expr ~ ("OR" ~ and_expr)* }
and_expr = { factor ~ ("AND" ~ factor)* }
factor   = { "(" ~ expr ~ ")" | "NOT" ~ factor | predicate }

predicate = {
    rating_pred | album_rating_pred | last_played_pred |
    play_count_pred | playlist_pred | tag_pred | title_pred |
    artist_pred | genre_pred | year_pred
}

rating_pred       = { "Rating" ~ ("is" ~ "null" | cmp_op ~ integer) }
album_rating_pred = { "AlbumRating" ~ cmp_op ~ integer }
last_played_pred  = { "LastPlayed" ~ ("NotInLast" | "InLast") ~ integer ~ time_unit }
play_count_pred   = { "PlayCount" ~ cmp_op ~ integer }
playlist_pred     = { ("InPlaylist" | "NotInPlaylist") ~ ident }
tag_pred          = { "HasTag" ~ string }
title_pred        = { "Title" ~ ("contains" | "is") ~ string }
artist_pred       = { "Artist" ~ ("contains" | "is") ~ string }
year_pred         = { "Year" ~ cmp_op ~ integer }

limit    = { "LIMIT" ~ integer ~ ("tracks" | "minutes" | "hours") }
cmp_op   = { ">=" | "<=" | ">" | "<" | "=" | "!=" }
time_unit = { "Days" | "Weeks" | "Months" | "Years" }
integer  = { ASCII_DIGIT+ }
ident    = { (ASCII_ALPHANUMERIC | "_")+ }
string   = { "\"" ~ (!"\"" ~ ANY)* ~ "\"" | ident }
```

### Compilation to SQL

Each predicate maps to a SQL fragment evaluated against a view:

```sql
CREATE VIEW smart_playlist_view AS
SELECT
    r.id, r.title, r.duration_ms,
    ur.stars                              AS rating,
    rgr.stars                             AS album_rating,
    MAX(ph.played_at)                     AS last_played,
    COUNT(ph.id)                          AS play_count
FROM recording r
LEFT JOIN user_rating ur ON ur.recording_id = r.id
LEFT JOIN track t ON t.recording_id = r.id
LEFT JOIN medium m ON m.id = t.medium_id
LEFT JOIN release rel ON rel.id = m.release_id
LEFT JOIN release_group_rating rgr ON rgr.release_group_id = rel.release_group_id
LEFT JOIN play_history ph ON ph.recording_id = r.id
GROUP BY r.id;
```

Example predicate mappings:

| Query predicate | SQL |
|---|---|
| `Rating >= 4` | `rating >= 4` |
| `Rating is null` | `rating IS NULL` |
| `AlbumRating > 4` | `album_rating > 4` |
| `LastPlayed NotInLast 8 Months` | `last_played < datetime('now', '-8 months') OR last_played IS NULL` |
| `NotInPlaylist FixThisSkip` | `id NOT IN (SELECT recording_id FROM playlist_track WHERE playlist_id = (SELECT id FROM playlist WHERE name = 'FixThisSkip'))` |
| `PlayCount >= 10` | `play_count >= 10` |
| `HasTag "chill"` | `id IN (SELECT recording_id FROM recording_tag WHERE tag = 'chill')` |

### Duration LIMIT handling

`LIMIT 25 minutes` cannot be expressed as a SQL row limit. The query engine:
1. Runs the full SQL query with no LIMIT, ordered by `RANDOM()`
2. Iterates the result set, accumulating `duration_ms`
3. Stops when the accumulated duration exceeds the target

This happens entirely in Rust after the SQL query returns.

---

## Audio Engine States

```
Stopped → Loading → Playing → Paused → Playing → ...
                        ↓
                     Stopped  (end of queue or user action)
```

The engine runs on a dedicated Rust thread. The main thread communicates via channels (`crossbeam-channel`):
- **Commands**: `Play(source)`, `Pause`, `Resume`, `Seek(ms)`, `SetVolume(f32)`, `Stop`
- **Events** (sent to Tauri frontend): `PositionUpdate { ms }`, `TrackEnded`, `Error(msg)`

---

## Frontend Structure (React)

```
src/
  components/
    PlayerBar/          # Now playing, scrubber, volume, transport controls
    LibraryView/        # Artist/Album/Track browser (virtualized list)
    PlaylistView/       # Static playlist editor (drag-and-drop)
    SmartPlaylistEditor/# Query input with syntax highlighting + preview
    SourceManager/      # Add/remove sources, re-scan, view duplicates
  hooks/
    usePlayer.ts        # Tauri event subscription → player state
    useLibrary.ts       # Query hooks for library data
    useSmartPlaylist.ts # Parse/validate/preview smart playlist queries
  stores/
    playerStore.ts      # Zustand store for playback state
    libraryStore.ts     # Zustand store for library browsing state
```

---

## Build Order (Implementation Phases)

### Phase 1 — Foundation
1. [x] Initialize Tauri 2 project with React/TypeScript/Vite
2. [x] Set up `sqlx` with SQLite, write migrations for the full schema
3. [x] Implement `MetadataScanner` (using `lofty`)
4. [x] Implement `FingerprintService` (`rusty-chromaprint` + AcoustID HTTP client)
5. [x] Implement the import pipeline (scan → hash → fingerprint → deduplicate → store)
6. [x] Tauri commands: `import_paths`, `get_library_summary`

### Phase 2 — Playback
7. [x] Implement `LocalFileSource` using symphonia
8. [x] Implement `AudioEngine` with cpal output, command/event channels
9. [x] Tauri commands: `play`, `pause`, `resume`, `seek`, `set_volume`
10. [x] Basic `PlayerBar` React component wired to Tauri events
11. [x] Basic `LibraryView` (list recordings, queue/play from the UI)

### Phase 3 — Library UI
12. [ ] Artist/Album/Track tree view with virtualized lists (browser exists; virtualization deferred — needs react-window or similar npm dep)
13. [x] Cover art extraction and display (`get_cover_art` Tauri command via lofty; displayed in the player panel for the active track)
14. [x] Inline rating UI (per recording ✓; per album ✓ — `set_release_group_rating` command + `RatingStars` in album browser)
15. [x] Tag management UI (tags derived from file `comment` field — not editable in app; `list_all_tags` command; Tags column shows read-only chips, click to filter; `parse_comment_tags` handles `#token` and `#key:value` parameterized forms alongside delimiter-split plain tags)

### Phase 4 — Smart Playlists
16. [ ] Implement pest grammar + AST types
17. [ ] Implement SQL codegen + duration-limit post-filter
18. [ ] `SmartPlaylistEditor` with live preview (shows first N matching tracks)
19. [ ] Save/load smart playlists; evaluate at queue time

### Phase 5 — Additional Sources
20. [ ] `YouTubeSource` via yt-dlp subprocess
21. [ ] `HttpStreamSource` via reqwest streaming
22. [ ] UI for adding YouTube URLs / streams to a Recording
23. [ ] Source health checking (re-verify URLs periodically)

### Phase 6 — Polish
24. [ ] Full MusicBrainz metadata import (for well-known recordings)
25. [ ] Duplicate detection UI (show recordings with multiple sources)
26. [ ] Cross-fade, gapless playback
27. [ ] ReplayGain / volume normalization
28. [ ] Export playlist to M3U / JSON
