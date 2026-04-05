`taglib-helper` sidecar contract

Purpose:
Provide a tolerant metadata fallback when Lofty rejects malformed tags during import.

Invocation:
- The main app executes `taglib-helper` as a subprocess.
- It passes one JSON argument on the command line:

```json
{"path":"/absolute/path/to/file.mp3"}
```

Expected stdout on success:

```json
{
  "meta": {
    "title": "Track Title",
    "artist": "Artist",
    "album_artist": null,
    "album": "Album",
    "year": 2004,
    "track_number": 2,
    "track_total": null,
    "disc_number": 1,
    "duration_ms": 173520,
    "format": "mp3",
    "genre": "Alternative",
    "bpm": null,
    "comment": null,
    "replay_gain_track_db": null,
    "replay_gain_track_peak": null,
    "replay_gain_album_db": null,
    "replay_gain_album_peak": null
  },
  "warning": "Recovered metadata with TagLib after malformed ID3 decoding."
}
```

Behavior:
- Exit `0` and emit exactly one JSON object on stdout when metadata was recovered.
- Exit non-zero and write a short diagnostic to stderr when metadata could not be recovered.
- `warning` is optional but recommended so the app can log why fallback was needed.

Discovery order used by the app:
1. `THMP5_TAGLIB_HELPER`
2. sibling of the current executable
3. macOS app bundle `../Resources/taglib-helper`
4. `taglib-helper` on `PATH`

Packaging guidance:
- Preferred: ship a per-platform `taglib-helper` sidecar built against TagLib.
- For Tauri packaging, place the helper alongside the main executable or in the app resources and make sure the final packaged app preserves execute permissions.

Build locally:

```sh
cd src-tauri/taglib-helper
cmake -S . -B build
cmake --build build --config Release
```

During development:

```sh
export THMP5_TAGLIB_HELPER="$PWD/build/taglib-helper"
```

On Windows, point `THMP5_TAGLIB_HELPER` at `build\\Release\\taglib-helper.exe` or wherever your generator emits the executable.

Windows notes:

- Preferred: install TagLib with `vcpkg` and set `VCPKG_ROOT`.
- The Rust build script will automatically pass `CMAKE_TOOLCHAIN_FILE=%VCPKG_ROOT%\\scripts\\buildsystems\\vcpkg.cmake` when building for a Windows target if `CMAKE_TOOLCHAIN_FILE` is not already set.
- Alternative: set `TAGLIB_ROOT` to a TagLib install prefix that contains `include\\taglib\\tag.h` and the corresponding `.lib`/`.dll` import library under `lib\\` or `bin\\`.
