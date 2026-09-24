#!/usr/bin/env bash
# Collect the built binaries into dist/release/ under their release names.
#
#   ./cross/package-release.sh
#
# Packaging only — nothing is compiled. Build first:
#
#   cargo build -p cascade-daemon --release --target aarch64-apple-darwin
#   cargo build -p cascade-daemon --release --target x86_64-apple-darwin
#   ./cross/bundle-macos.sh
#   ./cross/build.sh --release
#   ./cross/build-windows.sh all --release
#
# Every input must exist; a missing one stops the script rather than publishing a partial
# set. dist/release/ is emptied first, so it only ever holds the current build.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$ROOT/target"
OUT="$ROOT/dist/release"

# release name              built file
FILES=(
  "cascade-macos-arm64        $T/aarch64-apple-darwin/release/cascade"
  "cascade-macos-x86_64       $T/x86_64-apple-darwin/release/cascade"
  "cascade-linux-x86_64       $T/x86_64-unknown-linux-gnu/release/cascade"
  "cascade-linux-aarch64      $T/aarch64-unknown-linux-gnu/release/cascade"
  "cascade-windows-x86_64.exe $T/x86_64-pc-windows-gnu/release/cascade.exe"
  "cascade-windows-arm64.exe  $T/aarch64-pc-windows-gnullvm/release/cascade.exe"
)
APP="$ROOT/dist/Cascade.app"

missing=0
for entry in "${FILES[@]}"; do
  src="${entry##* }"
  [[ -f "$src" ]] || { echo "missing: $src" >&2; missing=1; }
done
[[ -d "$APP" ]] || { echo "missing: $APP (run ./cross/bundle-macos.sh)" >&2; missing=1; }
(( missing == 0 )) || { echo "build the missing targets first (see the top of this script)" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT"

for entry in "${FILES[@]}"; do
  name="${entry%% *}"
  src="${entry##* }"
  cp "$src" "$OUT/$name"
  chmod +x "$OUT/$name"
done

# A release asset must be a file, so the app goes up zipped. ditto, not zip: it keeps the
# bundle's symlinks, extended attributes and code signature intact.
ditto -c -k --keepParent "$APP" "$OUT/Cascade-macos.zip"

( cd "$OUT" && shasum -a 256 * > SHA256SUMS.txt )

echo "dist/release/:"
ls -l "$OUT" | tail -n +2
