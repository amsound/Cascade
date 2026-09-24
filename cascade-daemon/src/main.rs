// Windows: link as a GUI-subsystem binary so the loader allocates no console. Double-
// clicking cascade.exe then starts the daemon silently instead of opening a terminal.
// Unconditional, NOT gated on debug_assertions: this crate has no [profile] overrides, so
// a debug build is opt-level 0 throughout — including the zita resampler and the vendored
// C opus — and is not a build anyone should run audio through. Tying console visibility to
// it would be an invitation to do exactly that. Under this subsystem stdout goes nowhere,
// which is why log_path() below always opens a file.
//
// Inner attribute: must precede every outer attribute in the file, doc comments included.
#![cfg_attr(windows, windows_subsystem = "windows")]

/// Cascade — audio-over-IP for broadcast.

mod net;
mod audio;
mod config;
mod config_saver;
mod api;
mod lifecycle;
mod meter;
mod platform;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc;
use tracing::{info, warn, debug, error};
use anyhow::Result;
use clap::Parser;

use net::protocol::{derive_token, PacketType};
use net::udp::{UdpEngine, OutboundPacket};
use net::peer::{PeerCommand, PeerConfig, PeerStats, PeerTask, SendRequest, ChannelLabel};
use audio::{AudioEngine, CaptureEngine};
use config::Config;
use api::{AppState, ChannelInfo, OutgoingInfo};


/// Owns clones of the per-peer lookup maps that are shared (Arc<RwLock>) between the select
/// loop and the receive path. The point of bundling them is a single matched insert/remove
/// pair: a peer's entries are added in one place and removed in one place, so the teardown
/// of a removed/disabled remote can't drift out of sync with its setup — a map added to setup
/// but forgotten in teardown leaves a stale render group or leaked atomics. Adding a new shared per-peer map means adding it here, which forces updating
/// both insert and remove.
///
/// NOTE: the two NON-shared plain maps (peer_cmds, peer_tx_atomics) are deliberately NOT held
/// here — peer_cmds carries mpsc::Senders used across .await, which must not live behind a
/// lock guard. They are removed alongside registry.remove() in the single teardown helper, so
/// they share the one teardown site and don't drift either.
#[derive(Clone)]
struct PeerRegistry {
    stats:   Arc<RwLock<HashMap<String, PeerStats>>>,
    by_addr: Arc<RwLock<HashMap<SocketAddr, String>>>,
    by_tok:  Arc<RwLock<HashMap<[u8; 16], String>>>,
    learned: Arc<RwLock<HashMap<String, SocketAddr>>>,
    rx_atom: Arc<RwLock<HashMap<String, Arc<AtomicU64>>>>,
    sr_atom: Arc<RwLock<HashMap<String, Arc<AtomicBool>>>>,
}

impl PeerRegistry {
    /// Ensure a stats entry exists (other maps are inserted by callers that have the values:
    /// addr/tok/atomics are produced during AddRemote). Returns the rx/sr atomics for the
    /// peer, creating them if absent — so AddRemote gets them from one call.
    fn ensure_stats(&self, name: &str) {
        self.stats.write().unwrap_or_else(|e| e.into_inner())
            .entry(name.to_string()).or_insert_with(PeerStats::default);
    }
    fn rx_atomic(&self, name: &str) -> Arc<AtomicU64> {
        self.rx_atom.write().unwrap_or_else(|e| e.into_inner())
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(AtomicU64::new(0))).clone()
    }
    fn sr_atomic(&self, name: &str) -> Arc<AtomicBool> {
        self.sr_atom.write().unwrap_or_else(|e| e.into_inner())
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false))).clone()
    }
    fn insert_addr(&self, addr: SocketAddr, name: &str) {
        self.by_addr.write().unwrap_or_else(|e| e.into_inner()).insert(addr, name.to_string());
    }
    fn insert_tok(&self, tok: [u8; 16], name: &str) {
        self.by_tok.write().unwrap_or_else(|e| e.into_inner()).insert(tok, name.to_string());
    }
    /// Forget the address and token a peer was reachable by, keeping its stats and counters.
    /// For a re-add — after a host, port or password change, or our own rename — so the old
    /// address and token stop resolving to this remote. The fresh ones are registered
    /// straight after, and a passive peer's address is learned again from its next poke.
    fn forget_reachability(&self, name: &str) {
        self.by_addr.write().unwrap_or_else(|e| e.into_inner()).retain(|_, v| v != name);
        self.by_tok.write().unwrap_or_else(|e| e.into_inner()).retain(|_, v| v != name);
        self.learned.write().unwrap_or_else(|e| e.into_inner()).remove(name);
    }
    /// Remove ALL shared per-peer entries for a removed/disabled remote, in one place.
    fn remove(&self, name: &str) {
        self.stats.write().unwrap_or_else(|e| e.into_inner()).remove(name);
        self.by_addr.write().unwrap_or_else(|e| e.into_inner()).retain(|_, v| v != name);
        self.by_tok.write().unwrap_or_else(|e| e.into_inner()).retain(|_, v| v != name);
        self.learned.write().unwrap_or_else(|e| e.into_inner()).remove(name);
        self.rx_atom.write().unwrap_or_else(|e| e.into_inner()).remove(name);
        self.sr_atom.write().unwrap_or_else(|e| e.into_inner()).remove(name);
    }
}


/// Shared inputs needed to build either audio engine, bundled once in main() so the build
/// logic can run at boot AND later from the SetInput/OutputDevice hot-command handler
/// (enables a direction without a restart). Holds clones of the stable Arcs/senders the
/// constructors need; the device name + per-remote routing are read from `cfg` inside each
/// method (matching boot exactly), so a late caller only needs to have saved the chosen
/// device to config first (which the settings-save handler already does). These methods
/// CONSTRUCT engines only — publishing the resulting handles into the AppState LateHandles is
/// the caller's responsibility (boot seeds them; the hot path publishes), keeping build and
/// publish separate.
#[derive(Clone)]
struct BuildCtx {
    socket:          Option<crate::net::udp::SharedSocket>,
    learned_addrs:   Arc<RwLock<HashMap<String, SocketAddr>>>,
    any_connected:   Arc<AtomicBool>,
    tx_bytes_accum:  Arc<AtomicU64>,
    event_tx:        tokio::sync::broadcast::Sender<String>,
    /// Line-up tone level in dBFS, from `[tone] level_db`. Read when the capture engine
    /// is built, so a change takes effect at startup or on an input-device rebuild.
    tone_level_db:   f32,
    startup_warnings: Arc<std::sync::Mutex<Vec<String>>>,
    /// Per-remote crypto state (CASCADE_ENCRYPTION_SPEC), shared with the send path.
    crypto_map:      net::CryptoMap,
}

impl BuildCtx {
    /// Build the OUTPUT (playback) engine from the device named in cfg. Mirrors the boot
    /// output build exactly: device lookup, warning on a configured-but-absent device,
    /// AudioEngine::start, per-remote receive routing + buffer from cfg. Returns the engine
    /// (Arc) + its OutputControl, or (None, None) if no device / not found / start failed.
    fn build_output(&self, cfg: &Config)
        -> (Option<Arc<AudioEngine>>, Option<crate::audio::engine::OutputControl>)
    {
        // Resolve by persistent UID first, name as fallback (see audio::resolve_device).
        let dev = if cfg.audio.output_device.is_empty() {
            None
        } else {
            crate::audio::resolve_device(false, &cfg.audio.output_device_uid,
                                         &cfg.audio.output_device)
                .map(|r| r.device)
        };
        match dev {
            None if !cfg.audio.output_device.is_empty() => {
                let msg = format!("Output device '{}' not found.", cfg.audio.output_device);
                warn!("{}", msg);
                self.startup_warnings.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
                (None, None)
            }
            None => (None, None),
            Some(d) => {
                let send_samples = crate::config::frame_ms_to_samples(
                    crate::config::DEFAULT_FRAME_MS);
                // Initial output chunk follows the MIN receive buffer over ENABLED remotes —
                // the same set `min_output_period` takes its minimum over — so the stream opens
                // at the size the first reconcile settles on instead of being rebuilt seconds
                // after boot. Falls back to the default buffer when no remote is enabled.
                let initial_buffer_ms = cfg.remotes.iter()
                    .filter(|r| r.enabled)
                    .map(|r| r.receive_buffer_ms)
                    .min()
                    .unwrap_or(crate::config::DEFAULT_RECEIVE_BUFFER_MS);
                // Report a failure rather than discarding it: an output device that cannot
                // be opened at startup must say so, as the input's "Audio capture failed"
                // line does. Silence here reads as success.
                let started = match AudioEngine::start(&d, initial_buffer_ms, send_samples,
                                                       self.event_tx.clone()) {
                    Ok(v)  => Some(v),
                    Err(e) => { warn!("Audio playback failed: {}", e); None }
                };
                match started {
                    Some((e, oc)) => {
                        // Enabled remotes only. A disabled remote carries no runtime state —
                        // DisableRemote removes it, and AddRemote applies all of this when the
                        // remote is enabled — so boot must not load it either.
                        for r in cfg.remotes.iter().filter(|r| r.enabled) {
                            // Phase lock BEFORE the buffer: set_peer_buffer reads the sync
                            // flag to choose its floor (20ms sync-on, 5ms sync-off), so a
                            // buffer applied first would take the sync-off floor and the
                            // later flag would not retroactively raise it.
                            e.set_phase_lock(&r.name, r.phase_lock);
                            e.set_peer_buffer(&r.name, r.receive_buffer_ms);
                            if let Some(ref matrix_str) = r.receive_matrix {
                                let recv_routes =
                                    crate::audio::routing::RoutingTable::parse_matrix(matrix_str);
                                e.set_recv_routing(&r.name, &recv_routes);
                                debug!("Remote '{}': receive routing from config ({} routes)",
                                      r.name, recv_routes.len());
                            }
                            debug!("Remote '{}': incoming buffer={}ms", r.name, r.receive_buffer_ms.clamp(5, 10000));
                        }
                        (Some(Arc::new(e)), Some(oc))
                    }
                    None => (None, None),
                }
            },
        }
    }

    /// Build the INPUT (capture/send) engine from the device named in cfg. Mirrors the boot
    /// input build exactly. Returns the engine plus the outgoing
    /// channel labels + OutgoingInfo rows the caller applies to its shared label/outgoing
    /// stores (boot did this inline; returning them keeps the method free of caller locals).
    /// On no-device / not-found / no-socket / start-failure returns (None, empty, empty).
    /// A busy port does NOT gate this. The audio path is built regardless of whether the
    /// socket has reached its configured address — capture, encoders and peers all come up,
    /// and the socket moves under them when the port frees: audio and devices are up and
    /// logged before the bind failure is reported.
    fn build_input(&self, cfg: &Config)
        -> (Option<CaptureEngine>, Vec<ChannelLabel>, Vec<OutgoingInfo>)
    {
        let mut labels: Vec<ChannelLabel> = Vec::new();
        let mut outgoing: Vec<OutgoingInfo> = Vec::new();
        // Resolve by persistent UID first, name as fallback (see audio::resolve_device).
        let input_dev = if cfg.audio.input_device.is_empty() {
            None
        } else {
            crate::audio::resolve_device(true, &cfg.audio.input_device_uid,
                                         &cfg.audio.input_device)
                .map(|r| r.device)
        };
        let eng = match input_dev {
            None if !cfg.audio.input_device.is_empty() => {
                let msg = format!("Input device '{}' not found.", cfg.audio.input_device);
                warn!("{}", msg);
                self.startup_warnings.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
                None
            }
            None => None,
            // No audio socket at all — the placeholder bind failed as well as the
            // configured one — so there is nothing to send through.
            Some(_) if self.socket.is_none() => {
                warn!("Audio capture not started — this daemon has no audio socket");
                None
            }
            Some(d) => {
                // Backend buffer sizing follows the SMALLEST frame size any enabled
                // remote is configured for (buffer = min(frame, 480)) — a larger
                // buffer would deliver multi-frame bursts for the small-frame
                // streams. Per-remote frame sizes are registered as encode prefs
                // below; this is only the seed for the fold, used when no remote is
                // enabled.
                let boot_frame_ms = cfg.remotes.iter()
                    .filter(|r| r.enabled)
                    .map(|r| r.frame_ms)
                    .fold(crate::config::DEFAULT_FRAME_MS, f32::min);
                match CaptureEngine::start(
                    &d,
                    crate::audio::encode::OUTGOING_CHANNELS_MAX,
                    self.tone_level_db,
                    cfg.audio.bitrate_kbps,
                    // No instance-wide mode: every remote carries its own, registered as
                    // an encode pref below. This is the template a peer inherits before its
                    // own preference lands.
                    &crate::config::AudioMode::default(),
                    boot_frame_ms,
                    self.socket.clone().expect("socket presence checked above"),
                    self.learned_addrs.clone(),
                    self.any_connected.clone(),
                    self.tx_bytes_accum.clone(),
                    self.event_tx.clone(),
                    self.crypto_map.clone(),
                ) {
                    Ok(eng) => {
                        for ch in 0..eng.num_out_ch {
                            let label = cfg.audio.channel_labels.get(ch)
                                .filter(|s| !s.is_empty())
                                .cloned()
                                .unwrap_or_else(|| format!("Ch {}", ch + 1));
                            labels.push(ChannelLabel {
                                channel: ch as u32,
                                label:   label.clone(),
                            });
                            outgoing.push(OutgoingInfo {
                                channel: ch as u8,
                                label,
                                active:  false,
                            });
                        }
                        // Every encoder runs constrained VBR — set_vbr + set_vbr_constraint,
                        // both directions of the bitrate setting — so CVBR is the fact
                        // whether the rate is a target or left to Opus. No figure is quoted
                        // for auto: Opus picks per channel and the value is not this line's
                        // to predict.
                        let bps_desc = if cfg.audio.bitrate_kbps == 0 {
                            "CVBR, bitrate auto".to_string()
                        } else {
                            format!("CVBR, {} kbps/ch", cfg.audio.bitrate_kbps)
                        };
                        info!("Encode: up to {} channels, {}", eng.num_out_ch, bps_desc);
                        // Enabled remotes only. Nothing runs DisableRemote for a remote that
                        // is already off, so a disabled remote's routes loaded here would never
                        // be removed: its channels would be Opus-encoded every frame for a
                        // destination never sent to, and its frame size would count toward the
                        // shared callback period. AddRemote loads them on enable.
                        for r in cfg.remotes.iter().filter(|r| r.enabled) {
                            let routes = if let Some(ref matrix_str) = r.send_matrix {
                                let parsed = crate::audio::routing::RoutingTable::parse_matrix(matrix_str);
                                debug!("Remote '{}': send routing from config ({} routes)",
                                      r.name, parsed.len());
                                parsed
                            } else {
                                vec![]
                            };
                            // Per-remote encode prefs first — frame_ms → frame bucket, plus the
                            // remote's mode — so the routing below lands straight on this
                            // remote's own streams rather than on the template's.
                            eng.set_peer_enc_prefs(&r.name, r.frame_ms, r.mode);
                            eng.set_send_routing(&r.name, &routes);
                        }
                        Some(eng)
                    }
                    Err(e) => { warn!("Audio capture failed: {}", e); None }
                }
            }
        };
        (eng, labels, outgoing)
    }
}


/// Where the settings file lives when `--config` was not given.
///
/// `cascade.toml` in the working directory WHEN IT ALREADY EXISTS, else a per-user
/// application directory. The cwd check comes first so running the binary from a terminal
/// keeps using the file sitting beside it, exactly as it always has.
///
/// The fallback is not a nicety. A daemon launched from Finder or Explorer does not choose
/// its own working directory — macOS gives an app bundle cwd `/` — and `Config::load`
/// CREATES the file when it is absent. A relative default therefore means the launcher
/// decides where settings are written, and under a bundle it means trying to write to `/`,
/// failing, and exiting before anything can report why.
///
///   macOS    ~/Library/Application Support/Cascade
///   Windows  %APPDATA%\Cascade
///   Linux    $XDG_CONFIG_HOME/cascade, else ~/.config/cascade
///
/// The directory is created here rather than left to `Config::load`, because the log wants
/// it too and opens first.
fn resolve_config_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit { return p; }

    let cwd_file = PathBuf::from("cascade.toml");
    if cwd_file.exists() { return cwd_file; }

    let dir = app_data_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join("cascade.toml")
}

/// Per-user application directory, by platform convention. Falls back to the working
/// directory when the environment names no home — a headless container, say — rather than
/// inventing a path.
fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join("Library/Application Support/Cascade");
        }
    }
    #[cfg(windows)]
    {
        if let Some(a) = std::env::var_os("APPDATA") {
            return PathBuf::from(a).join("Cascade");
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(x).join("cascade");
        }
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join(".config/cascade");
        }
    }
    PathBuf::from(".")
}

/// Wire the OS signals to `lifecycle::request`. Spawned once at startup; each future
/// resolves at most once, and the first to do so decides.
fn spawn_signal_handlers() {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            crate::lifecycle::request(crate::lifecycle::Exit::Quit);
        }
    });
    tokio::spawn(async {
        terminate_requested().await;
        crate::lifecycle::request(crate::lifecycle::Exit::Quit);
    });
}

/// Resolves when the OS asks the process to terminate, as distinct from an interactive
/// interrupt. Ctrl-C alone stopped being enough the moment Cascade could run with no
/// terminal attached: Activity Monitor's Quit, `kill`, `taskkill`, launchd and systemd all
/// send a terminate signal, and without this they bypass the clean shutdown — leaving the
/// output device hogged and any config change still inside the save debounce.
///
/// Unix: SIGTERM. That is the whole of it in practice.
///
/// WINDOWS GETS NOTHING USEFUL HERE, and the arm below should not be read as saying
/// otherwise. Tokio implements `ctrl_close`/`ctrl_shutdown` — like `ctrl_c` — through
/// `SetConsoleCtrlHandler`, so they are CONSOLE control events. Cascade links as a
/// GUI-subsystem binary and has no console, so none of them is ever delivered; the handlers
/// register and wait forever. The arm is kept because it costs nothing and does fire if the
/// binary is ever run with a console attached. A log off, restart or shutdown reaches the
/// daemon through the hidden top-level window in `platform::windows` instead, and Task
/// Manager's End task calls `TerminateProcess`, which nothing can intercept.
async fn terminate_requested() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => { sig.recv().await; }
            // Registration can only fail if the handler cannot be installed; the process
            // still runs, it just loses the graceful path. Never resolve, so the select
            // arm stays inert rather than spinning.
            Err(e) => { warn!("SIGTERM handler unavailable ({e}) — only Ctrl-C will shut down cleanly");
                        std::future::pending::<()>().await; }
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_close, ctrl_shutdown};
        let (mut close, mut shutdown) = match (ctrl_close(), ctrl_shutdown()) {
            (Ok(c), Ok(s)) => (c, s),
            _ => { warn!("console-control handlers unavailable — only Ctrl-C will shut down cleanly");
                   std::future::pending::<()>().await; unreachable!() }
        };
        tokio::select! { _ = close.recv() => {}, _ = shutdown.recv() => {} }
    }
}

/// The log sits beside the settings file, whichever one is in play — so wherever you find
/// the config you are already in the right directory to read the log.
fn log_path(config: &std::path::Path) -> PathBuf {
    // Named after the CONFIG, not a fixed "cascade.log": two instances sharing a directory
    // with different --config files would otherwise write to one file and rotate each
    // other's away on startup. `cascade.toml` still gives `cascade.log`, so the ordinary
    // case is unchanged.
    let stem = config.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cascade".into());
    config.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .join(format!("{stem}.log"))
}

/// Open the log, rotating one generation aside so a run is never read mixed with the last.
/// Returns None if it cannot be opened — a read-only directory, say. That is not fatal:
/// on a terminal the stdout layer still reports, and a daemon that refuses to start
/// because it could not write a log would be the worse failure.
fn open_log(path: &std::path::Path) -> Option<std::fs::File> {
    if path.exists() {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
}

#[derive(Parser, Debug)]
#[command(name = "cascade", about = "Cascade audio-over-IP daemon")]
struct Args {
    /// None means "decide from the environment" — see resolve_config_path().
    #[arg(long)]
    config: Option<PathBuf>,
    /// Measure this machine's resampler cost and exit. No toolchain needed — run it from
    /// a copied binary to compare machines.
    #[arg(long)] selftest: bool,
    #[arg(long)] name:    Option<String>,
    #[arg(long)] port:    Option<u16>,
    #[arg(long)] remotes: Option<String>,
}

/// Wire labels for one peer, derived from a set of send routes (src → dst slot).
/// THE LABEL TRAVELS WITH THE AUDIO: a destination slot is labelled with the name
/// of the source routed to it, so "Signal A" routed to stream 9 arrives as stream 9
/// "Signal A" at the receiver. Slots with no route get NO label (the wire format pads
/// them empty) — a connection carrying 4 streams advertises labels for exactly
/// those 4. A routed but unnamed source sends "Ch {src+1}" so the
/// receiver still sees the sender's physical channel numbering.
fn labels_from_routes(channel_labels: &[String],
                      routes: &[audio::routing::RouteEntry])
                      -> Vec<net::peer::ChannelLabel> {
    routes.iter().map(|r| {
        let label = channel_labels.get(r.src as usize)
            .filter(|s| !s.is_empty()).cloned()
            .unwrap_or_else(|| format!("Ch {}", r.src + 1));
        net::peer::ChannelLabel { channel: r.dst as u32, label }
    }).collect()
}

/// Same, looking the peer's saved send_matrix up in the config.
fn routed_labels_for_peer(cfg: &config::Config, peer_name: &str)
                          -> Vec<net::peer::ChannelLabel> {
    let matrix = cfg.remotes.iter().find(|r| r.name == peer_name)
        .and_then(|r| r.send_matrix.clone()).unwrap_or_default();
    let routes = audio::routing::RoutingTable::parse_matrix(&matrix);
    labels_from_routes(&cfg.audio.channel_labels, &routes)
}

/// Overlay tone routing onto a peer's wire labels: a destination slot carrying
/// line-up tone advertises "Tone L" / "Tone R" — the label travels with the
/// audio exactly as for a signal source. Tone replaces any signal label on the slot
/// (one source per destination).
fn overlay_tone_labels(mut labels: Vec<net::peer::ChannelLabel>,
                       tone_slots: Option<&Vec<u8>>)
                       -> Vec<net::peer::ChannelLabel> {
    if let Some(slots) = tone_slots {
        for (ci, &t) in slots.iter().enumerate() {
            if t != 0 {
                labels.retain(|l| l.channel != ci as u32);
                labels.push(net::peer::ChannelLabel {
                    channel: ci as u32,
                    label: if t == 1 { "Tone L".to_string() }
                           else      { "Tone R".to_string() },
                });
            }
        }
    }
    labels
}

/// The label revision after `v`: +1, wrapping 255 to 1 so it never lands on 0.
fn next_label_revision(v: u8) -> u8 {
    if v == 255 { 1 } else { v + 1 }
}

/// A label reload (CASCADE_WIRE_PROTOCOL_SPEC §3.2/§3.3): fix how many entries label pushes
/// carry from now on (net::peer::LABEL_ENTRIES — the input's channel count, up to 128, while
/// one is running; 128 while none is), then advance the label revision every poke carries
/// at 0x0A so peers re-request.
///
/// Labels reload twice at launch (configuration load, then audio start — so the first poke
/// carries 2), on every label edit, on every change to per-remote send routing or tone
/// (whose labels travel with them), and on every audio restart: a device change or loss, an
/// exclusive-access change, and a callback-period change while audio is running.
fn reload_labels(indicator: &std::sync::atomic::AtomicU8,
                 in_channels: &api::LateHandle<std::sync::atomic::AtomicUsize>) {
    let live_in = in_channels.get()
        .map(|a| a.load(std::sync::atomic::Ordering::Relaxed) as u32).unwrap_or(0);
    let entries = if live_in > 0 { live_in.min(net::peer::LABEL_SLOTS) }
                  else { net::peer::LABEL_SLOTS };
    net::peer::LABEL_ENTRIES.store(entries, std::sync::atomic::Ordering::Relaxed);
    bump_label_revision(indicator);
}

/// Advance the label revision by one step (`next_label_revision`).
fn bump_label_revision(indicator: &std::sync::atomic::AtomicU8) {
    let _ = indicator.fetch_update(std::sync::atomic::Ordering::Relaxed,
                                   std::sync::atomic::Ordering::Relaxed,
                                   |v| Some(next_label_revision(v)));
}

#[cfg(test)]
mod label_revision_tests {
    use super::next_label_revision;

    #[test]
    fn advances_by_one() {
        assert_eq!(next_label_revision(1), 2);
        assert_eq!(next_label_revision(254), 255);
    }

    #[test]
    fn wraps_to_one_never_zero() {
        assert_eq!(next_label_revision(255), 1);
        assert_eq!(next_label_revision(0), 1);
    }

    #[test]
    fn two_launch_loads_put_two_on_the_wire() {
        let v = (0..2).fold(0u8, |v, _| next_label_revision(v));
        assert_eq!(v, 2);
    }
}

/// Build the runtime and run the daemon. Returns when the daemon has shut down.
///
/// Explicit multi-thread runtime so thread priority can be raised on the threads that carry
/// audio work. Two workers is enough: audio runs on its own dedicated encode/decode
/// threads, so these only carry I/O, and keeping the count low keeps total OS threads down.
///
/// `block_on` drives `async_main` — the select loop and e.receive — on THE CALLING THREAD,
/// not a worker, which is why `async_main` raises its own thread directly.
/// `on_thread_start` covers the WORKER threads (peer I/O, device managers, decode dispatch
/// wrappers) so they match.
fn run_daemon() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .on_thread_start(|| crate::audio::encode::set_qos_user_initiated())
        .build()?;
    rt.block_on(async_main())
}

fn main() -> Result<()> {
    // macOS: AppKit owns the main thread, so the daemon moves to a thread of its own.
    //
    // NSApplication must be created and run on the PROCESS MAIN THREAD and its run loop
    // never returns, so a menu-bar item cannot coexist with `block_on` here. The daemon
    // keeps its priority regardless: `set_qos_user_initiated()` raises whichever thread
    // calls it, and `async_main` calls it on itself. The main thread becomes a menu pump at
    // default priority, which is correct for what it now does.
    //
    // Shutdown needs no negotiation between the two loops. The daemon thread exits the
    // process once `async_main` has returned — by which point the clean shutdown has
    // already run — and that ends the AppKit loop with it.
    #[cfg(target_os = "macos")]
    {
        std::thread::Builder::new()
            .name("cascade-daemon".into())
            .spawn(|| {
                let code = match run_daemon() {
                    Ok(()) => 0,
                    Err(e) => { report_fatal(&e); 1 }
                };
                std::process::exit(code);
            })?;
        crate::platform::macos::run_menu_bar();
    }

    #[cfg(not(target_os = "macos"))]
    {
        run_daemon().inspect_err(report_fatal)
    }
}

/// Build time as a readable UTC stamp, from the value build.rs baked in.
fn build_stamp() -> String {
    let secs: u64 = env!("CASCADE_BUILD_UNIX").parse().unwrap_or(0);
    // Civil date from a unix timestamp, without pulling in a date crate for one log line.
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let mut y = 1970i64;
    let mut d = days;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if d < len { break; }
        d -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let ml = [31, if leap {29} else {28}, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 0usize;
    while mo < 12 && d >= ml[mo] { d -= ml[mo]; mo += 1; }
    format!("{y:04}-{:02}-{:02} {h:02}:{m:02}:{s:02}Z", mo + 1, d + 1)
}

/// Report a startup failure somewhere a person will actually find it.
///
/// Returning `Err` from `main` prints to stderr — and neither a Windows GUI-subsystem binary
/// nor a macOS app bundle HAS a stderr anyone sees, so the process vanished after a handful
/// of log lines with no explanation anywhere. Anything fatal goes to the log file, and on
/// Windows and macOS to a dialog as well, since that is the only channel a double-clicked
/// daemon has.
fn report_fatal(e: &anyhow::Error) {
    error!("fatal: {e:#}");
    #[cfg(windows)]
    crate::platform::windows::show_report(
        "Cascade — failed to start", &format!("{e:#}"));
    #[cfg(target_os = "macos")]
    crate::platform::macos::show_report(
        "Cascade — failed to start", &format!("{e:#}"));
}

/// Bind the audio socket. One attempt, on every platform: a busy port is reported at once.
///
/// Port and interface changes rebind in place — no process hands the port to a successor —
/// so a busy port is a genuine conflict with another application, and is reported
/// immediately.
fn bind_audio_socket(addr: SocketAddr) -> Result<UdpEngine> {
    UdpEngine::bind(addr)
}


async fn async_main() -> Result<()> {
    // Arguments are parsed FIRST because the log's location follows the config's, and the
    // log has to be open before anything worth reading is emitted.
    // try_parse, not parse: a parse failure — or --help — writes to stderr and exits, and a
    // Windows GUI-subsystem binary has no stderr, so the process would vanish with nothing
    // said anywhere. On Windows the message goes to a dialog first. Everywhere else e.exit()
    // prints and exits exactly as parse() would.
    let args = match Args::try_parse() {
        Ok(a) => a,
        Err(e) => {
            #[cfg(windows)]
            crate::platform::windows::show_report("Cascade", &e.to_string());
            e.exit();
        }
    };

    // Measurement mode: no config, no devices, no sockets.
    //
    // The report goes to three places because the binary has to be usable in all three
    // situations: stdout for a terminal on macOS or Linux, a text file beside the
    // executable for a copied binary with nowhere to print, and on Windows a dialog —
    // where the GUI subsystem means stdout does not exist at all and printing is silent.
    if args.selftest {
        // Printed section by section rather than assembled and printed at the end: a
        // section that hangs then still leaves the earlier ones on screen.
        let mut report = String::new();
        for section in [crate::audio::zita::self_test(),
                        crate::audio::encode::self_test()] {
            println!("{section}");
            report.push_str(&section);
            report.push('\n');
        }
        #[cfg(target_os = "linux")]
        {
            let section = crate::audio::scheduler::pool::self_test();
            println!("{section}");
            report.push_str(&section);
        }
        let written = std::env::current_exe().ok()
            .and_then(|e| e.parent().map(|d| d.join("cascade-selftest.txt")))
            .filter(|p| std::fs::write(p, &report).is_ok());
        if let Some(ref p) = written {
            println!("written to {}", p.display());
        }
        #[cfg(windows)]
        {
            let shown = match written {
                Some(ref p) => format!("{report}\n\nAlso written to {}", p.display()),
                None => report,
            };
            crate::platform::windows::show_report("Cascade — resampler self-test", &shown);
        }
        return Ok(());
    }

    // Before ANYTHING is opened. The process being replaced still holds the exclusive audio
    // device and both sockets until it exits, and an exclusive endpoint admits one owner.
    //
    let config_path = resolve_config_path(args.config.clone());
    let log_file    = open_log(&log_path(&config_path));

    // Honour RUST_LOG when set (e.g. RUST_LOG=cascade=debug); fall back to
    // cascade=info only when RUST_LOG is absent — never appended, which would override
    // RUST_LOG.
    //
    // Two sinks, because the daemon has two lives. Run from a terminal it should print as
    // it always has; run from Finder, Explorer or a service manager there is no terminal —
    // on Windows, under the GUI subsystem, not even a stdout to write to — and the file is
    // the only record. The stdout layer is therefore attached only when stdout really is a
    // terminal, so a backgrounded run does not pay to format every event twice.
    //
    // ANSI is off for the file unconditionally: tracing-subscriber colours its output
    // whether or not the sink can display it, which fills a log file with escape codes.
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use std::io::IsTerminal;
    let stdout_is_tty = std::io::stdout().is_terminal();
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("cascade=info")))
        .with(log_file.map(|f| tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Arc::new(f))))
        .with(stdout_is_tty.then(|| tracing_subscriber::fmt::layer()))
        .init();

    // The select loop and e.receive run on the thread that calls block_on, and the
    // runtime's on_thread_start covers only WORKER threads — so this thread needs the
    // elevation applied directly. It puts the calling thread in the audio-adjacent class:
    // USER_INITIATED on macOS, nice -10 on Linux, MMCSS "Audio" on Windows. Called after
    // tracing init so the result is logged.
    crate::audio::encode::set_qos_user_initiated();

    // Take a process activity assertion (LatencyCritical + idle-sleep and termination
    // disabled) so macOS App Nap and timer coalescing cannot freeze our threads under
    // system load. Held for the process lifetime.
    crate::audio::activity::begin_activity();

    // Ctrl-C and terminate feed lifecycle::request rather than being select arms, so they
    // take the identical path as a tray or menu-bar Quit.
    spawn_signal_handlers();

    let mut cfg = Config::load(&config_path)?;
    let config_path_for_save = config_path.clone();
    // FIRST line worth having in any log: which binary is this? Everything else is
    // ambiguous without it.
    info!("Cascade {} — build {}", env!("CARGO_PKG_VERSION"), build_stamp());
    info!("Settings {}", config_path.display());
    if let Some(n) = args.name    { cfg.general.name = n; }
    if let Some(p) = args.port    { cfg.general.port = p; }
    if let Some(r) = args.remotes {
        cfg.remotes = r.split(',').filter_map(|spec| {
            let p: Vec<&str> = spec.splitn(2, '=').collect();
            if p.len() != 2 { return None; }
            let addr: SocketAddr = p[1].trim().parse().ok()?;
            Some(config::RemoteConfig { name: p[0].trim().to_string(),
                host: addr.ip().to_string(), port: addr.port(), enabled: true,
                phase_lock: false, password: String::new(),
                receive_buffer_ms: crate::config::DEFAULT_RECEIVE_BUFFER_MS,
                mode: crate::config::AudioMode::default(),
                frame_ms: crate::config::DEFAULT_FRAME_MS,
                encryption: false,
                send_matrix: None, receive_matrix: None })
        }).collect();
    }
    let cfg_arc = Arc::new(RwLock::new(cfg.clone()));

    // our_token is derived per-remote: MD5(UPPER(our_name) + UPPER(remote.password))
    // Each remote entry carries its own name and password for the connection.
    info!("Instance '{}'", cfg.general.name);

    // All enabled remotes — resolve hostnames asynchronously. Passive / receive-only
    // remotes have an empty host and start with addr=None (they never send POKEs;
    // their PeerTask will re-resolve on each reconnect cycle once a host is set).
    let mut all_remotes: Vec<(String, Option<SocketAddr>)> = Vec::new();
    for r in cfg.remotes.iter().filter(|r| r.enabled) {
        let addr = if r.host.is_empty() {
            None
        } else {
            match net::resolve_addr_async(&r.host, r.port).await {
                Ok(a)  => { info!("Remote '{}': DNS '{}' → {}", r.name, r.host, a); Some(a) }
                Err(e) => { warn!("Remote '{}': DNS '{}' failed: {} (passive until resolved)",
                                   r.name, r.host, e); None }
            }
        };
        all_remotes.push((r.name.clone(), addr));
    }

    // Bytes 0x08-0x0B are four independent single-byte fields (spec §2.1): flags,
    // sample rate, label revision, destination index. POKE writes its
    // version at byte 0x0A; POKE_RSP echoes the incoming POKE's word, so each side's
    // RSPs carry the OTHER side's version.

    let startup_warnings: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let bind_addr: SocketAddr = {
        // Clone the configured name so the immutable borrow of `cfg` is released
        // before the None arm can heal it.
        let iface = cfg.general.network_interface.clone();
        match net::iface::resolve_bind_addr(&iface, cfg.general.port) {
            Some(addr) => {
                if iface.eq_ignore_ascii_case("any") {
                    info!("Network interface: any (0.0.0.0:{port})", port = cfg.general.port);
                } else {
                    info!("Network interface: {} → {}", iface, addr);
                }
                addr
            }
            None => {
                // The configured interface isn't currently present. Bind to any (0.0.0.0)
                // as a fallback so the daemon starts. The preference is preserved in config,
                // and the interface watcher rebinds to it in place when it appears. A startup
                // warning surfaces this in the UI on browser connect.
                let msg = format!("Network interface '{}' unavailable. Bound to any (0.0.0.0).",
                                  iface);
                warn!("{}", msg);
                startup_warnings.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
                format!("0.0.0.0:{}", cfg.general.port).parse()?
            }
        }
    };
    // Attempt to bind the UDP port. EADDRINUSE means another application (or another
    // instance) already holds it: the daemon still comes up completely on a placeholder
    // socket, and the probe below moves it onto the configured address the moment that
    // frees. `port_conflict_arc` is true only for that window. Any other bind error is fatal.
    let port_conflict_arc = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (engine_built, mut inbound_rx_opt, outbound_tx_opt, audio_socket_opt, recv_ctx_opt) = {
        match bind_audio_socket(bind_addr) {
            Ok(engine) => {
                let (rx, tx, sock, ctx) = engine.start();
                (true, Some(rx), Some(tx), Some(sock), Some(ctx))
            }
            Err(e) => {
                let is_in_use = e.downcast_ref::<std::io::Error>()
                    .map(|io| io.kind() == std::io::ErrorKind::AddrInUse)
                    .unwrap_or(false);
                if is_in_use {
                    error!("Socket cannot bind to specified port {}, 'Address already in \
                            use' — taking it as soon as it frees", cfg.general.port);
                    port_conflict_arc.store(true, std::sync::atomic::Ordering::Relaxed);

                    // A BUSY PORT IS NOT A DEAD END, AND NOT A REASON TO COME UP CRIPPLED.
                    //
                    // The daemon builds completely — audio devices, encoders, decoders,
                    // peer tasks — on a placeholder socket, and the probe below moves it
                    // onto the real address the moment that address frees. Nothing is
                    // restarted and nothing is built late, because everything downstream
                    // holds the socket CELL rather than a socket.
                    //
                    // Audio and devices come up first, the bind failure is logged, and the
                    // bind is re-attempted on the periodic monitor cycle until it succeeds —
                    // taking the port within about a second of it becoming free.
                    let placeholder = SocketAddr::from(([127, 0, 0, 1], 0));
                    match bind_audio_socket(placeholder) {
                        Ok(engine) => {
                            let (rx, tx, sock, ctx) = engine.start();
                            // Probe on EVERY platform, once a second. A failed
                            // attempt is the ordinary case while another process holds the
                            // port and says nothing; only the transition is logged.
                            {
                                let cell = sock.clone();
                                let pc   = Arc::clone(&port_conflict_arc);
                                let cfg_probe = Arc::clone(&cfg_arc);
                                std::thread::Builder::new()
                                    .name("cascade-port-probe".into())
                                    .spawn(move || loop {
                                        std::thread::sleep(
                                            std::time::Duration::from_secs(1));
                                        if crate::lifecycle::is_stopping() { return; }
                                        // Already on a real address — a port or interface
                                        // change from settings got there first. Stop: carrying
                                        // on would later move the socket BACK to whatever this
                                        // thread was aiming at, undoing that change.
                                        if !pc.load(std::sync::atomic::Ordering::Relaxed) {
                                            return;
                                        }
                                        // Aim at the CURRENTLY configured address, read fresh
                                        // each attempt — it may have changed while the port
                                        // was busy.
                                        let (iface, port) = {
                                            let c = cfg_probe.read()
                                                .unwrap_or_else(|e| e.into_inner());
                                            (c.general.network_interface.clone(), c.general.port)
                                        };
                                        let want = crate::net::iface::resolve_bind_addr(&iface, port)
                                            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], port)));
                                        if crate::net::udp::rebind(&cell, want).is_ok()
                                            && pc.swap(false, std::sync::atomic::Ordering::Relaxed)
                                        {
                                            info!("Socket is open and listening on UDP \
                                                   port {} at address '{}'",
                                                  want.port(), want.ip());
                                            return;
                                        }
                                    }).ok();
                            }
                            (true, Some(rx), Some(tx), Some(sock), Some(ctx))
                        }
                        Err(e2) => {
                            error!("port {} is busy and a placeholder socket could not be \
                                    bound either ({e2}) — no audio path", cfg.general.port);
                            (false, None, None, None, None)
                        }
                    }
                } else {
                    return Err(e);
                }
            }
        }
    };
    // Start OS-native interface watcher. Event-driven (kqueue on macOS, netlink
    // on Linux) — zero overhead between events. Fires only on actual topology
    // changes (cable plug/unplug, Wi-Fi join, VPN up/down).
    // Event broadcast channel — created before any background threads or peer tasks
    // so that all of them can push WS events from the start.
    let (event_tx_early, _) = tokio::sync::broadcast::channel::<String>(32);

    {
        let iface_event_tx = event_tx_early.clone();
        // The socket cell the watcher rebinds through, and the config it reads the interface
        // and port from. Read LIVE on every event, not captured here: an interface or port
        // chosen in settings after startup must be watched too, and a loss must fall back to
        // the port in use now rather than the one the daemon started with.
        let iface_socket = audio_socket_opt.clone();
        let iface_cfg    = Arc::clone(&cfg_arc);
        // The watcher invokes this inline on its own thread on each topology change —
        // no second handler thread, no channel hop. The work is cheap: build the
        // UI payload + a presence check.
        //
        // (interface last watched, whether it was present then).
        let mut watched: (String, bool) = {
            let name = cfg.general.network_interface.clone();
            let present = name.eq_ignore_ascii_case("any") || net::iface::interface_present(&name);
            (name, present)
        };
        net::iface::start_watcher(move |current| {
            // Push the fresh interface list to the UI so the Settings dropdown
            // updates immediately (single source of truth, pushed — same model as
            // the audio device inventory). Sent on every topology change.
            let list: Vec<serde_json::Value> = current.iter().map(|i| {
                serde_json::json!({"value": i.name,
                                   "label": format!("{} ({})", i.name, i.addr)})
            }).collect();
            let _ = iface_event_tx.send(
                serde_json::json!({"type":"interfaces","interfaces": list}).to_string());

            let (configured_iface, iface_port) = {
                let c = iface_cfg.read().unwrap_or_else(|e| e.into_inner());
                (c.general.network_interface.clone(), c.general.port)
            };
            if configured_iface.eq_ignore_ascii_case("any") {
                watched = (configured_iface, true);
                return;
            }
            let now_present = net::iface::interface_present(&configured_iface);
            // A different interface from last time means settings changed it, and that path
            // has already bound it. Start watching it from here without firing a transition.
            if configured_iface != watched.0 {
                watched = (configured_iface, now_present);
                return;
            }
            let was_present = watched.1;
            // Both transitions rebind in place — port and interface are bound together and
            // nothing restarts. Falling back to any on removal, and returning to the
            // configured interface when it reappears, is Cascade's own policy.
            if was_present && !now_present {
                warn!("Network interface '{}' has disappeared — falling back to any",
                      configured_iface);
                if let Some(ref cell) = iface_socket {
                    let any = SocketAddr::from(([0, 0, 0, 0], iface_port));
                    if let Err(e) = crate::net::udp::rebind(cell, any) {
                        warn!("fallback to {} failed: {}", any, e);
                    }
                }
                let _ = iface_event_tx.send(
                    serde_json::json!({"type":"network_error",
                        "msg": format!("Network interface '{}' unavailable — using any.",
                                       configured_iface)
                    }).to_string());
            } else if !was_present && now_present {
                info!("Network interface '{}' is back — rebinding to it", configured_iface);
                if let Some(ref cell) = iface_socket {
                    match crate::net::iface::resolve_bind_addr(&configured_iface, iface_port) {
                        Some(addr) => {
                            if let Err(e) = crate::net::udp::rebind(cell, addr) {
                                warn!("rebind to {} failed: {}", addr, e);
                            }
                        }
                        None => warn!("interface '{}' reported present but has no address",
                                      configured_iface),
                    }
                }
                let _ = iface_event_tx.send(
                    serde_json::json!({"type":"network_error",
                        "msg": format!("Network interface '{}' back in use.", configured_iface)
                    }).to_string());
            }
            watched.1 = now_present;
        });
    }

    // audio_socket: the bound UDP socket shared with the main loop so audio
    // packets are sent directly — no intermediate task wakeup on the send path.
    // Control packets (POKE, RSP, ACK, labels) still route through outbound_tx.

    let peer_stats_map:    Arc<RwLock<HashMap<String, PeerStats>>> = Arc::new(RwLock::new(HashMap::new()));
    let incoming_channels: Arc<RwLock<std::collections::HashMap<String, Vec<ChannelInfo>>>> = Arc::new(RwLock::new(std::collections::HashMap::new()));
    // Last time an audio packet arrived for each (peer, slot). Drives the `active`
    // flag's decay: a channel is "active" only while it is actually streaming, so the
    // RX page can drop a source the moment its stream stops — independent of whether
    // it still has a name.
    let incoming_seen: Arc<std::sync::Mutex<std::collections::HashMap<(String, u8), std::time::Instant>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let outgoing_channels: Arc<RwLock<Vec<OutgoingInfo>>>          = Arc::new(RwLock::new(Vec::new()));

    // Dynamic address map — updated when passive peers send their first poke
    let learned_addrs: Arc<RwLock<HashMap<String, SocketAddr>>> = Arc::new(RwLock::new(
        all_remotes.iter()
            .filter_map(|(name, addr)| addr.map(|a| (name.clone(), a)))
            .collect()
    ));

    // Audio gate: don't send audio until at least one peer is Connected.
    // Gate: send audio only after at least one peer reaches Connected state.
    // The passive side starts audio only after its own POKE→POKE_RSP completes; sending
    // before that makes the peer report unhandled audio packets.
    let any_connected = Arc::new(AtomicBool::new(false));

    // tx byte accumulator + tone channel list — defined here so BuildCtx can be assembled once and used for BOTH the output and input
    // builds below, and reused later by the SetInput/OutputDevice hot path.
    let tx_bytes_accum = Arc::new(AtomicU64::new(0));
    // Per-remote crypto state (CASCADE_ENCRYPTION_SPEC): one PeerCrypto per
    // configured remote, generated once here (fresh random X25519 keypair each).
    // Shared across the peer tasks (key exchange), the send path (encrypt), and the
    // receive path (decrypt). Created before the capture engine so the send path
    // captures the same map.
    let crypto_map: net::CryptoMap = Arc::new(RwLock::new(HashMap::new()));
    {
        let mut cm = crypto_map.write().unwrap_or_else(|e| e.into_inner());
        for r in &cfg.remotes {
            cm.insert(r.name.clone(),
                Arc::new(net::crypto::PeerCrypto::new(r.encryption)));
        }
    }
    // Per-remote link indices (header bytes 0x0B/0x11 — see net::LinkIndices), shared by
    // the peer tasks (learn from pongs), the send path (stamp) and the receive path (check).
    let link_map: net::LinkMap = Arc::new(RwLock::new(HashMap::new()));
    // Incoming-signal metering (meter.rs): the watch the API touches, the pre-decode meter
    // worker the receive path feeds, and the snapshot every viewer reads.
    let meters = meter::Meters::start();

    let build_ctx = BuildCtx {
        socket:           audio_socket_opt.clone(),
        learned_addrs:    learned_addrs.clone(),
        any_connected:    any_connected.clone(),
        tx_bytes_accum:   tx_bytes_accum.clone(),
        event_tx:         event_tx_early.clone(),
        tone_level_db:    cfg.tone.level_db,
        startup_warnings: startup_warnings.clone(),
        crypto_map:       crypto_map.clone(),
    };

    // Audio output start is INDEPENDENT of the UDP bind: it is gated on device presence
    // alone. A busy port changes nothing here — the socket moves onto the configured
    // address under a fully built engine when the port frees.
    // Audio output build (device lookup + AudioEngine::start + per-remote recv routing from
    // cfg) lives in BuildCtx::build_output so the same path serves boot and the hot
    // enable.
    // `audio_engine` is `mut` so the SetOutputDevice None-branch can build it live and
    // reassign None→Some; the in-loop readers (`if let Some(ref e) = audio_engine`)
    // then see the new engine. Boot-capture sites snapshot handles via LateHandle / standalone
    // Arcs, which the late build republishes.
    let (mut audio_engine, mut output_ctrl) = build_ctx.build_output(&cfg);

    let (disconnect_tx, mut disconnect_rx) =
        tokio::sync::mpsc::unbounded_channel::<String>();

    let (stats_tx, mut stats_rx) = mpsc::channel::<(String, PeerStats)>(64);
    // Active-stream decay: every 500ms, clear `active` for any (peer, slot) that
    // hasn't received a packet in the last 1.5s. This makes the RX page drop a
    // source as soon as its stream stops, regardless of any name still attached.
    {
        let incoming_c = incoming_channels.clone();
        let seen_c     = incoming_seen.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
            const STALE: std::time::Duration = std::time::Duration::from_millis(1500);
            loop {
                tick.tick().await;
                let now = std::time::Instant::now();
                // LOCK ORDER (must match the channel registration in net::udp's receive
                // path): always take incoming_channels BEFORE incoming_seen. The opposite
                // order deadlocks against registration, each side holding one lock and
                // waiting on the other. Keep this order anywhere both locks are held.
                let mut chans = incoming_c.write().unwrap_or_else(|e| e.into_inner());
                let seen = seen_c.lock().unwrap_or_else(|e| e.into_inner());
                for (peer, list) in chans.iter_mut() {
                    for ci in list.iter_mut() {
                        if !ci.active { continue; }
                        let fresh = seen.get(&(peer.clone(), ci.channel))
                            .map(|t| now.duration_since(*t) < STALE)
                            .unwrap_or(false);
                        if !fresh { ci.active = false; }
                    }
                }
            }
        });
    }
    // Signal channel: stats task notifies main loop to rebuild the send destinations on connect.
    let (rebuild_tx, mut rebuild_rx) = tokio::sync::mpsc::channel::<()>(8);
    // Notify channel: stats task fires this when incoming channel labels update,
    // so the main loop can push a WS event immediately rather than waiting for pollSlow.
    let (label_notify_tx, mut label_notify_rx) = tokio::sync::mpsc::channel::<String>(16);
    // Per-peer numeric connection status for the send side's §3.1 per-destination gate
    // (0 = down, 1 = connected w/ address mismatch, 2 = connected clean). Created here
    // because the stats pump below is spawned before the capture engine exists; the engine
    // adopts this same map once built, so both see one set of live atomics.
    let peer_status_map: Arc<RwLock<HashMap<String, Arc<std::sync::atomic::AtomicU8>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    {
        let stats_map_c    = peer_stats_map.clone();
        let status_map_c   = peer_status_map.clone();
        let incoming_c     = incoming_channels.clone();
        let connected_flag = any_connected.clone();
        let disc_tx        = disconnect_tx.clone();
        let rebuild_tx_c   = rebuild_tx.clone();
        let label_tx_c     = label_notify_tx.clone();
        tokio::spawn(async move {
            // Peer → (publishing task's instance, last state).
            let mut prev: HashMap<String, (u64, String)> = HashMap::new();
            while let Some((name, s)) = stats_rx.recv().await {
                // A task's final report as it shuts down: withdraw what it published, unless
                // a newer task already owns the name (a re-add can start its successor before
                // this arrives). Its earlier reports cannot come after this one — they share a
                // sender and were queued first — so nothing it said survives it.
                if s.ended {
                    if prev.get(&name).map(|(i, _)| *i) == Some(s.instance) {
                        prev.remove(&name);
                        {
                            let mut sm = stats_map_c.write().unwrap_or_else(|e| e.into_inner());
                            if sm.get(&name).map(|p| p.instance) == Some(s.instance) {
                                sm.remove(&name);
                            }
                        }
                        // Down, not removed: the send path's cached destinations hold this
                        // Arc, and a successor task stores into the same one.
                        if let Some(a) = status_map_c.read().unwrap_or_else(|e| e.into_inner())
                            .get(&name) {
                            a.store(0, Ordering::Relaxed);
                        }
                        connected_flag.store(prev.values().any(|(_, st)| st == "connected"),
                                             Ordering::Relaxed);
                    }
                    continue;
                }
                let state_changed = prev.get(&name).map(|(_, p)| p != &s.state).unwrap_or(true);
                if state_changed {
                    // Only log state changes that are operator-visible.
                    // Suppress the initial idle→idle or idle→disconnected bounce that
                    // appears on every startup: 'idle' is the initial state of every
                    // peer task before the first POKE is exchanged, and seeing
                    // "[Cascade] idle latency=0.0ms" followed by "[Cascade] disconnected"
                    // looks like a problem when it isn't.
                    let was_connected = prev.get(&name).map(|(_, p)| p == "connected").unwrap_or(false);
                    match s.state.as_str() {
                        "connected" => info!("[{}] connected latency={:.1}ms", name, s.latency_ms),
                        "idle" if was_connected => info!("[{}] disconnected — audio send to this peer stopped", name),
                        _ => {}
                    }
                    if s.state == "idle" {
                        let _ = disc_tx.send(name.clone());
                    }
                }
                prev.insert(name.clone(), (s.instance, s.state.clone()));
                // Publish the numeric status for the per-destination send gate. Insert
                // before the rebuild signal below so a newly-connected peer's Arc is in
                // place when the cached destination table is rebuilt.
                {
                    let existing = status_map_c.read().unwrap_or_else(|e| e.into_inner())
                        .get(&name).cloned();
                    match existing {
                        Some(a) => a.store(s.status, Ordering::Relaxed),
                        None => {
                            status_map_c.write().unwrap_or_else(|e| e.into_inner())
                                .insert(name.clone(),
                                        Arc::new(std::sync::atomic::AtomicU8::new(s.status)));
                        }
                    }
                }
                // LIVE, not a latch. This flag is the send path's coarse gate: with it
                // false, encode_and_send_job is never dispatched, so no Opus encoding is
                // done at all. It has to fall as well as rise — set-only, it pins the
                // encoder to full load for the life of the process once any peer has ever
                // connected, whether or not one still is.
                //
                // CASCADE_AUDIO_SEND_SPEC §3.1 requires the connection gate to be
                // "re-evaluated fresh every cycle, rather than a stateful enable/disable
                // transition to manage". `prev` was just updated above, and a peer that is
                // torn down withdraws itself (the `ended` report), so it is the current state
                // of every peer that still exists.
                let any_now = prev.values().any(|(_, st)| st == "connected");
                connected_flag.store(any_now, Ordering::Relaxed);
                if s.state == "connected" {
                    // Address is now in learned_addrs — signal main loop to rebuild.
                    let _ = rebuild_tx_c.try_send(());
                }
                if !s.remote_labels.is_empty() {
                    let mut all_chans = incoming_c.write().unwrap_or_else(|e| e.into_inner());
                    let chans = all_chans.entry(name.clone()).or_insert_with(Vec::new);
                    // A LABEL batch is the remote's COMPLETE authoritative set.
                    // A peer sends one entry per CONFIGURED channel, so a slot can be
                    // removed by simply not appearing in the next batch, not only by an
                    // explicit empty label. Track which slots the batch mentions and clear
                    // every stored name it doesn't.
                    // A peer sends one entry per CONFIGURED channel, up to the 128-slot
                    // maximum. A slot can be
                    // removed by simply not appearing in the next batch, not only by
                    // an explicit empty label. Track which of the 128 slots the batch
                    // mentions and clear every stored name it doesn't.
                    const LABEL_SLOTS: usize = 128;
                    let mut mentioned = [false; LABEL_SLOTS];
                    for label in &s.remote_labels {
                        let idx = label.channel as usize;
                        if idx < LABEL_SLOTS { mentioned[idx] = true; }
                        if chans.len() <= idx {
                            chans.resize(idx + 1, ChannelInfo::default());
                        }
                        chans[idx].channel = idx as u8;
                        // Replace unconditionally: empty label = channel no longer
                        // present on the remote (explicit form of removal).
                        chans[idx].label = label.label.clone();
                    }
                    for (idx, ci) in chans.iter_mut().enumerate() {
                        if idx < LABEL_SLOTS && !mentioned[idx] && !ci.label.is_empty() {
                            ci.label.clear();   // implicit removal: absent from batch
                        }
                    }
                    // Notify the main loop to push a WS event so the UI reflects
                    // new labels immediately rather than waiting for pollSlow (5s).
                    let _ = label_tx_c.try_send(name.clone());
                }
                stats_map_c.write().unwrap_or_else(|e| e.into_inner()).insert(name, s);
            }
        });
    }

    // Phase sync command channel: API handler sends (peer, enabled) here.
    let (phase_tx, mut phase_rx) = tokio::sync::mpsc::channel::<(String, bool)>(16);

    // Phase lock is applied per-peer on first packet arrival (groups don't exist yet).
    for r in cfg.remotes.iter().filter(|r| r.phase_lock) {
        debug!("Phase lock restored: '{}'", r.name);
    }

    // Audio capture engine: the device callback hands each frame to the scheduler's
    // encode pool, which encodes and calls send_to on the shared socket cell directly.
    let mut outgoing_labels: Vec<ChannelLabel> = Vec::new();

    // Capture build (device lookup + CaptureEngine::start + per-remote send routing + the
    // outgoing-channel labels/info) factored into BuildCtx::build_input. Returns the labels
    // + OutgoingInfo rows, applied to the shared stores here.
    let mut _capture_engine: Option<CaptureEngine> = {
        let (eng, labels, outgoing) = build_ctx.build_input(&cfg);
        if !outgoing.is_empty() {
            let mut out = outgoing_channels.write().unwrap_or_else(|e| e.into_inner());
            out.extend(outgoing);
        }
        outgoing_labels.extend(labels);
        eng
    };
    // Share the peer-status atomics with the capture engine so the send path's §3.1
    // per-destination gate reads the same live values the stats pump writes.
    if let Some(ref mut e) = _capture_engine {
        e.adopt_peer_status_map(peer_status_map.clone());
        e.adopt_link_map(link_map.clone());
    }


    // Peer tasks
    let mut peer_cmds:    HashMap<String, mpsc::Sender<PeerCommand>> = HashMap::new();
    // These peer-lookup maps are written ONLY here in the select loop (peer setup, hot
    // add/remove) and read by the inline audio path on cascade-recv — so they are
    // Arc<RwLock<>> (single-writer / multi-reader). The select loop still reads them for
    // control packets. peer_tx_atomics is NOT shared (TX side only) → stays a plain map.
    let peer_by_addr: Arc<RwLock<HashMap<SocketAddr, String>>>                = Arc::new(RwLock::new(HashMap::new()));
    let peer_by_tok:  Arc<RwLock<HashMap<[u8; 16], String>>>                  = Arc::new(RwLock::new(HashMap::new()));
    let peer_rx_atomics: Arc<RwLock<HashMap<String, Arc<std::sync::atomic::AtomicU64>>>> = Arc::new(RwLock::new(HashMap::new()));
    let mut peer_tx_atomics: HashMap<String, Arc<std::sync::atomic::AtomicU64>> = HashMap::new();
    // Per-peer incoming-sample-rate flag: the audio receive path sets it from byte 9 of
    // each audio packet (true = sender advertised 44.1k). The peer task reads it per tick.
    let peer_sr_atomics: Arc<RwLock<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>> = Arc::new(RwLock::new(HashMap::new()));
    // Bundle the 6 shared per-peer maps for matched insert/remove (see PeerRegistry). Holds
    // clones of the SAME Arcs, so registry.remove() writes through to what the recv path sees.
    let registry = PeerRegistry {
        stats:   peer_stats_map.clone(),
        by_addr: peer_by_addr.clone(),
        by_tok:  peer_by_tok.clone(),
        learned: learned_addrs.clone(),
        rx_atom: peer_rx_atomics.clone(),
        sr_atom: peer_sr_atomics.clone(),
    };
    // The live input channel count — 0 while no input is running. Shared with the API
    // (TX grid) and read at every label reload (`reload_labels`).
    let in_channels_h: api::LateHandle<std::sync::atomic::AtomicUsize> =
        _capture_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.live_in_ch))).unwrap_or_else(api::LateHandle::empty);

    // Global label revision — one value for the whole app, shared with all peer tasks.
    // Starts at 0 and is never reset on disconnect. See `reload_labels`.
    let label_change_indicator: Arc<std::sync::atomic::AtomicU8> =
        Arc::new(std::sync::atomic::AtomicU8::new(0));

    // Two launch-time label reloads — configuration load, then audio start — so the first
    // poke carries 2. Both land BEFORE any peer task spawns: a peer's
    // first poke goes out microseconds after it starts, so bumping after the spawn loop
    // could put 0 or 1 on the wire.
    for _ in 0..2 {
        reload_labels(&label_change_indicator, &in_channels_h);
    }

    // Declared here (outside the block below) so the accounting plane handle is in scope at
    // the select loop's SetOutputDevice handler, which reuses it to attach the decode plane
    // on a late output enable. Assigned inside the block when the recv context is wired.
    let mut recv_acct: Option<Arc<net::udp::RecvAccounting>> = None;

    // Peers and the receive context need an audio socket to exist — ANY socket, not the
    // configured one. A busy port at startup must not skip this: the daemon runs on the
    // placeholder until the probe takes the configured port, and everything below holds the
    // socket cell, so peers start reaching the far side the moment it moves. Gating this on
    // the port conflict left a daemon that took the port and then never connected, and never
    // played received audio even after its remotes were toggled back on. Only a daemon with
    // no socket at all — the placeholder bind failed too — skips it.
    if engine_built {
        for (remote_name, remote_addr) in &all_remotes {
        // Per-remote password (Phase 1). Empty string = no password.
        let remote_password = cfg.remotes.iter()
            .find(|r| &r.name == remote_name)
            .map(|r| r.password.clone())
            .unwrap_or_default();

        // Per-remote password (CASCADE_WIRE_PROTOCOL_SPEC §4):
        // Token = MD5(UPPER(remote_name) + UPPER(password)) where both sides
        // use the same password for that connection.
        // our_token = MD5(our_name + this_remote's_password)
        // remote_token = MD5(remote_name + this_remote's_password)
        let our_token   = derive_token(&cfg.general.name, &remote_password);
        let rx_bytes_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let tx_bytes_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sr_44k_atomic = Arc::new(std::sync::atomic::AtomicBool::new(false));
        peer_rx_atomics.write().unwrap_or_else(|e| e.into_inner()).insert(remote_name.clone(), Arc::clone(&rx_bytes_atomic));
        peer_tx_atomics.insert(remote_name.clone(), tx_bytes_atomic.clone());
        peer_sr_atomics.write().unwrap_or_else(|e| e.into_inner()).insert(remote_name.clone(), Arc::clone(&sr_44k_atomic));

        // Get the full RemoteConfig for this remote (used for routing and other fields).
        let remote_cfg = cfg.remotes.iter().find(|r| &r.name == remote_name);


        let pcfg = PeerConfig {
            name:          remote_name.clone(),
            remote_name:   remote_name.clone(),
            host:          remote_cfg.map(|r| r.host.clone()).unwrap_or_default(),
            port:          remote_cfg.map(|r| r.port).unwrap_or(20102),
            remote_addr:   *remote_addr,
            our_token,
            password:      remote_password.clone(),
            learned_addrs: learned_addrs.clone(),
            rebuild_tx: Some(rebuild_tx.clone()),
            rx_bytes_atomic,
            tx_bytes_atomic: tx_bytes_atomic.clone(),
            sr_44k_atomic: Arc::clone(&sr_44k_atomic),
            stat_acc: audio_engine.as_ref().map(|e| e.stat_acc_handle()),
            label_change_indicator: Arc::clone(&label_change_indicator),
            event_tx: event_tx_early.clone(),
            crypto: crypto_map.write().unwrap_or_else(|e| e.into_inner())
                .entry(remote_name.clone())
                .or_insert_with(|| Arc::new(net::crypto::PeerCrypto::new(false)))
                .clone(),
            link: net::link_for(&link_map, &remote_name),
        };
        let (cmd_tx, cmd_rx) = mpsc::channel::<PeerCommand>(64);
        let out   = outbound_tx_opt.clone().unwrap();
        let stats = stats_tx.clone();
        tokio::spawn(async move {
            let (sr_tx, mut sr_rx) = mpsc::channel::<SendRequest>(256);
            tokio::spawn(async move {
                while let Some(sr) = sr_rx.recv().await {
                    let _ = out.send(OutboundPacket { to: sr.to, data: sr.data }).await;
                }
            });
            PeerTask::new(pcfg).run(cmd_rx, sr_tx, stats).await;
        });

        let tok = derive_token(remote_name, &remote_password);
        if let Some(addr) = remote_addr {
            info!("Remote '{}' → {} (active)", remote_name, addr);
            peer_by_addr.write().unwrap_or_else(|e| e.into_inner()).insert(*addr, remote_name.clone());
        } else {
            info!("Remote '{}' (passive — waiting for incoming poke)", remote_name);
        }
        peer_by_tok.write().unwrap_or_else(|e| e.into_inner()).insert(tok.0, remote_name.clone());
        peer_cmds.insert(remote_name.clone(), cmd_tx);
    }

    // ── Wire the inline-audio context for cascade-recv ──────────────────
    // Now that the audio engine and the four peer-lookup maps exist, populate the
    // RecvContext so cascade-recv processes AUDIO packets inline (no blocking_send
    // → no tokio-worker deschedule).
    // Build the receive ACCOUNTING plane unconditionally (independent of any output engine):
    // identification + RX byte/sr accounting + incoming-channel registration run whenever the
    // daemon is up, so an enabled remote shows its incoming rate + channels even with no output
    // device (e.g. wrong name/password → idle UI but data arriving = visible misconfig). The
    // DECODE plane (engine) is attached only when an output engine exists; a late-enabled output
    // device republishes the RecvContext with decode=Some. The acct Arc is kept in a
    // main() local (recv_acct) so the late-output handler can reuse it without rebuilding.
    recv_acct = recv_ctx_opt.as_ref().map(|ctx_cell| {
        let phase_lock_peers: std::collections::HashSet<String> =
            cfg.remotes.iter().filter(|r| r.phase_lock).map(|r| r.name.clone()).collect();
        let acct = Arc::new(net::udp::RecvAccounting {
            by_addr:           Arc::clone(&peer_by_addr),
            rx_atomics:        Arc::clone(&peer_rx_atomics),
            sr_atomics:        Arc::clone(&peer_sr_atomics),
            phase_lock_peers:  Arc::new(RwLock::new(phase_lock_peers)),
            incoming_channels: Arc::clone(&incoming_channels),
            incoming_seen:     Arc::clone(&incoming_seen),
            crypto:            crypto_map.clone(),
            links:             link_map.clone(),
            meters:            Arc::clone(&meters),
        });
        let decode = audio_engine.as_ref().map(|e| Arc::new(net::udp::RecvDecode {
            engine: Arc::clone(e),
        }));
        let ctx = net::udp::RecvContext { acct: Arc::clone(&acct), decode };
        *ctx_cell.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(ctx));
        acct
    });


    // labels travel with the audio to the destination slot; unrouted = empty).
    // The indicator was already advanced to 2 before the peer tasks spawned, so
    // no increment here — just fan out the initial labels.
    {
        let cfg_snap = cfg_arc.read().unwrap_or_else(|e| e.into_inner()).clone();
        for (name, cmd_tx) in peer_cmds.iter() {
            let labels = routed_labels_for_peer(&cfg_snap, name);
            let _ = cmd_tx.send(PeerCommand::LabelsChanged(labels)).await;
        }
    }
    // Register per-peer TX atomics with CaptureEngine now that both are populated.
    if let Some(ref eng) = _capture_engine {
        for (name, atom) in &peer_tx_atomics {
            eng.register_tx_atomic(name, Arc::clone(atom));
        }
    }
    info!("Sent routed channel labels to {} peer(s)", peer_cmds.len());
    } // end if engine_built

    // Web API
    let (hot_tx, mut hot_rx) = tokio::sync::mpsc::channel::<api::HotCommand>(32);
    let peer_stats_tx = stats_tx.clone(); // original mpsc sender, used by hot-add PeerTask spawn

    // Reuse the broadcast sender created before the peer loop so peer tasks share the same
    // channel as AppState (not a disconnected second sender).
    let event_tx = event_tx_early;
    let on_air_arc = audio_engine.as_ref()
        .map(|e| Arc::clone(&e.on_air))
        .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));

    // Debounced, non-blocking config persistence. cfg_arc is the source of truth;
    // this writes it to disk lazily so rapid edits coalesce and callers never block.
    let saver = crate::config_saver::ConfigSaver::new(
        cfg_arc.clone(), config_path_for_save.clone());
    let saver_main = saver.clone();  // for the hot-command loop's persistence

    // Per-peer receive-buffer health, refreshed by the main loop's stats tick and published
    // as a plain map, so API handlers read a snapshot instead of reaching into the engine.
    // Diagnostic surface, off unless asked for: an env var rather than a config key, so it
    // leaves no trace in cascade.toml and cannot be left on by accident across a restart.
    let debug_api = std::env::var("CASCADE_DEBUG_API").map(|v| v == "1").unwrap_or(false);
    let depth_view: Arc<std::sync::RwLock<std::collections::HashMap<String, Vec<(u8, usize)>>>> =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let buffer_view: Arc<std::sync::RwLock<std::collections::HashMap<String, (f32, f32, f32, f32, bool)>>> =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    // Per-peer live INCOMING frame size (samples), refreshed by the stats tick. The API
    // surfaces this so the web UI can floor the receive-buffer list at 2× the incoming
    // frame. Absent until a peer's first packet is decoded.
    let frame_view: Arc<std::sync::RwLock<std::collections::HashMap<String, usize>>> =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    // Live routed-and-decoding RX channel count, polled from the output engine by the stats
    // tick (engine is !Send). Shared Arc (created once, survives engine rebuilds); 0 when no
    // output engine present.
    let active_recv: Arc<std::sync::atomic::AtomicUsize> =
        Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Live connected-peer outgoing stream count, polled from the capture engine by the stats
    // tick. Shared Arc; 0 when no capture engine or no connected peers.
    let active_send: Arc<std::sync::atomic::AtomicUsize> =
        Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Late-publishable handles to engine-owned shared state (meters/counts/tone). Created
    // here so they survive engine replacement: at boot we seed them from whichever engines
    // exist; a direction enabled AFTER boot publishes its engine's handles into
    // these SAME cells, and every AppState clone (the API handlers) sees them on the next
    // read. Kept in main() scope so the SetInput/OutputDevice build-from-None path can
    // publish into them. Input-side handles come from _capture_engine, output-side from
    // audio_engine.
    let input_peaks_h =
        _capture_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.input_peaks))).unwrap_or_else(api::LateHandle::empty);
    let tone_peaks_h =
        _capture_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.tone_peaks))).unwrap_or_else(api::LateHandle::empty);
    let output_peaks_h =
        audio_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.output_peaks))).unwrap_or_else(api::LateHandle::empty);
    let incoming_peaks_h =
        audio_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.incoming_peaks))).unwrap_or_else(api::LateHandle::empty);
    let tone_dests_h =
        _capture_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.tone_dests))).unwrap_or_else(api::LateHandle::empty);
    let active_streams_h =
        _capture_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.active_streams))).unwrap_or_else(api::LateHandle::empty);
    // The 6 device/count/rate handles — same late-publish treatment (6 fix): a late-built
    // engine publishes its real Arcs here so the UI shows channels/rate/device-name instead of
    // 0/"(unavailable)"/"— none —". Engine field names differ from the AppState names
    // (live_out_ch→out_channels etc.). Kept in main() scope for the build-from-None handlers.
    let out_channels_h: api::LateHandle<std::sync::atomic::AtomicUsize> =
        audio_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.live_out_ch))).unwrap_or_else(api::LateHandle::empty);
    let out_sample_rate_h: api::LateHandle<std::sync::atomic::AtomicU32> =
        audio_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.live_out_rate))).unwrap_or_else(api::LateHandle::empty);
    let in_sample_rate_h: api::LateHandle<std::sync::atomic::AtomicU32> =
        _capture_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.live_in_rate))).unwrap_or_else(api::LateHandle::empty);
    let live_out_device_h: api::LateHandle<std::sync::Mutex<String>> =
        audio_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.live_out_device))).unwrap_or_else(api::LateHandle::empty);
    let live_in_device_h: api::LateHandle<std::sync::Mutex<String>> =
        _capture_engine.as_ref().map(|e| api::LateHandle::with(Arc::clone(&e.live_in_device))).unwrap_or_else(api::LateHandle::empty);

    let api_state = AppState {
        config:            cfg_arc.clone(),
        config_path:       config_path.clone(),
        peer_stats:        peer_stats_map.clone(),
        incoming_channels: incoming_channels.clone(),
        outgoing_channels: outgoing_channels.clone(),
        phase_tx:          Some(phase_tx),
        input_peaks:       input_peaks_h.clone(),
        tone_peaks:        tone_peaks_h.clone(),
        output_peaks:      output_peaks_h.clone(),
        incoming_peaks:    incoming_peaks_h.clone(),
        meters:            Arc::clone(&meters),
        on_air:            on_air_arc,
        tone_dests:        tone_dests_h.clone(),
        active_streams:    active_streams_h.clone(),
        buffer_view:       buffer_view.clone(),
        depth_view:        depth_view.clone(),
        frame_view:        frame_view.clone(),
        active_recv:       active_recv.clone(),
        active_send:       active_send.clone(),
        out_channels:      out_channels_h.clone(),
        in_channels:       in_channels_h.clone(),
        out_sample_rate:   out_sample_rate_h.clone(),
        in_sample_rate:    in_sample_rate_h.clone(),
        live_out_device:   live_out_device_h.clone(),
        live_in_device:    live_in_device_h.clone(),
        startup_warnings:  startup_warnings.clone(),
        hot_tx:            Some(hot_tx),
        saver:             Some(saver),
        event_tx,
        port_conflict:     port_conflict_arc.clone(),
        api_rebind:        Arc::new(tokio::sync::Notify::new()),
    };

    api::spawn_background_tasks(api_state.clone());
    api::spawn_meter_publisher(api_state.clone());
    let api_state_hot = api_state.clone(); // kept for hot-command handler after spawn moves api_state

    let api_port = cfg.api.port;
    tokio::spawn(async move {
        // Rebindable: an [api] bind/port change moves the listener in place.
        if let Err(e) = api::serve_rebindable(api_state).await {
            warn!("Web API: {}", e);
        }
    });

    // Where the audio socket actually is. During a startup port conflict it sits on a
    // loopback placeholder until the probe takes the configured port, and the probe logs
    // that moment itself.
    let bound = audio_socket_opt.as_ref().and_then(|c| c.load().local_addr().ok());
    match bound {
        Some(_) if port_conflict_arc.load(std::sync::atomic::Ordering::Relaxed) =>
            info!("UDP port {} is busy — waiting for it to free  Web UI: http://localhost:{}",
                  cfg.general.port, api_port),
        Some(addr) => info!("Listening on {}  Web UI: http://localhost:{}", addr, api_port),
        None => info!("No audio socket  Web UI: http://localhost:{}", api_port),
    }

    // Desktop integration, once the port it needs to open is known. Windows starts a hidden
    // top-level window on its own thread: it carries the tray icon, and — the part that is
    // not cosmetic — it is the only way this process hears a log off, restart or shutdown,
    // since a console-less binary receives no console control events and Windows has no
    // SIGTERM. macOS is driven from main() instead, because AppKit demands the main thread.
    #[cfg(windows)]
    crate::platform::windows::start(api_port);
    // macOS: the item is already in the menu bar; this is the port its Open config opens.
    #[cfg(target_os = "macos")]
    crate::platform::macos::set_api_port(api_port);

    // (audio_count removed with the inline audio fast-path move, .)

    // ── Parked-device re-discovery (detached) ────────────────────────────────
    // Device enumeration (CoreAudio) is expensive: ~30ms output, ~75ms input on this
    // hardware. It MUST NOT run inside the main select! task: that task is !Send (it
    // owns device streams), so it is pinned to a single worker, and awaiting anything
    // inside an arm suspends the WHOLE task — every other arm (POKE forwarding, packet
    // receive) stalls until the await completes. That is what inflated POKE latency to
    // 45-77ms in lock-step with the enumeration.
    //
    // The fix: a fully independent task owns enumeration + the device-state machine. It
    // holds only Send handles (live-name Arc<Mutex<String>>, the lost-flag AtomicBool,
    // config) — NEVER an engine. It sends cheap DeviceActions (carrying an already-resolved
    // Device, or a stop) to the main loop, which applies them.
    //
    // Both INPUT and OUTPUT are driven by independent device_manager tasks: each runs the
    // device_manager::step state machine (immediate stop on loss, parked re-acquire),
    // enumerates off-thread, and sends cheap DeviceActions (StopInput/StopOutput/Rebuild*,
    // carrying an already-resolved Device) to the main loop, which applies them.
    use crate::audio::device_manager as dm;
    let (dev_action_tx, mut dev_action_rx) =
        tokio::sync::mpsc::channel::<dm::DeviceAction>(8);

    // Device-change generation counter, bumped by the OS device-change watcher (Stage 4).
    // The manager tasks watch it to re-acquire a parked device immediately on an OS change
    // instead of waiting for the slow poll; the inventory task is nudged the same way.
    let device_gen = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Start the native device-change watcher. On a change it bumps device_gen (wakes the
    // manager re-acquire) and nudges the inventory enumerator (invalidate_device_cache →
    // re-enumerate + push to the UI). If no native watcher exists on this platform the
    // periodic polls remain the mechanism. The returned guard must stay alive for the
    // listener to remain registered, so it is held for the process lifetime.
    let _device_watcher = {
        let gen = Arc::clone(&device_gen);
        let w = crate::audio::device_watcher::DeviceWatcher::start(move || {
            let g = gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            // DEBUG, not info: this fires once per endpoint per state, property and
            // default-device change, so a multi-endpoint device appearing (a Dante Virtual
            // Soundcard registers a pair per route) produces a burst of a dozen or more for
            // one plug event. The transitions worth reading are the OUTCOMES, which already
            // log at info — "Input device → X", "Output 'Y': rebuilt", device lost/returned.
            tracing::debug!("device watcher: OS device set changed (gen {}) — re-checking", g);
            api::invalidate_device_cache();
        });
        if let Some(ref w) = w {
            info!("Device-change watcher active (push: {})", w.backend());
        } else {
            info!("No native device-change watcher on this platform — using periodic poll");
        }
        w
    };

    {
        // OUTPUT manager task — see device_manager::run_manager_task.
        let live_h = audio_engine.as_ref().map(|e| Arc::clone(&e.live_out_device));
        let lost_h = audio_engine.as_ref().map(|e| Arc::clone(&e.output_device_lost));
        let cfg_h = cfg_arc.clone();
        let act_tx = dev_action_tx.clone();
        let gen_h = Arc::clone(&device_gen);
        let watcher_on = _device_watcher.is_some();
        if let (Some(live_h), Some(lost_h)) = (live_h, lost_h) {
            tokio::spawn(dm::run_manager_task(dm::Dir::Output, live_h, lost_h, cfg_h, act_tx, gen_h, watcher_on));
        }
    }
    {
        // INPUT manager task — identical driver, input direction.
        let live_h = _capture_engine.as_ref().map(|e| Arc::clone(&e.live_in_device));
        let lost_h = _capture_engine.as_ref().map(|e| Arc::clone(&e.input_device_lost));
        let cfg_h = cfg_arc.clone();
        let act_tx = dev_action_tx.clone();
        let gen_h = Arc::clone(&device_gen);
        let watcher_on = _device_watcher.is_some();
        if let (Some(live_h), Some(lost_h)) = (live_h, lost_h) {
            tokio::spawn(dm::run_manager_task(dm::Dir::Input, live_h, lost_h, cfg_h, act_tx, gen_h, watcher_on));
        }
    }

    // Dedicated 2s stats ticker — fires even when no audio is arriving (TX-only scenario).
    let mut stats_tick = tokio::time::interval(std::time::Duration::from_secs(2));
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // tx_bytes_accum is drained from Arc<AtomicU64> set by the encode thread
    // Decode slots are keyed by (peer_name, slot): the wire channel IS the engine slot,
    // namespaced by peer, so two remotes both sending ch0 never collide and there is no
    // per-peer channel offset.

    // ── Single per-peer teardown path ─────────────────────────────────────────────
    // Both RemoveRemote and DisableRemote tear a peer down identically (they differ ONLY
    // in whether the config entry is deleted afterwards). Routing both through this one
    // macro keeps the two from drifting out of sync, where a map is cleaned up in one path
    // but leaked in the other (a stale render group, leaked atomics). Covers EVERY per-peer map + both engines:
    //   - the PeerTask command channel (Shutdown sent, then dropped)
    //   - the 6 shared lookup maps (via registry.remove)
    //   - peer_tx_atomics (the one plain TX-side map, not in the registry)
    //   - link_map (the remote's learned link indices)
    //   - AudioEngine per-peer state (routing/buffer/frames/group/slots/snaps)
    //   - CaptureEngine per-peer state (send_routing/tx_atomics/tone_dests)
    // Async because the Shutdown send awaits; expands inline so it can borrow the loop locals.
    macro_rules! teardown_peer {
        ($name:expr) => {{
            let __name: &str = $name;
            if let Some(tx) = peer_cmds.remove(__name) {
                let _ = tx.send(net::peer::PeerCommand::Shutdown).await;
            }
            registry.remove(__name);
            peer_tx_atomics.remove(__name);
            // A remote added again later starts from the initial link indices.
            link_map.write().unwrap_or_else(|e| e.into_inner()).remove(__name);
            if let Some(ref eng) = _capture_engine { eng.remove_peer(__name); }
            if let Some(ref e)   = audio_engine    { e.remove_peer(__name); }
        }};
    }

    // ── Late device-manager spawn ───────────────────────────────────────
    // When a direction is enabled after boot (engine built from None in the SetInput/
    // OutputDevice handler), it has no manager task yet — the boot spawn was gated on the
    // engine existing. This spawns one for that direction so the newly-enabled direction is
    // first-class: it heals on device loss/return exactly like a boot-present direction.
    // Mirrors the boot spawn blocks (Dir::Output/Input) using the same shared inputs, which
    // are all still in scope (cloned, never moved).
    macro_rules! spawn_manager {
        ($dir:expr, $live_h:expr, $lost_h:expr) => {{
            tokio::spawn(dm::run_manager_task(
                $dir, $live_h, $lost_h,
                cfg_arc.clone(),
                dev_action_tx.clone(),
                Arc::clone(&device_gen),
                _device_watcher.is_some(),
            ));
        }};
    }

    // Last channels_changed signature emitted per peer, so the label-notify handler can
    // dedupe redundant events (see the label_notify_rx arm below).
    let mut last_ch_sig: HashMap<String, String> = HashMap::new();

    // Reconcile the SINGLE shared callback period across the capture (input) and render
    // (output) units (CASCADE_AUDIO_SEND_SPEC §3 / CASCADE_AUDIO_RECEIVE_SPEC §5.2, §13):
    //   incoming_hint = §13.2's exact-match table over enabled remotes' receive buffers
    //                   → engine.min_output_period()
    //   outgoing_hint = the smallest frame size any remote is sent at, capped at 480
    //                   → capture.min_resolved_send_frame()
    //   shared        = min(incoming_hint, outgoing_hint), never shorter than the callback
    //                   both assigned devices can run → audio::shortest_request()
    // Both units run at `shared`. A unit is rebuilt only when its own period has to move, it
    // is running, and its device honours requested periods. Units are rebuilt, never
    // reconfigured in place (§5.2): stop, settle, drop inline, build, then start input before
    // output. Returns true if it actually rebuilt a unit this call — so callers (the stats
    // tick) can tell an EXPECTED ~100ms callback resize from a genuine select-loop stall.
    // The macro's edge-detection cells live out here, one of each for every expansion: a
    // `static` declared inside a macro body is instantiated separately at each expansion
    // site, so every call site would keep its own copy and report one change once per site.
    static LAST_SHARED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static LAST_AGREED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static PERIOD_OVER_SETPOINT: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    macro_rules! reconcile_callback_period {
        () => {{
            // §13.2: the RECEIVE half takes its minimum over channels that are enabled, and
            // that gate alone — see `min_output_period` for why adding an "is it decoding
            // yet" test rebuilds CoreAudio at connect time and costs buffer depth §5.2 has
            // no way to recover.
            //
            // §13.3: the SEND half additionally skips remotes with no active route, which
            // is the one direction where an activity gate belongs: an unrouted destination
            // is not carrying audio and has no frame size to contribute.
            //
            // The `enabled` gate applies to the RECEIVE half here; the send half inherits
            // it from `send_routing`, which holds enabled remotes only — boot and AddRemote
            // load it for enabled remotes, and `DisableRemote` clears it via `remove_peer`.
            //
            // The config lock is taken and released within this one statement: the rebuild
            // below sleeps and opens devices, and a settings save must not wait behind it.
            let enabled: std::collections::HashSet<String> =
                cfg_arc.read().unwrap_or_else(|e| e.into_inner())
                    .remotes.iter().filter(|r| r.enabled).map(|r| r.name.clone()).collect();
            // The shortest callback period both assigned devices can run
            // (`audio::agreed_period`; always 0 on macOS). When it moves — a device assigned,
            // stopped or lost — outgoing frames below the new shortest frame size are
            // re-resolved and receive setpoints re-floored before the period is worked out.
            let agreed = crate::audio::agreed_period();
            if LAST_AGREED.swap(agreed, std::sync::atomic::Ordering::Relaxed) != agreed {
                if agreed > 0 {
                    info!("devices' shared shortest callback period {} frames ({:.1} ms): \
                           outgoing frames {} ms or longer, receive buffer {} ms or more",
                          agreed, agreed as f32 / 48.0,
                          crate::audio::shortest_frame() as f32 / 48.0,
                          crate::audio::channel_sync::period_floor_samples(agreed) / 48);
                } else {
                    info!("no device callback limit: every outgoing frame size and receive \
                           buffer available");
                }
                if let Some(e) = _capture_engine.as_ref() { e.apply_frame_floor(); }
                #[cfg(not(target_os = "macos"))]
                if let Some(e) = audio_engine.as_ref() { e.apply_period_floor(); }
            }
            let incoming_hint = audio_engine.as_ref()
                .map(|e| e.min_output_period(&enabled))
                .unwrap_or(crate::audio::encode::IO_BUF_CAP_FRAMES);
            // The send half is driven by the smallest frame size any remote that routes a
            // channel is sent at — see `min_resolved_send_frame`.
            //
            // Nothing routed leaves the send half at its DEFAULT (§13.3): the table starts
            // at 480 and is only ever overwritten by a remote that is actually sent to.
            // Falling back to an instance-wide default instead let an idle send side impose
            // its setting on the shared period — clearing every route dropped the period to
            // that value and rebuilt both audio units, and restoring the routes rebuilt them
            // again, so each routing change cost ~150 ms of interrupted audio for a direction
            // that was sending nothing.
            let outgoing_hint = _capture_engine.as_ref()
                .and_then(|e| e.min_resolved_send_frame())
                .map(|f| f.min(crate::audio::encode::IO_BUF_CAP_FRAMES))
                .unwrap_or(crate::audio::encode::IO_BUF_CAP_FRAMES);
            // Never shorter than what both assigned devices can run (`audio::shortest_request`):
            // a shorter request would be granted the same longer period, so asking for it
            // would only rebuild units to change nothing.
            let floor = crate::audio::shortest_request();
            let shared = incoming_hint.min(outgoing_hint).max(floor);
            // The callback period is load-bearing: if it ever equals the receive setpoint,
            // one render cycle consumes the whole buffer and the ring empties every cycle.
            // That failure was silent for a long time, so the value is logged whenever it
            // moves — cheap, and it makes "period == setpoint" visible at a glance.
            let prev_shared = LAST_SHARED.swap(shared, std::sync::atomic::Ordering::Relaxed);
            {
                let prev = prev_shared;
                if prev != shared {
                    // Which half decided it. On a tie neither is "smaller", and saying one
                    // of them is invites a hunt for a difference that is not there.
                    let why = if incoming_hint.min(outgoing_hint) < floor {
                        format!("the devices' shortest shared callback (receive {incoming_hint}, \
                                 send {outgoing_hint})")
                    } else if incoming_hint == outgoing_hint {
                        format!("receive and send agree ({incoming_hint})")
                    } else if incoming_hint < outgoing_hint {
                        format!("receive is smaller (receive {incoming_hint}, send {outgoing_hint})")
                    } else {
                        format!("send is smaller (receive {incoming_hint}, send {outgoing_hint})")
                    };
                    // `prev` is 0 before the first reconcile — a starting value, not a period
                    // anything ran at, so the first line states the period rather than a move.
                    if prev == 0 {
                        info!("callback period {} samples ({:.1} ms) — {}",
                              shared, shared as f32 / 48.0, why);
                    } else {
                        info!("callback period {} → {} samples ({:.1} → {:.1} ms) — {}",
                              prev, shared, prev as f32 / 48.0, shared as f32 / 48.0, why);
                    }
                }
            }
            // Which units need to move to `shared`.
            // A DEVICE THAT SUBSTITUTES ITS OWN PERIOD CANNOT ACT ON A NEW REQUEST, so
            // rebuilding for one costs an audio interruption and changes nothing.
            //
            // Dante Virtual Soundcard's WDM endpoints, for one, grant 512 frames whatever is
            // asked: rebuilding them on every frame-size change interrupts audio for a
            // quarter of a second to arrive at exactly the period already running.
            //
            // A device that merely ROUNDS (240 -> 224) is not one of these: it honours requests
            // and must be allowed to move. `PeriodTracker` tells them apart by whether two
            // different requests got the same answer.
            let out_substitutes = audio_engine.as_ref().map_or(false, |e| e.out_period.is_fixed());
            let in_substitutes = _capture_engine.as_ref().map_or(false, |e| e.in_period.is_fixed());

            let out_needs = audio_engine.is_some()
                && !out_substitutes
                && output_ctrl.as_ref().map_or(false, |oc| oc.current_frames() != shared);
            let in_needs = !in_substitutes
                && _capture_engine.as_ref().map_or(false, |e| e.current_input_frames() != shared);
            if out_substitutes || in_substitutes {
                debug!("callback period {} not applied to the {} device: it substitutes its \
                        own period, so a rebuild would interrupt audio and change nothing",
                       shared,
                       match (out_substitutes, in_substitutes) {
                           (true, true) => "input and output",
                           (true, false) => "output",
                           _ => "input",
                       });
            }
            // A unit that is not running is not resurrected as a side effect of a period
            // change — neither an input nor an output the user set to "— none —", nor one
            // parked after its device was lost (the device manager brings that back).
            let in_live = _capture_engine.as_ref()
                .map_or(false, |e| !e.live_in_device.lock()
                    .unwrap_or_else(|p| p.into_inner()).is_empty());
            let out_live = audio_engine.as_ref()
                .map_or(false, |e| !e.live_out_device.lock()
                    .unwrap_or_else(|p| p.into_inner()).is_empty());
            // A callback-period change while audio runs is an audio restart, which is a
            // label reload. A first reconcile (`prev_shared` 0) is the
            // launch, already counted by the startup bumps.
            if prev_shared != 0 && prev_shared != shared && (in_live || out_live) {
                reload_labels(&label_change_indicator, &in_channels_h);
            }
            // EACH UNIT IS REBUILT ONLY WHEN ITS OWN PERIOD HAS TO MOVE.
            //
            // §5.2 reconfigures the two units together, and for devices that honour requested
            // periods that is still what happens: both compare against the same `shared`, so
            // both need to move or neither does. They part company only when one device
            // substitutes its own period — and rebuilding that one changes nothing, while
            // rebuilding it anyway on the other's account was an audio interruption for no
            // purpose.
            let rebuild_out = out_needs && out_live;
            let rebuild_in  = in_needs && in_live;
            let needs = rebuild_out || rebuild_in;

            // ── Phase 1: STOP BOTH, SETTLE, DISPOSE BOTH ──
            // §5.2's `stop_audio`: listeners off, `AudioOutputUnitStop` on input then
            // output, a 20 ms settle with both stopped but still allocated, then
            // uninitialise and dispose both. No device or rate changes on this path, so no
            // `DEVICE_RATE_SETTLE`.
            //
            // BLOCKING sleep, deliberately — never an `.await`: suspending the select loop
            // mid-rebuild would let other arms run against half-torn-down units. No lock is
            // held across it, and the audio callback threads are unaffected by this thread
            // parking.
            let stopped_in = if rebuild_in {
                _capture_engine.as_mut().and_then(|e| e.pause_input_for_rebuild())
            } else { None };
            let stopped_out = if rebuild_out {
                if let (Some(e), Some(oc)) = (audio_engine.as_ref(), output_ctrl.as_mut()) {
                    e.pause_output_for_rebuild(oc)
                } else { None }
            } else { None };
            if needs {
                std::thread::sleep(crate::audio::UNIT_STOP_SETTLE);
            }
            drop(stopped_in);
            drop(stopped_out);
            if needs {
                crate::audio::backend::set_power_hint();
            }

            // ── Phase 1b: BUILD (configured + stopped; neither running) ──
            if rebuild_out {
                if let (Some(e), Some(oc)) = (audio_engine.as_ref(), output_ctrl.as_mut()) {
                    if let Err(err) = e.rebuild_output_prepare(oc, None, Some(shared)) {
                        warn!("output prepare (shared {}): {}", shared, err);
                    }
                }
            }
            if rebuild_in {
                if let Some(e) = _capture_engine.as_mut() {
                    if let Err(err) = e.rebuild_input_prepare(None, Some(shared)) {
                        warn!("input prepare (shared {}): {}", shared, err);
                    }
                }
            }
            // ── Phase 2: START — input first, THEN output. The order is fixed.
            if rebuild_in {
                if let Some(e) = _capture_engine.as_ref() {
                    if let Err(err) = e.rebuild_input_start() { warn!("input start (shared {}): {}", shared, err); }
                }
            }
            if rebuild_out {
                if let (Some(e), Some(oc)) = (audio_engine.as_ref(), output_ctrl.as_mut()) {
                    if let Err(err) = e.rebuild_output_start(oc) { warn!("output start (shared {}): {}", shared, err); }
                }
                // Exclusive access was released with the old unit; re-apply it to the new
                // one. Read from the atomic the
                // settings path keeps current, so no config lock is taken mid-rebuild.
                if crate::audio::hog_mode::wanted() {
                    if let (Some(e), Some(oc)) = (audio_engine.as_ref(), output_ctrl.as_ref()) {
                        e.apply_exclusive_output(oc, true);
                    }
                }
            }
            // ── The one period constraint a BACKEND can break ──────────────
            // The callback period must stay strictly BELOW the receive setpoint. At or
            // above it, a single render cycle consumes the entire buffer, so the ring
            // empties every cycle and the jitter buffer protects nothing — it is drained
            // exactly as fast as it fills. Continuous underrun, not an occasional glitch.
            //
            // `min_output_period` requests at most half the setpoint for every buffer the
            // settings offer, so this needs a backend that grants a LONGER period than the
            // one requested — which is why the granted period is compared here rather than
            // the requested one — or a buffer between the request table's entries, which only
            // the API or a hand-edited config can set.
            //
            // On Windows and Linux a driver can grant a longer period (ALSA rounds to a size
            // the hardware supports; some WASAPI drivers grant their own fixed period), and
            // every setpoint there is floored at twice the granted period or more
            // (`AudioEngine::period_floor`), which `min_active_setpoint` includes — so this
            // cannot trigger on those platforms. It reports what that floor does not cover,
            // on macOS.
            //
            // Logged, not fatal. Detection happens after the streams are built and running,
            // and aborting a live daemon over one device's quirk would take working audio
            // down for every other peer on the machine — a worse outcome than the underrun
            // it would be preventing. Edge-triggered so a persistent bad configuration says
            // so once rather than on every reconcile, and re-arms if it is corrected.
            if let Some(e) = audio_engine.as_ref() {
                let granted = e.live_out_period.load(std::sync::atomic::Ordering::Relaxed);
                if let (true, Some(setpoint)) = (granted > 0, e.min_active_setpoint(&enabled)) {
                    if granted >= setpoint {
                        if !PERIOD_OVER_SETPOINT.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            error!("callback period {} frames ({:.1} ms) is at or above the \
                                    receive setpoint {} ({:.1} ms) — every render cycle drains \
                                    the whole jitter buffer, so incoming audio will underrun \
                                    continuously. The backend granted a larger period than the \
                                    {} requested. Raise the receive buffer setting above {:.1} \
                                    ms, or use a device that honours the requested period.",
                                   granted, granted as f32 / 48.0,
                                   setpoint, setpoint as f32 / 48.0,
                                   shared, granted as f32 / 48.0);
                        }
                    } else {
                        PERIOD_OVER_SETPOINT.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }

            let rebuilt = needs;
            rebuilt
        }};
    }

    /// Reconcile the stored device identity against what the hardware reports: learn the
    /// UID for a device configured by name alone, and refresh the name for one matched by
    /// UID that has since been renamed. Requests a save when anything moved.
    ///
    /// Called at boot, once the engines are up, and after every device selection —
    /// including the one that builds an engine for a direction that had none — so every
    /// path that sets a device resolves its identity the same way.
    macro_rules! learn_device_identity {
        () => {{
            let (in_uid, in_name, out_uid, out_name) = {
                let c = cfg_arc.read().unwrap_or_else(|e| e.into_inner());
                (c.audio.input_device_uid.clone(),  c.audio.input_device.clone(),
                 c.audio.output_device_uid.clone(), c.audio.output_device.clone())
            };
            let mut changed = false;
            for (is_input, uid, name) in [(true, in_uid, in_name), (false, out_uid, out_name)] {
                // Nothing configured in either form: no device, nothing to reconcile.
                if uid.is_empty() && name.is_empty() { continue; }
                // An empty NAME means no device is selected — "— none —" in the UI. A UID
                // left beside it is stale, and resolving it would find the hardware and put
                // the deselected device back. Clear it instead; the name is what says
                // whether a device is chosen at all.
                if name.is_empty() {
                    info!("Clearing stale {} device UID {uid} — no device is selected",
                          if is_input { "input" } else { "output" });
                    let mut c = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                    if is_input { c.audio.input_device_uid.clear(); }
                    else        { c.audio.output_device_uid.clear(); }
                    changed = true;
                    continue;
                }
                let Some(r) = crate::audio::resolve_device(is_input, &uid, &name) else { continue };
                // Resolved BEFORE the config lock is taken: device enumeration is a HAL call
                // and holding a write lock across it is the shape that has deadlocked here
                // before. Only needed when the two disagree, so it is computed lazily.
                let name_exists = if !r.name.is_empty() && r.name != name {
                    crate::audio::resolve_device(is_input, "", &name).is_some()
                } else {
                    false
                };
                let dir = if is_input { "input" } else { "output" };
                let mut c = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                if !r.uid.is_empty() && r.uid != uid {
                    if uid.is_empty() {
                        info!("Learned {dir} device UID for '{}': {}", r.name, r.uid);
                    } else {
                        info!("{dir} device '{}' now reports UID {} (was {}) — updating config",
                              r.name, r.uid, uid);
                    }
                    if is_input { c.audio.input_device_uid  = r.uid.clone(); }
                    else        { c.audio.output_device_uid = r.uid.clone(); }
                    changed = true;
                }
                if !r.name.is_empty() && r.name != name {
                    // The UID resolved to a device that is not the one named in the config.
                    // Two very different situations produce that, and picking the wrong one
                    // silently discards a user's device change:
                    //
                    //   * The configured NAME still matches a device that exists → the user
                    //     selected that device and the UID beside it is stale. The name wins,
                    //     and the UID is relearned from it above on the next pass.
                    //   * The configured name matches NOTHING → the device really was renamed
                    //     underneath us, and the UID is the only durable identity. The name
                    //     follows the UID.
                    //
                    // Resolving by name alone is what tells them apart. Without this test the
                    // first case is misread as the second, and a swap reverts on the next save.
                    if name_exists {
                        info!("{dir} device '{}' selected; discarding stale UID {} \
                               (it resolves to '{}')", name, uid, r.name);
                        if is_input { c.audio.input_device_uid.clear(); }
                        else        { c.audio.output_device_uid.clear(); }
                    } else {
                        info!("Device renamed: '{}' → '{}' (matched by UID {}) — updating config",
                              name, r.name, r.uid);
                        if is_input { c.audio.input_device  = r.name; }
                        else        { c.audio.output_device = r.name; }
                    }
                    changed = true;
                }
            }
            if changed { saver_main.request(); }
        }};
    }
    learn_device_identity!();

    // Apply the persisted exclusive-output (hog mode) setting at boot, before audio flows.
    // Reports the ACTUAL outcome back into config so a failed claim (device busy) isn't
    // remembered as enabled.
    if cfg_arc.read().unwrap_or_else(|e| e.into_inner()).audio.exclusive_output {
        let held = match (audio_engine.as_ref(), output_ctrl.as_ref()) {
            (Some(e), Some(oc)) => e.apply_exclusive_output(oc, true),
            _ => false,
        };
        if !held {
            warn!("Exclusive output was enabled in config but could not be claimed — \
                   continuing in shared mode");
            cfg_arc.write().unwrap_or_else(|e| e.into_inner()).audio.exclusive_output = false;
        }
    }

    // Swap an audio DEVICE with the same discipline as startup: reconfigure BOTH
    // AudioUnits while BOTH are stopped, then start input, then output.
    //
    // Why both units, when only one device changed: input and output are reconfigured
    // together and one is never rebuilt while the other keeps running. That
    // ordering is what biases any swap-time imperfection toward a temporary SURPLUS of
    // buffered audio (input is already writing before output starts draining), which drains
    // back to setpoint on its own. Rebuilding only the changed unit leaves the other running
    // across the swap, which can bias the other way — and a receive buffer that lands BELOW
    // setpoint does not recover on a zero-loss link, because padding needs a sequence gap
    // and nothing else adds samples: a 120ms buffer can sit at 80ms indefinitely.
    //
    // `out_dev`/`in_dev`: Some(device) for the side whose device is changing (marks it a
    // device change, so that side's render state is rebuilt), None for the unchanged side —
    // which is still stopped and reconfigured on its existing device IF it was running. A
    // unit that was not running stays stopped.
    macro_rules! swap_devices {
        ($out_dev:expr, $in_dev:expr) => {{
            let out_dev: Option<crate::audio::Device> = $out_dev;
            let in_dev:  Option<crate::audio::Device> = $in_dev;
            let mut out_err: Option<String> = None;
            let mut in_err:  Option<String> = None;
            // ── Phase 0: STOP BOTH, SETTLE, DISPOSE BOTH ──
            // §5.2's `stop_audio`, in its order: `AudioOutputUnitStop` on input then
            // output, a 20 ms settle with both stopped but still allocated, then
            // uninitialise and dispose both. The settle only means anything in that gap —
            // it is what lets an in-flight callback return before the instance it is
            // running on goes away. Both are torn down before either is rebuilt, so no
            // unit is ever running while another is being configured.
            // Whether each unit was live BEFORE this rebuild stopped it — the test has to
            // be taken here, since stopping a unit is what makes it look absent below.
            let stopped_in_was_live = _capture_engine.as_ref()
                .map_or(false, |e| !e.live_in_device.lock()
                    .unwrap_or_else(|p| p.into_inner()).is_empty());
            let stopped_out_was_live = audio_engine.as_ref()
                .map_or(false, |e| !e.live_out_device.lock()
                    .unwrap_or_else(|p| p.into_inner()).is_empty());
            // Input stopped first, then output — the mirror of the start order, and the
            // order `stop_audio` uses.
            let stopped_in = _capture_engine.as_mut()
                .and_then(|e| e.pause_input_for_rebuild());
            let stopped_out = if let (Some(e), Some(oc)) = (audio_engine.as_ref(),
                                                            output_ctrl.as_mut()) {
                e.pause_output_for_rebuild(oc)
            } else { None };
            std::thread::sleep(crate::audio::UNIT_STOP_SETTLE);
            drop(stopped_in);
            drop(stopped_out);
            // §5.2's device path: the system sample rate is set across this window, and a
            // nominal-rate change is asynchronous — the HAL accepts the write and reports
            // the new value about 100 ms later — so the pause is what lets the hardware
            // arrive at the requested rate before a unit is initialised against it.
            //
            // BLOCKING, like the period path's settle, and never an `.await`: suspending the
            // select loop mid-rebuild lets other arms run against half-torn-down units. The
            // audio callback threads are unaffected by this thread parking.
            std::thread::sleep(crate::audio::DEVICE_RATE_SETTLE);
            crate::audio::backend::set_power_hint();
            // WHICH DIRECTION THE CALLER ACTUALLY CHANGED. Both are rebuilt on any device
            // change, because the two units share one callback period — but only the
            // caller's own direction decides whether its operation succeeded.
            let changing_output = out_dev.is_some();
            let changing_input  = in_dev.is_some();
            // Ask each newly assigned device for its shortest callback period now, while
            // nothing holds either device and before either unit is built: the builds below
            // request no shorter a callback than both devices can run.
            if let Some(d) = out_dev.as_ref() { crate::audio::learn_shortest_period(d, false); }
            if let Some(d) = in_dev.as_ref()  { crate::audio::learn_shortest_period(d, true); }
            // ── Phase 1: BUILD both (configured + stopped; neither running) ──
            // Never resurrect a stopped unit as a side effect of the OTHER direction's change:
            // each is built only when it is the side changing, or when it was running before
            // this rebuild began. A unit the user set to "— none —", or one parked after its
            // device was lost, stays stopped.
            if let (Some(e), Some(oc)) = (audio_engine.as_ref(), output_ctrl.as_mut()) {
                if changing_output || stopped_out_was_live {
                    if let Err(e2) = e.rebuild_output_prepare(oc, out_dev, None) {
                        out_err = Some(format!("output: {}", e2));
                    }
                }
            }
            if let Some(e) = _capture_engine.as_mut() {
                if changing_input || stopped_in_was_live {
                    if let Err(e2) = e.rebuild_input_prepare(in_dev, None) {
                        in_err = Some(format!("input: {}", e2));
                    }
                }
            }
            // A newly assigned device that did not open imposes no limit.
            if changing_output && out_err.is_some() { crate::audio::forget_shortest_period(false); }
            if changing_input && in_err.is_some()   { crate::audio::forget_shortest_period(true); }
            // ── Phase 2: START — input FIRST, then output (§5.2 order) ──
            if let Some(e) = _capture_engine.as_ref() {
                if let Err(e2) = e.rebuild_input_start() { warn!("input start: {}", e2); }
            }
            if let (Some(e), Some(oc)) = (audio_engine.as_ref(), output_ctrl.as_mut()) {
                if let Err(e2) = e.rebuild_output_start(oc) { warn!("output start: {}", e2); }
            }
            // Exclusive access was released with the old unit; re-apply it to the new one —
            // and only if there IS a new one, or the claim would hog a device nothing is
            // playing through. From the atomic, as `reconcile_callback_period!` does.
            if crate::audio::hog_mode::wanted() {
                if let (Some(e), Some(oc)) = (audio_engine.as_ref(), output_ctrl.as_ref()) {
                    if !e.live_out_device.lock().unwrap_or_else(|p| p.into_inner()).is_empty() {
                        e.apply_exclusive_output(oc, true);
                    }
                }
            }
            // A failure in the OTHER direction is reported on its own account and must not
            // fail this operation. Rebuilding the pair is an implementation detail of the
            // shared callback period; a working output change is a working output change
            // even when the input device happens to be one the driver will not open.
            let (mine, theirs) = match (changing_output, changing_input) {
                (true,  false) => (out_err.clone(), in_err.clone()),
                (false, true)  => (in_err.clone(),  out_err.clone()),
                // Neither named a device (a rebuild of what is already selected), or both
                // did: every failure belongs to this operation, and both are reported.
                _ => (match (out_err.clone(), in_err.clone()) {
                          (Some(o), Some(i)) => Some(format!("{o}; {i}")),
                          (o, i) => o.or(i),
                      }, None),
            };
            if let Some(m) = theirs {
                warn!("the other direction also failed during this rebuild: {} \
                       (it is selected separately and unaffected by this change)", m);
            }
            // A device swap is an audio restart, which is a label reload. Advanced once the units are back rather than before
            // the half-second rebuild, so a peer's re-request lands after it, not during it.
            reload_labels(&label_change_indicator, &in_channels_h);
            match mine { Some(m) => Err(anyhow::anyhow!(m)), None => Ok(()) }
        }};
    }

    // Seed BOTH callbacks to the config-derived shared min at startup, before any audio
    // flows. The shared value is fully known from config (setpoint from the saved receive
    // buffer per §3.2, send frame from config), so per the spec there is no connect-time
    // transient — the engine opens the output from the receive hint alone, which can differ
    // when the send frame is the smaller one, so this one-time reconcile closes that gap.
    reconcile_callback_period!();

    // Remotes currently sending pokes that disagree with our encryption setting — logged
    // once as the disagreement starts, again only after it has cleared and returned.
    let mut poke_mismatch_logged: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Hosts already reported for sending pokes no configured remote claims.
    let mut unhandled_probe_hosts: std::collections::HashSet<std::net::IpAddr> =
        std::collections::HashSet::new();

    loop {
        tokio::select! {
            // The ONE shutdown path. Every requester — interrupt, terminate signal, a
            // Windows session ending, a tray or menu-bar item — arrives here, so none of
            // them can skip the device release or the config flush.
            exit = crate::lifecycle::requested() => {
                let _ = exit;
                info!("Shutdown requested — flushing config and exiting");
                // Let every socket user notice and step away from the socket before the
                // process exits. The receive loop, the control sender and the audio encode
                // workers all check lifecycle::is_stopping; this is the window in which they
                // do it. On Windows a thread still inside a socket call when ExitProcess
                // arrives orphans the endpoint, and the next process to start cannot bind the
                // port for over a minute — so this pause is what lets a quick relaunch work.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                // Release exclusive (hog) access so the output device isn't left locked to
                // other applications. OS-enforced, so this must happen before we exit.
                crate::audio::hog_mode::release();
                // A failed write is already logged by the saver; there is nothing further to
                // do with it on the way out.
                let _ = saver_main.flush_now();
                // Take the tray icon down before the process goes, or Windows leaves a dead
                // icon in the notification area until the user hovers over it.
                #[cfg(windows)]
                crate::platform::windows::stop();

                break;
            }
            // Incoming channel labels updated — push event so UI refreshes immediately,
            // but ONLY when the peer's channel set actually changed. The label batch that
            // triggers this fires every stats cycle (~2s) whether or not anything changed;
            // emitting unconditionally made every client issue a redundant /api/channels
            // GET every 2s (and the RX grid guarded against a needless rebuild client-side).
            // Dedupe here on a signature of the peer's channels (index + active + label) so
            // the event — and the client GET — fire only on a real change. The signature
            // includes `active`, so a source appearing or decaying still emits promptly.
            Some(peer_name) = label_notify_rx.recv() => {
                let sig = {
                    let all = incoming_channels.read().unwrap_or_else(|e| e.into_inner());
                    all.get(&peer_name).map(|list| list.iter()
                        .map(|c| format!("{}:{}:{}", c.channel, c.active as u8, c.label))
                        .collect::<Vec<_>>().join("|")).unwrap_or_default()
                };
                if last_ch_sig.get(&peer_name) != Some(&sig) {
                    last_ch_sig.insert(peer_name.clone(), sig);
                    let ev = serde_json::json!({"type":"channels_changed","peer":peer_name}).to_string();
                    let _ = api_state_hot.event_tx.send(ev);
                }
            }
            // Dedicated 2s stats drain — fires even when no audio is arriving (TX-only).
            _ = stats_tick.tick() => {
                let _tick_t0 = std::time::Instant::now();
                // Connected-peer set — used to gate BOTH the live RX and TX counts so a
                // disconnected peer contributes 0 to each, without tearing down its decode
                // state (the warm jitter buffer is left intact for fast blip recovery).
                let connected: std::collections::HashSet<String> = {
                    let ps = peer_stats_map.read().unwrap_or_else(|e| e.into_inner());
                    ps.iter().filter(|(_, p)| p.state == "connected")
                        .map(|(n, _)| n.clone()).collect()
                };
                if let Some(ref e) = audio_engine {
                    // CONSUMING read: this is the only caller, so each tick reports the
                    // mean and spread over exactly the interval since the last one.
                    let report: std::collections::HashMap<String, (f32, f32, f32, f32, bool)> =
                        e.buffer_report().into_iter()
                            .map(|(p, avg, lo, hi, t, h)| (p, (avg, lo, hi, t, h))).collect();
                    *buffer_view.write().unwrap_or_else(|e| e.into_inner()) = report;
                    // Diagnostic only. `channel_depth_report` try_locks and returns None if
                    // the render callback holds the groups, in which case the previous
                    // sample stands rather than this thread waiting on the audio thread.
                    if debug_api {
                        if let Some(d) = e.channel_depth_report() {
                            *depth_view.write().unwrap_or_else(|e| e.into_inner()) =
                                d.into_iter().collect();
                        }
                    }
                    // Live incoming frame size (samples) per peer, for the UI buffer floor.
                    let fr: std::collections::HashMap<String, usize> =
                        e.peer_frame_report().into_iter().collect();
                    *frame_view.write().unwrap_or_else(|e| e.into_inner()) = fr;
                    // Live routed-and-decoding RX channel count, CONNECTED peers only. A
                    // disconnected peer's dec_slots linger (warm buffer kept for reconnect),
                    // so filter by connection rather than counting raw dec_slots.len().
                    active_recv.store(e.active_recv_channels(&connected),
                        std::sync::atomic::Ordering::Relaxed);
                } else {
                    // No output engine → nothing decoding.
                    *frame_view.write().unwrap_or_else(|e| e.into_inner()) =
                        std::collections::HashMap::new();
                    active_recv.store(0, std::sync::atomic::Ordering::Relaxed);
                }
                // ── Stream invalidated: rebuild rather than tear down ──────────────
                // The backend raises StreamInvalidated when the device's sample rate changes
                // underneath us (e.g. the user switching it in Audio MIDI Setup). The stream
                // is dead but the device is fine, so the fix is to rebuild — which re-runs
                // the config path, re-asserts 48 kHz and verifies the hardware clock moved.
                // Without this the daemon went silent until a restart or a device toggle,
                // because the callback stops firing and nothing else notices.
                //
                // Both units are rebuilt together and started input-then-output, matching
                // the ordering used everywhere else, so the receive buffer keeps its setpoint
                // across the swap.
                let invalidation_rebuilt;
                {
                    let out_invalid = audio_engine.as_ref().map_or(false, |e|
                        e.output_stream_invalid.swap(false, std::sync::atomic::Ordering::Relaxed));
                    let in_invalid = _capture_engine.as_ref().map_or(false, |e|
                        e.input_stream_invalid.swap(false, std::sync::atomic::Ordering::Relaxed));
                    // Detected per side, rebuilt as a pair: whichever side was invalidated, the
                    // rebuild goes through the shared two-phase path below, which rebuilds every
                    // unit that was running (see the §5.2 note there).
                    invalidation_rebuilt = out_invalid || in_invalid;
                    if out_invalid || in_invalid {
                        info!("{} device rate changed — reconfiguring both units",
                              match (in_invalid, out_invalid) {
                                  (true, true)  => "Input and output",
                                  (true, false) => "Input",
                                  _             => "Output",
                              });
                        // The published rate describes a stream that no longer exists. It is
                        // stored from the REQUESTED config (always 48 kHz by construction), so
                        // leaving it up would claim 48 kHz while the device sat at 44.1 kHz and
                        // nothing was playing — confidently wrong at exactly the moment it
                        // matters. Zero it; the UI renders that as no rate at all, and the
                        // rebuild re-publishes once the hardware clock is verified.
                        if out_invalid {
                            if let Some(e) = audio_engine.as_ref() {
                                e.live_out_rate.store(0, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        if in_invalid {
                            if let Some(e) = _capture_engine.as_ref() {
                                e.live_in_rate.store(0, std::sync::atomic::Ordering::Relaxed);
                            }
                        }

                        if let Some(e) = audio_engine.as_ref() {
                            e.output_rebuilding.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        if let Some(e) = _capture_engine.as_ref() {
                            e.input_rebuilding.store(true, std::sync::atomic::Ordering::Relaxed);
                        }

                        // ROUTE THROUGH THE SHARED TWO-PHASE PATH — prepare BOTH units,
                        // then start input before output. §5.2 is explicit that this ordering
                        // is the fix, and that per-direction rebuilds are the named failure:
                        //
                        //   "an implementation whose device-change handling calls separate,
                        //    single-unit reconfigure/restart functions per direction ... will
                        //    reproduce exactly this stuck-low symptom — only the changed unit
                        //    ever stops, while the other keeps running and continues draining
                        //    or filling throughout"
                        //
                        // Input writing fresh samples before output starts draining biases the
                        // disturbance toward a temporary SURPLUS, which drains back passively.
                        // A deficit has no passive recovery — nothing adds samples beyond the
                        // arrival rate — which is why the buffer got stuck at 40ms of 120ms.
                        //
                        // So both units rebuild whichever side was invalidated, and there is no
                        // flush: §5.2 also rules out compensating after the fact, since that
                        // "masks the ordering defect rather than fixing it".
                        // Through the SAME implementation every other reconfigure uses
                        // (§5.2: "route every reconfigure trigger through one shared
                        // implementation"). Both devices unchanged — this rebuilds both
                        // units on the devices already configured, which re-asserts 48 kHz
                        // on the one whose rate moved.
                        if let Err(err) = swap_devices!(None, None) {
                            warn!("rebuild (invalidated): {}", err);
                        }

                        // Drop the echo: our own rate-setting is a rate change, so CoreAudio
                        // re-reports the stream invalid and we would rebuild a second time for
                        // no reason. The rate has just been set AND verified, so anything still
                        // genuinely wrong will raise the flag again on the next callback.
                        if let Some(e) = audio_engine.as_ref() {
                            e.output_stream_invalid.store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                        if let Some(e) = _capture_engine.as_ref() {
                            e.input_stream_invalid.store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                        if let Some(e) = audio_engine.as_ref() {
                            e.output_rebuilding.store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                        if let Some(e) = _capture_engine.as_ref() {
                            e.input_rebuilding.store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
                // Catch-all for anything that moved the shared callback period without
                // reconciling itself — a remote enabled, disabled or removed, say. No-op (no
                // rebuild) unless a unit's period actually has to move.
                let cb_rebuilt = reconcile_callback_period!() || invalidation_rebuilt;
                // Live outgoing stream count to CONNECTED peers (signal + active tone), engine
                // truth gated on connection. A disconnected peer's routing persists but it is
                // not transmitting, so it is excluded.
                if let Some(ref ce) = _capture_engine {
                    active_send.store(ce.live_send_streams(&connected),
                        std::sync::atomic::Ordering::Relaxed);
                } else {
                    active_send.store(0, std::sync::atomic::Ordering::Relaxed);
                }
                // Input and output device loss/recovery are both owned by the
                // device_manager tasks: they commit a stop on loss and emit
                // StopInput/StopOutput/Rebuild* as DeviceActions. Nothing device-related
                // happens on this tick — it is purely the buffer-stats drain.
                let _tick_ms = _tick_t0.elapsed().as_secs_f64()*1000.0;
                if _tick_ms > 3.0 {
                    if cb_rebuilt {
                        // Expected: rebuilding the capture+render CoreAudio units to the new
                        // shared callback period is ~100ms of synchronous OS work. It blocks the
                        // control loop only (audio runs on separate threads), and only on a frame
                        // change that moves the shared min — not a stall.
                        info!("Audio units reconfigured ({:.0} ms — expected device work)", _tick_ms);
                    } else {
                        // No rebuild ran this tick, so this is a genuine stall.
                        warn!("stats_tick blocked select loop for {:.1}ms", _tick_ms);
                    }
                }
            }
            // DeviceAction from a device_manager task (input or output). The expensive
            // enumeration already happened off-thread; here we only do the cheap engine
            // op on the main loop (it owns the engines). Any resolved Device is carried
            // in the message — no enumeration on this loop.
            Some(action) = dev_action_rx.recv() => {
                match action {
                    dm::DeviceAction::StopOutput => {
                        if let Some(ref e) = audio_engine {
                            // Only stop if actually running (debounce may have fired after
                            // a manual change already stopped it).
                            let running = !e.live_out_device.lock()
                                .unwrap_or_else(|e| e.into_inner()).is_empty();
                            if running {
                                if let Some(oc) = output_ctrl.as_mut() { e.stop_output(oc); }
                                api::invalidate_device_cache();
                                let _ = api_state_hot.event_tx.send(
                                    serde_json::json!({"type":"device_lost","device":"output"}).to_string());
                                // A device dying is an audio restart, which is a label reload.
                                reload_labels(&label_change_indicator, &in_channels_h);
                            }
                        }
                    }
                    dm::DeviceAction::RebuildOutput { name, device } => {
                        if let Some(ref e) = audio_engine {
                            // Re-check still parked + still the configured name — the user
                            // may have changed selection between discovery and now.
                            let still_parked = e.live_out_device.lock()
                                .unwrap_or_else(|e| e.into_inner()).is_empty()
                                && cfg_arc.read().unwrap_or_else(|e| e.into_inner())
                                    .audio.output_device == name;
                            if still_parked {
                                // Both units stopped → reconfigured → input started, then
                                // output (the audio-start order). The callback period is
                                // reconciled separately, so no frame size is forced here.
                                let _ = e;
                                match swap_devices!(Some(device), None) {
                                    Ok(()) => {
                                        info!("Output device '{}' returned — reactivated", name);
                                        api::invalidate_device_cache();
                                        let _ = api_state_hot.event_tx.send(
                                            serde_json::json!({"type":"device_recovered",
                                                "device":"output","name":name}).to_string());
                                    }
                                    Err(e) => warn!("Auto-reactivate output '{}' failed: {}", name, e),
                                }
                                reconcile_callback_period!();
                            }
                        }
                    }
                    dm::DeviceAction::StopInput => {
                        if let Some(ref mut e) = _capture_engine {
                            let running = !e.live_in_device.lock()
                                .unwrap_or_else(|e| e.into_inner()).is_empty();
                            if running {
                                e.stop_input();
                                api::invalidate_device_cache();
                                let _ = api_state_hot.event_tx.send(
                                    serde_json::json!({"type":"device_lost","device":"input"}).to_string());
                                // A device dying is an audio restart, which is a label reload.
                                reload_labels(&label_change_indicator, &in_channels_h);
                            }
                        }
                    }
                    dm::DeviceAction::RebuildInput { name, device } => {
                        if let Some(ref mut e) = _capture_engine {
                            let still_parked = e.live_in_device.lock()
                                .unwrap_or_else(|e| e.into_inner()).is_empty()
                                && cfg_arc.read().unwrap_or_else(|e| e.into_inner())
                                    .audio.input_device == name;
                            if still_parked {
                                // Both units stopped → reconfigured → input started, then output.
                                let _ = e;
                                match swap_devices!(None, Some(device)) {
                                    Ok(()) => {
                                        info!("Input device '{}' returned — reactivated", name);
                                        api::invalidate_device_cache();
                                        let _ = api_state_hot.event_tx.send(
                                            serde_json::json!({"type":"device_recovered",
                                                "device":"input","name":name}).to_string());
                                    }
                                    Err(e) => warn!("Auto-reactivate input '{}' failed: {}", name, e),
                                }
                                reconcile_callback_period!();
                            }
                        }
                    }
                }
            }
            // Incoming network packets — dormant in degraded mode (no UDP engine)
            Some(pkt) = async { match inbound_rx_opt { Some(ref mut rx) => rx.recv().await, None => std::future::pending().await } } => {
                // Audio packets are handled INLINE on cascade-recv via RecvContext
                // and never arrive here once the context is wired. If one slips
                // through before that (startup), drop it — the render-alive gate would
                // drop pre-render audio anyway.
                if pkt.ptype == PacketType::Audio { continue; }

                // ── Poke gates, ahead of routing and address learning ──
                if pkt.ptype == PacketType::Poke {
                    let tok_peer = peer_by_tok.read().unwrap_or_else(|e| e.into_inner())
                        .get(&pkt.sender_token.0).cloned();
                    let Some(name) = tok_peer else {
                        // No active remote has this sender's identity. A disabled remote
                        // with it is ignored silently; anything else is reported, once per
                        // sending host.
                        let disabled_match = cfg_arc.read().unwrap_or_else(|e| e.into_inner())
                            .remotes.iter().filter(|r| !r.enabled)
                            .any(|r| net::protocol::derive_token(&r.name, &r.password)
                                     == pkt.sender_token);
                        if !disabled_match && unhandled_probe_hosts.insert(pkt.from.ip()) {
                            warn!("Unhandled probe packets received from address {}. \
                                   Check stream name/password", pkt.from.ip());
                        }
                        continue;
                    };
                    // Encryption agreement (CASCADE_ENCRYPTION_SPEC §3.2): a 53-byte poke
                    // (no key) to a remote with encryption ON, or an 87-byte poke (key
                    // attached) to one with it OFF, is dropped here whole — no address
                    // learning, no ACK, no pong, no byte count. Other lengths pass.
                    let encrypting = crypto_map.read().unwrap_or_else(|e| e.into_inner())
                        .get(&name).is_some_and(|c| c.is_enabled());
                    let len = pkt.raw.len();
                    let mismatch = (len == net::protocol::POKE_LEN && encrypting)
                        || (len == net::protocol::POKE_LEN_EXT && !encrypting);
                    if mismatch {
                        if poke_mismatch_logged.insert(name.clone()) {
                            if encrypting {
                                warn!("Remote '{}' is sending UNENCRYPTED packets. Encryption \
                                       is required for remote '{}' and must be enabled on the \
                                       remote side", name, name);
                            } else {
                                warn!("Remote '{}' is sending ENCRYPTED packets. Encryption is \
                                       disabled for remote '{}' on this system and must be \
                                       disabled on the remote side", name, name);
                            }
                        }
                        continue;
                    }
                    if (len == net::protocol::POKE_LEN || len == net::protocol::POKE_LEN_EXT)
                        && poke_mismatch_logged.remove(&name) {
                        info!("Remote '{}' encryption setting now matches", name);
                    }
                }

                // Route control packets by sender token, fall back to source address
                let peer_name = peer_by_tok.read().unwrap_or_else(|e| e.into_inner()).get(&pkt.sender_token.0).cloned()
                    .or_else(|| peer_by_addr.read().unwrap_or_else(|e| e.into_inner()).get(&pkt.from).cloned());

                if let Some(name) = peer_name {
                    // Learn passive peer addresses on first contact
                    peer_by_addr.write().unwrap_or_else(|e| e.into_inner()).entry(pkt.from).or_insert_with(|| name.clone());

                    if let Some(tx) = peer_cmds.get(&name) {
                        // LABEL packets carry the segment total at 0x10 (label_meta), not a
                        // label revision at 0x0A — pass that through instead for those.
                        let label_rev = if pkt.ptype == PacketType::Label {
                            pkt.label_meta as u8
                        } else { pkt.label_revision };
                        let _ = tx.send(PeerCommand::Packet {
                            from: pkt.from, ptype: pkt.ptype, ts: pkt.ts,
                            label_revision: label_rev, raw: pkt.raw,
                            sender_token: pkt.sender_token,
                            label_meta: pkt.label_meta,
                            payload: pkt.payload,
                        }).await;
                    }
                } else {
                    debug!("Packet from unconfigured peer {}", pkt.from);
                }
            }

            // Hot reconfiguration commands from API endpoints (curl-testable).
            // Peer connected — rebuild the send destinations with its newly learned address.
            Some(()) = rebuild_rx.recv() => {
                if let Some(ref eng) = _capture_engine {
                    eng.rebuild_cached_per_ch();
                }
            }

            Some(cmd) = hot_rx.recv() => {
                match cmd {
                    api::HotCommand::SetSendRouting { peer, matrix } => {
                        let routes = crate::audio::routing::RoutingTable::parse_matrix(&matrix);
                        if let Some(ref eng) = _capture_engine {
                            eng.set_send_routing(&peer, &routes);
                            info!("[{}] send routing: {} route{}", peer, routes.len(),
                                  if routes.len() == 1 { "" } else { "s" });
                        }
                        // Labels travel with routing: re-derive this peer's wire
                        // labels from the new routes and re-announce. (Derived from
                        // the routes themselves, not config — the WS handler fires
                        // this command before its config write lands.)
                        reload_labels(&label_change_indicator, &in_channels_h);
                        let ch_labels = cfg_arc.read().unwrap_or_else(|e| e.into_inner()).audio.channel_labels.clone();
                        if let Some(tx) = peer_cmds.get(&peer) {
                            let tone_slots = _capture_engine.as_ref().and_then(|e|
                                e.tone_dests.lock().unwrap_or_else(|p| p.into_inner())
                                 .get(&peer).cloned());
                            let _ = tx.send(net::peer::PeerCommand::LabelsChanged(
                                overlay_tone_labels(
                                    labels_from_routes(&ch_labels, &routes),
                                    tone_slots.as_ref()))).await;
                        }
                        // A route change can move the smallest frame size any channel is
                        // encoded at, so the shared callback period follows it now rather than
                        // on the next stats tick.
                        reconcile_callback_period!();
                    }
                    api::HotCommand::SetBindAddress { iface, port } => {
                        // Move the audio socket in place. Peers, decoders, jitter buffers and
                        // routing are all left alone: the receive path re-arms only on a
                        // genuine drain (CASCADE_AUDIO_RECEIVE_SPEC §4.2) and has no
                        // socket-keyed trigger, so there is nothing here to tear down.
                        //
                        // No explicit re-poke afterwards. Peers ping on a 1-2 s cadence
                        // regardless of reachability and the very next ping is answered once
                        // it lands (CASCADE_WIRE_PROTOCOL_SPEC §7.2), and a changed source
                        // address is the tolerated address-mismatch state (§7.1), not a
                        // rejection — so recovery costs one ping interval and needs no
                        // message of its own.
                        match audio_socket_opt {
                            None => warn!("bind address change ignored — no audio socket"),
                            Some(ref cell) => {
                                // A named interface that is not present resolves to nothing;
                                // binding the wildcard is better than not binding at all.
                                let addr = crate::net::iface::resolve_bind_addr(&iface, port)
                                    .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], port)));
                                match crate::net::udp::rebind(cell, addr) {
                                    Ok(())  => {
                                        // On a real, configured address now, so any conflict
                                        // from startup is over — and clearing it is what stops
                                        // the port probe, which would otherwise later move the
                                        // socket back to the address it was aiming at.
                                        port_conflict_arc.store(false,
                                            std::sync::atomic::Ordering::Relaxed);
                                        info!("audio socket now {}", addr);
                                    }
                                    Err(e)  => warn!("audio socket rebind to {} failed: {}",
                                                     addr, e),
                                }
                            }
                        }
                    }
                    api::HotCommand::SetReceiveRouting { peer, matrix } => {
                        if let Some(ref e) = audio_engine {
                            let routes = crate::audio::routing::RoutingTable::parse_matrix(&matrix);
                            e.set_recv_routing(&peer, &routes);
                            info!("[{}] receive routing: {} route{}", peer, routes.len(),
                                  if routes.len() == 1 { "" } else { "s" });
                        }
                    }
                    api::HotCommand::SetTone { peer, slots } => {
                        if let Some(ref eng) = _capture_engine {
                            if !api_state_hot.on_air.load(std::sync::atomic::Ordering::Relaxed) {
                                eng.set_tone_routing(&peer, &slots);
                                info!("[{}] tone routing updated ({} active slots)",
                                    peer, slots.iter().filter(|&&b| b != 0).count());
                                // The label travels with the audio: tone slots
                                // advertise "Tone L"/"Tone R", cleared slots revert
                                // to their routed source's label. Advance the
                                // revision and re-announce this peer's labels.
                                reload_labels(&label_change_indicator, &in_channels_h);
                                let labels = {
                                    let cfg_snap = cfg_arc.read().unwrap_or_else(|e| e.into_inner());
                                    routed_labels_for_peer(&cfg_snap, &peer)
                                };
                                let tone_slots = if slots.iter().all(|&b| b == 0) {
                                    None } else { Some(slots.clone()) };
                                if let Some(tx) = peer_cmds.get(&peer) {
                                    let _ = tx.send(net::peer::PeerCommand::LabelsChanged(
                                        overlay_tone_labels(labels, tone_slots.as_ref()))).await;
                                }
                            } else {
                                warn!("[{}] tone routing blocked — ON AIR is active", peer);
                            }
                        }
                        // A tone leg is an encoded stream like any other, so a change can move
                        // the smallest encoded frame size.
                        reconcile_callback_period!();
                    }
                    api::HotCommand::SetBitrate(kbps) => {
                        if let Some(ref eng) = _capture_engine {
                            if let Err(e) = eng.set_bitrate_hot(kbps) {
                                warn!("set_bitrate_hot: {}", e);
                                let _ = api_state_hot.event_tx.send(
                                    serde_json::json!({"type":"audio_error",
                                        "msg": format!("Bitrate change failed: {}", e)
                                    }).to_string());
                            } else {
                                // Persist to config so TOML stays in sync
                                cfg_arc.write().unwrap_or_else(|e| e.into_inner()).audio.bitrate_kbps = kbps;
                                saver_main.request();
                            }
                        }
                    }
                    api::HotCommand::SetFrameSize { frame_ms } => {
                        // Reinit the Opus encoders to the new frame size (the ENCODE frame; the
                        // shared CoreAudio callback period is handled by the reconciler below).
                        if let Some(ref mut eng) = _capture_engine {
                            if let Err(e) = eng.reinit_encoders(frame_ms, None) {
                                warn!("reinit_encoders (frame {}ms): {}", frame_ms, e);
                                let _ = api_state_hot.event_tx.send(
                                    serde_json::json!({"type":"audio_error",
                                        "msg": format!("Frame size change to {}ms failed: {}", frame_ms, e)
                                    }).to_string());
                            }
                        }
                        // Persist. The global control sets every remote's frame size —
                        // reinit_encoders already overwrote all live encode prefs, so write every
                        // remote, which is where the setting lives and what a restart reads back.
                        // (The reconcile below reads the encoders, not this config.)
                        // Each remote goes through RemoteConfig::select_frame, so a Voice
                        // remote given a frame under 20 ms moves to Audio; its encoder prefs are
                        // then re-applied with the mode it ended up in.
                        let prefs: Vec<(String, f32, crate::config::AudioMode)> = {
                            let mut c = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                            c.remotes.iter_mut().map(|r| {
                                r.select_frame(frame_ms as f32);
                                (r.name.clone(), r.frame_ms, r.mode)
                            }).collect()
                        };
                        if let Some(ref eng) = _capture_engine {
                            for (name, f, m) in &prefs { eng.set_peer_enc_prefs(name, *f, *m); }
                        }
                        // Reconcile the single shared capture+render callback period to the new
                        // send frame (rebuilds each unit only if the shared min moved).
                        reconcile_callback_period!();
                        saver_main.request();
                        info!("Frame size: {}ms applied (buffers resized live, \
                               timestamp counters continued)", frame_ms);
                    }
                    api::HotCommand::SetMode { mode } => {
                        if let Some(ref eng) = _capture_engine {
                            if let Err(e) = eng.reinit_encoders(-1.0, Some(&mode)) {
                                warn!("reinit_encoders (mode {}): {}", mode, e);
                                let _ = api_state_hot.event_tx.send(
                                    serde_json::json!({"type":"audio_error",
                                        "msg": format!("Mode change to '{}' failed: {}", mode, e)
                                    }).to_string());
                            } else {
                                info!("Encoder mode: {} applied", mode);
                                let m: crate::config::AudioMode = if mode == "voice" {
                                    crate::config::AudioMode::Voice
                                } else { crate::config::AudioMode::Audio };
                                // The global control overwrote every live encode pref's mode
                                // (reinit_encoders), so persist it on every remote — that is
                                // where the setting lives. Each goes through
                                // RemoteConfig::select_mode, so a remote moved to Voice with a
                                // frame under 20 ms is raised to 20 ms, and its encoder prefs
                                // are re-applied with that frame.
                                let prefs: Vec<(String, f32)> = {
                                    let mut c = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                                    c.remotes.iter_mut().map(|r| {
                                        r.select_mode(m);
                                        (r.name.clone(), r.frame_ms)
                                    }).collect()
                                };
                                for (name, f) in &prefs { eng.set_peer_enc_prefs(name, *f, m); }
                                saver_main.request();
                            }
                        }
                    }
                    api::HotCommand::SetInputDevice(name) => {
                        // The device choice is already persisted to config by the settings
                        // save handler; here we rebuild the INPUT stream on the new device
                        // live. The capture clock, encoders, and routing all survive; the new
                        // device's channel count is read live by the builder (n_ch gate), and
                        // routes to channels beyond the new device stay in config, dormant
                        // (not sent). After the rebuild the outgoing label table is rebuilt
                        // and LabelsChanged is sent to every peer.
                        if _capture_engine.is_some() {
                            if name.is_empty() {
                                // "— none —" selected: stop the stream, no fallback.
                                // No device_lost event — this is deliberate, not a failure.
                                // The UI refetches status on save and shows "— none —".
                                if let Some(ref mut eng) = _capture_engine { eng.stop_input(); }
                                // Clear the UID with the name. A stored UID is only
                                // meaningful alongside a selected device; leaving one behind
                                // describes hardware nothing is configured to use, and the
                                // identity reconciler would resolve it and put the device back.
                                cfg_arc.write().unwrap_or_else(|e| e.into_inner())
                                    .audio.input_device_uid.clear();
                                saver_main.request();
                                info!("Input device set to none (stopped)");
                                // An audio-device change is an audio restart, which is a label reload.
                                reload_labels(&label_change_indicator, &in_channels_h);
                            } else {
                            // The UI sends a NAME (that's what the dropdown lists). Drop the
                            // stored UID FIRST: it describes the device being replaced, and
                            // `learn_device_identity!` resolves by UID before name — so a
                            // stale one there resolves to the OLD hardware, sees the new name
                            // beside it, and "corrects" the selection back to the old device.
                            // That is the swap silently reverting on the next save.
                            cfg_arc.write().unwrap_or_else(|e| e.into_inner())
                                .audio.input_device_uid.clear();
                            let dev = crate::audio::resolve_device(true, "", &name)
                                .map(|r| r.device);
                            match dev {
                                // Both units stopped -> reconfigured -> input started, then
                                // output (the audio-start order).
                                Some(d) => match swap_devices!(None, Some(d)) {
                                    Ok(()) => {
                                        info!("Input device → '{}'", name);
                                        learn_device_identity!();
                                        // User has explicitly fixed the device — clear any startup warning.
                                        startup_warnings.lock().unwrap_or_else(|e| e.into_inner())
                                            .retain(|w| !w.contains("Input device"));
                                        let _ = api_state_hot.event_tx.send(
                                            serde_json::json!({"type":"clear_warnings"}).to_string());
                                        // Rebuild label/channel state — 128 slots, the
                                        // wire format's label-table size.
                                        let cfg_snap = cfg_arc.read().unwrap_or_else(|e| e.into_inner()).clone();
                                        // Re-borrow the capture engine (the swap above needed it
                                        // unborrowed); the rest of this block only reads it.
                                        let eng = _capture_engine.as_ref().expect("capture engine present");
                                        outgoing_labels.clear();
                                        {
                                            let mut out = outgoing_channels.write().unwrap_or_else(|e| e.into_inner());
                                            out.clear();
                                            for ch in 0..eng.num_out_ch {
                                                let label = cfg_snap.audio.channel_labels.get(ch)
                                                    .filter(|s| !s.is_empty()).cloned()
                                                    .unwrap_or_else(|| format!("Ch {}", ch + 1));
                                                outgoing_labels.push(ChannelLabel { channel: ch as u32, label: label.clone() });
                                                out.push(OutgoingInfo { channel: ch as u8, label, active: false });
                                            }
                                        }
                                        // Fan updated labels to all peers. The revision they
                                        // compare to decide whether to re-fetch was advanced once,
                                        // by the device swap above.
                                        for (pname, tx) in peer_cmds.iter() {
                                            let labels = routed_labels_for_peer(&cfg_snap, pname);
                                            let tone_slots = eng.tone_dests.lock()
                                                .unwrap_or_else(|p| p.into_inner()).get(pname).cloned();
                                            let _ = tx.send(net::peer::PeerCommand::LabelsChanged(
                                                overlay_tone_labels(labels, tone_slots.as_ref()))).await;
                                        }
                                    }
                                    Err(e) => {
                                        warn!("SetInputDevice rebuild ('{}'): {}", name, e);
                                        let _ = api_state_hot.event_tx.send(
                                            serde_json::json!({"type":"device_error",
                                                "device":"input","name":name,
                                                "msg": format!("Failed to open '{}'", name)}).to_string());
                                    }
                                },
                                None => {
                                    warn!("SetInputDevice: input device '{}' not found", name);
                                    let _ = api_state_hot.event_tx.send(
                                        serde_json::json!({"type":"device_error",
                                            "device":"input","name":name,
                                            "msg": format!("Device '{}' not found", name)}).to_string());
                                }
                            }
                            } // end else (non-empty name)
                        } else {
                            // No capture engine yet (input direction not enabled at
                            // boot). Build it live from the now-saved config — no restart, the
                            // live OUTPUT direction + all peer links are untouched.
                            // build_input only declines here on a port conflict; a machine
                            // with no routes still opens its input device and simply sends
                            // nothing, because transmission is decided by the send matrix.
                            if name.is_empty() {
                                info!("SetInputDevice: no engine and no device selected — nothing to do");
                            } else {
                                let cfg_snap = cfg_arc.read().unwrap_or_else(|e| e.into_inner()).clone();
                                let (eng_new, labels, outgoing) =
                                    build_ctx.build_input(&cfg_snap);
                                match eng_new {
                                    Some(mut eng) => {
                                        // Same peer-status atomics as the boot path, so the
                                        // §3.1 per-destination gate works on a live-built engine.
                                        eng.adopt_peer_status_map(peer_status_map.clone());
                                        eng.adopt_link_map(link_map.clone());
                                        // Publish the input-side meter/count handles → UI lights
                                        // up within one meter tick (≤40ms).
                                        input_peaks_h.publish(Arc::clone(&eng.input_peaks));
                                        tone_peaks_h.publish(Arc::clone(&eng.tone_peaks));
                                        tone_dests_h.publish(Arc::clone(&eng.tone_dests));
                                        active_streams_h.publish(Arc::clone(&eng.active_streams));
                                        // 6: publish the device/count/rate handles so the UI
                                        // shows the real channels/rate/device-name.
                                        in_channels_h.publish(Arc::clone(&eng.live_in_ch));
                                        in_sample_rate_h.publish(Arc::clone(&eng.live_in_rate));
                                        live_in_device_h.publish(Arc::clone(&eng.live_in_device));
                                        // Apply the outgoing channel labels / info (as boot does).
                                        outgoing_labels.clear();
                                        outgoing_labels.extend(labels);
                                        {
                                            let mut out = outgoing_channels.write().unwrap_or_else(|e| e.into_inner());
                                            out.clear();
                                            out.extend(outgoing);
                                        }
                                        // Spawn the per-direction manager so input heals on
                                        // device loss/return like a boot-present direction.
                                        spawn_manager!(dm::Dir::Input,
                                            Arc::clone(&eng.live_in_device),
                                            Arc::clone(&eng.input_device_lost));
                                        // Enabling a direction is an audio restart, which is a label reload.
                                        reload_labels(&label_change_indicator, &in_channels_h);
                                        // Fan labels to peers before moving eng.
                                        // Acquire the tone_dests lock per-iteration and drop it
                                        // BEFORE the .await (the LabelsChanged send) — a
                                        // std::sync::Mutex guard must never be held across an
                                        // await point.
                                        for (pname, tx) in peer_cmds.iter() {
                                            let plabels = routed_labels_for_peer(&cfg_snap, pname);
                                            let tone_slots = eng.tone_dests.lock()
                                                .unwrap_or_else(|p| p.into_inner()).get(pname).cloned();
                                            let _ = tx.send(net::peer::PeerCommand::LabelsChanged(
                                                overlay_tone_labels(plabels, tone_slots.as_ref()))).await;
                                        }
                                        _capture_engine = Some(eng);
                                        // Register the already-connected peers' TX byte atomics
                                        // into the freshly-built engine (mirrors the boot loop).
                                        // Without this the encode path increments the engine's own
                                        // (empty) atomics while the stats tick reads peer_tx_atomics
                                        // → the UI TX rate would show only POKE traffic (~0.001
                                        // Mbps) instead of the real audio send rate.
                                        if let Some(ref eng2) = _capture_engine {
                                            for (pn, atom) in &peer_tx_atomics {
                                                eng2.register_tx_atomic(pn, Arc::clone(atom));
                                            }
                                        }
                                        startup_warnings.lock().unwrap_or_else(|e| e.into_inner())
                                            .retain(|w| !w.contains("Input device"));
                                        let _ = api_state_hot.event_tx.send(
                                            serde_json::json!({"type":"clear_warnings"}).to_string());
                                        info!("Input device → '{}'", name);
                                        learn_device_identity!();
                                    }
                                    None => {
                                        // The only remaining reason build_input returns None
                                        // here is that the device failed to open (a port
                                        // conflict is refused earlier, before this command).
                                        warn!("SetInputDevice: build of '{}' produced no engine", name);
                                        let _ = api_state_hot.event_tx.send(
                                            serde_json::json!({"type":"device_error",
                                                "device":"input","name":name,
                                                "msg": format!("Failed to open '{}'", name)}).to_string());
                                    }
                                }
                            }
                        }
                    }
                    api::HotCommand::SetOutputDevice(name) => {
                        // As SetInputDevice, for the OUTPUT (render) side. render_groups and
                        // dec_slots are cleared on a device change so receive render state is
                        // rebuilt fresh against the new device; recv_routing is reconciled to
                        // the new device's channel count inside set_recv_routing.
                        if let Some(ref e2) = audio_engine {
                            if name.is_empty() {
                                // "— none —" selected: stop the stream, no fallback.
                                // No device_lost event — this is deliberate, not a failure.
                                if let Some(oc) = output_ctrl.as_mut() { e2.stop_output(oc); }
                                // As the input path: the UID goes with the name.
                                cfg_arc.write().unwrap_or_else(|e| e.into_inner())
                                    .audio.output_device_uid.clear();
                                saver_main.request();
                                info!("Output device set to none (stopped)");
                                // An audio-device change is an audio restart, which is a label reload.
                                reload_labels(&label_change_indicator, &in_channels_h);
                            } else {
                            // As the input path: drop the outgoing device's UID before
                            // resolving, or the reconciler resolves it and reverts the swap.
                            cfg_arc.write().unwrap_or_else(|e| e.into_inner())
                                .audio.output_device_uid.clear();
                            let resolved = crate::audio::resolve_device(false, "", &name);
                            let dev = resolved.map(|r| r.device);
                            match dev {
                                // Both units stopped → reconfigured → input started, then output
                                // (the audio-start order), so any swap-time imperfection
                                // biases toward a buffer SURPLUS that drains back to setpoint —
                                // never a deficit, which would stick permanently.
                                Some(d) => match swap_devices!(Some(d), None) {
                                    Ok(()) => {
                                        info!("Output device → '{}'", name);
                                        learn_device_identity!();
                                        // Move any exclusive (hog) claim onto the NEW device —
                                        // claim() releases the previously-held one first.
                                        let want_excl = cfg_arc.read()
                                            .unwrap_or_else(|e| e.into_inner()).audio.exclusive_output;
                                        if want_excl {
                                            if let Some(oc) = output_ctrl.as_ref() {
                                                let held = e2.apply_exclusive_output(oc, true);
                                                if !held {
                                                    warn!("Exclusive output could not be claimed on \
                                                           '{}' — continuing shared", name);
                                                    cfg_arc.write().unwrap_or_else(|e| e.into_inner())
                                                        .audio.exclusive_output = false;
                                                }
                                            }
                                        }
                                        startup_warnings.lock().unwrap_or_else(|e| e.into_inner())
                                            .retain(|w| !w.contains("Output device"));
                                        let _ = api_state_hot.event_tx.send(
                                            serde_json::json!({"type":"clear_warnings"}).to_string());
                                    }
                                    Err(e) => {
                                        warn!("SetOutputDevice rebuild ('{}'): {}", name, e);
                                        let _ = api_state_hot.event_tx.send(
                                            serde_json::json!({"type":"device_error",
                                                "device":"output","name":name,
                                                "msg": format!("Failed to open '{}'", name)}).to_string());
                                    }
                                },
                                None => {
                                    warn!("SetOutputDevice: output device '{}' not found", name);
                                    let _ = api_state_hot.event_tx.send(
                                        serde_json::json!({"type":"device_error",
                                            "device":"output","name":name,
                                            "msg": format!("Device '{}' not found", name)}).to_string());
                                }
                            }
                            } // end else (non-empty name)
                        } else {
                            // No output engine yet (direction not enabled at boot).
                            // Build it live from the now-saved config — no restart, the live
                            // INPUT direction + all peer links are untouched. Only acts on a
                            // real device name (empty with no engine = nothing to do).
                            if name.is_empty() {
                                info!("SetOutputDevice: no engine and no device selected — nothing to do");
                            } else {
                                let cfg_snap = cfg_arc.read().unwrap_or_else(|e| e.into_inner()).clone();
                                let (eng_new, oc_new) = build_ctx.build_output(&cfg_snap);
                                match eng_new {
                                    Some(eng) => {
                                        // Publish the output-side meter handles so the UI lights
                                        // up within one meter tick (≤40ms). on_air / live_out_*
                                        // are standalone Arcs the engine writes into directly.
                                        output_peaks_h.publish(Arc::clone(&eng.output_peaks));
                                        incoming_peaks_h.publish(Arc::clone(&eng.incoming_peaks));
                                        // 6: publish the device/count/rate handles so the UI
                                        // shows the real channels/rate/device-name (not 0 /
                                        // "(unavailable)" / "— none —").
                                        out_channels_h.publish(Arc::clone(&eng.live_out_ch));
                                        out_sample_rate_h.publish(Arc::clone(&eng.live_out_rate));
                                        live_out_device_h.publish(Arc::clone(&eng.live_out_device));
                                        // Attach the DECODE plane to the receive context: the
                                        // accounting plane is already live from boot (recv_acct),
                                        // so we just republish RecvContext carrying the same acct
                                        // Arc + decode=Some(new engine). Incoming accounting was
                                        // never interrupted; now audio also decodes/plays.
                                        if let Some(ref ctx_cell) = recv_ctx_opt {
                                            // Reuse the boot accounting plane; rebuild it only in
                                            // the unexpected case it wasn't created at boot.
                                            let acct = recv_acct.clone().unwrap_or_else(|| {
                                                let phase_lock_peers: std::collections::HashSet<String> =
                                                    cfg_snap.remotes.iter().filter(|r| r.phase_lock)
                                                        .map(|r| r.name.clone()).collect();
                                                Arc::new(net::udp::RecvAccounting {
                                                    by_addr:           Arc::clone(&peer_by_addr),
                                                    rx_atomics:        Arc::clone(&peer_rx_atomics),
                                                    sr_atomics:        Arc::clone(&peer_sr_atomics),
                                                    phase_lock_peers:  Arc::new(RwLock::new(phase_lock_peers)),
                                                    incoming_channels: Arc::clone(&incoming_channels),
                                                    incoming_seen:     Arc::clone(&incoming_seen),
                                                    crypto:            crypto_map.clone(),
                                                    links:             link_map.clone(),
                                                    meters:            Arc::clone(&meters),
                                                })
                                            });
                                            let decode = Some(Arc::new(net::udp::RecvDecode {
                                                engine: Arc::clone(&eng),
                                            }));
                                            let ctx = net::udp::RecvContext { acct, decode };
                                            *ctx_cell.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(ctx));
                                        }
                                        // Spawn the per-direction manager so output heals on
                                        // device loss/return like a boot-present direction.
                                        spawn_manager!(dm::Dir::Output,
                                            Arc::clone(&eng.live_out_device),
                                            Arc::clone(&eng.output_device_lost));
                                        audio_engine = Some(eng);
                                        output_ctrl  = oc_new;
                                        startup_warnings.lock().unwrap_or_else(|e| e.into_inner())
                                            .retain(|w| !w.contains("Output device"));
                                        let _ = api_state_hot.event_tx.send(
                                            serde_json::json!({"type":"clear_warnings"}).to_string());
                                        info!("Output device → '{}'", name);
                                        learn_device_identity!();
                                        // Enabling a direction is an audio restart, which is a label reload.
                                        reload_labels(&label_change_indicator, &in_channels_h);
                                    }
                                    None => {
                                        warn!("SetOutputDevice: build of '{}' produced no engine", name);
                                        let _ = api_state_hot.event_tx.send(
                                            serde_json::json!({"type":"device_error",
                                                "device":"output","name":name,
                                                "msg": format!("Failed to open '{}'", name)}).to_string());
                                    }
                                }
                            }
                        }
                    }
                    api::HotCommand::SetRemoteFrameSize { peer, frame_ms } => {
                        // Per-remote outgoing frame size. set_peer_enc_prefs → reconcile_encoders
                        // moves this remote onto the streams for its new size — independent of
                        // the CoreAudio callback period handled below.
                        // Persist the new send frame first: a frame under 20 ms on a Voice
                        // remote also moves it to Audio (RemoteConfig::select_frame), and the
                        // encoder takes whatever the remote ends up with. The shared callback
                        // period below is driven by the encoders set_peer_enc_prefs resolves,
                        // not by this config.
                        let (frame_ms, mode) = {
                            let mut c = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                            match c.remotes.iter_mut().find(|r| r.name == peer) {
                                Some(r) => { r.select_frame(frame_ms); (r.frame_ms, r.mode) }
                                None    => (frame_ms, crate::config::AudioMode::default()),
                            }
                        };
                        if let Some(ref mut eng) = _capture_engine {
                            eng.set_peer_enc_prefs(&peer, frame_ms, mode);
                        }
                        // The send frame moved → reconcile the SINGLE shared capture+render
                        // callback period so BOTH units track min(incoming, outgoing). A
                        // send-frame shrink must shrink the render callback too: an output
                        // period left too large takes several packets per drain and truncates
                        // them at the write clamp.
                        reconcile_callback_period!();
                        saver_main.request();
                        info!("Remote '{}': outgoing frame size → {}ms", peer, frame_ms);
                    }
                    api::HotCommand::SetRemoteMode { peer, mode } => {
                        let mode_cfg = if mode == "voice" {
                            crate::config::AudioMode::Voice
                        } else {
                            crate::config::AudioMode::Audio
                        };
                        // Choosing Voice on a remote with a frame under 20 ms raises the frame
                        // to 20 ms (RemoteConfig::select_mode); the encoder takes the result.
                        let pm = {
                            let mut c = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                            match c.remotes.iter_mut().find(|r| r.name == peer) {
                                Some(r) => { r.select_mode(mode_cfg); r.frame_ms }
                                None    => crate::config::DEFAULT_FRAME_MS,
                            }
                        };
                        if let Some(ref eng) = _capture_engine {
                            eng.set_peer_enc_prefs(&peer, pm, mode_cfg);
                        }
                        saver_main.request();
                        info!("Remote '{}': encoder mode → {}", peer, mode);
                    }
                    api::HotCommand::SetEncryption { peer, on } => {
                        // Flip the live crypto flag (get-or-create the state so a
                        // toggle before the peer task exists still takes effect) and
                        // persist. When turning on, the next poke carries our public
                        // key and the handshake completes within a poke interval.
                        {
                            let mut cm = crypto_map.write().unwrap_or_else(|e| e.into_inner());
                            cm.entry(peer.clone())
                                .or_insert_with(|| Arc::new(net::crypto::PeerCrypto::new(on)))
                                .set_enabled(on);
                        }
                        {
                            let mut c = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                            if let Some(r) = c.remotes.iter_mut().find(|r| r.name == peer) {
                                r.encryption = on;
                            }
                        }
                        saver_main.request();
                        info!("Remote '{}': encryption {}", peer, if on {"ON"} else {"off"});
                    }
                    api::HotCommand::SetReceiveBuffer { peer, ms } => {
                        // 20ms minimum applies ONLY when Sync is enabled for this peer
                        // at the moment the setting is applied (CASCADE_AUDIO_RECEIVE_
                        // SPEC §4): a smaller request is silently treated as 20ms. With
                        // Sync off the requested value is used as given (5ms floor is
                        // the config-range bound, not a sync floor).
                        let sync_on = {
                            let c = cfg_arc.read().unwrap_or_else(|e| e.into_inner());
                            c.remotes.iter().find(|r| r.name == peer)
                                .map(|r| r.phase_lock).unwrap_or(false)
                        };
                        let clamped = if sync_on { ms.max(20).clamp(5, 10000) }
                                      else       { ms.clamp(5, 10000) };
                        if clamped != ms {
                            info!("[{}] receive buffer → {} ms (requested {} ms, raised to the \
                                   phase-lock minimum) — re-buffering", peer, clamped, ms);
                        } else {
                            info!("[{}] receive buffer → {} ms — re-buffering", peer, clamped);
                        }
                        if let Some(ref e) = audio_engine {
                            e.retarget_peer_buffer(&peer, clamped);
                        }
                        // The receive setpoint changed → the incoming callback hint may have
                        // moved, so reconcile the single shared capture+render period. Rebuilds
                        // each unit only if the shared min actually changed.
                        reconcile_callback_period!();
                        // Persist so it survives restart.
                        {
                            let mut c = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                            if let Some(r) = c.remotes.iter_mut().find(|r| r.name == peer) {
                                r.receive_buffer_ms = clamped;
                            }
                        }
                        saver_main.request();
                    }
                    api::HotCommand::AddRemote(remote_cfg) => {
                        // No port_conflict gate. Peer tasks are built whether or not the
                        // socket has reached its configured address yet — they hold the
                        // socket cell, so they simply start reaching the far side when the
                        // probe takes the port. Only a daemon with no audio socket at all
                        // (the placeholder bind failed too) has nothing to build a peer on.
                        let Some(out2) = outbound_tx_opt.clone() else {
                            warn!("Remote '{}' not started — this daemon has no audio socket",
                                  remote_cfg.name);
                            continue;
                        };
                        let rname = remote_cfg.name.clone();
                        // Shut down existing task for this peer if present (re-config)
                        if let Some(old_tx) = peer_cmds.remove(&rname) {
                            let _ = old_tx.send(net::peer::PeerCommand::Shutdown).await;
                            // Its address, port or password may be what changed: the old
                            // address and token must stop resolving to this remote before the
                            // new ones are registered below. Audio is attributed by address
                            // alone, so a stale entry would credit the old host's audio here.
                            registry.forget_reachability(&rname);
                        }
                        registry.ensure_stats(&rname);
                        let tx_bytes_atomic = peer_tx_atomics.entry(rname.clone())
                            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicU64::new(0)))
                            .clone();
                        let rx_bytes_atomic = registry.rx_atomic(&rname);
                        let sr_44k_atomic   = registry.sr_atomic(&rname);
                        // Register TX atomic with capture engine
                        if let Some(ref eng) = _capture_engine {
                            eng.tx_atomics.write().unwrap_or_else(|e| e.into_inner())
                                .insert(rname.clone(), Arc::clone(&tx_bytes_atomic));
                        }
                        let remote_addr = net::resolve_addr_async(
                            &remote_cfg.host, remote_cfg.port).await;
                        // Keep the resolved address so peer_by_addr can reuse it below
                        // rather than issuing a second DNS lookup (which, on round-robin
                        // DNS, could return a different IP than the PeerTask is using).
                        let resolved_addr = remote_addr.as_ref().ok().copied();
                        if let Ok(addr) = &remote_addr {
                            info!("Remote '{}': DNS '{}' → {}", rname, remote_cfg.host, addr);
                        } else if !remote_cfg.host.is_empty() {
                            warn!("Remote '{}': DNS '{}' failed — PeerTask will retry on reconnect",
                                  rname, remote_cfg.host);
                        }
                        let remote_password = remote_cfg.password.clone();
                        // Our identity comes from the LIVE config, never the `cfg` snapshot
                        // taken at startup. A rename commits the new name to cfg_arc and then
                        // re-adds every enabled remote precisely so this line re-derives the
                        // token; the startup copy would re-spawn every peer with the old
                        // identity, and the rename would never reach the wire.
                        let our_name = cfg_arc.read().unwrap_or_else(|e| e.into_inner())
                            .general.name.clone();
                        let our_token = derive_token(&our_name, &remote_password);
                        // Restore routing from the remote's saved matrices, so that
                        // enabling/reconnecting a remote brings its routes back live.
                        let cfg_matrices = {
                            let c = cfg_arc.read().unwrap_or_else(|e| e.into_inner());
                            c.remotes.iter().find(|r| r.name == rname)
                                .map(|r| (r.send_matrix.clone(), r.receive_matrix.clone()))
                        };
                        let send_routes: Vec<crate::audio::routing::RouteEntry> =
                            match cfg_matrices.as_ref().and_then(|(s, _)| s.as_ref()) {
                                Some(m) => crate::audio::routing::RoutingTable::parse_matrix(m),
                                None => vec![],
                            };
                        // Apply receive routing to the audio engine too.
                        if let Some(ref e) = audio_engine {
                            if let Some(m) = cfg_matrices.as_ref().and_then(|(_, r)| r.as_ref()) {
                                let recv_routes = crate::audio::routing::RoutingTable::parse_matrix(m);
                                e.set_recv_routing(&rname, &recv_routes);
                            }
                            // Re-apply the peer's config settings, matching the boot loop —
                            // enabling a remote must land in the same state a restart would.
                            // Phase lock first (set_peer_buffer reads it to pick the 20ms
                            // sync-on floor). Without these two the peer came back with the
                            // engine-wide default buffer and phase lock off, because
                            // remove_peer drops the runtime entries on disable.
                            e.set_phase_lock(&rname, remote_cfg.phase_lock);
                            e.set_peer_buffer(&rname, remote_cfg.receive_buffer_ms);
                        }
                        if let Some(ref eng) = _capture_engine {
                            // Prefs before routing, as at boot: the routes land straight on
                            // this remote's own streams.
                            eng.set_peer_enc_prefs(&rname, remote_cfg.frame_ms,
                                                   remote_cfg.mode);
                            eng.set_send_routing(&rname, &send_routes);
                        }
                        let pcfg = net::peer::PeerConfig {
                            name:         rname.clone(),
                            remote_name:  rname.clone(),
                            host:         remote_cfg.host.clone(),
                            port:         remote_cfg.port,
                            remote_addr:  remote_addr.ok(),
                            our_token,
                            password:     remote_password.clone(),
                            learned_addrs: learned_addrs.clone(),
                            rebuild_tx: Some(rebuild_tx.clone()),
                            rx_bytes_atomic,
                            tx_bytes_atomic,
                            sr_44k_atomic,
                            stat_acc: audio_engine.as_ref().map(|e| e.stat_acc_handle()),
                            label_change_indicator: Arc::clone(&label_change_indicator),
                            event_tx: api_state_hot.event_tx.clone(),
                            crypto: crypto_map.write().unwrap_or_else(|e| e.into_inner())
                                .entry(rname.clone())
                                .or_insert_with(|| Arc::new(
                                    net::crypto::PeerCrypto::new(remote_cfg.encryption)))
                                .clone(),
                            link: net::link_for(&link_map, &rname),
                                        };
                        let (cmd_tx, cmd_rx) = mpsc::channel::<crate::net::peer::PeerCommand>(64);
                        let stats2 = peer_stats_tx.clone();
                        tokio::spawn(async move {
                            let (sr_tx, mut sr_rx) = mpsc::channel::<net::peer::SendRequest>(256);
                            tokio::spawn(async move {
                                while let Some(sr) = sr_rx.recv().await {
                                    let _ = out2.send(net::udp::OutboundPacket { to: sr.to, data: sr.data }).await;
                                }
                            });
                            net::peer::PeerTask::new(pcfg).run(cmd_rx, sr_tx, stats2).await;
                        });
                        if let Some(addr) = resolved_addr {
                            registry.insert_addr(addr, &rname);
                            info!("Added remote '{}' → {} (active)", rname, addr);
                        } else {
                            info!("Added remote '{}' (passive)", rname);
                        }
                        let tok = derive_token(&rname, &remote_password);
                        registry.insert_tok(tok.0, &rname);
                        peer_cmds.insert(rname.clone(), cmd_tx);
                        // Send the new peer its routed labels (from its saved matrix —
                        // AddRemote runs after the config commit, so this is fresh).
                        {
                            let cfg_snap = cfg_arc.read().unwrap_or_else(|e| e.into_inner()).clone();
                            if let Some(tx) = peer_cmds.get(&rname) {
                                let _ = tx.send(net::peer::PeerCommand::LabelsChanged(
                                    routed_labels_for_peer(&cfg_snap, &rname))).await;
                            }
                        }
                        // Rebuild address cache
                        if let Some(ref eng) = _capture_engine {
                            eng.rebuild_cached_per_ch();
                        }
                        // Broadcast routing snapshot to all connected WS clients
                        {
                            let cfg_snap = cfg_arc.read().unwrap_or_else(|e| e.into_inner());
                            let routing_msg = api::build_routing_snapshot(&cfg_snap);
                            drop(cfg_snap);
                            let _ = api_state_hot.event_tx.send(routing_msg);
                        }
                    }
                    api::HotCommand::RemoveRemote(name) => {
                        teardown_peer!(&name);
                        // DELETE from persistent config too — otherwise the remote
                        // and its send_matrix/receive_matrix survive restart and the
                        // routing "comes back". Deletion clears it from memory + disk.
                        {
                            let mut cfg_w = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                            cfg_w.remotes.retain(|r| r.name != name);
                        }
                        saver_main.request();
                        info!("Removed remote '{}' (cleared from config + routing)", name);
                        {
                            let cfg_snap = cfg_arc.read().unwrap_or_else(|e| e.into_inner());
                            let routing_msg = api::build_routing_snapshot(&cfg_snap);
                            drop(cfg_snap);
                            let _ = api_state_hot.event_tx.send(routing_msg);
                        }
                    }
                    api::HotCommand::DisableRemote(name) => {
                        // Full teardown like RemoveRemote, but KEEP the config entry
                        // (UI has set enabled=false and will save it). Enabling later
                        // re-spawns via AddRemote and restores routing from the matrices.
                        // learned_addrs is dropped inside teardown_peer! so the send cache
                        // can't keep streaming to a disabled remote — it repopulates on the
                        // next handshake when re-enabled.
                        teardown_peer!(&name);
                        info!("[{}] disabled — config retained", name);
                        {
                            let cfg_snap = cfg_arc.read().unwrap_or_else(|e| e.into_inner());
                            let routing_msg = api::build_routing_snapshot(&cfg_snap);
                            drop(cfg_snap);
                            let _ = api_state_hot.event_tx.send(routing_msg);
                        }
                    }
                    api::HotCommand::SetExclusiveOutput(on) => {
                        // Claim/release CoreAudio hog mode on the live OUTPUT device, then
                        // persist. A claim can legitimately fail (another app holds the device,
                        // or it doesn't support hogging) — we report the ACTUAL state so the UI
                        // never shows exclusive when we're really shared.
                        let held = match (audio_engine.as_ref(), output_ctrl.as_ref()) {
                            (Some(e), Some(oc)) => e.apply_exclusive_output(oc, on),
                            _ => {
                                if !on { crate::audio::hog_mode::release(); }
                                false
                            }
                        };
                        // Definitive result to the operator, via the shared banner (same surface
                        // as "Saved."): a transient notice on success, a sticky error when the
                        // OS refused the claim.
                        if on && !held {
                            let _ = api_state_hot.event_tx.send(serde_json::json!({
                                "type":"audio_error",
                                "msg":"Exclusive output unavailable - device in use by another \
                                       application. Continuing shared."
                            }).to_string());
                        } else {
                            let _ = api_state_hot.event_tx.send(serde_json::json!({
                                "type":"notice",
                                "msg": if held { "Exclusive output active." }
                                       else    { "Exclusive output released." },
                                "ms": 2000
                            }).to_string());
                        }
                        {
                            let mut c = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                            c.audio.exclusive_output = held;
                        }
                        saver_main.request();
                        info!("Exclusive output (hog mode): requested {}, active {}", on, held);
                        // An exclusive-access change is an audio restart, which is a label reload.
                        reload_labels(&label_change_indicator, &in_channels_h);
                    }
                    api::HotCommand::SetOnAir(enabled) => {
                        if let Some(ref e) = audio_engine {
                            e.on_air.store(enabled, std::sync::atomic::Ordering::Relaxed);
                        }
                        // Going on air forgets every tone route — a line-up tone must
                        // never survive into a live transmission. Re-announce labels
                        // for any peer that was carrying tone so "Tone L/R" clears at
                        // the receiver too.
                        if enabled {
                            if let Some(ref eng) = _capture_engine {
                                let toned: Vec<String> = eng.tone_dests.lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .keys().cloned().collect();
                                eng.clear_tone();
                                if !toned.is_empty() {
                                    reload_labels(&label_change_indicator, &in_channels_h);
                                    let cfg_snap = cfg_arc.read().unwrap_or_else(|e| e.into_inner()).clone();
                                    for peer in &toned {
                                        if let Some(tx) = peer_cmds.get(peer.as_str()) {
                                            let _ = tx.send(net::peer::PeerCommand::LabelsChanged(
                                                routed_labels_for_peer(&cfg_snap, peer))).await;
                                        }
                                    }
                                }
                            }
                            info!("ON AIR — all tone routes cleared");
                            // Clearing tone can move the smallest encoded frame size.
                            reconcile_callback_period!();
                        }
                    }
                    api::HotCommand::SetLabel { channel, label } => {
                        // Update the outgoing channel label and re-send labels to all peers.
                        if channel < outgoing_labels.len() {
                            outgoing_labels[channel].label = label.clone();
                        }
                        // Persist to config (labels are global and must survive restart).
                        {
                            let mut cfg_w = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                            if cfg_w.audio.channel_labels.len() <= channel {
                                cfg_w.audio.channel_labels.resize(channel + 1, String::new());
                            }
                            cfg_w.audio.channel_labels[channel] = label.clone();
                        }
                        saver_main.request();
                        // A label edit advances the revision; then fan out LabelsChanged.
                        reload_labels(&label_change_indicator, &in_channels_h);
                        let cfg_snap = cfg_arc.read().unwrap_or_else(|e| e.into_inner()).clone();
                        for (name, cmd_tx) in peer_cmds.iter() {
                            let _ = cmd_tx.send(net::peer::PeerCommand::LabelsChanged(
                                routed_labels_for_peer(&cfg_snap, name))).await;
                        }
                        // 1-based for display: `channel` is a 0-based index everywhere
                        // internally, but the UI numbers channels from 1, and a log that
                        // disagreed with the matrix the user is looking at is worse than
                        // no log.
                        info!("Label ch{} → {:?}", channel + 1, label);
                    }
                }
            }
            // Phase sync commands from the API.
            Some((peer, enabled)) = phase_rx.recv() => {
                if let Some(ref e) = audio_engine {
                    e.set_phase_lock(&peer, enabled);
                    info!("[{}] phase lock {}", peer, if enabled {"on"} else {"off"});
                }
                // Persist to cascade.toml so it survives restart (debounced).
                {
                    let mut cfg_w = cfg_arc.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(r) = cfg_w.remotes.iter_mut().find(|r| r.name == peer) {
                        r.phase_lock = enabled;
                    }
                }
                saver_main.request();
            }
            Some(peer) = disconnect_rx.recv() => {
                // Jitter arrival state is not reset here, or anywhere on a disconnect: it
                // lives on the receive thread (CASCADE_SESSION_STATS_SPEC §2.3/§2.4 put the
                // measurement at packet arrival), and it carries straight across an outage —
                // a reconnect reports the break as one large jitter sample, and counts any
                // sequence gap it left as loss. See ArrivalStats in net/udp.rs.
                //
                // Stop sending audio to a peer that has TIMED OUT — audio is gated on
                // connection state. The send cache is driven by routing + learned
                // addresses, independent of the peer task, so without this audio would keep
                // streaming to a dead remote. Routing config is untouched; when the peer
                // reconnects, its poke re-learns the address and the watcher rebuilds the
                // cache, so audio resumes automatically.
                learned_addrs.write().unwrap_or_else(|e| e.into_inner()).remove(&peer);
                if let Some(ref eng) = _capture_engine {
                    eng.rebuild_cached_per_ch();
                }
            }


        }
    }

    Ok(())
}
