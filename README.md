# thmp5

thmp: **TH**eoretical **M**usic **P**layer, pronounced like a bass drum kick.

## For Users

### What is it?

A desktop application that figures out what kind of music you like, and plays
that music for you, without forcing you to worry about details such as whether
or not the music exists as a file on your harddrive.

You have mp3s on your harddrive? Great, I'll play those. Oh, it looks like a lot
of these mp3 files are recordings of Daft Punk. It looks like Daft Punk
released a new single last week, and the song's available on YouTube. I'll play
that for you next. And it looks like there's a bootleg Daft Punk mashup on
Soundcloud. I'll queue that one up for you right after this Grooveshark mix I
found.


## Technical Details

### Database

SQLite is stored at the platform data directory:

| Platform | Path |
|----------|------|
| Linux    | `~/.local/share/thmp5/thmp5.db` |
| macOS    | `~/Library/Application Support/thmp5/thmp5.db` |
| Windows  | `%APPDATA%\thmp5\thmp5.db` |

Migrations run automatically on startup.

## For Devs

### Prerequisites

- [Rust](https://rustup.rs/) (stable toolchain)
- [Node.js](https://nodejs.org/) 18+
- A C compiler and system libraries for your platform — see [Tauri prerequisites](https://tauri.app/start/prerequisites/)

On Arch/Manjaro:
```sh
sudo pacman -S webkit2gtk base-devel
```

On Ubuntu/Debian:
```sh
sudo apt install libwebkit2gtk-4.1-dev build-essential
```

### Running in development

```sh
npm install          # install JS dependencies (first time only)
npm run tauri dev    # start with hot reload
```

The app window opens automatically. The React frontend hot-reloads on save; Rust changes trigger a full recompile.

### Building for production

```sh
npm run tauri build
```

The resulting binary and installer are written to `src-tauri/target/release/bundle/`.

### Running tests

```sh
cargo test           # Rust unit tests
```

### Optional: AcoustID fingerprint lookups

thmp5 can match imported audio against the [AcoustID](https://acoustid.org) database for automatic metadata enrichment. To enable it, [register an application](https://acoustid.org/new-application) and set your API key before launching:

```sh
ACOUSTID_API_KEY=your_key_here npm run tauri dev
```

Without the key the import pipeline still works — it falls back to tag-based deduplication.