#!/usr/bin/env bash
# Entry point for the desktop launcher: rebuild the checkout if anything
# changed, then run it. If the build fails, fall back to the last successful
# release binary so the launcher still works mid-refactor.
set -uo pipefail

cd "$(dirname "$0")/.."

bin=src-tauri/target/release/thmp5
log=src-tauri/target/launch-build.log
mkdir -p src-tauri/target

# `vite build` rewrites dist/ with fresh mtimes, and Tauri embeds dist/ into
# the binary, so running it unconditionally forces a ~30s Rust recompile on
# every launch. Only rebuild the frontend when one of its inputs is newer.
frontend_inputs=(src public index.html package.json package-lock.json
    vite.config.ts tsconfig.json tsconfig.node.json src-tauri/icons/icon-source.svg)

frontend_stale() {
    [[ ! -f dist/index.html ]] ||
        [[ -n "$(find "${frontend_inputs[@]}" -newer dist/index.html -print -quit 2>/dev/null)" ]]
}

build() {
    if frontend_stale; then
        npm run build:app || return 1
    fi
    npx tauri build --no-bundle --config '{"build":{"beforeBuildCommand":""}}'
}


# Per-user secrets such as ACOUSTID_API_KEY, kept outside the repo.
env_file="${XDG_CONFIG_HOME:-$HOME/.config}/thmp5/env"
if [[ -f "$env_file" ]]; then
    set -a
    source "$env_file"
    set +a
fi

if ! build >"$log" 2>&1; then
    if [[ -x "$bin" ]]; then
        notify-send -a thmp5 "thmp5 build failed" \
            "Launching the last successful build. See $PWD/$log" 2>/dev/null || true
    else
        notify-send -a thmp5 -u critical "thmp5 build failed" \
            "No previous build to fall back to. See $PWD/$log" 2>/dev/null || true
        exit 1
    fi
fi

exec "$bin" "$@"
