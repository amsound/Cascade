#!/bin/sh
# Build cross/sysroot/<target>/ from Debian packages. Run once; cross/build.sh needs it.
#
# Only ALSA is fetched. Everything else Cascade links is either vendored (libopus, via
# opus-1.6.1/) or supplied by zig itself (glibc, libc headers, the linker), which is why
# this stays a two-package download rather than a full distro sysroot.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEST="$ROOT/cross/sysroot"
POOL=http://deb.debian.org/debian/pool/main/a/alsa-lib

# Debian 12 (bookworm) ships alsa-lib 1.2.8, and so does Raspberry Pi OS Lite bookworm.
# Building against the OLDEST supported release keeps one binary valid on Debian 12 and 13:
# ALSA holds its ABI, and glibc is backward compatible, so older-target binaries run on
# newer systems but not the reverse.
ALSA_VERSION=1.2.8-1+b1

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

for pair in "amd64 x86_64-unknown-linux-gnu" "arm64 aarch64-unknown-linux-gnu"; do
  set -- $pair
  arch=$1
  target=$2
  out="$DEST/$target"

  echo "══ $target ══"
  rm -rf "$out"
  mkdir -p "$out"

  # libasound2-dev supplies the headers, the alsa.pc used by pkg-config, and the
  # libasound.so development symlink; libasound2 supplies the actual shared object that
  # symlink points at. Linking needs both.
  for pkg in libasound2-dev libasound2; do
    deb="${pkg}_${ALSA_VERSION}_${arch}.deb"
    echo "  fetching $deb"
    curl -sfL --max-time 120 -o "$TMP/$deb" "$POOL/$deb"
    ( cd "$TMP" && rm -rf x && mkdir x && cd x && ar x "../$deb" && tar xf data.tar.* -C "$out" )
  done

  test -f "$out/usr/include/alsa/asoundlib.h" || { echo "  MISSING headers" >&2; exit 1; }
  echo "  ok"
done

echo
echo "Sysroots ready. Build with: ./cross/build.sh --release"
