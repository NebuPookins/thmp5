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
- Use `sqlx::query!` macros for compile-time query checking where possible

## Build & Run
```sh
npm run tauri dev    # dev mode with hot reload
npm run tauri build  # production build
cargo test           # run Rust unit tests
```
