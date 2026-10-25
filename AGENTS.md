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
- Ratings, play history, and smart-playlist membership are tracked per **Recording** (not per file)
- The smart playlist query language compiles to SQL + a post-filter for duration limits

## Development Conventions
- Run `cargo fmt` and `cargo clippy` before committing Rust code
- All Tauri commands are typed end-to-end (Rust `#[tauri::command]` + generated TypeScript types)
- Database migrations live in `src-tauri/migrations/` and are managed by `sqlx migrate`
- Use `sqlx::query!` macros for compile-time query checking where possible. `build.rs` builds a scratch DB from `migrations/` and sets `DATABASE_URL` to it, so queries are always checked against the current schema (no `.sqlx` offline data, no `.env` needed). SQLite's nullability inference is conservative: TEXT primary keys and `INTEGER PRIMARY KEY` come out as `Option`, so use `AS "id!"` overrides; only mark a nullable column like `file_path` with `!` when the query filters `IS NOT NULL`
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
- **Gotcha**: real-world ID3 values often carry a trailing `\0` (TRCK, TPOS, multi-value frames); always `trim_end_matches('\0')` before parsing numbers.
- **Gotcha**: `source.duration_ms` is the single source of truth for track length, and playback uses it. Don't trust header-derived durations (symphonia `n_frames`, lofty, Xing): symphonia 0.5.5 reported 38:11 for an MP3 whose real length is 23:18 despite a valid Xing header. `audio_probe::measure_duration_ms` (sum of packet durations) corrects them at import/rescan. For ground truth use `ffmpeg -i <file> -f null -`.
- **Gotcha**: `Cargo.lock` pins symphonia 0.5.x, but `~/.cargo/registry/src` also contains 0.6.x. Read the version directory matching the lockfile when checking symphonia behavior.
