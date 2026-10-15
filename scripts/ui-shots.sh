#!/usr/bin/env sh
# Builds talus-app and renders UI screenshots (light + dark) headlessly into $1 (default target/ui-shots).
set -eu
out="${1:-$PWD/target/ui-shots}"
mkdir -p "$out/light" "$out/dark"
out="$(cd "$out" && pwd)" # tests run from the crate dir: the path must be absolute
TALUS_SCREENSHOT_DIR="$out/light" cargo test -p talus-app -q
TALUS_SHOT_THEME=dark TALUS_SCREENSHOT_DIR="$out/dark" cargo test -p talus-app -q
echo "screenshots: $out/light $out/dark"
