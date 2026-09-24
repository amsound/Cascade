#!/bin/sh
# Assemble Cascade.app from the release binaries. See cross/README.md.
#
#   ./cross/bundle-macos.sh              # universal (arm64 + x86_64) into dist/
#   ./cross/bundle-macos.sh --arm64      # this Mac's architecture only, faster
#
# PACKAGING ONLY — the daemon binary is copied in unmodified. What the bundle changes is
# how macOS launches it, and that is the whole point: Finder runs a bare Mach-O through
# Terminal.app, so double-clicking today opens a terminal window. A bundle does not.
#
# Two things in Info.plist do the work:
#
#   LSUIElement                    Agent app: no Dock icon, no menu bar, launches silent
#                                  and stays running. (LSBackgroundOnly is the stricter
#                                  variant that forbids UI outright — this one leaves the
#                                  door open for a status item later.)
#   NSMicrophoneUsageDescription   REQUIRED, not optional. Cascade opens input devices,
#                                  and macOS kills any app that touches audio input with
#                                  no usage string. Run from a terminal the permission
#                                  belongs to Terminal.app; bundled, it belongs to
#                                  Cascade.app, so the first launch prompts once.
#
# CFBundleExecutable MUST BE THE MACH-O, never a shell script that execs it. LaunchServices
# reads the supported architectures out of the main executable's header; a script has no
# header, so it cannot tell the app is arm64-capable and falls back to x86_64. The whole
# daemon then runs under Rosetta — Get Info shows "Open using Rosetta" already ticked — and
# the first launch stalls the machine translating a universal binary before any audio runs.
# The daemon resolves its own config and log paths (resolve_config_path in main.rs), so no
# wrapper is needed for those.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DIST="$ROOT/dist"
APP="$DIST/Cascade.app"
VERSION=$(awk -F'"' '/^version/{print $2; exit}' "$ROOT/cascade-daemon/Cargo.toml")

ARM="$ROOT/target/aarch64-apple-darwin/release/cascade"
X86="$ROOT/target/x86_64-apple-darwin/release/cascade"

case "${1:-}" in
  --arm64) SLICES="$ARM" ;;
  --x86)   SLICES="$X86" ;;
  *)       SLICES="$ARM $X86" ;;
esac

for f in $SLICES; do
  [ -f "$f" ] || { echo "error: missing $f — build it first:" >&2
                   echo "         cargo build -p cascade-daemon --release --target $(basename $(dirname $(dirname $f)))" >&2
                   exit 1; }
done

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# The icon is generated from the same hexagon the web UI draws — see cross/make-icons.py.
# Regenerated on every bundle so the two cannot drift. Cosmetic: a missing icon must never
# fail the build, so the plist key is only written when the file is actually there.
ICON=""
if python3 "$ROOT/cross/make-icons.py" >/dev/null 2>&1 && [ -f "$ROOT/assets/Cascade.icns" ]; then
  cp "$ROOT/assets/Cascade.icns" "$APP/Contents/Resources/Cascade.icns"
  ICON='	<key>CFBundleIconFile</key>          <string>Cascade</string>'
else
  echo "warning: could not generate assets/Cascade.icns — bundling without an icon" >&2
fi

# One fat binary when both slices are present, so the same .app runs on Apple silicon and
# Intel. lipo with a single input is just a copy. Named "cascade" and named directly by
# CFBundleExecutable — see the architecture note above.
lipo -create $SLICES -output "$APP/Contents/MacOS/cascade"
chmod +x "$APP/Contents/MacOS/cascade"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>              <string>Cascade</string>
	<key>CFBundleDisplayName</key>       <string>Cascade</string>
	<key>CFBundleIdentifier</key>        <string>com.cascade.daemon</string>
	<key>CFBundleExecutable</key>        <string>cascade</string>
$ICON
	<key>CFBundlePackageType</key>       <string>APPL</string>
	<key>CFBundleVersion</key>           <string>$VERSION</string>
	<key>CFBundleShortVersionString</key><string>$VERSION</string>
	<key>LSMinimumSystemVersion</key>    <string>11.0</string>
	<key>LSUIElement</key>               <true/>
	<key>NSMicrophoneUsageDescription</key>
	<string>Cascade captures audio from the input device you select and sends it to your configured remotes.</string>
</dict>
</plist>
PLIST

# Ad-hoc signature. Not for distribution — it is for TCC: the microphone grant is keyed to
# a code signature, and an unsigned bundle can lose its permission when the binary changes.
codesign --force --sign - --timestamp=none "$APP" >/dev/null 2>&1 \
  || echo "warning: ad-hoc codesign failed — the mic permission may need re-granting after each rebuild" >&2

echo "built $APP"
lipo -archs "$APP/Contents/MacOS/cascade" | sed 's/^/  slices: /'
echo "  config: ~/Library/Application Support/Cascade/cascade.toml"
echo "  log:    ~/Library/Application Support/Cascade/cascade.log  (beside the config)"
