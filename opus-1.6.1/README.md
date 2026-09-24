# opus-1.6.1 — vendored libopus, built with `cc` only

Drop-in replacement for the crates.io `audiopus_sys` crate, wired in through
`[patch.crates-io]` in the root manifest. The safe `opus = "0.3"` wrapper is unchanged and
still comes from crates.io — only the `-sys` layer beneath it is replaced, so there are no
call-site changes anywhere in Cascade.

## Why

The upstream `audiopus_sys` build script tries `pkg_config` **first**, and only falls back
to a cmake build of its own bundled copy. Two consequences, both unacceptable here:

1. **Wrong version, silently.** On any machine with libopus installed, it links whatever is
   there — 1.3.1 on Debian 12, 1.4 on Debian 13, 1.5.2 on current Homebrew. Cascade
   requires exactly 1.6.1 on every platform.
2. **Heavy build requirements.** The fallback needs cmake and pkg-config present, which is
   a poor assumption on a headless Raspberry Pi OS Lite install.

This crate has no discovery logic at all. One code path: compile the vendored source. The
only external requirement is a C compiler **that can target the platform being built for** —
which is the whole story on Windows: the build script already handles MSVC (`USE_ALLOCA`),
and an `x86_64-pc-windows-gnu` build works out of the box because zig supplies the mingw
headers. An `x86_64-pc-windows-msvc` cross-build from macOS fails only for want of the
Windows SDK headers, which is a toolchain matter and not a change to this crate. See
`cross/README.md`.

## Provenance

| | |
|---|---|
| Source | `https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz` |
| SHA-256 | `6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1` |
| Verified against | `https://downloads.xiph.org/releases/opus/SHA256SUMS.txt` |
| Retrieved | 2026-08-12 |

`vendor/` is that tarball with `doc/`, `tests/`, `m4/`, `meson/` and `dnn/` removed —
5.7 MB instead of 30 MB. See "Pruned" below for what `dnn/` costs us (nothing, today).

`src/lib.rs` is the pre-generated bindgen output from `audiopus_sys` 0.2.2, reused verbatim
under its ISC licence (`LICENSE-audiopus_sys.md`). Nothing is generated at build time, so
bindgen is not a build dependency either.

## Build configuration

- **Float build.** `FIXED_POINT` is deliberately undefined — Cascade is f32 end to end and
  every target has hardware float.
- **Source lists are parsed from the vendored `*.mk` files**, not hardcoded in `build.rs`.
  A version bump does not require re-deriving the file list by hand.
- **`PACKAGE_VERSION` is read from `vendor/package_version`**, so it cannot drift from the
  vendored source. Without it opus reports `"libopus unknown"` and the version guard below
  cannot tell a correct build from a wrong one.
- **SIMD: compile-time baseline only, run-time CPU detection (`OPUS_HAVE_RTCD`) off.**
  RTCD is where cross-build fragility lives — it pulls in cpu probing and per-function
  dispatch tables. Presuming the SIMD level the target ABI already guarantees gets the bulk
  of the win with a single code path: SSE/SSE2 on x86_64, NEON on aarch64. Other
  architectures build the portable C path — correct everywhere, just slower.

### Pruned

`dnn/` (22 MB of neural weights) backs the `DEEP_PLC`, `DRED`, `OSCE` and `LOSSGEN` feature
groups added in opus 1.5+. The protocol uses none of them and a default upstream build
excludes them too. To enable one: restore `dnn/`
from the tarball, add its source group in `build.rs`, and define the matching `ENABLE_*`.

## Guard

`tests/version.rs` asserts the linked library reports 1.6.1. This is the check that fails
if someone reintroduces a system-discovery path or bumps `vendor/` unintentionally:

```bash
cargo test --manifest-path opus-1.6.1/Cargo.toml
```

## Upgrading

1. Download the new tarball and verify it against upstream `SHA256SUMS.txt`.
2. Replace `vendor/`, applying the same prune list.
3. Update the provenance table above.
4. Run the guard test. If a new source group became mandatory, `build.rs` panics naming the
   missing group rather than silently building something incomplete.
