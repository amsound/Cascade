//! Cascade web API — axum HTTP + WebSocket server.
//!
//! The WebSocket (`/ws`) is the web UI's live channel, JSON both ways:
//!
//! Client → Server: settings and hot controls — `save_config`, `set_buffer`,
//!   `set_frame_size`, `routing_tx`/`routing_rx`, `tone_routing`, `on_air`, `phase` and the
//!   other `set_*` messages `handle_ws_msg` handles.
//!
//! Server → Client: `state` on connect, then events as they happen — `config_saved`,
//!   `routing`, `devices`, `interfaces`, device, peer and network errors, and notices.
//!
//! Meters and statistics are polled over HTTP (`/api/peaks`, `/api/status`); a meter poll
//! also tells the daemon what is being viewed (crate::meter).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::{
    extract::{State, Path, ws::{WebSocket, WebSocketUpgrade, Message}},
    routing::{get, post},
    Router, Json,
    response::{Html, IntoResponse},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use anyhow::Result;

use crate::config::Config;
use crate::net::peer::PeerStats;

static WEB_HTML: &str = include_str!("web.html");

// ── Broadcast channels ──────────────────────────────────────────────────────

/// JSON event — pushed on state change (connections, routing, on_air).
pub type EventSender = broadcast::Sender<String>;

// ── Shared app state ────────────────────────────────────────────────────────

/// A handle to engine-owned shared state (meter arrays, counters, tone maps) that may not
/// exist at boot and can be published later when its engine is built. Captured by value at
/// startup, a direction enabled after boot (engine built late) could never light up its
/// meters/counts — the boot-time AppState clones would hold `None` forever. Every AppState
/// clone shares ONE cell, so the late build can `publish()` the engine's handles and every reader
/// (the API handlers) picks them up on its next read. `None` until published; reads clone the
/// inner `Arc` out and release the lock.
pub struct LateHandle<T>(Arc<RwLock<Option<Arc<T>>>>);

// Manual Clone: cloning a LateHandle clones the Arc (sharing the inner cell), so it must NOT
// require T: Clone. `#[derive(Clone)]` would wrongly add a `T: Clone` bound, which fails for
// the atomic/Mutex/RwLock payloads we store (and would be wrong anyway — we share, not copy).
impl<T> Clone for LateHandle<T> {
    fn clone(&self) -> Self { LateHandle(Arc::clone(&self.0)) }
}

impl<T> LateHandle<T> {
    pub fn empty() -> Self { LateHandle(Arc::new(RwLock::new(None))) }
    pub fn with(v: Arc<T>) -> Self { LateHandle(Arc::new(RwLock::new(Some(v)))) }
    /// Publish (or replace) the handle — seen by all clones on their next get().
    pub fn publish(&self, v: Arc<T>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Some(v);
    }
    /// Current handle, if any. Clones the inner Arc and releases the lock immediately.
    pub fn get(&self) -> Option<Arc<T>> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config:            Arc<RwLock<Config>>,
    pub config_path:       PathBuf,
    pub peer_stats:        Arc<RwLock<HashMap<String, PeerStats>>>,
    /// Per-peer incoming channel labels keyed by peer name.
    pub incoming_channels: Arc<RwLock<std::collections::HashMap<String, Vec<ChannelInfo>>>>,
    pub outgoing_channels: Arc<RwLock<Vec<OutgoingInfo>>>,
    pub phase_tx:          Option<tokio::sync::mpsc::Sender<(String, bool)>>,
    pub input_peaks:       LateHandle<Vec<std::sync::atomic::AtomicU32>>,
    /// Line-up tone peaks: [L, R]. Read by the TX tone-row meters.
    pub tone_peaks:        LateHandle<Vec<std::sync::atomic::AtomicU32>>,
    pub output_peaks:      LateHandle<Vec<std::sync::atomic::AtomicU32>>,
    /// Per-incoming peak levels keyed by peer name (slot-indexed). Read for RX meters.
    pub incoming_peaks:    LateHandle<std::sync::RwLock<std::collections::HashMap<String, Arc<Vec<std::sync::atomic::AtomicU32>>>>>,
    /// Incoming-signal metering: the watch `/api/peaks` touches and the snapshot it returns.
    pub meters:            Arc<crate::meter::Meters>,
    pub on_air:            Arc<std::sync::atomic::AtomicBool>,
    /// Per-peer tone routing (destination slot → tone active). Exposed so the TX page
    /// reflects which outputs are currently carrying line-up tone.
    pub tone_dests:        LateHandle<std::sync::Mutex<HashMap<String, Vec<u8>>>>,
    /// Active outgoing encoder count (routed source channels + tone streams).
    pub active_streams:    LateHandle<AtomicUsize>,
    /// Per-peer receive-buffer health: peer → (mean_ms, min_ms, max_ms, target_ms, holding)
    /// of the mean across active channels, folded over the interval since the previous tick
    /// rather than sampled at one instant. Refreshed by the main loop's stats tick, so API
    /// handlers read a snapshot and never reach into the engine the audio threads use.
    pub buffer_view:       Arc<RwLock<HashMap<String, (f32, f32, f32, f32, bool)>>>,
    /// Per-channel ring depths, peer → [(slot, depth_samples)]. Populated only when
    /// `CASCADE_DEBUG_API=1`; empty otherwise, and the route that reads it is not
    /// registered at all. Diagnostic surface, deliberately not part of the normal API.
    pub depth_view:        Arc<RwLock<HashMap<String, Vec<(u8, usize)>>>>,
    /// Per-peer live incoming frame size in samples (120/240/480/960 = 2.5/5/10/20 ms),
    /// from the engine. The web UI floors the receive-buffer list at 2× this. Absent for
    /// peers not yet decoding.
    pub frame_view:        Arc<RwLock<HashMap<String, usize>>>,
    /// Live count of received channels actually routed-and-decoding for connected peers,
    /// polled from the engine by the main-loop stats tick. 0 when no output engine. This is
    /// the live RX counterpart to active_streams (TX).
    pub active_recv:       Arc<AtomicUsize>,
    /// Live count of outgoing streams to CONNECTED peers (signal routes + active tone),
    /// polled by the stats tick via CaptureEngine::live_send_streams. The real "what's
    /// leaving me" TX total — engine truth, gated on connection (a disconnected peer's routing
    /// persists but isn't transmitting, so it's excluded). 0 when no capture engine.
    pub active_send:       Arc<AtomicUsize>,
    /// Actual channel counts / sample rates / device names of the running audio engines.
    /// LateHandle (not bare Arc) because a direction enabled after boot builds its engine
    /// late and must publish the engine's real Arcs here — a boot-time bare Arc would be a
    /// throwaway the late engine never writes to, leaving the UI showing 0 channels /
    /// "(unavailable)" / "— none —" even though audio runs. None until that direction exists.
    pub out_channels:      LateHandle<AtomicUsize>,
    pub in_channels:       LateHandle<AtomicUsize>,
    pub out_sample_rate:   LateHandle<std::sync::atomic::AtomicU32>,
    pub in_sample_rate:    LateHandle<std::sync::atomic::AtomicU32>,
    /// Actual running device names — reflect the device in use, which may differ
    /// from config if startup fell back to a default (e.g. configured device absent).
    pub live_in_device:    LateHandle<std::sync::Mutex<String>>,
    pub live_out_device:   LateHandle<std::sync::Mutex<String>>,
    /// Warnings accumulated during startup (before the WS server is running).
    /// Sent to every new browser connection so they're never silently missed.
    pub startup_warnings:  Arc<std::sync::Mutex<Vec<String>>>,
    pub hot_tx:            Option<tokio::sync::mpsc::Sender<HotCommand>>,
    /// Debounced, non-blocking config persistence. Call `saver.request()` after
    /// mutating `config`; the file is written lazily. Use `saver.flush_now()` when the
    /// file must be on disk before continuing (a settings save, shutdown).
    pub saver:             Option<crate::config_saver::ConfigSaver>,
    /// WebSocket event broadcast.
    pub event_tx:          EventSender,
    /// True while the audio socket is not yet on its configured address because the port was
    /// busy (EADDRINUSE). Everything else runs; the socket moves the moment the port frees,
    /// or when settings choose another address, and this clears.
    pub port_conflict:     Arc<std::sync::atomic::AtomicBool>,
    /// Notified when `[api] bind`/`port` change, so the listener rebinds in place.
    pub api_rebind:        Arc<tokio::sync::Notify>,
}

impl AppState {}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct ChannelInfo {
    pub channel: u8,
    pub label:   String,
    pub active:  bool,
    /// Whether this channel currently has a live decode/render channel — i.e. it is routed
    /// to at least one playable output. This is the selector for CASCADE_AUDIO_RECEIVE_SPEC
    /// §9.3's single-point meter switch: routed → §9's post-buffer peak, unrouted → §9.2's
    /// pre-decode peak. Written by the receive path, which performs the same routing check
    /// that gates real decode dispatch.
    #[serde(default)]
    pub routed:  bool,
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct OutgoingInfo {
    pub channel: u8,
    pub label:   String,
    pub active:  bool,
}

/// Commands sent from API/WS to main loop for hot reconfiguration.
#[derive(Debug, Clone)]
pub enum HotCommand {
    SetBitrate(u32),
    /// Move the audio socket to a new bind address, live. Port and interface travel
    /// together because together they ARE the address: they are bound in one call, and a
    /// change rebinds rather than restarting.
    SetBindAddress    { iface: String, port: u16 },
    SetSendRouting    { peer: String, matrix: String },
    SetReceiveRouting { peer: String, matrix: String },
    SetOnAir(bool),
    /// Claim/release EXCLUSIVE (hog) access to the output device. Persisted to config.
    SetExclusiveOutput(bool),
    SetLabel          { channel: usize, label: String },
    SetTone           { peer: String, slots: Vec<u8> },
    SetFrameSize      { frame_ms: f32 },
    SetMode           { mode: String },
    /// Change ONE remote's outgoing frame size live. The per-channel encoder
    /// latency resolves to the max across destinations sharing that channel
    /// (CASCADE_AUDIO_SEND_SPEC §4.1), so this can change encoders for channels
    /// this remote shares with others.
    SetRemoteFrameSize { peer: String, frame_ms: f32 },
    /// Change ONE remote's Opus application mode live.
    SetRemoteMode      { peer: String, mode: String },
    /// Change a remote's receive (jitter) buffer live.
    SetReceiveBuffer  { peer: String, ms: u32 },
    /// Toggle a remote's end-to-end encryption live (CASCADE_ENCRYPTION_SPEC).
    SetEncryption     { peer: String, on: bool },
    /// Add or re-configure a remote connection hot (no restart required).
    AddRemote(crate::config::RemoteConfig),
    /// Remove a remote connection hot (also deletes from persistent config).
    RemoveRemote(String),
    /// Disable a remote hot: full teardown (peer task + routing) like RemoveRemote,
    /// but the config entry is RETAINED (enabled=false) so it can be restored on
    /// enable, so a disabled remote simply drops out of the active set.
    DisableRemote(String),
    /// Switch the input (capture) device live — no restart. An empty name means NO DEVICE:
    /// the capture stream is stopped and nothing is captured or sent. Any other name is
    /// resolved and the input stream is rebuilt on it. The choice is already persisted to
    /// config by the settings-save handler, so it survives a later restart.
    SetInputDevice(String),
    /// Switch the output (render) device live — no restart. As `SetInputDevice`, for
    /// the output side.
    SetOutputDevice(String),
}

// ── Router ──────────────────────────────────────────────────────────────────

pub fn router(state: AppState) -> Router {
    // Diagnostic route, registered only when asked for. Not gated inside the handler with a
    // 403: an unregistered route simply does not exist, so the surface is absent rather than
    // present-and-refusing. Same env var main.rs uses to decide whether to populate it —
    // without that, the route would answer with a map nothing ever fills.
    let debug_api = std::env::var("CASCADE_DEBUG_API").map(|v| v == "1").unwrap_or(false);
    let r = Router::new()
        .route("/",                          get(get_ui))
        .route("/ws",                        get(ws_handler))
        .route("/api/status",                get(get_status))
        .route("/api/channels",              get(get_channels))
        .route("/api/bitrate",               post(post_bitrate))
        .route("/api/routing/:peer/send",    post(post_routing_send))
        .route("/api/routing/:peer/receive", post(post_routing_receive))
        .route("/api/peaks",                 get(get_peaks))
        // GET only. Saving goes through the WebSocket `save_config` message, which is the
        // one path that also APPLIES the change — opening or closing audio devices, adding
        // or removing remotes, moving the audio socket, re-deriving identity on a rename. A
        // POST that only wrote the file looked equivalent and was not: a device named
        // through it was persisted and never opened until the next restart.
        .route("/api/config",                get(get_config))
        .route("/api/devices",               get(get_devices))
        .route("/api/interfaces",            get(get_interfaces))
        .route("/api/phase/:peer",           post(post_phase));
    let r = if debug_api {
        tracing::info!("CASCADE_DEBUG_API=1 — /api/buffers exposed (per-channel ring depths)");
        r.route("/api/buffers", get(get_buffers))
    } else {
        r
    };
    r.layer(CorsLayer::permissive())
     .with_state(state)
}

/// Per-channel ring depths, peer → [{slot, depth_samples, depth_ms}].
///
/// Diagnostic only, and only reachable with `CASCADE_DEBUG_API=1`. The headline buffer
/// figure is a mean across a peer's channels with the interval's min/max around it; this is
/// what those aggregate over, so a single channel dipping can be attributed to a channel
/// rather than inferred from the range.
///
/// Reads a snapshot main.rs refreshes on its own tick — it never touches the engine or any
/// lock the render thread holds.
async fn get_buffers(State(s): State<AppState>) -> impl IntoResponse {
    let v = s.depth_view.read().unwrap_or_else(|e| e.into_inner());
    let out: serde_json::Map<String, Value> = v.iter().map(|(peer, chans)| {
        let rows: Vec<Value> = chans.iter().map(|(slot, d)| json!({
            "slot": slot, "depth_samples": d, "depth_ms": *d as f64 / 48.0,
        })).collect();
        (peer.clone(), json!(rows))
    }).collect();
    Json(Value::Object(out))
}

/// Serve the web UI, rebinding in place whenever the configured address or port changes.
///
/// The listener is the ONLY thing `[api]` settings own, and nothing downstream holds a
/// reference to it, so moving it is a drop and a bind.
///
/// A move that cannot bind — the new port already taken — goes back to the address it was
/// serving, puts that address back in the settings, and says so: in the log, as a banner,
/// and to every browser that connects afterwards (all of them lost their connection with the
/// old listener). Only a failure to bind at startup, or to reclaim the old address, ends the
/// web UI; the audio side is unaffected either way.
pub async fn serve_rebindable(state: AppState) -> Result<()> {
    const MOVE_FAILED: &str = "Web UI could not move to ";
    // The (bind, port) currently served, once there is one.
    let mut serving: Option<(String, u16)> = None;
    loop {
        let (bind, port) = {
            let c = state.config.read().unwrap_or_else(|e| e.into_inner());
            (c.api.bind.clone(), c.api.port)
        };
        let addr = format!("{bind}:{port}");
        let (listener, now_serving) = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => {
                state.startup_warnings.lock().unwrap_or_else(|e| e.into_inner())
                    .retain(|w| !w.starts_with(MOVE_FAILED));
                (l, (bind, port))
            }
            Err(e) => {
                let Some((old_bind, old_port)) = serving.clone() else {
                    tracing::warn!("Web API: cannot bind {addr}: {e}");
                    return Err(e.into());
                };
                let old_addr = format!("{old_bind}:{old_port}");
                let l = match tokio::net::TcpListener::bind(&old_addr).await {
                    Ok(l) => l,
                    Err(e2) => {
                        tracing::warn!("Web API: cannot bind {addr} ({e}), and {old_addr} could \
                                        not be reclaimed ({e2})");
                        return Err(e2.into());
                    }
                };
                let msg = format!("{MOVE_FAILED}{addr} ({e}) — still on {old_addr}.");
                tracing::warn!("{msg}");
                {
                    // Unless a newer change has replaced it in the meantime.
                    let mut c = state.config.write().unwrap_or_else(|e| e.into_inner());
                    if c.api.bind == bind && c.api.port == port {
                        c.api.bind = old_bind.clone();
                        c.api.port = old_port;
                    }
                }
                if let Some(ref saver) = state.saver { saver.request(); }
                state.startup_warnings.lock().unwrap_or_else(|e| e.into_inner()).push(msg.clone());
                let _ = state.event_tx.send(json!({"type":"network_error","msg": msg}).to_string());
                (l, (old_bind, old_port))
            }
        };
        tracing::info!("Web UI listening on {}:{}", now_serving.0, now_serving.1);
        serving = Some(now_serving);

        // Serve until the address changes under us. `notified()` is created before serving,
        // so a change arriving mid-serve is still delivered.
        let changed = state.api_rebind.notified();
        tokio::select! {
            r = axum::serve(listener, router(state.clone())) => {
                r?;
                return Ok(());
            }
            _ = changed => {
                tracing::info!("Web UI address changed — rebinding");
                // Drop happens at the end of this iteration; the next loop binds the new
                // address. A moment with no listener is unavoidable.
            }
        }
    }
}

/// Spawn the device-inventory background task.
pub fn spawn_background_tasks(state: AppState) {
    // ── Device-inventory task ─────────────────────────────────────────────────
    // The single enumeration point for the device list (one source of truth). Runs
    // off-thread (spawn_blocking — CoreAudio enumeration is slow and probes devices),
    // compares against the stored inventory, and on a CHANGE updates the store AND pushes
    // a `devices` WS event so browsers update their dropdowns immediately without polling.
    // A normal tick is slow (3s) to keep the mic-in-use flash infrequent; a `dirty` nudge
    // (set by invalidate_device_cache on device loss/recovery) makes the next short tick
    // re-check promptly. Stage 4's CoreAudio watcher will replace the periodic tick with
    // OS push, leaving only the dirty-driven re-check.
    let inv_state = state.clone();
    tokio::spawn(async move {
        // Short service tick; real enumeration is gated to the interval or a dirty nudge.
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut since_enum = std::time::Duration::ZERO;
        // Unconditional re-enumeration is only a BACKSTOP for changes that don't trigger a
        // `dirty` nudge (e.g. a device rename on some systems). Enumerating probes input
        // devices (mic-in-use flash), so this is kept slow (60s) to avoid a periodic flash.
        // The prompt path is the `dirty` nudge from invalidate_device_cache — set by the
        // CoreAudio device-change watcher (Stage 4) and on device loss/recovery — which
        // re-enumerates and pushes within ~500ms. So normal add/remove/recovery is near
        // instant; the 60s sweep only catches the rare missed-event case.
        const ENUM_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
        loop {
            tick.tick().await;
            let (dirty, populated) = {
                let inv = device_inventory().lock().unwrap_or_else(|e| e.into_inner());
                (inv.dirty, inv.populated)
            };
            since_enum += std::time::Duration::from_millis(500);
            // Enumerate when: first run (not yet populated), a dirty nudge, or the slow
            // interval elapsed. Otherwise skip (no probing, no mic flash).
            if populated && !dirty && since_enum < ENUM_INTERVAL {
                continue;
            }
            since_enum = std::time::Duration::ZERO;

            let (inputs, outputs) = tokio::task::spawn_blocking(|| {
                // Picker lists: real hardware endpoints only on Linux (see
                // `audio::is_selectable_device`). Resolution is deliberately NOT filtered.
                let inputs  = crate::audio::selectable_input_devices();
                let outputs = crate::audio::selectable_output_devices();
                (inputs, outputs)
            }).await.unwrap_or_default();

            let changed = {
                let mut inv = device_inventory().lock().unwrap_or_else(|e| e.into_inner());
                inv.dirty = false;
                let changed = !inv.populated || inv.inputs != inputs || inv.outputs != outputs;
                if changed {
                    inv.inputs = inputs.clone();
                    inv.outputs = outputs.clone();
                    inv.populated = true;
                }
                changed
            };
            if changed {
                let msg = json!({
                    "type": "devices",
                    "inputs": inputs,
                    "outputs": outputs,
                }).to_string();
                let _ = inv_state.event_tx.send(msg);
            }
        }
    });
}

// ── WebSocket handler ───────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(socket: WebSocket, state: AppState) {
    use axum::extract::ws::Message;
    let (mut sender, mut receiver) = socket.split();
    use futures_util::SinkExt;
    use futures_util::StreamExt;

    // Every client receives events: connection state, device and routing changes, notices.
    let mut event_sub = state.event_tx.subscribe();

    // Send initial state immediately.
    let init = build_state_msg(&state);
    let _ = sender.send(Message::Text(init)).await;

    loop {
        tokio::select! {
            // Inbound control messages from client.
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_ws_msg(&text, &state, &mut sender).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            // Event broadcast.
            ev = event_sub.recv() => {
                match ev {
                    Ok(msg) => {
                        if sender.send(Message::Text(msg)).await.is_err() { break; }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        }
    }
}

async fn handle_ws_msg(
    text: &str,
    state: &AppState,
    sender: &mut (impl futures_util::SinkExt<Message, Error = axum::Error> + Unpin),
) {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("phase") => {
            if let (Some(peer), Some(enabled)) = (
                v.get("peer").and_then(|p| p.as_str()),
                v.get("enabled").and_then(|e| e.as_bool()),
            ) {
                if let Some(ref tx) = state.phase_tx {
                    let _ = tx.try_send((peer.to_string(), enabled));
                    tracing::debug!("phase lock {} for peer '{}'",
                        if enabled { "enabled" } else { "disabled" }, peer);
                }
                // Persist to config (debounced, non-blocking).
                {
                    let mut cfg = state.config.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(r) = cfg.remotes.iter_mut().find(|r| r.name == peer) {
                        r.phase_lock = enabled;
                    }
                }
                if let Some(ref s) = state.saver { s.request(); }
                // Echo the applied state back so the UI shows what was applied, not a
                // stale phase-lock state.
                let ev = json!({"type":"state","peer":peer,"phase_lock":enabled}).to_string();
                let _ = state.event_tx.send(ev);
            }
        }

        Some("tone_routing") => {
            if let (Some(peer), Some(slots_val)) = (
                v.get("peer").and_then(|p| p.as_str()),
                v.get("slots").and_then(|s| s.as_array()),
            ) {
                let slots: Vec<u8> = slots_val.iter()
                    .map(|v| (v.as_u64().unwrap_or(0) as u8).min(2)).collect();
                if let Some(ref tx) = state.hot_tx {
                    let _ = tx.try_send(HotCommand::SetTone {
                        peer: peer.to_string(), slots,
                    });
                }
            }
        }

        Some("set_frame_size") => {
            let ms = v.get("ms").and_then(|m| m.as_f64()).unwrap_or(20.0) as f32;
            if let Some(ref tx) = state.hot_tx {
                let _ = tx.try_send(HotCommand::SetFrameSize { frame_ms: ms });
            }
        }

        Some("set_mode") => {
            let mode = v.get("mode").and_then(|m| m.as_str()).unwrap_or("audio").to_string();
            if let Some(ref tx) = state.hot_tx {
                let _ = tx.try_send(HotCommand::SetMode { mode });
            }
        }

        Some("set_remote_frame_size") => {
            let peer = v.get("peer").and_then(|p| p.as_str()).unwrap_or("").to_string();
            let ms   = v.get("ms").and_then(|m| m.as_f64()).unwrap_or(20.0) as f32;
            if !peer.is_empty() {
                if let Some(ref tx) = state.hot_tx {
                    let _ = tx.try_send(HotCommand::SetRemoteFrameSize { peer, frame_ms: ms });
                }
            }
        }

        Some("set_remote_mode") => {
            let peer = v.get("peer").and_then(|p| p.as_str()).unwrap_or("").to_string();
            let mode = v.get("mode").and_then(|m| m.as_str()).unwrap_or("audio").to_string();
            if !peer.is_empty() {
                if let Some(ref tx) = state.hot_tx {
                    let _ = tx.try_send(HotCommand::SetRemoteMode { peer, mode });
                }
            }
        }

        Some("set_encryption") => {
            let peer = v.get("peer").and_then(|p| p.as_str()).unwrap_or("").to_string();
            let on   = v.get("on").and_then(|b| b.as_bool()).unwrap_or(false);
            if !peer.is_empty() {
                if let Some(ref tx) = state.hot_tx {
                    let _ = tx.try_send(HotCommand::SetEncryption { peer, on });
                }
            }
        }

        Some("set_buffer") => {
            let peer = v.get("peer").and_then(|p| p.as_str()).unwrap_or("").to_string();
            let ms   = v.get("ms").and_then(|m| m.as_u64()).unwrap_or(120) as u32;
            if !peer.is_empty() {
                if let Some(ref tx) = state.hot_tx {
                    let _ = tx.try_send(HotCommand::SetReceiveBuffer { peer, ms });
                }
            }
        }

        Some("set_exclusive_output") => {
            let on = v.get("on").and_then(|b| b.as_bool()).unwrap_or(false);
            if let Some(ref tx) = state.hot_tx {
                let _ = tx.try_send(HotCommand::SetExclusiveOutput(on));
            }
        }

        Some("set_label") => {
            if let (Some(ch), Some(label)) = (
                v.get("channel").and_then(|c| c.as_u64()),
                v.get("label").and_then(|l| l.as_str()),
            ) {
                if let Some(ref tx) = state.hot_tx {
                    // Clamp the channel index to the protocol maximum before it reaches
                    // the hot-command consumer, which resizes channel_labels to
                    // channel+1 — an unbounded index here would force a huge allocation.
                    let channel = (ch as usize)
                        .min(crate::audio::encode::OUTGOING_CHANNELS_MAX - 1);
                    let _ = tx.try_send(HotCommand::SetLabel {
                        channel,
                        label: crate::net::peer::cap_label(label),
                    });
                }
            }
        }

        Some("on_air") => {
            let enabled = v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true);
            state.on_air.store(enabled, Ordering::Relaxed);
            let ev = json!({"type":"state","on_air":enabled}).to_string();
            let _ = state.event_tx.send(ev);
            // Forward to the hot-command loop: going on air force-clears all tone
            // routes (a line-up tone must never survive into a live transmission).
            if let Some(ref tx) = state.hot_tx {
                let _ = tx.try_send(HotCommand::SetOnAir(enabled));
            }
        }

        Some("set_bitrate") => {
            let kbps = v.get("kbps").and_then(|k| k.as_u64()).unwrap_or(128) as u32;
            if let Some(ref tx) = state.hot_tx {
                let _ = tx.try_send(HotCommand::SetBitrate(kbps));
            }
        }

        Some("routing_tx") => {
            if let (Some(peer), Some(matrix)) = (
                v.get("peer").and_then(|p| p.as_str()),
                v.get("matrix").and_then(|m| m.as_str()),
            ) {
                if let Some(ref tx) = state.hot_tx {
                    let _ = tx.try_send(HotCommand::SetSendRouting {
                        peer: peer.to_string(),
                        matrix: matrix.to_string(),
                    });
                }
                // Persist send routing to config so it survives page refresh.
                {
                    let mut cfg = state.config.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(r) = cfg.remotes.iter_mut().find(|r| r.name == peer) {
                        r.send_matrix = if matrix.is_empty() { None } else { Some(matrix.to_string()) };
                        tracing::debug!("routing_tx '{}': matrix='{}'", peer, matrix);
                    } else {
                        tracing::warn!("routing_tx: peer '{}' not found in config (remotes: {:?})",
                            peer, cfg.remotes.iter().map(|r| &r.name).collect::<Vec<_>>());
                    }
                }
                // Debounced, non-blocking persist (coalesces rapid matrix edits).
                if let Some(ref s) = state.saver { s.request(); }
                // Push updated routing to all other connected clients so they
                // reflect the change without waiting for pollSlow.
                let snap = { let cfg = state.config.read().unwrap_or_else(|e| e.into_inner());
                    build_routing_snapshot(&cfg) };
                let _ = state.event_tx.send(snap);
            }
        }

        Some("routing_rx") => {
            if let (Some(peer), Some(matrix)) = (
                v.get("peer").and_then(|p| p.as_str()),
                v.get("matrix").and_then(|m| m.as_str()),
            ) {
                if let Some(ref tx) = state.hot_tx {
                    let _ = tx.try_send(HotCommand::SetReceiveRouting {
                        peer: peer.to_string(),
                        matrix: matrix.to_string(),
                    });
                }
                // Persist receive routing to config.
                {
                    let mut cfg = state.config.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(r) = cfg.remotes.iter_mut().find(|r| r.name == peer) {
                        r.receive_matrix = if matrix.is_empty() { None } else { Some(matrix.to_string()) };
                        tracing::debug!("routing_rx '{}': matrix='{}'", peer, matrix);
                    } else {
                        tracing::warn!("routing_rx: peer '{}' not found in config (remotes: {:?})",
                            peer, cfg.remotes.iter().map(|r| &r.name).collect::<Vec<_>>());
                    }
                }
                if let Some(ref s) = state.saver { s.request(); }
                // Push updated routing to all other connected clients.
                let snap = { let cfg = state.config.read().unwrap_or_else(|e| e.into_inner());
                    build_routing_snapshot(&cfg) };
                let _ = state.event_tx.send(snap);
            }
        }

        Some("save_config") => {
            if let Some(new_cfg_val) = v.get("config") {
                if let Ok(new_cfg) = serde_json::from_value::<Config>(new_cfg_val.clone()) {
                    // Routing matrices and passwords are not the settings page's to set —
                    // see `restore_server_owned_fields`. Restored here, before the diff below
                    // compares them.
                    let mut new_cfg = new_cfg;
                    {
                        let old_cfg = state.config.read().unwrap_or_else(|e| e.into_inner());
                        restore_server_owned_fields(&mut new_cfg, new_cfg_val, &old_cfg);
                    }
                    // Voice exists only at 20 ms (RemoteConfig::normalise_voice).
                    for r in new_cfg.remotes.iter_mut() { r.normalise_voice(); }

                    // Snapshot the diff decisions while we still hold the OLD config,
                    // BEFORE committing the new one. We emit the hot commands AFTER the
                    // commit so AddRemote (which restores routing from the live config)
                    // reads the FRESH matrices: looked up in the old config, a renamed
                    // remote would not be found and would spawn with empty routing.
                    enum RemoteAction { Add(crate::config::RemoteConfig), Remove(String), Disable(String) }
                    let mut actions: Vec<RemoteAction> = Vec::new();
                    {
                        let old_cfg = state.config.read().unwrap_or_else(|e| e.into_inner());
                        for old_r in &old_cfg.remotes {
                            if !new_cfg.remotes.iter().any(|nr| nr.name == old_r.name) {
                                actions.push(RemoteAction::Remove(old_r.name.clone()));
                            }
                        }
                        for new_r in &new_cfg.remotes {
                            match old_cfg.remotes.iter().find(|or| or.name == new_r.name) {
                                None => {
                                    if new_r.enabled {
                                        actions.push(RemoteAction::Add(new_r.clone()));
                                    }
                                }
                                Some(o) => {
                                    let was_on = o.enabled;
                                    let now_on = new_r.enabled;
                                    let details_changed = o.host != new_r.host
                                        || o.port != new_r.port
                                        || o.password != new_r.password;
                                    if was_on && !now_on {
                                        actions.push(RemoteAction::Disable(new_r.name.clone()));
                                    } else if !was_on && now_on {
                                        actions.push(RemoteAction::Add(new_r.clone()));
                                    } else if now_on && details_changed {
                                        actions.push(RemoteAction::Add(new_r.clone()));
                                    }
                                }
                            }
                        }
                    }

                    // Work out what changed against the old config before committing. Nothing
                    // here restarts the daemon; each change has its own in-place path.
                    //
                    // PORT and NETWORK INTERFACE are together the audio socket's bind address,
                    // which moves in place (SetBindAddress).
                    //
                    // The NAME is baked into the wire token. The token is derived per peer when
                    // that peer's task is built, from the live config, and `AddRemote` rebuilds a
                    // peer task in place — so re-adding every enabled remote re-derives every
                    // token without touching the socket: identity hashes are recomputed during
                    // the config rebuild, never by restarting.
                    //
                    // Device changes apply HOT, including building a direction's engine from
                    // nothing when it wasn't enabled at boot (the SetInput/OutputDevice
                    // handlers build from None). The API address rebinds in place too — nothing
                    // downstream holds the HTTP listener (see api::serve_rebindable).
                    let (input_dev_changed, output_dev_changed,
                         bind_changed, name_changed, api_addr_changed) = {
                        let old_cfg = state.config.read().unwrap_or_else(|e| e.into_inner());
                        let in_changed  = old_cfg.audio.input_device  != new_cfg.audio.input_device;
                        let out_changed = old_cfg.audio.output_device != new_cfg.audio.output_device;
                        (
                            in_changed,
                            out_changed,
                            old_cfg.general.port              != new_cfg.general.port ||
                            old_cfg.general.network_interface != new_cfg.general.network_interface,
                            old_cfg.general.name              != new_cfg.general.name,
                            old_cfg.api.port != new_cfg.api.port || old_cfg.api.bind != new_cfg.api.bind,
                        )
                    };

                    // COMMIT the new config to shared state FIRST, so the hot-command
                    // handlers (esp. AddRemote restoring routing) see the fresh config.
                    *state.config.write().unwrap_or_else(|e| e.into_inner()) = new_cfg.clone();

                    // Rebind AFTER the commit — serve_rebindable reads the address back out
                    // of the config it has just been told changed.
                    if api_addr_changed {
                        state.api_rebind.notify_one();
                    }

                    // NOW emit the hot commands against the committed config.
                    if let Some(ref tx) = state.hot_tx {
                        for a in actions {
                            match a {
                                RemoteAction::Remove(n)  => { let _ = tx.try_send(HotCommand::RemoveRemote(n)); }
                                RemoteAction::Disable(n) => { let _ = tx.try_send(HotCommand::DisableRemote(n)); }
                                RemoteAction::Add(r)     => { let _ = tx.try_send(HotCommand::AddRemote(r)); }
                            }
                        }
                    }

                    // Persist synchronously: a settings save is infrequent, it must survive a
                    // crash or quit straight after, and a hot device change relies on the
                    // choice being persisted here. flush_now snapshots the just-committed
                    // cfg_arc.
                    let saved_ok = if let Some(ref s) = state.saver {
                        s.flush_now().is_ok()
                    } else {
                        new_cfg.save(&state.config_path.clone()).is_ok()
                    };
                    if saved_ok {
                        let routing_snap = build_routing_snapshot(&new_cfg);

                        let msg = json!({"type":"config_saved","ok":true}).to_string();
                        let _ = sender.send(Message::Text(msg)).await;
                        let _ = state.event_tx.send(routing_snap);

                        {
                            // EVERYTHING applies hot; nothing here restarts the daemon. The
                            // config was already saved above, so the choice persists across
                            // restarts; here we just tell the running system to pick it up.
                            if let Some(ref tx) = state.hot_tx {
                                // Port and interface are the audio socket's bind address, and
                                // it moves in place — the receive thread reloads the socket on
                                // its next iteration and peers re-poke from the new address.
                                // Peers, decoders and jitter buffers are untouched: the
                                // receive path re-arms only on a genuine drain
                                // (CASCADE_AUDIO_RECEIVE_SPEC §4.2), so there is nothing here
                                // to tear down.
                                if bind_changed {
                                    let _ = tx.try_send(HotCommand::SetBindAddress {
                                        iface: new_cfg.general.network_interface.clone(),
                                        port:  new_cfg.general.port,
                                    });
                                }
                                // A RENAME IS AN IDENTITY CHANGE, not a label. Peers match on
                                // MD5(UPPER(TRIM(name)) + UPPER(TRIM(password)))
                                // (CASCADE_WIRE_PROTOCOL_SPEC §4), and each peer task derives
                                // that token once when it is built. `AddRemote` shuts down an
                                // existing task for a peer and rebuilds it, so re-adding every
                                // enabled remote re-derives every token against the new name.
                                //
                                // The peers then re-pair on their own: identity travels in the
                                // ordinary 1–2 s ping and there is no connect handshake, so the
                                // very next ping is answered once it matches (§7.2).
                                //
                                // BOTH SIDES MUST BE RENAMED. The far end checks our identity
                                // against the name IT has configured for us, and a mismatch is
                                // silent — no rejection, just unanswered pings until its 20 s
                                // timeout (§4). Nothing on the wire negotiates this.
                                //
                                // Disabled remotes are skipped: adding one would start a task
                                // for a peer the user has switched off.
                                if name_changed {
                                    for r in new_cfg.remotes.iter().filter(|r| r.enabled) {
                                        let _ = tx.try_send(HotCommand::AddRemote(r.clone()));
                                    }
                                    tracing::info!("Instance renamed to '{}'",
                                                   new_cfg.general.name);
                                }
                                if input_dev_changed {
                                    let _ = tx.try_send(HotCommand::SetInputDevice(
                                        new_cfg.audio.input_device.clone()));
                                }
                                if output_dev_changed {
                                    let _ = tx.try_send(HotCommand::SetOutputDevice(
                                        new_cfg.audio.output_device.clone()));
                                }
                            }
                        }
                    } else {
                        // The settings are live and applied; only the file write failed — and
                        // the saver has already logged why. Say so, rather than leaving the
                        // page waiting on a save that will never be confirmed.
                        let msg = json!({"type":"config_saved","ok":false}).to_string();
                        let _ = sender.send(Message::Text(msg)).await;
                    }
                }
            }
        }

        _ => {}
    }
}

fn build_state_msg(state: &AppState) -> String {
    let on_air = state.on_air.load(Ordering::Relaxed);
    let peers: Vec<Value> = state.peer_stats.read().unwrap_or_else(|e| e.into_inner()).iter().map(|(name, p)| {
        // Include any current DNS error + sample-rate-mismatch flag so a newly-connected
        // client shows the banner(s) immediately, without waiting for the next peer_error
        // WS re-fire.
        json!({"name": name, "state": p.state, "dns_error": p.dns_error,
               "sr_mismatch": p.sr_mismatch})
    }).collect();
    let warnings: Vec<String> = state.startup_warnings
        .lock().unwrap_or_else(|e| e.into_inner()).clone();
    json!({"type":"state","on_air":on_air,"peers":peers,"startup_warnings":warnings}).to_string()
}

/// Configured outgoing/incoming crosspoint counts for one remote — what the routing is set up
/// to carry. TX = signal send-matrix routes PLUS tone crosspoints routed to this remote (a tone
/// crosspoint IS an outgoing route — signal vs tone is just the source). `tone_slots` is this
/// peer's tone map (engine tone_dests entry): a slot is an active tone route when its value is
/// non-zero. Signal and tone are mutually exclusive per slot, so there is no double count.
/// RX = receive-matrix routes. Counting tone here keeps the per-remote display consistent with
/// the global live count (which also counts signal + tone): per-remote 62 + 62 then sums to the
/// global total rather than mismatching it.
fn configured_crosspoints(r: &crate::config::RemoteConfig, tone_slots: Option<&Vec<u8>>) -> (usize, usize) {
    let signal_tx = r.send_matrix.as_ref()
        .map(|m| crate::audio::routing::RoutingTable::parse_matrix(m).len()).unwrap_or(0);
    let tone_tx = tone_slots.map(|v| v.iter().filter(|&&t| t != 0).count()).unwrap_or(0);
    let rx = r.receive_matrix.as_ref()
        .map(|m| crate::audio::routing::RoutingTable::parse_matrix(m).len()).unwrap_or(0);
    (signal_tx + tone_tx, rx)
}

pub fn build_routing_snapshot(cfg: &Config) -> String {
    let remotes: Vec<Value> = cfg.remotes.iter().map(|r| json!({
        "name":           r.name,
        "send_matrix":    r.send_matrix,
        "receive_matrix": r.receive_matrix,
    })).collect();
    json!({"type":"routing","remotes":remotes}).to_string()
}

// ── REST handlers (kept for compatibility + curl testing) ───────────────────

async fn get_ui() -> Html<&'static str> { Html(WEB_HTML) }

async fn get_status(State(s): State<AppState>) -> Json<Value> {
    let cfg   = s.config.read().unwrap_or_else(|e| e.into_inner());
    let peers = s.peer_stats.read().unwrap_or_else(|e| e.into_inner());
    // Receive-buffer health per peer (mean depth/target across active channels),
    // refreshed by the main loop's stats tick.
    let bufs: HashMap<String, (f32, f32, f32, f32, bool)> = s.buffer_view.read().unwrap_or_else(|e| e.into_inner()).clone();
    // Per-peer live incoming frame size (samples) → ms, for the UI buffer-floor.
    let frames: HashMap<String, usize> = s.frame_view.read().unwrap_or_else(|e| e.into_inner()).clone();
    // Per-peer tone routing (engine tone_dests) — tone crosspoints count toward the configured
    // TX, since a tone route is still an outgoing route.
    let tone_by_peer: HashMap<String, Vec<u8>> = s.tone_dests.get()
        .map(|t| t.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .unwrap_or_default();
    let peer_list: Vec<Value> = peers.iter().map(|(name, p)| {
        // Configured crosspoint counts for THIS remote — persistent. TX = signal matrix routes
        // + tone crosspoints routed to this remote (a tone route is still a route). Consistent
        // with the global live count so per-remote sums match the instance total. (Request 2)
        let (cfg_tx, cfg_rx) = cfg.remotes.iter().find(|r| &r.name == name)
            .map(|r| configured_crosspoints(r, tone_by_peer.get(name)))
            .unwrap_or((0, 0));
        let mut v = json!({
            "name": name, "state": p.state, "status": p.status,
            "latency_ms": p.latency_ms,
            "tx_mbps": p.tx_bps as f64 / 1_000_000.0,
            "rx_mbps": p.rx_bps as f64 / 1_000_000.0,
            "loss_count": p.loss_count,
            "pct_lost": p.pct_lost, "jitter_ms": p.jitter_ms,
            "host_match": p.host_match,
            "sr_mismatch": p.sr_mismatch,
            "dns_error": p.dns_error,
            "enc_active": p.enc_active,
            "cfg_tx": cfg_tx, "cfg_rx": cfg_rx,
        });
        if let Some((avg, lo, hi, t, hold)) = bufs.get(name.as_str()) {
            // `buf_ms` is the interval MEAN of the across-channel mean — the figure the bar
            // is drawn and coloured from, averaged on both axes so the depth sawtooth and
            // one-off dips cancel instead of driving the display. `buf_min_ms`/`buf_max_ms`
            // are the spread that mean moved through, for the tooltip only: they say how
            // much it actually moved without letting either extreme define the reading.
            v["buf_ms"] = json!(avg);
            v["buf_min_ms"] = json!(lo); v["buf_max_ms"] = json!(hi);
            v["buf_target_ms"] = json!(t);
            // Prebuffer-hold tap point (CASCADE_AUDIO_RECEIVE_SPEC §10): true while
            // at least one of this peer's channels is silently refilling to target.
            v["buf_hold"] = json!(hold);
        }
        if let Some(&samples) = frames.get(name.as_str()) {
            // 48kHz: samples → ms. 120/240/480/960 = 2.5/5/10/20.
            v["in_frame_ms"] = json!(samples as f64 / 48.0);
        }
        v
    }).collect();
    // CONFIGURED stream totals (instance-level, summed across remotes from the matrices):
    // TX = outgoing routes, RX = incoming routes. parse_matrix drops value==0 cells. A source
    // routed to N remotes counts N times. These are "what is set up", NOT live — they do not
    // drop on disconnect/disable and do not include tone. The LIVE counts below are the real
    // active state shown in the Instance header.
    let (tx_streams, rx_streams) = {
        let mut tx = 0usize;
        let mut rx = 0usize;
        for r in &cfg.remotes {
            if let Some(ref m) = r.send_matrix {
                tx += crate::audio::routing::RoutingTable::parse_matrix(m).len();
            }
            if let Some(ref m) = r.receive_matrix {
                rx += crate::audio::routing::RoutingTable::parse_matrix(m).len();
            }
        }
        (tx, rx)
    };
    // LIVE stream totals for the Instance header — the REAL active state (Request 1), engine
    // truth (NOT config):
    //   live_tx = outgoing streams to CONNECTED peers (signal routes + active tone), summed by
    //             the stats tick via CaptureEngine::live_send_streams into active_send. A
    //             disconnected remote contributes 0 (its routing persists but it isn't sending).
    //   live_rx = received channels actually ROUTED AND DECODING (engine dec_slots.len() via
    //             active_recv) — NOT channels merely arriving.
    let live_tx_streams = s.active_send.load(Ordering::Relaxed);
    let live_rx_streams = s.active_recv.load(Ordering::Relaxed);
    Json(json!({"name":cfg.general.name,"port":cfg.general.port,
        "token":crate::net::protocol::derive_token(&cfg.general.name, "").to_string(),"peers":peer_list,
        "out_channels":s.out_channels.get().map(|a| a.load(Ordering::Relaxed)).unwrap_or(0),"in_channels":s.in_channels.get().map(|a| a.load(Ordering::Relaxed)).unwrap_or(0),
        "out_sample_rate":s.out_sample_rate.get().map(|a| a.load(Ordering::Relaxed)).unwrap_or(0),
        "in_sample_rate":s.in_sample_rate.get().map(|a| a.load(Ordering::Relaxed)).unwrap_or(0),
        "live_in_device":  s.live_in_device.get().map(|d| d.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap_or_default(),
        "live_out_device": s.live_out_device.get().map(|d| d.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap_or_default(),
        // The shortest callback period both assigned devices can run, in frames
        // (`audio::agreed_period`). The web UI offers no receive buffer or outgoing frame
        // size below what it allows. 0 when not learned — always on macOS.
        "shortest_period": crate::audio::agreed_period(),
        // ACTUAL exclusive-output state (hog mode held), not merely the config request — a
        // claim can fail if another app owns the device, and the UI must reflect reality.
        "exclusive_output_active": crate::audio::hog_mode::is_active(),
        // True where exclusivity comes from opening the device rather than from a claim
        // the user makes (ALSA). The UI renders the control as a state, not a switch.
        "exclusive_output_inherent": crate::audio::hog_mode::is_inherent(),
        "active_streams": s.active_streams.get()
            .map(|a| a.load(Ordering::Relaxed)).unwrap_or(0),
        "tx_streams": tx_streams,
        "rx_streams": rx_streams,
        "live_tx_streams": live_tx_streams,
        "live_rx_streams": live_rx_streams,
        "on_air":s.on_air.load(Ordering::Relaxed),
        "port_conflict":s.port_conflict.load(Ordering::Relaxed)}))
}

async fn get_channels(State(s): State<AppState>) -> Json<Value> {
    let incoming = s.incoming_channels.read().unwrap_or_else(|e| e.into_inner()).clone();
    let outgoing = s.outgoing_channels.read().unwrap_or_else(|e| e.into_inner()).clone();
    let tone_by_peer = s.tone_dests.get()
        .map(|t| t.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .unwrap_or_default();
    Json(json!({ "incoming_by_peer": incoming, "outgoing": outgoing,
                 "tone_by_peer": tone_by_peer }))
}

async fn post_phase(
    State(s): State<AppState>,
    Path(peer): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let enabled = body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    if let Some(ref tx) = s.phase_tx { let _ = tx.try_send((peer, enabled)); }
    StatusCode::NO_CONTENT
}

async fn get_config(State(s): State<AppState>) -> Json<Value> {
    let cfg = s.config.read().unwrap_or_else(|e| e.into_inner()).clone();
    Json(masked_config_json(&cfg))
}

/// The config as the API shows it. Remote passwords never leave the daemon once saved:
/// each remote carries `password_set` in place of its password, so the UI can show that one
/// exists without holding it.
fn masked_config_json(cfg: &Config) -> Value {
    let mut v = serde_json::to_value(cfg).unwrap_or(Value::Null);
    if let Some(remotes) = v.get_mut("remotes").and_then(|r| r.as_array_mut()) {
        for (r, rc) in remotes.iter_mut().zip(&cfg.remotes) {
            if let Some(obj) = r.as_object_mut() {
                obj.remove("password");
                obj.insert("password_set".into(), Value::Bool(!rc.password.is_empty()));
            }
        }
    }
    v
}

/// Put back the per-remote fields a save payload does not get to set.
///
/// Routing matrices are owned by the routing pages (`routing_tx`/`routing_rx`), never by a
/// settings save: each remote takes the stored matrices, whatever the payload carried, so an
/// offline routing edit cannot be clobbered by a later settings save.
///
/// Passwords are write-only: the API never returns one (`masked_config_json`), so a settings
/// save carries a remote's `password` only when it was typed — an empty string sent
/// deliberately clears it — and a remote without the key keeps the stored password.
///
/// Both follow a rename. The `orig_name` the UI attaches — the name the remote was loaded
/// under — names the stored remote, so renaming one keeps its routing and its password; a
/// client that sends no `orig_name` is matched by name as before. A remote with no stored
/// match is new and keeps whatever it arrived with.
fn restore_server_owned_fields(new_cfg: &mut Config, sent: &Value, old_cfg: &Config) {
    let sent_remotes = sent.get("remotes").and_then(|r| r.as_array());
    for (i, nr) in new_cfg.remotes.iter_mut().enumerate() {
        let entry = sent_remotes.and_then(|a| a.get(i));
        let key = entry.and_then(|e| e.get("orig_name")).and_then(|n| n.as_str())
            .unwrap_or(&nr.name).to_string();
        let Some(or) = old_cfg.remotes.iter().find(|o| o.name == key) else { continue };
        nr.send_matrix    = or.send_matrix.clone();
        nr.receive_matrix = or.receive_matrix.clone();
        if entry.map_or(false, |e| e.get("password").is_some()) { continue; }
        nr.password = or.password.clone();
    }
}

#[cfg(test)]
mod saved_config_tests {
    use super::{masked_config_json, restore_server_owned_fields};
    use crate::config::Config;
    use serde_json::json;

    fn cfg(remotes: serde_json::Value) -> Config {
        serde_json::from_value(json!({ "remotes": remotes })).unwrap()
    }

    #[test]
    fn the_api_view_carries_no_password() {
        let c = cfg(json!([{"name":"A","host":"h","port":1,"password":"secret"},
                           {"name":"B","host":"h","port":1}]));
        let v = masked_config_json(&c);
        assert!(v["remotes"][0].get("password").is_none());
        assert_eq!(v["remotes"][0]["password_set"], json!(true));
        assert_eq!(v["remotes"][1]["password_set"], json!(false));
        assert!(!v.to_string().contains("secret"));
    }

    #[test]
    fn an_untouched_password_is_kept() {
        let old = cfg(json!([{"name":"A","host":"h","port":1,"password":"secret"}]));
        let sent = json!({"remotes":[{"name":"A","host":"h2","port":1,"orig_name":"A"}]});
        let mut new: Config = serde_json::from_value(sent.clone()).unwrap();
        restore_server_owned_fields(&mut new, &sent, &old);
        assert_eq!(new.remotes[0].password, "secret");
    }

    #[test]
    fn a_typed_password_replaces_it_and_an_empty_one_clears_it() {
        let old = cfg(json!([{"name":"A","host":"h","port":1,"password":"secret"},
                             {"name":"B","host":"h","port":1,"password":"other"}]));
        let sent = json!({"remotes":[
            {"name":"A","host":"h","port":1,"orig_name":"A","password":"new"},
            {"name":"B","host":"h","port":1,"orig_name":"B","password":""}]});
        let mut new: Config = serde_json::from_value(sent.clone()).unwrap();
        restore_server_owned_fields(&mut new, &sent, &old);
        assert_eq!(new.remotes[0].password, "new");
        assert_eq!(new.remotes[1].password, "");
    }

    #[test]
    fn a_renamed_remote_keeps_its_password_and_its_routing() {
        let old = cfg(json!([{"name":"A","host":"h","port":1,"password":"secret",
                              "send_matrix":"0:0:1","receive_matrix":"1:1:1"}]));
        let sent = json!({"remotes":[{"name":"Renamed","host":"h","port":1,"orig_name":"A"}]});
        let mut new: Config = serde_json::from_value(sent.clone()).unwrap();
        restore_server_owned_fields(&mut new, &sent, &old);
        assert_eq!(new.remotes[0].password, "secret");
        assert_eq!(new.remotes[0].send_matrix.as_deref(), Some("0:0:1"));
        assert_eq!(new.remotes[0].receive_matrix.as_deref(), Some("1:1:1"));
    }

    /// A settings save never carries routing, and a stale copy in the payload is ignored.
    #[test]
    fn stored_routing_wins_over_the_payload() {
        let old = cfg(json!([{"name":"A","host":"h","port":1,
                              "send_matrix":"0:0:1","receive_matrix":"1:1:1"}]));
        let sent = json!({"remotes":[{"name":"A","host":"h","port":1,"orig_name":"A",
                                      "send_matrix":"9:9:1"}]});
        let mut new: Config = serde_json::from_value(sent.clone()).unwrap();
        restore_server_owned_fields(&mut new, &sent, &old);
        assert_eq!(new.remotes[0].send_matrix.as_deref(), Some("0:0:1"));
        assert_eq!(new.remotes[0].receive_matrix.as_deref(), Some("1:1:1"));
    }

    /// A remote that is genuinely new keeps what it arrived with — it has nothing stored.
    #[test]
    fn a_new_remote_is_left_alone() {
        let old = cfg(json!([{"name":"A","host":"h","port":1,"password":"secret"}]));
        let sent = json!({"remotes":[{"name":"A","host":"h","port":1,"orig_name":"A"},
                                     {"name":"B","host":"h","port":2,"password":"typed"}]});
        let mut new: Config = serde_json::from_value(sent.clone()).unwrap();
        restore_server_owned_fields(&mut new, &sent, &old);
        assert_eq!(new.remotes[1].password, "typed");
        assert_eq!(new.remotes[1].send_matrix, None);
    }
}

// ── Device inventory: single source of truth ────────────────────────────────
// The daemon holds the authoritative device list in memory here. A background task
// (spawn_background_tasks) enumerates devices off-thread on an interval, and when the
// set CHANGES it (a) updates this store and (b) pushes a `devices` WS event to all
// browsers. Both the push and the `/api/devices` endpoint are VIEWS of this one value —
// only the background task ever enumerates, so the two can never disagree.
//
// `invalidate_device_cache()` does not clear anything destructively; it just nudges the
// enumerator to re-check promptly (e.g. right after a device loss/recovery) so the UI
// reflects reality without waiting for the next interval.
static DEVICE_INVENTORY: std::sync::OnceLock<std::sync::Mutex<DeviceInventory>> =
    std::sync::OnceLock::new();

#[derive(Default, Clone)]
struct DeviceInventory {
    inputs: Vec<String>,
    outputs: Vec<String>,
    /// Set true to ask the enumerator to re-check on its next (short) tick.
    dirty: bool,
    /// Whether at least one enumeration has completed (so /api/devices can tell
    /// "empty because nothing connected" from "not yet enumerated").
    populated: bool,
}

fn device_inventory() -> &'static std::sync::Mutex<DeviceInventory> {
    DEVICE_INVENTORY.get_or_init(|| std::sync::Mutex::new(DeviceInventory::default()))
}

/// Nudge the background enumerator to re-check promptly. Called by the device manager on
/// loss/recovery so the dropdowns update without waiting for the periodic tick.
pub fn invalidate_device_cache() {
    device_inventory().lock().unwrap_or_else(|e| e.into_inner()).dirty = true;
}

/// Current inventory as the JSON the UI consumes ({inputs, outputs}). A pure read of the
/// in-memory store — no enumeration here.
fn inventory_json() -> Value {
    let inv = device_inventory().lock().unwrap_or_else(|e| e.into_inner());
    json!({ "inputs": inv.inputs.clone(), "outputs": inv.outputs.clone() })
}

async fn get_devices() -> Json<Value> {
    // Pure read of the in-memory inventory. If the background enumerator hasn't run yet
    // (very early startup), enumerate once inline (off the executor) to seed it so the
    // first page load isn't empty.
    let populated = device_inventory().lock().unwrap_or_else(|e| e.into_inner()).populated;
    if !populated {
        let (inputs, outputs) = tokio::task::spawn_blocking(|| {
            let inputs  = crate::audio::selectable_input_devices();
            let outputs = crate::audio::selectable_output_devices();
            (inputs, outputs)
        }).await.unwrap_or_default();
        let mut inv = device_inventory().lock().unwrap_or_else(|e| e.into_inner());
        // Only seed if still unpopulated (the background task may have won the race).
        if !inv.populated {
            inv.inputs = inputs;
            inv.outputs = outputs;
            inv.populated = true;
        }
    }
    Json(inventory_json())
}

async fn get_interfaces() -> Json<Value> {
    let ifaces: Vec<serde_json::Value> = tokio::task::spawn_blocking(|| {
        crate::net::iface::list_interfaces()
            .into_iter().map(|i| json!({
                "value": i.name,
                "label": format!("{} ({})", i.name, i.addr)
            })).collect()
    }).await.unwrap_or_default();
    Json(json!({ "interfaces": ifaces, "default": "any" }))
}

async fn post_bitrate(State(s): State<AppState>, Json(body): Json<Value>) -> impl IntoResponse {
    let kbps = body.get("kbps").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if let Some(ref tx) = s.hot_tx {
        let _ = tx.try_send(HotCommand::SetBitrate(kbps));
        (StatusCode::OK, Json(json!({"ok":true,"kbps":kbps}))).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not available").into_response()
    }
}

async fn post_routing_send(
    State(s): State<AppState>, Path(peer): Path<String>, Json(body): Json<Value>,
) -> impl IntoResponse {
    let matrix = match body.get("matrix").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return (StatusCode::BAD_REQUEST, "missing 'matrix'").into_response(),
    };
    if let Some(ref tx) = s.hot_tx {
        let _ = tx.try_send(HotCommand::SetSendRouting { peer, matrix });
        (StatusCode::OK, Json(json!({"ok":true}))).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not available").into_response()
    }
}

async fn post_routing_receive(
    State(s): State<AppState>, Path(peer): Path<String>, Json(body): Json<Value>,
) -> impl IntoResponse {
    let matrix = match body.get("matrix").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return (StatusCode::BAD_REQUEST, "missing 'matrix'").into_response(),
    };
    if let Some(ref tx) = s.hot_tx {
        let _ = tx.try_send(HotCommand::SetReceiveRouting { peer, matrix });
        (StatusCode::OK, Json(json!({"ok":true}))).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not available").into_response()
    }
}

/// The web UI's meter source. A poll names what it is showing — `?peer=NAME` on the
/// receive page, nothing elsewhere — and that keeps the metering for it running
/// (crate::meter::Watch). It returns the latest published snapshot and resets nothing, so
/// any number of viewers read identical values.
async fn get_peaks(State(s): State<AppState>,
                   axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>)
                   -> Json<Value> {
    s.meters.watch.touch(q.get("peer").map(|p| p.as_str()).filter(|p| !p.is_empty()));
    Json((*s.meters.snapshot()).clone())
}

/// Publish meter snapshots while anyone is watching (crate::meter). Every SNAPSHOT_PERIOD it
/// drains each peak accumulator — a running max the audio paths write — into one snapshot,
/// so the period is the peak-hold window and no transient between snapshots is missed.
/// This task is the ONLY reader that resets the accumulators.
///
/// Idle while nothing is watched: no reads, no resets. The first cycle after idle drains
/// without publishing, so maxima left from before the idle period never reach a viewer.
pub fn spawn_meter_publisher(s: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::meter::SNAPSHOT_PERIOD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut active = false;
        loop {
            tick.tick().await;
            if !s.meters.watch.any_live() {
                active = false;
                continue;
            }
            let snap = drain_peaks(&s);
            if active { s.meters.publish(snap); }
            active = true;
        }
    });
}

/// Drain every peak accumulator into one snapshot (§9.3). Per incoming channel, exactly one
/// source by the routing state that gates decode dispatch: routed → the post-buffer peak,
/// unrouted → the pre-decode peak. Not a max-merge — the two never compete. Every
/// pre-decode cell is reset whichever branch was taken.
fn drain_peaks(s: &AppState) -> Value {
    let take = |a: &std::sync::atomic::AtomicU32| f32::from_bits(a.swap(0, Ordering::Relaxed));
    let in_peaks: Vec<f32> = s.input_peaks.get()
        .map(|p| p.iter().map(take).collect()).unwrap_or_default();
    let out_peaks: Vec<f32> = s.output_peaks.get()
        .map(|p| p.iter().map(take).collect()).unwrap_or_default();
    let tone_peaks: Vec<f32> = s.tone_peaks.get()
        .map(|p| p.iter().map(take).collect()).unwrap_or_default();
    // Post-buffer peaks, per remote, slot-indexed.
    let mut incoming: HashMap<String, Vec<f32>> = match s.incoming_peaks.get() {
        Some(m) => m.read().unwrap_or_else(|e| e.into_inner()).iter()
            .map(|(peer, arr)| (peer.clone(), arr.iter().map(take).collect())).collect(),
        None => HashMap::new(),
    };
    // Pre-decode peaks, drained unconditionally; used for the unrouted channels.
    let pre: HashMap<String, Vec<f32>> = s.meters.all_pre_cells().into_iter()
        .map(|(peer, cells)| (peer, cells.iter().map(take).collect())).collect();
    {
        let chans = s.incoming_channels.read().unwrap_or_else(|e| e.into_inner());
        for (peer, list) in chans.iter() {
            let entry = incoming.entry(peer.clone()).or_default();
            let pre_peer = pre.get(peer);
            for ci in list.iter() {
                let idx = ci.channel as usize;
                if entry.len() <= idx { entry.resize(idx + 1, 0.0); }
                if !ci.routed {
                    entry[idx] = pre_peer.and_then(|v| v.get(idx)).copied().unwrap_or(0.0);
                }
                // routed → keep the post-buffer value already in `entry[idx]`
            }
        }
    }
    json!({ "input": in_peaks, "output": out_peaks, "incoming": incoming, "tone": tone_peaks })
}
