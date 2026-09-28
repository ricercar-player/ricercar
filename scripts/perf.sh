#!/usr/bin/env bash
# Load test on big synthetic libraries (see docs/perf.md): builds release
# binaries, generates the libraries once, then runs the headless perf tour
# (RICERCAR_SNAPSHOT_TOUR=perf) and prints its [profile] lines.
#
#   scripts/perf.sh <work-dir> [tracks=50000] [files=5000]
#
# Everything (libraries, XDG dirs, cover cache) stays under <work-dir>; the
# audio goes to the null sink.
set -euo pipefail

W=${1:?usage: $0 <work-dir> [tracks] [files]}
TRACKS=${2:-50000}
FILES=${3:-5000}
cd "$(dirname "$0")/.."
cargo build --release -q -p ricercar-ui --bin ricercar
cargo build --release -q -p ricercar-core --example synth_library
T=${CARGO_TARGET_DIR:-target}
SYNTH=$T/release/examples/synth_library
BIN=$T/release/ricercar
mkdir -p "$W"
W=$(cd "$W" && pwd)
[ -f "$W/db/library.db" ] || "$SYNTH" --tracks "$TRACKS" --out "$W/db"
[ -d "$W/files/music" ] || "$SYNTH" --files --tracks "$FILES" --out "$W/files"

# run <label> <xdg dir name> <library roots, TOML list items> [ricercar args...]
run() {
  local name=$1 x="$W/xdg-$2" roots=$3
  shift 3
  mkdir -p "$x/config/ricercar"
  cat >"$x/config/ricercar/config.toml" <<EOF
[library]
roots = [$roots]
watch = false

[network]
renderer = false
media_server = false

[online]
lyrics = false
cover_art = false
radio = false

[ui]
tray = false
notifications = false
EOF
  XDG_CONFIG_HOME="$x/config" XDG_DATA_HOME="$x/data" XDG_CACHE_HOME="$x/cache" \
    XDG_STATE_HOME="$x/state" RICERCAR_PROFILE=1 RICERCAR_SNAPSHOT="$W/shots-$name" \
    RICERCAR_SNAPSHOT_TOUR=perf RUST_LOG=error \
    timeout 900 "$BIN" --device null --no-upnp --no-mpris --no-session "$@" 2>&1 |
    grep '^\[profile\]' | sed "s/^\[profile\]/[$name]/" || true
}

rm -rf "$W"/xdg-*
# Database only: UI measurements, first with a cold cover cache.
run big-cold big "" --db "$W/db/library.db"
run big-warm big "" --db "$W/db/library.db"
# Real files: initial scan, then a rescan with nothing changed.
run files-scan files "\"$W/files/music\""
run files-rescan files "\"$W/files/music\""
