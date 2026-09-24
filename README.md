# Cascade

Audio-over-IP daemon for **macOS, Windows and Linux**. Headless — it runs without a display
and is controlled entirely from a built-in web interface. Written in Rust.

Multi-channel Opus audio over UDP (uncompressed RAW16/RAW24 is received and played too),
with clock-skew correction, per-remote send and receive routing, per-channel metering, and
optional end-to-end encryption.

---

## Install a release

Prebuilt binaries are on the [Releases](../../releases) page. Pick the one for your machine:

| File | For |
|---|---|
| `Cascade-macos.zip` | macOS, Apple silicon and Intel — the app (one universal `Cascade.app`) |
| `cascade-macos-arm64` / `cascade-macos-x86_64` | macOS, if you want the bare binary |
| `cascade-windows-x86_64.exe` | Windows on Intel/AMD |
| `cascade-windows-arm64.exe` | Windows on ARM (Surface, Snapdragon X) |
| `cascade-linux-x86_64` | Linux on Intel/AMD, glibc 2.36+ |
| `cascade-linux-aarch64` | Linux on ARM64, glibc 2.36+ — Raspberry Pi 4/5 |

`SHA256SUMS.txt` alongside them lists each file's checksum.

### macOS

Unzip `Cascade-macos.zip` and open `Cascade.app`. It runs in the background with a menu-bar
icon — **Open config** and **Quit Cascade** — and no terminal window.

Two things to expect on first launch:

- **A microphone prompt.** Cascade opens input devices, so macOS asks once. Click Allow.
- **Gatekeeper.** Release builds are not notarised, so macOS will refuse to open a
  downloaded copy at first. Go to **System Settings → Privacy & Security**, find the blocked
  item, and choose **Open Anyway**.

### Windows

Run the `.exe`. There is no console window; it starts in the background with a tray icon
offering **Open config** and **Exit Cascade**.

### Linux

```bash
chmod +x cascade-linux-x86_64
./cascade-linux-x86_64
```

Cascade uses ALSA directly and opens hardware devices (`hw:`) exclusively. For best results
under load, allow it to raise thread priority — see [Linux notes](#linux-notes).

---

## First run

Cascade creates its settings file on first run and logs where (`Settings …`). Open the web
UI at **http://localhost:8080** to set audio devices, the audio port, and your remotes.

The web UI listens on every network interface by default and has no login, so anyone who
can reach port 8080 can change the settings. On an untrusted network, set `[api] bind` to
`127.0.0.1` in the settings file, or firewall the port.

Settings and the log live together, so finding one finds the other:

| | Settings and log |
|---|---|
| A `cascade.toml` already exists in the working directory | that directory |
| Otherwise, macOS | `~/Library/Application Support/Cascade/` |
| Otherwise, Windows | `%APPDATA%\Cascade\` |
| Otherwise, Linux | `$XDG_CONFIG_HOME/cascade/`, or `~/.config/cascade/` |

`--config <path>` overrides all of this and puts the log beside whatever you name.
`cascade.toml.example` is an annotated copy of every setting.

The log rotates one generation at each start (`cascade.log.1`). Running from a terminal you
also get the usual coloured output; run in the background and the file is the record.

### Stopping it

The tray or menu-bar item is the graceful way, and every path — that item, `Ctrl-C`, `kill`,
a system log off or restart — releases the audio device and flushes pending settings first.
Force Quit and Task Manager's *End task* do not; they terminate the process outright, which
can lose a setting changed in the last second or two.

---

## Building from source

Requires the [Rust toolchain](https://rustup.rs) and a C compiler (Xcode Command Line Tools
on macOS, `build-essential` on Debian/Ubuntu, the Visual Studio Build Tools on Windows).

```bash
git clone <this repo> && cd cascade
cargo build --release
```

The binary lands in `target/release/cascade`. **libopus is vendored** (`opus-1.6.1/`) and
compiled from source with the C compiler alone — no `pkg-config`, Homebrew or CMake — so
every platform gets the identical codec version, 1.6.1.

Linux additionally needs ALSA headers to compile against:

```bash
sudo apt install libasound2-dev     # Debian/Ubuntu
```

### Cross-compiling from macOS

All six release targets build from one Mac, with no Docker or VM. One-time setup:

```bash
brew install zig
cargo install cargo-zigbuild
rustup target add aarch64-apple-darwin x86_64-apple-darwin
rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
rustup target add x86_64-pc-windows-gnu aarch64-pc-windows-gnullvm
./cross/fetch-sysroot.sh              # ALSA headers + libs, ~4 MB
```

Then a full release:

```bash
cargo build -p cascade-daemon --release --target aarch64-apple-darwin
cargo build -p cascade-daemon --release --target x86_64-apple-darwin
./cross/bundle-macos.sh                  # dist/Cascade.app (universal)
./cross/build.sh --release               # both Linux targets
./cross/build-windows.sh all --release   # both Windows targets
./cross/package-release.sh               # dist/release/ — the files listed above
```

`cross/README.md` explains what each script does and why — the glibc pinning, the sysroot,
the Windows toolchain choice, and the packaging details that are easy to get wrong.

### What's in the repository

```
Cargo.toml, Cargo.lock          workspace manifest and locked dependency versions
.cargo/config.toml              per-target CPU baselines for release builds
cascade-daemon/                 the daemon — all of Cascade
cascade-daemon/build.rs         links the Windows icon resource into the .exe
cascade-relay/                  placeholder crate, not yet implemented
opus-1.6.1/                     vendored libopus, built by its own build script
assets/                         generated icons (.icns, .ico, menu-bar template, master PNG)
cascade.toml.example            annotated example settings file
cross/build.sh                  Linux x86_64 + aarch64, via cargo-zigbuild
cross/build-windows.sh          Windows x86_64 + aarch64, and the MSVC alternative
cross/bundle-macos.sh           assembles dist/Cascade.app
cross/package-release.sh        names and zips the built files into dist/release/
cross/fetch-sysroot.sh          downloads the ALSA sysroot (output is gitignored)
cross/make-icons.py             regenerates assets/ from the UI's hexagon; no dependencies
cross/zita-compare.cc           C++ harness the resampler's comparison test is checked against
cross/README.md                 build and packaging notes
spec/                           the seven specifications
```

`cross/sysroot/` is deliberately gitignored — it is Debian binaries, reproducible any time
with `./cross/fetch-sysroot.sh`.

---

## Linux notes

Cascade wants two privileges it does not require. Without them it runs correctly at normal
priority and logs one warning each:

- `CAP_SYS_NICE` — to put its audio-adjacent threads at `nice -10`
- an `rtprio` limit — to put the device callback at `SCHED_FIFO`

Grant them per-user in `/etc/security/limits.conf`:

```
@audio   -  rtprio   80
@audio   -  nice    -10
```

...and add yourself to that group. Cascade opens ALSA hardware devices directly and never
resamples: if a device cannot provide the requested rate and channel count, configuration
fails and says so rather than silently converting.

### Running as a service

```ini
# /etc/systemd/user/cascade.service
[Unit]
Description=Cascade AoIP daemon

[Service]
ExecStart=%h/bin/cascade
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now cascade
```

`systemctl stop` shuts down cleanly — the terminate signal releases the device and flushes
settings before exit.

### macOS at login

`Cascade.app` can be added to **System Settings → General → Login Items**. For the bare
binary instead, a LaunchAgent works — grant microphone access by running it once by hand
first:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key>            <string>com.cascade.daemon</string>
  <key>ProgramArguments</key> <array><string>/path/to/cascade</string></array>
  <key>RunAtLoad</key>        <true/>
  <key>KeepAlive</key>        <true/>
</dict></plist>
```

Save as `~/Library/LaunchAgents/com.cascade.daemon.plist`, then
`launchctl load ~/Library/LaunchAgents/com.cascade.daemon.plist`. No `StandardOutPath` is
needed — Cascade writes its own log beside the settings file.

---

## Specifications

Seven documents in `spec/` describe the protocol, every mechanism Cascade implements and
its web UI, in pseudocode, in reading order:

| | |
|---|---|
| **`CASCADE_WIRE_PROTOCOL_SPEC.md`** | The UDP wire format: packet header, byte order, the opcode table (ping/pong, audio, config, label exchange), identity and authentication, the connection state machine and its timers, and HTTP interfaces. |
| **`CASCADE_AUDIO_SEND_SPEC.md`** | The outgoing pipeline: capture, per-remote encoders shared by reference count, Opus encoding, frame-size buckets, and send-side peak metering. |
| **`CASCADE_AUDIO_RECEIVE_SPEC.md`** | The incoming pipeline: arrival checks, decode (Opus, RAW16 and RAW24), the jitter/ring buffer and its management, routing to outputs and the output stage, receive-side metering, and the shared device callback period. |
| **`CASCADE_SYNC_MECHANISM_SPEC.md`** | Clock-skew correction: the Sync on/off gate, the resampler correction path, the discrete splice mechanism, and the deadband and hysteresis governing when correction engages. |
| **`CASCADE_SESSION_STATS_SPEC.md`** | Per-connection telemetry: byte counts, packet loss, jitter and latency, and the accumulate-and-snapshot pattern behind both the UI and the API. |
| **`CASCADE_ENCRYPTION_SPEC.md`** | Optional end-to-end audio encryption: X25519 key exchange, BLAKE2b key derivation, AES-256-GCM, the on-wire packet layout, and the gating logic on both sides. |
| **`CASCADE_UI_SPEC.md`** | The web UI: its principles, pages, how changes apply, metering, the buffer gauge, device states, banners and wording. |
