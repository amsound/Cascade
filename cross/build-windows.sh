#!/bin/sh
# Cross-build Cascade for Windows from macOS. See cross/README.md.
#
#   ./cross/build-windows.sh                  # x86_64 gnu, debug
#   ./cross/build-windows.sh --release        # x86_64 gnu, release
#   ./cross/build-windows.sh arm64 --release  # aarch64 gnullvm, release
#   ./cross/build-windows.sh all --release    # both architectures
#   ./cross/build-windows.sh msvc --release   # x86_64 msvc (needs the SDK — see below)
#
# Two toolchains, because they have different prerequisites:
#
#   gnu  (x86_64-pc-windows-gnu, aarch64-pc-windows-gnullvm) — DEFAULT. Uses the zig
#        toolchain already required for the Linux targets, so it needs nothing further
#        installed and no licence accepted. zig ships the mingw-w64 headers, which is what
#        lets the vendored libopus compile.
#
#        The two architectures use different Rust target triples: x86_64 has a real mingw
#        target, while on aarch64 Windows the only GNU-flavoured target is `gnullvm`, which
#        links against the LLVM mingw runtime rather than gcc's. Same zig invocation, and
#        the resulting .exe is an ordinary native ARM64 Windows binary — it is not emulated
#        and does not need the x86_64 build on an ARM machine. Add it once with:
#            rustup target add aarch64-pc-windows-gnullvm
#
#   msvc (x86_64-pc-windows-msvc) — the first-class Windows target. Needs cargo-xwin, which
#        downloads Microsoft's CRT and Windows SDK. That download requires ACCEPTING THE
#        MICROSOFT LICENCE, which is a decision for the project owner, so this script will
#        not do it silently: set XWIN_ACCEPT_LICENSE=1 to proceed. Read the terms first —
#        https://go.microsoft.com/fwlink/?LinkId=2086102
#
# Every build produces a self-contained cascade.exe with NO console window (main.rs sets
# windows_subsystem = "windows"), so double-clicking runs the daemon in the background and
# the log goes to a file beside the config. The difference between the toolchains is the C
# runtime the vendored libopus is compiled against; nothing in Cascade exposes a C ABI to
# other Windows software, so for a standalone daemon the practical difference is small —
# but msvc is what Windows users expect, and it is the release target.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)

toolchain=gnu
arch=x86_64
cargo_args=""
for arg in "$@"; do
  case "$arg" in
    gnu)          toolchain=gnu ;;
    msvc)         toolchain=msvc ;;
    arm64|aarch64) arch=arm64 ;;
    x86_64|x64)   arch=x86_64 ;;
    all)          arch=all ;;
    *)            cargo_args="$cargo_args $arg" ;;
  esac
done

# One target triple per (toolchain, arch). msvc is x86_64 only here: the aarch64 MSVC
# target would work the same way, but it is behind the same unaccepted licence.
targets=""
case "$toolchain:$arch" in
  gnu:x86_64)  targets="x86_64-pc-windows-gnu" ;;
  gnu:arm64)   targets="aarch64-pc-windows-gnullvm" ;;
  gnu:all)     targets="x86_64-pc-windows-gnu aarch64-pc-windows-gnullvm" ;;
  msvc:x86_64|msvc:all) targets="x86_64-pc-windows-msvc" ;;
  msvc:arm64)  echo "error: the msvc path here covers x86_64 only — use arm64 with the default gnu toolchain" >&2; exit 1 ;;
esac

build_gnu() {
  command -v cargo-zigbuild >/dev/null 2>&1 || {
    echo "error: cargo-zigbuild not found — see cross/README.md one-time setup" >&2
    exit 1
  }
  rustup target list --installed | grep -qx "$1" || {
    echo "error: rust target $1 not installed. Add it with:" >&2
    echo "         rustup target add $1" >&2
    exit 1
  }
  echo "══ $1 (mingw via zig) ══"
  cd "$ROOT" && cargo zigbuild -p cascade-daemon --target "$1" $cargo_args
}

build_msvc() {
  command -v cargo-xwin >/dev/null 2>&1 || {
    echo "error: cargo-xwin not found. Install it with:" >&2
    echo "         cargo install cargo-xwin --locked" >&2
    exit 1
  }
  if [ "${XWIN_ACCEPT_LICENSE:-}" != "1" ]; then
    cat >&2 <<'EOF'
error: the msvc target needs Microsoft's CRT and Windows SDK, which cargo-xwin downloads.

That download requires accepting the Microsoft Software Licence Terms:
  https://go.microsoft.com/fwlink/?LinkId=2086102

This script will not accept them on your behalf. Once you have read them, re-run with:

  XWIN_ACCEPT_LICENSE=1 ./cross/build-windows.sh msvc --release

The gnu targets need none of this and build a working cascade.exe today:

  ./cross/build-windows.sh all --release
EOF
    exit 1
  fi
  echo "══ $1 (MSVC via cargo-xwin) ══"
  cd "$ROOT" && cargo xwin build -p cascade-daemon --target "$1" $cargo_args
}

for target in $targets; do
  case "$toolchain" in
    gnu)  build_gnu  "$target" ;;
    msvc) build_msvc "$target" ;;
  esac
done

# List ONLY what this invocation produced. Globbing */cascade.exe also turns up the other
# profile's binary — a stale multi-hundred-MB debug build sitting next to a fresh release
# one is exactly how the wrong .exe gets copied to a test machine.
case " $cargo_args " in
  *\ --release\ *) profile=release ;;
  *)               profile=debug ;;
esac
echo
for target in $targets; do
  ls -l "$ROOT/target/$target/$profile/cascade.exe" 2>/dev/null || true
done
exit 0
