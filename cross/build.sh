#!/bin/sh
# Cross-build Cascade for Linux from macOS. See cross/README.md.
#
#   ./cross/build.sh                     # both Linux targets, debug
#   ./cross/build.sh --release           # both, release
#   ./cross/build.sh x86_64 --release    # one target
#
# Everything the build needs is vendored or fetched into cross/sysroot — there is no
# dependency on what happens to be installed on this Mac beyond zig and cargo-zigbuild.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SYSROOT_BASE="$ROOT/cross/sysroot"

# glibc 2.36 is Debian 12 (bookworm) and Raspberry Pi OS Lite bookworm. Pinning the
# OLDEST glibc in the supported range is what makes one binary run on Debian 12 AND 13 —
# glibc is backward compatible, so building against 2.41 (trixie) would produce something
# that fails to start on bookworm with a version-mismatch error.
GLIBC=2.36

targets=""
cargo_args=""
for arg in "$@"; do
  case "$arg" in
    x86_64|amd64)   targets="$targets x86_64-unknown-linux-gnu" ;;
    aarch64|arm64)  targets="$targets aarch64-unknown-linux-gnu" ;;
    *)              cargo_args="$cargo_args $arg" ;;
  esac
done
[ -n "$targets" ] || targets="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu"

for target in $targets; do
  case "$target" in
    x86_64-*)  deb_triple=x86_64-linux-gnu ;;
    aarch64-*) deb_triple=aarch64-linux-gnu ;;
  esac
  sysroot="$SYSROOT_BASE/$target"

  if [ ! -f "$sysroot/usr/include/alsa/asoundlib.h" ]; then
    echo "error: no sysroot at $sysroot — run cross/fetch-sysroot.sh first" >&2
    exit 1
  fi

  echo "══ $target (glibc $GLIBC) ══"

  # The sysroot's library path goes in a GENERATED CONFIG FILE, not in RUSTFLAGS.
  #
  # Cargo takes its extra rustc flags from exactly ONE source, and an environment RUSTFLAGS
  # outranks every config file — so setting it here would silently DISCARD the per-target
  # baselines in .cargo/config.toml (`target-cpu`), building x86_64 at the bare baseline
  # instead of the v2 one that file pins. Array values from config FILES are joined, so a
  # file passed with --config adds this path to those flags rather than replacing them.
  flags_toml="$ROOT/target/cross-$target-flags.toml"
  mkdir -p "$(dirname "$flags_toml")"
  printf '[target.%s]\nrustflags = ["-L", "%s"]\n' \
    "$target" "$sysroot/usr/lib/$deb_triple" > "$flags_toml"

  # A RUSTFLAGS the CALLER set still outranks both, so the path is added to theirs as well —
  # their flags then decide the CPU baseline too, which is what asking for them means.
  if [ -n "${RUSTFLAGS:-}" ]; then
    echo "note: RUSTFLAGS is set, so .cargo/config.toml's flags for $target are not applied"
    RUSTFLAGS="$RUSTFLAGS -L $sysroot/usr/lib/$deb_triple"
    export RUSTFLAGS
  fi

  # PKG_CONFIG_SYSROOT_DIR makes pkg-config rewrite the .pc file's absolute /usr paths to
  # point inside our sysroot; PKG_CONFIG_LIBDIR (not _PATH) REPLACES the default search
  # path rather than extending it, so a stray host .pc can never satisfy the query.
  PKG_CONFIG_SYSROOT_DIR="$sysroot" \
  PKG_CONFIG_LIBDIR="$sysroot/usr/lib/$deb_triple/pkgconfig" \
  PKG_CONFIG_ALLOW_CROSS=1 \
    cargo zigbuild -p cascade-daemon --config "$flags_toml" \
      --target "$target.$GLIBC" $cargo_args
done
