# Building and packaging Cascade from macOS

Produces all six release binaries on a Mac — macOS, Linux and Windows, each for `x86_64`
and `aarch64` — plus the macOS app bundle. No Docker, no VM, no Linux or Windows box.

## One-time setup

```bash
brew install zig
cargo install cargo-zigbuild
rustup target add aarch64-apple-darwin x86_64-apple-darwin
rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
rustup target add x86_64-pc-windows-gnu aarch64-pc-windows-gnullvm
./cross/fetch-sysroot.sh
```

## Building

```bash
./cross/build.sh --release              # both Linux targets
./cross/build.sh aarch64 --release      # Raspberry Pi only
./cross/build.sh x86_64                 # debug

./cross/build-windows.sh --release       # Windows x86_64, mingw — needs nothing extra
./cross/build-windows.sh arm64 --release # Windows aarch64
./cross/build-windows.sh all --release   # both Windows architectures
./cross/build-windows.sh msvc --release  # Windows, MSVC — needs the SDK, see below
```

Output lands in `target/<target>/<profile>/cascade` (`cascade.exe` on Windows).

## A full release

```bash
cargo build -p cascade-daemon --release --target aarch64-apple-darwin
cargo build -p cascade-daemon --release --target x86_64-apple-darwin
./cross/bundle-macos.sh                  # dist/Cascade.app, universal
./cross/build.sh --release               # both Linux targets
./cross/build-windows.sh all --release   # both Windows targets
./cross/package-release.sh               # dist/release/, named for upload
```

`package-release.sh` compiles nothing. It copies each built binary into `dist/release/`
under its release name, zips the app bundle, and writes `SHA256SUMS.txt`; it stops if any
target has not been built:

| Release file | Built from |
|---|---|
| `Cascade-macos.zip` | `dist/Cascade.app` (universal), zipped with `ditto` |
| `cascade-macos-arm64` | `target/aarch64-apple-darwin/release/cascade` |
| `cascade-macos-x86_64` | `target/x86_64-apple-darwin/release/cascade` |
| `cascade-linux-x86_64` | `target/x86_64-unknown-linux-gnu/release/cascade` |
| `cascade-linux-aarch64` | `target/aarch64-unknown-linux-gnu/release/cascade` |
| `cascade-windows-x86_64.exe` | `target/x86_64-pc-windows-gnu/release/cascade.exe` |
| `cascade-windows-arm64.exe` | `target/aarch64-pc-windows-gnullvm/release/cascade.exe` |

`bundle-macos.sh` does not compile either: it `lipo`s the two macOS binaries already in
`target/`, so build both macOS targets first or it packages stale ones.

## Windows: two toolchains

`gnu` (`x86_64-pc-windows-gnu`, and `aarch64-pc-windows-gnullvm` for ARM) is what the
release builds use. It needs **nothing beyond the zig setup the Linux targets already
require** — zig ships the mingw-w64 headers, which is what lets the vendored libopus
compile — and produces a self-contained `cascade.exe`.

`msvc` (`x86_64-pc-windows-msvc`) is available as an alternative. It needs `cargo-xwin`,
which downloads Microsoft's CRT and Windows SDK, and **that download requires accepting the
Microsoft licence**. `build-windows.sh` will not accept it for you: read
<https://go.microsoft.com/fwlink/?LinkId=2086102>, then set `XWIN_ACCEPT_LICENSE=1`.

Nothing in Cascade exposes a C ABI to other Windows software, so for a standalone daemon
the practical difference between the two runtimes is small.

## How it works, and why each piece is there

| Need | Supplied by |
|---|---|
| Rust cross-compilation | rustup targets |
| C cross-compilation + linker | `zig cc`, via `cargo-zigbuild` |
| glibc for the target | zig (bundles headers/stubs for every glibc version) |
| libopus | vendored and built from source — see `opus-1.6.1/` |
| ALSA headers + library | `cross/sysroot/`, from Debian packages |
| ALSA bindings | the `alsa` crate, used DIRECTLY by `audio/backend/alsa_backend.rs` — cpal is not a dependency on any platform |

ALSA is the only thing needing a sysroot, which is what keeps this a two-package download
rather than a full distro image.

### glibc pinning

`build.sh` targets **glibc 2.36** (`--target x86_64-unknown-linux-gnu.2.36`), which is
Debian 12 bookworm and Raspberry Pi OS Lite bookworm. glibc is backward compatible, so
building against the oldest supported release produces one binary that runs on Debian 12
**and** 13. Building against 2.41 (trixie) instead would fail to start on bookworm.

Verify the floor held after any toolchain change:

```bash
strings -a target/x86_64-unknown-linux-gnu/release/cascade | grep -oE 'GLIBC_2\.[0-9]+' | sort -uV | tail -1
```

Currently reports `GLIBC_2.34` — under the 2.36 ceiling with room to spare.

### ALSA version

The sysroot pins alsa-lib **1.2.8** (bookworm). Same reasoning: ALSA holds its ABI, so
linking against the oldest supported version runs against the newer runtimes on Debian 13
and later, but not the reverse.

## Known noise

zig's linker prints `ignoring deprecated linker optimization setting '1'` once per link.
That is rustc passing its default `-Wl,-O1`, which zig 0.16 no longer honours. It is not
an error and not ours to fix; Cascade's own source compiles with zero warnings on all six
targets.

## macOS: `Cascade.app`

`./cross/bundle-macos.sh` wraps the release binaries in a bundle so double-clicking runs
the daemon in the background instead of opening a terminal. Build both Mac targets first;
the script lipos them into one universal binary. `--arm64` / `--x86` build a single-slice
bundle instead.

The daemon itself is copied in unmodified — this is packaging, not a code change. What the
bundle adds:

| | |
|---|---|
| `LSUIElement` | Agent app: no Dock icon, no menu bar, launches silently and keeps running. |
| `NSMicrophoneUsageDescription` | Required. Cascade opens input devices, and macOS terminates any app that touches audio input without a usage string. Run from a terminal the permission belongs to Terminal.app; bundled, it belongs to Cascade.app, so the first launch prompts once. |

**`CFBundleExecutable` must name the Mach-O, never a shell script that execs it.**
LaunchServices reads an app's supported architectures out of its main executable's header.
A script has no header, so it cannot tell the app is arm64-capable, falls back to x86_64,
and the entire daemon runs under Rosetta — Get Info shows "Open using Rosetta" already
ticked, and the first launch stalls the machine while a universal binary is translated.
This is why the daemon resolves its own paths instead of being handed them by a wrapper.

If a bundle ever did get launched that way, the stale preference survives in the
LaunchServices database: untick it in Get Info, or `lsregister -f` the bundle.

The bundle is ad-hoc signed. That is not for distribution: the microphone grant is keyed to
a code signature, and an unsigned bundle can lose its permission whenever the binary
changes. Shipping it to another Mac needs a Developer ID signature and notarisation.

## Where settings and the log live

`--config` decides both, and the log is always written beside the settings file, so finding
one finds the other. With no `--config`:

1. `cascade.toml` in the working directory, **if it already exists** — so a terminal run
   started in a folder holding one uses it.
2. Otherwise a per-user directory: `~/Library/Application Support/Cascade` (macOS),
   `%APPDATA%\Cascade` (Windows), `$XDG_CONFIG_HOME/cascade` or `~/.config/cascade`
   (Linux).

Step 2 exists because a daemon launched from Finder or Explorer does not choose its own
working directory — an app bundle gets cwd `/` — and `Config::load` creates the file when
absent. A relative default would let the launcher decide where settings are written, and
under a bundle it means failing to write to `/` and exiting before anything can say why.

A consequence worth knowing: **a bundled Cascade and a terminal Cascade use different
config files.** To carry settings across, copy `cascade.toml` into the per-user directory.

The log rotates one generation at launch (`cascade.log.1`), and is written without ANSI —
`tracing-subscriber` colours its output whether or not the sink can display it. The
terminal still gets its coloured stdout on top, but only when stdout really is a terminal,
so a background run does not format every event twice.

## Windows: two architectures

```bash
./cross/build-windows.sh --release          # x86_64
./cross/build-windows.sh arm64 --release    # aarch64 (Surface, Snapdragon X, ARM VMs)
./cross/build-windows.sh all --release      # both
```

aarch64 needs its target installed once — `rustup target add aarch64-pc-windows-gnullvm`.
`gnullvm` rather than `gnu` because on ARM Windows that is the only GNU-flavoured Rust
target: it links the LLVM mingw runtime instead of gcc's. Same zig toolchain, and the
output is an ordinary native ARM64 `.exe`, not an emulated one.

## Windows: no console window

`cascade.exe` is linked as a GUI-subsystem binary (`windows_subsystem = "windows"` in
`main.rs`), so double-clicking starts the daemon with no terminal. The attribute is
unconditional rather than gated on `debug_assertions`: this crate has no `[profile]`
overrides, so a debug build is opt-level 0 throughout — including the zita resampler and
the vendored C opus — and is not a build to run audio through. Under this subsystem there
is no stdout at all, which is why the log file is not optional.

## Stopping a background daemon

Both SIGINT and a terminate request run the clean shutdown — release the device hog, flush
any config change still inside the save debounce. Ctrl-C, `kill`, Activity Monitor's Quit,
`taskkill`, launchd and systemd are therefore all safe. Force Quit / `kill -9` is not, and
skips both.

## Not covered here

Linux packaging (`.deb` + systemd unit) — these scripts produce the binary only. The root
`README.md` has a user-level systemd unit to start from.
