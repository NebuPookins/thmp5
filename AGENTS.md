# thmp5 — Music Player Project

## Project Overview
A desktop music player built with **Tauri** (Rust backend + TypeScript/React frontend).

## Tech Stack
- **Runtime**: Tauri 2.x
- **Backend**: Rust (stable)
- **Frontend**: TypeScript + React + Vite
- **Database**: SQLite via `sqlx` (with compile-time query checking)
- **Audio decoding**: `symphonia` (pure Rust, no ffmpeg)
- **Audio output**: `cpal` (cross-platform device output)
- **Fingerprinting**: `rusty-chromaprint` (pure Rust Chromaprint impl)
- **Query parser**: `pest` (PEG grammar)
- **YouTube**: `yt-dlp` subprocess

## Key Architectural Decisions
- See `ARCHITECTURE.md` for the full plan and data model
- The `Source` trait abstracts over local files / YouTube / HTTP streams so the audio engine is source-agnostic
- Recordings are not stored: since migration 014, `MemoryCatalog` (`storage/memory.rs`) derives them at load by grouping `source` rows (MBID, then file_hash, then fingerprint). Ratings, play history, playlist entries and file-issue notes all reference `source.id`, so deleting or merging source rows is how recordings disappear or merge. ARCHITECTURE.md's data model predates this.
- The smart playlist query language compiles to SQL + a post-filter for duration limits

## Development Conventions
- Run `cargo fmt` and `cargo clippy` before committing Rust code
- Tauri commands are called via untyped `invoke<T>` with hand-written TS types next to the caller (no codegen), so keep the Rust struct and the TS type in sync by hand.
- Database migrations live in `src-tauri/migrations/` and are managed by `sqlx migrate`
- Never edit an existing migration (sqlx checksums applied ones, and the user may have applied it on any machine); always add a new one, unless the user explicitly says the migration hasn't been applied and editing it is safe
- Use `sqlx::query!` macros for compile-time query checking where possible. `build.rs` builds a scratch DB from `migrations/` and sets `DATABASE_URL` to it, so queries are always checked against the current schema (no `.sqlx` offline data, no `.env` needed). SQLite's nullability inference is conservative: TEXT primary keys and `INTEGER PRIMARY KEY` come out as `Option`, so use `AS "id!"` overrides; only mark a nullable column like `file_path` with `!` when the query filters `IS NOT NULL`
- To pass a list of ids to `sqlx::query!`, bind a JSON array and use `IN (SELECT value FROM json_each(?))`. A `json_each` inside a scalar subquery fails the compile-time check ('no such table column: json_each.value').
- Tauri commands return `Result<T, CommandError>` (`src-tauri/src/command_error.rs`) and use `?`/`.context(...)`; converting an error into `CommandError` logs it once, with the command's span fields. Annotate every `#[tauri::command]` with `#[tracing::instrument(skip_all, fields(...))]` listing its identifying inputs (never secrets) as `fields(id = id)` / `fields(%path)` — a bare `fields(id)` records an *empty* field. Use `CommandError::rejected(msg)` for invalid input / not found. Only `#[tauri::command]` fns return `CommandError`; helpers and background tasks (e.g. the Last.fm listener in `lib.rs`) return `anyhow::Result` (with `Option` for "not configured"), because converting into `CommandError` logs an ERROR "Command failed" even outside a command.

## Build & Run
```sh
npm run tauri dev    # dev mode with hot reload
npm run tauri build  # production build
cargo test           # run Rust unit tests
```

## Where Things Live (don't re-search)
- **User's library DB**: `~/.local/share/net.nebupookins.thmp5/library.db` (SQLite; inspect with `sqlite3`). Don't `find /` for it — that times out.
  - `source` table: `file_path`, `track_total`, `raw_tags_json` (JSON array of `[frame_id, value]` pairs, e.g. `["TRCK","2/50\u0000"]`). Query e.g. `select file_path, raw_tags_json from source where raw_tags_json like '%<album title>%'`.
  - Other tables: `play_history`, `playlist`, `playlist_track`, `source_rating`, `app_config`.
- **App log**: `~/.local/share/net.nebupookins.thmp5/thmp5.log` (hundreds of MB, so grep it, don't Read it). It's the quickest evidence of how real external APIs behave: e.g. counting `loved=` values in `Last.fm: checked track loved status` lines showed `track.getInfo` does return `userloved`. The frontend logs to it too via `frontendLog()` (`src/frontendLog.ts`, target `thmp5_lib::frontend`), including every "Clearing current track" with its reason. To tell a webview reload/crash from a frontend state bug: compare the `child[<pid> WebKitWebProcess]` pid across `Memory snapshot` lines, and look for a fresh `lastfm_get_loved_tracks called` (every page load makes one).
- **Gotcha**: real-world ID3 values often carry a trailing `\0` (TRCK, TPOS, multi-value frames); always `trim_end_matches('\0')` before parsing numbers.
- **Gotcha**: `source.duration_ms` is the single source of truth for track length, and playback uses it. Don't trust header-derived durations (symphonia `n_frames`, lofty, Xing): symphonia 0.5.5 reported 38:11 for an MP3 whose real length is 23:18 despite a valid Xing header. `audio_probe::scan_audio` (sum of packet durations; the same pass decodes every frame to detect corruption, stored in `source.corruption`) corrects them at import/rescan. For ground truth use `ffmpeg -i <file> -f null -`.
- **Gotcha**: `~/.cargo/registry/src` holds several versions of some crates (e.g. symphonia). Check `src-tauri/Cargo.lock` for the version in use before reading a crate's source.
- **Gotcha**: `AppState.pending_jobs` / the `job-update` event only drives the status bar's queue count; library data reloads when the backend emits `catalog-reloaded` (from `CatalogStore::reload` with `Announce::Frontend`). Long-running background work (e.g. `library/integrity_backfill.rs`) must still report progress through its own event, not by bumping `pending_jobs`, or the status bar shows a stuck queue.
- **Gotcha**: the frontend must load library data only through `requestLibraryLoad` (`src/App.tsx`), never by calling `loadLibraryData`/`loadRecordings` directly. Overlapping loads let a stale one overwrite a newer one. After a backend write, prefer `reload_catalog(state, Announce::Frontend)` and let the `catalog-reloaded` event refresh the UI. Use `Announce::Quiet` only when the command's response already carries everything the UI needs (today: `set_source_rating`).
- To check how the backend groups the real library (recordings, release groups, track positions), copy the DB with `sqlite3 <library.db> ".backup <scratch>/lib.db"`, then load it in a temporary `#[ignore]` test: `crate::db::init_pool(path)`, then `MemoryCatalog::load_from_db`, then print. Delete the test afterwards.
- **Gotcha**: `App` re-renders about 4 times a second during playback (`player-position` → `setPlayerState`), and almost nothing under it is memoized, so skipping a `setState` doesn't stop rendering. Anything expensive or long-lived must be a `memo` component with stable props; see `FileIssueList` and its `issueActions` for how to keep handlers fresh.
- **Gotcha**: `currentTrack` is set before `play()`, but `playerState.source_id` only changes on a later player-state event, so for a moment they disagree. A listener handling a result for "the playing track" must check `source_id` against both `playerSourceIdRef.current` and `currentTrackRef.current?.source.id`, or a late result for the previous track lands on the next one. Likewise, apply UI transitions (history, `clearCurrentTrack`) *before* any `await`, and after an `await` re-check that `currentTrackRef.current` is still the track you started with; otherwise a track started meanwhile (File Issues ▶, queue advance) gets clobbered. Send `play` only through `startPlayback()`.
