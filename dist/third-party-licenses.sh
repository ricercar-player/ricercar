#!/bin/sh
# Write the notices of every bundled third-party crate to
# target/THIRD-PARTY-LICENSES (or the path given as first argument).
# Needs cargo-about.
set -eu
cd "$(dirname "$0")/.."
OUT="${1:-target/THIRD-PARTY-LICENSES}"
mkdir -p "$(dirname "$OUT")"
cargo about generate --locked --fail about.hbs -o "$OUT"
# Slint crates declare their license without shipping its text.
{
    printf '\n%s\n' "--------------------------------------------------------------------------------"
    cat dist/licenses/LicenseRef-Slint-Royalty-free-2.0.md
} >> "$OUT"
echo "$OUT"
