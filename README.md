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
- `cmake`
- `TagLib`
- `libopus` (for Opus/`.opus` file support — optional, see below)
- `pkg-config` or equivalent metadata discovery tooling

thmp5 now builds a small `taglib-helper` sidecar automatically as part of the normal Rust/Tauri build. That helper is used only as a fallback when Lofty rejects malformed tags, so broken real-world files can still import with a warning instead of failing outright.

On Arch/Manjaro:
```sh
sudo pacman -S webkit2gtk base-devel cmake pkgconf taglib opus
```

On Ubuntu/Debian:
```sh
sudo apt install libwebkit2gtk-4.1-dev build-essential cmake pkg-config libtag1-dev libopus-dev
```

On macOS with Homebrew:
```sh
brew install cmake pkgconf taglib opus
```

On Windows:
1. Install Rust and Node.js.
2. Install the normal Tauri prerequisites for WebView2 and MSVC build tools.
3. Install `vcpkg`.
4. Install TagLib with `vcpkg install taglib`.
5. Set `VCPKG_ROOT` to your `vcpkg` checkout path.

#### Opus support (optional)

thmp5 supports `.opus` files via libopus, enabled by default (the `opus` Cargo feature). If libopus is not found via `pkg-config`, the build will compile it from source using `cmake` — so no pre-installation is strictly required as long as cmake and a C compiler are available.

To build without Opus support entirely (e.g. for a fully static/pure-Rust binary):

```sh
cargo build --no-default-features
```

Attempting to play an `.opus` file in a build without Opus support will produce a clear error rather than a crash.

The Rust build script will automatically look for the vcpkg CMake toolchain on Windows when building the `taglib-helper` sidecar. If you are not using vcpkg, set one of:

- `CMAKE_TOOLCHAIN_FILE`
- `TAGLIB_ROOT`

`TAGLIB_ROOT` should point at a TagLib install prefix containing the headers under `include/` and the corresponding library files under `lib/` or `bin/`.

### TagLib fallback helper

The metadata import path works like this:

1. Try Lofty first.
2. If Lofty rejects malformed tags, run the bundled `taglib-helper` sidecar.
3. If TagLib can recover metadata, continue the import and log a warning to stdout.
4. If both fail, the import still fails as before.

For normal development you do not need to build the helper manually. `npm run tauri dev` and `npm run tauri build` will build it automatically through `src-tauri/build.rs`.

Manual helper build is still available if you want to debug it directly:

```sh
cd src-tauri/taglib-helper
cmake -S . -B build
cmake --build build --config Release
```

The automatic build used by `npm run tauri dev` now compiles the helper under Cargo's build output directory, not inside `src-tauri/`, to avoid Tauri file-watch rebuild loops.

If you want to override helper discovery during development, set:

```sh
THMP5_TAGLIB_HELPER=/absolute/path/to/taglib-helper npm run tauri dev
```

### Running in development

```sh
npm install          # install JS dependencies (first time only)
npm run tauri dev    # start with hot reload
```

The app window opens automatically. The React frontend hot-reloads on save; Rust changes trigger a full recompile.
If `cmake` and TagLib are installed, the `taglib-helper` sidecar is rebuilt automatically as part of the backend build.

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
