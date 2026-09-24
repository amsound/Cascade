//! Audio output engine — a pull model.
//!
//! Threading model:
//!
//!   RECEIVE THREAD →  dispatch to the channel's SERIAL decode queue
//!                     Opus decode → SPSC ring write                     [no shared lock]
//!
//!   DEVICE CALLBACK → SPSC ring read → resample (phase lock only) → mix [see below]
//!
//!   No mutex is held across decode, and the ring between the two sides is lock-free SPSC:
//!
//!   - Each decode runs under its own channel's slot mutex, which only that channel's
//!     serial queue takes — no contention.
//!   - New channels are announced via a tiny Mutex<Vec<...>> mailbox that render drains
//!     with try_lock (non-blocking). The receive thread holds it for microseconds (rare).
//!   - SPSC producers live on the decode side, pushed without any shared lock.
//!
//!   A mutex held across decode would block the device callback behind Opus decode;
//!   nothing here does that.
//!
//!   ONE lock remains in the render path, and it is deliberate: render_groups
//!   (Arc<parking_lot::Mutex<HashMap<String, PeerGroup>>>) is locked for the duration
//!   of the render. The other holders keep it briefly: set_recv_routing (mask updates, and
//!   dropping channels that lost their route), retarget_peer_buffer (a map lookup),
//!   remove_peer (dropping the peer's group), and the output stop and device-change paths
//!   (clearing the groups while the stream is stopped). channel_depth_report only ever
//!   try_locks. None holds it across decode.

use std::collections::HashMap;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::Instant;


use std::sync::atomic::{AtomicBool, Ordering};
use anyhow::Result;
use tracing::{info, warn, debug, trace};

use super::spsc;
use super::channel_sync::{PeerGroup, Limiter, GainShare, ring_capacity_floored, SwapInbox};
use super::routing::PeerReceiveRouting;
use super::pool::{ChannelDecodeSlot, DecodeSlot, StatAccumulator, PeerFrameSize};
use parking_lot::Mutex as ParkingMutex;
use crate::audio::{Device, Stream};
use super::scheduler::{make_scheduler, ChannelQueue};

/// A resolved per-(peer,channel) decode handle: the four `Arc`s the hot receive path
/// needs to dispatch a decode. Resolving it (`resolve_channel`) takes the engine-wide
/// map locks ONCE, when the channel is first seen; thereafter the recv handler caches
/// the handle and dispatches via `dispatch_decode` with ZERO engine-map locks. Each
/// incoming channel owns its serial decode queue and the dispatch goes straight to it —
/// the socket-readable handler does no per-packet lookup.

#[derive(Clone)]
pub struct ChannelHandle {
    slot:  DecodeSlot,                 // Arc<ParkingMutex<ChannelDecodeSlot>>
    /// Lifted out of the slot at resolve time so the receive hot path can read it with no
    /// lock. See ChannelDecodeSlot::meter_live for what it gates.
    meter_live: Arc<std::sync::atomic::AtomicBool>,
    // No stats accumulator here: loss and jitter are accumulated at packet ARRIVAL
    // (CASCADE_SESSION_STATS_SPEC §2.3/§2.4), not on the decode side, so this handle —
    // which only exists for ROUTED channels — is the wrong place for them.
    queue: Arc<dyn ChannelQueue>,
    // `sync` lives inside the slot (s.sync); not needed separately on the hot path.
}

impl ChannelHandle {
    /// True once this channel's post-buffer meter has produced at least one sample, i.e.
    /// once the §9.3 read-time switch may safely select it. Lock-free.
    #[inline]
    pub fn meter_live(&self) -> bool {
        self.meter_live.load(std::sync::atomic::Ordering::Relaxed)
    }
}

// ── New-channel mailbox ────────────────────────────────────────────────────────
//
// Written by the network task (rare: once per new channel), drained by the render
// closure at the top of each callback using try_lock (returns immediately if busy).
// The Rust equivalent of an atomic nil→pointer store for new channels.

struct NewChannelMsg {
    peer:      String,
    channel:   u8,
    rx:        spsc::Consumer,
    out_mask:  u128,
    sync:      Arc<AtomicBool>,
    buffer_ms: u32,
    /// Expected incoming frame size (samples) at creation, for the initial 2×frame target
    /// floor. A later frame-size change re-floors via the swap path.
    frame_samples: usize,
    swap_inbox: SwapInbox,
    cm_dir_shared: super::pool::DriftDir,
    /// §6.6's per-channel adjusting flag. Shared for the same reason `cm_dir_shared`
    /// is: the render thread computes and consumes it, but the decode thread has to be
    /// able to clear it when it resets the averaging window underneath it.
    skew_flag: super::pool::DriftDir,
    prebuffer_hold: super::pool::PrebufferHold,
    boxcar_reset: Arc<AtomicBool>,
    skew_ref: Arc<std::sync::atomic::AtomicI64>,
    /// This peer's shared per-incoming peak array (slot-indexed). The PeerGroup
    /// stores levels into it during render; the API reads it for the RX meters.
    peaks:     Arc<Vec<std::sync::atomic::AtomicU32>>,
}

// ── AudioEngine ───────────────────────────────────────────────────────────────

pub struct AudioEngine {
    /// New channel mailbox. Network task pushes; render closure drains with try_lock.
    /// Lock is held for microseconds (Vec push / drain), never during decode or render.
    new_channels: Arc<Mutex<Vec<NewChannelMsg>>>,

    /// Per-channel decode slots: decoder + ring writer.
    /// Network task only — Mutex is never contended on hot path.
    dec_slots: Mutex<HashMap<(String, u8), DecodeSlot>>,
    /// Bumped whenever dec_slots is structurally torn down (clear/retain/remove). The recv
    /// hot path reads this (one relaxed load) and drops its per-(peer,channel) ChannelHandle
    /// cache when it changes, so a cached handle can never dispatch into an orphaned slot
    /// after a routing/device change. Cheap and lock-free on the hot path.
    slots_epoch: std::sync::atomic::AtomicU64,

    /// Per-peer stats accumulators. Pool workers write; main loop drains.
    /// Wrapped in Arc so peer tasks can hold a direct handle (via stat_acc_handle)
    /// without going through Arc<AudioEngine>.
    stat_acc: Arc<Mutex<HashMap<String, Arc<StatAccumulator>>>>,

    /// Memo cache of (peer, channel) → decode queue, so the hot path resolves a handle
    /// without hashing into the scheduler each time. The queues are NOT owned here: they
    /// belong to a process-wide pool of 128 serial queues — GCD queues targeting the
    /// global concurrent queue on macOS, queues over the shared worker pool on Linux —
    /// and are handed out by channel index, so two peers using the same channel number
    /// share one queue and removing an entry frees no queue or thread. A queue owns no
    /// thread on either platform; the pool's thread count is independent of how many
    /// queues exist.
    dec_queues: std::sync::Mutex<HashMap<(String, u8), Arc<dyn ChannelQueue>>>,

    /// Per-peer sync flags shared between API handler and render closure via AtomicBool.
    /// Mutex is uncontended (API calls are rare and never overlap with themselves).
    sync_flags: Mutex<HashMap<String, Arc<AtomicBool>>>,

    pub num_out_ch: u8,

    /// Live output-channel count, updated by the stream builder on every build (so it
    /// reflects the current device after a hot switch). The API reads it to size the
    /// routing UI's output channels. `num_out_ch` above is the startup snapshot.
    pub live_out_ch: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Confirmed sample rate the output stream is running at (Hz). Written by the stream
    /// builder after a successful build; 0 before the first build completes.
    pub live_out_rate: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// EFFECTIVE output callback period in frames — what the backend actually granted,
    /// not what we requested. Written by the stream builder after a successful build;
    /// 0 before the first build completes and after a device is lost.
    ///
    /// Published because the two values that must be compared to detect a fatal period
    /// live in different scopes: the granted period is only known inside the stream-builder
    /// closure (a `Box<dyn Fn>` with no `self`), while the setpoint lives behind
    /// `dec_slots`/`peer_buffer_ms` on the engine. See `min_active_setpoint`.
    pub live_out_period: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Whether the output device honours requested periods — see `audio::PeriodTracker`.
    pub out_period: std::sync::Arc<super::PeriodTracker>,
    /// Actual device name the output stream is running on — empty whenever no output stream
    /// is running (none selected, or the device was lost), which is what the rebuild paths
    /// test to leave a stopped output stopped.
    pub live_out_device: std::sync::Arc<std::sync::Mutex<String>>,
    /// Set by the device-fault callback when the output device is lost (unplugged).
    /// Consumed by the output device-manager task, which asks the main loop to stop the
    /// stream and then watches for the device to return.
    pub output_device_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set when the backend reports the output stream must be rebuilt — on macOS this is
    /// raised for ANY device sample-rate change.
    /// The stream is dead at that point but the DEVICE is fine, so this is distinct from
    /// `output_device_lost`: the remedy is a rebuild, not a teardown.
    pub output_stream_invalid: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// True while we are rebuilding this stream. Setting the device's rate back to 48 kHz
    /// is itself a rate change, so CoreAudio reports the stream invalid again — an echo of
    /// our own correction. The callback checks this to log the echo quietly instead of
    /// raising a second warning about a problem that is already being fixed.
    pub output_rebuilding: std::sync::Arc<std::sync::atomic::AtomicBool>,

    /// Receive buffer latency in ms — global default; drives ring size and warmup threshold.
    pub buffer_ms: u32,

    /// Per-peer shared frame-size tracker for log deduplication.
    /// Key = peer name. Value shared across all channels of that peer.
    peer_frame_sizes: Mutex<HashMap<String, PeerFrameSize>>,

    /// Per-peer receive routing: slot → local output channels. Set from the saved receive
    /// matrix at startup and on remote enable, and live from the web UI.
    recv_routing: Mutex<HashMap<String, PeerReceiveRouting>>,

    /// Per-peer buffer overrides (ms). Set from RemoteConfig.buffer_ms at startup.
    /// Clamped 20–10000ms. If absent for a peer, `buffer_ms` is used.
    peer_buffer_ms: Mutex<HashMap<String, u32>>,

    /// Per-output-channel peak levels (bit-cast f32, post-limiter).
    /// Updated in the output device callback. Read by the API's /api/peaks.
    pub output_peaks: Arc<Vec<std::sync::atomic::AtomicU32>>,

    /// Per-incoming peak levels, keyed by peer name → slot-indexed array.
    /// Populated by receive() when a peer's first channel arrives; each peer's
    /// array is shared with its PeerGroup, which writes levels during render.
    /// Read (lock-free on the atomics, brief read-lock on the map) by the API.
    pub incoming_peaks: Arc<std::sync::RwLock<HashMap<String, Arc<Vec<std::sync::atomic::AtomicU32>>>>>,
    /// Per-peer rough buffer readout (depth, target) in samples, published lock-free by
    /// each PeerGroup at the end of render(). The stats tick reads THIS instead of taking
    /// render_groups.lock() (held by the RT render callback), so the buffer display can't
    /// stall render or the select loop. Populated when a group is created.
    pub buffer_snaps: Arc<std::sync::RwLock<HashMap<String,
        Arc<super::channel_sync::BufferSnap>>>>,
    /// ON AIR state — false means all tone routing silently fails.
    pub on_air: Arc<std::sync::atomic::AtomicBool>,

    /// True once the output callback has run. receive() drops audio until then
    /// (see the gate comment in start()) so warm-up depth is deterministic.
    render_alive: Arc<std::sync::atomic::AtomicBool>,
    render_groups: Arc<parking_lot::Mutex<HashMap<String, PeerGroup>>>,
}

/// Output-stream lifecycle state, owned by the MAIN thread (never shared, never in an
/// Arc). Holds the four interior-mutable / non-Sync fields kept off `AudioEngine`:
/// the live output stream, its re-invokable builder, the current
/// output device, and the current send-frame size. Splitting these out is what makes
/// `AudioEngine` itself `Sync`, so it can be shared (behind an Arc) to the receive
/// thread (`cascade-recv`), which calls `AudioEngine::receive` inline on a single
/// context. `rebuild_output`/`stop_output` are `AudioEngine` methods
/// that take `&mut OutputControl` for these fields and `&self` for the shared render
/// state; both are only ever called from the main select loop, so `OutputControl` never
/// needs to cross a thread.
pub struct OutputControl {
    /// Live output stream. `None` only transiently inside `rebuild_output`.
    _stream: RefCell<Option<Stream>>,
    /// Re-invokable builder for the output stream. Captures Arc clones of the shared
    /// render state (all Send+Sync); called by `rebuild_output` to recreate the
    /// stream with a new send-frame buffer (min(frame,480)) and/or a new output device.
    /// The render_groups (jitter buffers) being shared survive a rebuild untouched, so
    /// the receive side is seamless. Device is a per-call parameter (not captured).
    output_builder: Box<dyn Fn(&Device, usize) -> Result<Stream>>,
    /// The output device currently in use. Held so a live device switch can rebuild on
    /// it; a frame-size rebuild reuses it. A `Device` is cheap to clone.
    current_output_device: RefCell<Device>,
    /// The callback period last requested for the output stream, in samples (the backend
    /// is asked for min(this, 480)), never shorter than `audio::shortest_request` was when
    /// it was built. A device-only rebuild reuses it; a period rebuild replaces it.
    current_send_frame: std::cell::Cell<usize>,
}

impl OutputControl {
    /// The live render callback period (CoreAudio buffer size) — the shared min(incoming,
    /// outgoing) currently applied. Used by the reconciler to skip a no-op rebuild.
    pub fn current_frames(&self) -> usize { self.current_send_frame.get() }
}

/// Give a channel a fresh ring and setpoint for its current buffer setting, frame size and
/// period floor, with the prebuffer re-armed: a brief re-buffer gap, then resume.
///
/// Same geometry the frame-size path computes, against the channel's own frame size, so the
/// 2×frame and period floors both apply. Decoder, resampler and routing are untouched.
fn refill_at_current_geometry(s: &mut ChannelDecodeSlot) {
    let target = super::channel_sync::target_samples_floored(
        s.buffer_ms, s.expected_frame_samples, s.period_floor);
    let cap = ring_capacity_floored(s.buffer_ms, s.expected_frame_samples, s.period_floor);
    let (tx, rx) = super::spsc::channel(cap);
    s.producer = tx;
    s.setpoint = target;
    // The consumer picks the new ring up on its next render and re-arms its own prebuffer
    // hold; this side arms the shared flag so the two agree immediately rather than for the
    // one render in between.
    *s.swap_inbox.lock() = Some((rx, target));
    s.prebuffer_hold.store(true, std::sync::atomic::Ordering::Relaxed);
    s.last_pushed_ts = None;      // new ring (see ChannelDecodeSlot::last_pushed_ts)
    s.discontinuity  = false;
    s.cm.reset();
    s.cm_dir_shared.store(0, std::sync::atomic::Ordering::Relaxed);
    s.skew_flag.store(0, std::sync::atomic::Ordering::Relaxed);
}

impl AudioEngine {
    pub fn start(device: &Device, buffer_ms: u32,
                 send_frame_samples: usize,
                 _event_tx: tokio::sync::broadcast::Sender<String>)
                 -> Result<(Self, OutputControl)> {
        // Build the shared zita filter table NOW, on this normal-stack startup
        // thread. Channels (and their resamplers) are created lazily inside the
        // real-time render callback; the ~64KB table build must not run there —
        // the CoreAudio IO thread's stack is too small for it (EXC_BAD_ACCESS).
        super::zita::warm_filter_table();
        // Startup probe: only the channel count is wanted here. The format the stream
        // actually opens with is negotiated per build, inside build_output_stream below.
        let config = crate::audio::backend::find_config(
            device, crate::audio::backend::Dir::Output,
            send_frame_samples.min(crate::audio::encode::IO_BUF_CAP_FRAMES))?;
        let num_out_ch = config.channels() as u8;
        let nch        = num_out_ch as usize;
        // Live output-channel count, written by the stream builder on every build (so it
        // tracks the current device after a hot switch); the API reads it to size the
        // routing UI's output channels. Seeded with the startup device's count.
        let live_out_ch_shared = std::sync::Arc::new(
            std::sync::atomic::AtomicUsize::new(nch));

        let new_channels: Arc<Mutex<Vec<NewChannelMsg>>> = Arc::new(Mutex::new(Vec::new()));

        // Output peak meters — sized to the output channel count (nch).
        let output_peaks_shared: Arc<Vec<std::sync::atomic::AtomicU32>> = Arc::new(
            (0..nch.max(1)).map(|_| std::sync::atomic::AtomicU32::new(0)).collect());

        // Per-incoming peak registry (peer → slot-indexed array). Populated by
        // receive(); shared with each PeerGroup. The output callback does not touch
        // this map — it only writes the per-peer Arc it received via NewChannelMsg.
        let incoming_peaks_shared: Arc<std::sync::RwLock<HashMap<String, Arc<Vec<std::sync::atomic::AtomicU32>>>>> =
            Arc::new(std::sync::RwLock::new(HashMap::new()));

        // Per-peer rough buffer-readout registry (peer → (depth,target) atomics).
        // Populated when a PeerGroup is created in the render callback; read lock-free by
        // the stats tick so the buffer display never takes render_groups.
        let buffer_snaps_shared: Arc<std::sync::RwLock<HashMap<String,
            Arc<super::channel_sync::BufferSnap>>>> =
            Arc::new(std::sync::RwLock::new(HashMap::new()));

        let on_air_shared = Arc::new(std::sync::atomic::AtomicBool::new(false)); // OFF AIR by default

        // Render-aliveness gate. False until the output callback has actually run.
        // receive() drops audio packets until then, so a channel's warm-up can only
        // begin under a live render cadence — audio units run from launch and a peer
        // connects later. Without this, a restart while a peer is already streaming
        // accumulates packets during CoreAudio spin-up; the warm latch then engages far
        // above target and, because the fill is open-loop after warming with no drain
        // path, the overshoot is permanent extra latency (a 60ms setting can hold
        // ~140ms after a restart).
        let render_alive_shared = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Shared peer groups — accessible from both the render closure and set_recv_routing.
        // PeerGroups are created/updated in the render callback; set_recv_routing
        // uses this to reroute existing channels when routing changes mid-session.
        let render_groups_shared: Arc<parking_lot::Mutex<HashMap<String, PeerGroup>>> =
            Arc::new(parking_lot::Mutex::new(HashMap::new()));

        // ── re-invokable per-stream output builder ──────────────────────────
        // Builds ONLY the output stream + its render callback. Called once now,
        // and again by rebuild_output() on a live send-frame change (new buffer =
        // min(send_frame,480)). Captures clones of the shared render state above;
        // each call re-clones them into a fresh callback. render_groups (the jitter
        // buffers) are shared, so the receive side is seamless across a rebuild —
        // only the per-stream limiter/gain/metering state resets.
        // (device is a per-call parameter below, not captured — supports switch.)
        let new_channels_b  = Arc::clone(&new_channels);
        let output_peaks_b  = Arc::clone(&output_peaks_shared);
        let render_alive_b  = Arc::clone(&render_alive_shared);
        let buffer_snaps_b  = Arc::clone(&buffer_snaps_shared);
        let render_groups_b = Arc::clone(&render_groups_shared);
        let live_out_ch_b   = std::sync::Arc::clone(&live_out_ch_shared);
        let live_out_rate_shared = std::sync::Arc::new(
            std::sync::atomic::AtomicU32::new(config.sample_rate()));
        let live_out_device_shared = std::sync::Arc::new(
            std::sync::Mutex::new(crate::audio::device_name(device)));
        let live_out_rate_b = std::sync::Arc::clone(&live_out_rate_shared);
        let live_out_period_shared = std::sync::Arc::new(
            std::sync::atomic::AtomicUsize::new(0));
        let live_out_period_b = std::sync::Arc::clone(&live_out_period_shared);
        let out_period_shared = std::sync::Arc::new(super::PeriodTracker::default());
        let out_period_b = std::sync::Arc::clone(&out_period_shared);
        let live_out_device_b = std::sync::Arc::clone(&live_out_device_shared);
        let output_device_lost_shared = std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false));
        let output_device_lost_b = std::sync::Arc::clone(&output_device_lost_shared);
        let output_stream_invalid_shared = std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false));
        let output_rebuilding_shared = std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false));
        let err_rebuilding_flag = std::sync::Arc::clone(&output_rebuilding_shared);
        let err_invalid_flag = std::sync::Arc::clone(&output_stream_invalid_shared);

        let build_output_stream: Box<dyn Fn(&Device, usize) -> Result<Stream>> =
            Box::new(move |device: &Device, send_frame_samples: usize| -> Result<Stream> {
        // Output buffer size we request = min(send_frame, 480).
        let out_buf_frames = send_frame_samples.min(crate::audio::encode::IO_BUF_CAP_FRAMES);
        let config = crate::audio::backend::find_config(
            device, crate::audio::backend::Dir::Output, out_buf_frames)?;
        let num_out_ch = config.channels() as u8;
        let nch        = num_out_ch as usize;
        // Publish the current device's channel count, confirmed sample rate, and name.
        live_out_ch_b.store(nch, std::sync::atomic::Ordering::Relaxed);
        // The rate we successfully OPENED at. Since 0.18 the build path sets the device's
        // physical format and our patch verifies the hardware clock actually reached it, so
        // a stream existing means this value is real — but it describes THIS stream only,
        // and is zeroed when the stream is invalidated (see main.rs).
        live_out_rate_b.store(config.sample_rate(), std::sync::atomic::Ordering::Relaxed);
        let device_name = crate::audio::device_name(device);
        if let Ok(mut g) = live_out_device_b.lock() { *g = device_name.clone(); }
        let err_device_name = device_name.clone();
        let err_lost_flag = std::sync::Arc::clone(&output_device_lost_b);
        let err_invalid_flag_cb = std::sync::Arc::clone(&err_invalid_flag);
        let err_rebuilding_flag_cb = std::sync::Arc::clone(&err_rebuilding_flag);
        // Single line for the opened output device: channel count, rate and the REQUESTED
        // buffer. What the backend actually granted is only known once the stream is open, so
        // a period it did not honour is reported separately by `verify_period` below.
        // One decimal, not zero: 120 frames is 2.5 ms, and {:.0} renders that as "2".
        info!("Output '{}': {} ch, {} kHz, {}-frame buffer ({:.1} ms)",
              crate::audio::device_name(device), nch,
              config.sample_rate() / 1000, out_buf_frames,
              out_buf_frames as f32 / 48.0);
        // Persistent (per-stream) output-peak scratch — allocated once per stream,
        // reused each callback (RT-safe: no allocation inside the output callback).
        let mut out_peak_scratch: Vec<f32> = vec![0.0f32; nch.max(1)];
        // ── Render closure state (fresh per stream; resets on rebuild) ──
        let mut limiter = Limiter::new();
        let mut gain_share = GainShare::new();
        let mut n_active: Vec<u32> = vec![0; nch];
        let mut cb_count: u64 = 0;
        // Worst-case trackers (reset each reporting window)
        let mut worst_us:       u128 = 0;
        let mut worst_mailbox:  u128 = 0;
        let mut worst_render:   u128 = 0;
        let mut worst_limiter:  u128 = 0;
        let mut worst_frames: usize = 0;
        let mut worst_chans:  usize = 0;
        // per-call clones consumed by the render callback below
        let render_alive_cb  = Arc::clone(&render_alive_b);
        let new_channels_cb  = Arc::clone(&new_channels_b);
        let output_peaks_cb  = Arc::clone(&output_peaks_b);
        let render_groups_cb = Arc::clone(&render_groups_b);
        let buffer_snaps_cb  = Arc::clone(&buffer_snaps_b);
        let live_out_period_cb = Arc::clone(&live_out_period_b);
        let mut first_callback_seen = false;

        // The render body is written ONCE, against f32, and the integer formats wrap it.
        // Same shape as the capture side: one implementation of the actual audio work, with
        // conversion confined to the callback boundary.
        let render_f32 = move |data: &mut [f32]| {
                // Real-time class for the device callback, once per callback thread. No-op
                // on macOS, where CoreAudio applies a time-constraint policy to its own IO
                // thread; on Linux nothing does it for us, and this is the only thread in
                // the process that takes a real-time class. A rebuild gives the backend a new
                // thread, which is why the guard is per-thread rather than per-process.
                #[cfg(target_os = "linux")]
                super::scheduler::pool::elevate_audio_callback_thread();
                let t0 = std::time::Instant::now();
                render_alive_cb.store(true, Ordering::Relaxed);
                let frames = data.len() / nch;

                // Once per stream: the first callback's size against the period the backend
                // reported granting. `verify_period` already says when the grant differs from
                // the request; a callback that differs from the grant is the one mismatch it
                // cannot see. Logged as the EXCEPTION only — the accumulators compensate, but
                // the callback period is then not what the device claimed.
                if !first_callback_seen {
                    first_callback_seen = true;
                    let granted = live_out_period_cb.load(Ordering::Relaxed);
                    if granted != 0 && frames != granted {
                        tracing::warn!(
                            "Output device delivers {} frames ({:.2} ms) per callback, not the \
                             {} it granted",
                            frames, frames as f32 / 48.0, granted);
                    }
                }
                let mut render_groups = render_groups_cb.lock();

                // ── Drain new-channel mailbox (non-blocking try_lock) ──────────
                if let Ok(mut mb) = new_channels_cb.try_lock() {
                    for msg in mb.drain(..) {
                        let peer_name = msg.peer.clone();
                        let existed = render_groups.contains_key(&peer_name);
                        let group = render_groups
                            .entry(msg.peer)
                            .or_insert_with(|| PeerGroup::new_with_sync(num_out_ch, msg.sync.clone(), msg.buffer_ms, msg.peaks.clone()));
                        // A group created here missed the reservation made when the stream was
                        // built; give it the same period before its first channel joins.
                        #[cfg(not(target_os = "macos"))]
                        if !existed {
                            group.reserve_for_period(live_out_period_cb.load(Ordering::Relaxed));
                        }
                        group.add_channel(msg.channel, msg.rx, msg.out_mask, msg.swap_inbox, msg.cm_dir_shared, msg.skew_flag, msg.prebuffer_hold, msg.boxcar_reset, msg.skew_ref, msg.frame_samples); // source_slot = msg.channel
                        // Register this group's lock-free buffer-readout handle once, on
                        // first creation, so the stats tick can read depth without the
                        // render_groups lock. (try_lock: never block render; the map
                        // is only written here, read by the stats tick.)
                        if !existed {
                            if let Ok(mut bs) = buffer_snaps_cb.try_write() {
                                bs.insert(peer_name, group.buffer_snap_handle());
                            }
                        }
                    }
                }
                let t_mailbox = t0.elapsed().as_micros();

                // ── Render (render_groups already locked above, exclusively owned
                //    for the rest of this callback — no further locking below) ──────
                data.fill(0.0);
                for n in n_active.iter_mut() { *n = 0; }

                let mut chans = 0usize;
                for g in render_groups.values_mut() {
                    chans += g.num_channels();
                    g.render(data, frames, nch, &mut n_active);
                }
                let t_render = t0.elapsed().as_micros();

                // ── MASTER OUTPUT STAGE ──
                // A hard clip to ±1.0 here sounds poor on real broadcast material, so
                // this deliberately does something else: a smoothed 1/N gain-share per
                // output (glides on mix changes to avoid the −6 dB step click), then a
                // transparent −1 dB catch limiter. Everything upstream — decode, jitter
                // buffer, gap-fill, per-channel level gain, sync-path DSP — follows the
                // specs exactly; this stage is an owned design choice, so do NOT "fix"
                // it back to a hard clip.
                gain_share.process(data, nch, &n_active);
                limiter.process(data, nch);

                // Sample output peaks post-limiter — what actually hits the speakers.
                // Scan interleaved data: nch channels interleaved, abs-max per channel.
                // Dynamic (was [_;64]) so an output device with >64 channels can't panic.
                let ch_peaks = &mut out_peak_scratch[..];
                for p in ch_peaks.iter_mut() { *p = 0.0; }
                for frame in data.chunks_exact(nch) {
                    for (ch, &s) in frame.iter().enumerate() {
                        let a = s.abs();
                        if a > ch_peaks[ch] { ch_peaks[ch] = a; }
                    }
                }
                // Running MAX, drained by the meter snapshot task (accumulate-then-snapshot,
                // spec §9).
                let opub = nch.min(output_peaks_cb.len());
                for ch in 0..opub {
                    output_peaks_cb[ch].fetch_max(
                        ch_peaks[ch].to_bits(),
                        std::sync::atomic::Ordering::Relaxed);
                }

                let us = t0.elapsed().as_micros();

                // ── Track worst per section ──────────────────────────────────────
                if us > worst_us { worst_us = us; worst_frames = frames; worst_chans = chans; }
                let render_us  = t_render - t_mailbox;
                let limiter_us = us - t_render;
                if t_mailbox  > worst_mailbox  { worst_mailbox  = t_mailbox; }
                if render_us  > worst_render   { worst_render   = render_us; }
                if limiter_us > worst_limiter  { worst_limiter  = limiter_us; }

                cb_count += 1;
                // Log render stats every 30s (debug only — not useful in normal operation).
                // With 480-sample callbacks at 48kHz: 100 callbacks/s → 3000 = 30s.
                let period = ((48_000 / frames.max(1)).max(1) * 30) as u64;
                if cb_count % period == 0 {
                    let deadline_us = worst_frames as u128 * 1_000_000 / 48_000;
                    let pct = if deadline_us > 0 { worst_us * 100 / deadline_us } else { 0 };
                    debug!("RENDER-TIME: worst {}µs / {}µs deadline ({}% of budget), {} ch, {} frames/cb",
                           worst_us, deadline_us, pct, worst_chans, worst_frames);
                    debug!("RENDER-BREAKDOWN: mailbox={}µs render={}µs limiter={}µs",
                           worst_mailbox, worst_render, worst_limiter);
                    for g in render_groups.values() {
                        let depths: Vec<(u8, usize)> = g.channel_depths();
                        debug!("RING-A DIAG: sync={} depths={:?}", g.sync_enabled(), depths);
                    }
                    worst_us = 0; worst_mailbox = 0; worst_render = 0; worst_limiter = 0;
                }
        };

        let err_cb = move |fault: crate::audio::backend::StreamFault| {
                use crate::audio::backend::StreamFault as SF;
                match fault {
                    // The hardware genuinely disappeared, so drive the device-lost path.
                    SF::DeviceLost => {
                        warn!("Output '{}': device no longer available (disconnected)",
                              err_device_name);
                        // Set the lost flag only. The main loop's stats tick reads it and sends
                        // a single `device_lost` event — sending device_error here too produced a
                        // duplicate "disconnected" then "unavailable" banner sequence.
                        err_lost_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    // The stream is invalid but the DEVICE is present, so the remedy is a
                    // rebuild rather than the device-lost teardown. Rebuilding re-runs the
                    // config path, which re-asserts 48 kHz and verifies the hardware clock
                    // actually moved — so switching the device to 44.1 kHz externally now
                    // pulls it back rather than going permanently silent.
                    SF::Invalidated => {
                        // Quiet while a rebuild is already under way: setting the rate back is
                        // itself a rate change, so this is the ECHO of our own correction, not
                        // a new fault. Warning twice about one event reads as two problems.
                        if err_rebuilding_flag_cb.load(std::sync::atomic::Ordering::Relaxed) {
                            debug!("Output device '{}': invalidation echo during rebuild",
                                   err_device_name);
                        } else {
                            warn!("Output '{}': device rate changed — reconfiguring", err_device_name);
                        }
                        err_invalid_flag_cb.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Recoverable glitch: log it but do NOT tear down and re-acquire the
                    // device, which would be a destructive response.
                    SF::Transient(what) => {
                        warn!("Output '{}': {} (transient — device retained)",
                              err_device_name, what);
                    }
                }
        };

        // The render body is written ONCE, against f32. Whatever the device's own sample
        // format is, the backend converts at its boundary — see `backend::open_output`.
        // The stream comes back PREPARED BUT NOT STARTED, which is what the two-phase
        // reconfiguration order needs; `rebuild_output_start` plays it.
        let stream = crate::audio::backend::open_output(device, &config, render_f32, err_cb)?;
        // verify_period returns the EFFECTIVE period: the granted size when the backend
        // substituted one, the requested size otherwise. Publishing it is what lets the
        // reconciler compare it against the setpoint (see AudioEngine::min_active_setpoint).
        let granted = super::verify_period(
            &stream, out_buf_frames, &format!("output '{}'", device_name));
        live_out_period_b.store(granted, std::sync::atomic::Ordering::Relaxed);
        out_period_b.record(out_buf_frames, granted);
        // Size the receive windows for the granted period while the stream is not yet
        // running (see PeerGroup::reserve_for_period). No render callback can hold the groups
        // lock here: the previous stream is already gone and this one has not started.
        #[cfg(not(target_os = "macos"))]
        for g in render_groups_b.lock().values_mut() {
            g.reserve_for_period(granted);
        }
        Ok(stream)
            });

        // Initial output-callback size. The device output chunk follows
        // channel_sync::output_chunk_for_min_buffer: a codeswitch over the MIN receive
        // buffer across connected peers. At start() no peer is connected yet, but we know
        // the SAVED receive buffer (buffer_ms) the configured remote(s) will use, so open
        // the stream at the size it will SETTLE to — avoiding a rebuild-on-connect, and its
        // latch re-arm and settle stall, a few seconds into startup. If the first
        // peer happens to connect at a different buffer, the normal buffer-edge path rebuilds
        // then; in the common case (saved buffer == connect buffer) no rebuild occurs.
        // This is the OUTPUT callback size only; the send frame drives encode independently.
        // §13.2's exact-match table, the same one `min_output_period` applies. It has to be
        // the same table: boot opens the stream at this value and the first reconcile
        // computes the other, so a formula here and a table there would disagree for buffer
        // settings between the exact matches and rebuild the stream seconds after boot for
        // no reason.
        //
        // Asked for its shortest callback period first, while nothing holds it; the request
        // is never shorter than what the assigned devices can run (`audio::shortest_request`).
        // The answer is kept only once the stream is running — see `audio::LearnedPeriod`.
        let learned = super::LearnedPeriod::learn(device, false);
        let initial_out_buf = super::channel_sync::receive_half_for_buffer(buffer_ms)
            .max(super::shortest_request());
        let stream = build_output_stream(device, initial_out_buf)?;
        crate::audio::backend::start(&stream)?;   // boot: build+start immediately (no paired input handshake here)
        learned.keep();

        Ok((Self {
            new_channels,
            dec_slots:        Mutex::new(HashMap::new()),
            slots_epoch:      std::sync::atomic::AtomicU64::new(0),
            stat_acc:         Arc::new(Mutex::new(HashMap::new())),
            dec_queues:       std::sync::Mutex::new(HashMap::new()),
            sync_flags:       Mutex::new(HashMap::new()),
            num_out_ch,
            live_out_ch:      live_out_ch_shared,
            live_out_rate:    live_out_rate_shared,
            live_out_period:  live_out_period_shared,
            out_period:       out_period_shared,
            live_out_device:  live_out_device_shared,
            output_device_lost: output_device_lost_shared,
            output_stream_invalid: output_stream_invalid_shared,
            output_rebuilding: output_rebuilding_shared,
            buffer_ms,
            peer_frame_sizes: Mutex::new(HashMap::new()),
            recv_routing:     Mutex::new(HashMap::new()),
            peer_buffer_ms:   Mutex::new(HashMap::new()),
            output_peaks:     output_peaks_shared,
            incoming_peaks:   incoming_peaks_shared,
            buffer_snaps:     buffer_snaps_shared,
            render_groups:    render_groups_shared,
            on_air:           on_air_shared,
            render_alive:     render_alive_shared,
        }, OutputControl {
            _stream:          RefCell::new(Some(stream)),
            output_builder:   build_output_stream,
            current_output_device: RefCell::new(device.clone()),
            current_send_frame:    std::cell::Cell::new(initial_out_buf),
        }))
    }

    /// Apply the exclusive-output (hog mode) setting to the LIVE output device.
    /// `enable` true → claim exclusive access; false → release whatever we hold. Call at boot,
    /// whenever the setting is toggled, and after an output DEVICE change (to move the claim
    /// onto the new device). Returns whether exclusive access is held afterwards — a claim can
    /// legitimately fail (another app owns the device, or it doesn't support hogging), in which
    /// case we simply stay in shared mode. Output only, by design: the input device is never
    /// hogged so other apps can still capture.
    pub fn apply_exclusive_output(&self, oc: &OutputControl, enable: bool) -> bool {
        // Remember the SETTING, so a rebuild can re-claim without reading the config lock.
        super::hog_mode::set_wanted(enable);
        if enable {
            let dev = oc.current_output_device.borrow();
            super::hog_mode::claim_output(&dev)
        } else {
            super::hog_mode::release();
            false
        }
    }

    /// STOP the output unit — `AudioOutputUnitStop` — and hand the stopped stream back.
    ///
    /// Narrower than `stop_output`, deliberately: it does not clear the live device name,
    /// render state or meters, because the device is not going away — it is about to be
    /// reopened. §5.2 sequences a rebuild as stop both, settle, dispose both, build both, so
    /// stopping and disposing have to be separable steps and the caller owns the gap between
    /// them. Dropping the returned stream is the dispose.
    pub fn pause_output_for_rebuild(&self, oc: &mut OutputControl) -> Option<Stream> {
        // Exclusive access is released with the unit and re-claimed with the new one, as
        // §5.2's `stop_audio` does — the claim belongs to the instance, not to the process.
        // The caller re-applies it after the rebuild if the setting is on.
        super::hog_mode::release();
        let mut stream = oc._stream.borrow_mut().take()?;
        crate::audio::backend::detach_faults(&mut stream);
        let _ = crate::audio::backend::stop(&stream);
        Some(stream)
    }

    /// Stop the output stream and clear the running device name, render state and meters.
    /// Used when the user selects "— none —" in Settings and when the device is lost.
    pub fn stop_output(&self, oc: &mut OutputControl) {
        let mut s = oc._stream.borrow_mut();
        if let Some(mut stream) = s.take() {
            crate::audio::backend::detach_faults(&mut stream);
            let _ = crate::audio::backend::stop(&stream);
            drop(stream);
        }
        drop(s);
        // The device is no longer ours to hold — release exclusive access so other
        // applications can open it (hog mode is OS-enforced and would otherwise persist).
        super::hog_mode::release();
        // Mark the render path dead so receive() early-returns and stops decoding.
        // Without this, incoming packets keep being decoded into jitter buffers that no
        // callback drains — the buffers fill and the jitter/latency stats churn on stale
        // fill levels ("wheels spinning") until a device returns. The flag is set true
        // again by the render callback when a stream is rebuilt.
        self.render_alive.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut g) = self.live_out_device.lock() { g.clear(); }
        self.live_out_rate.store(0, std::sync::atomic::Ordering::Relaxed);
        self.live_out_period.store(0, std::sync::atomic::Ordering::Relaxed);
        self.out_period.reset();
        super::forget_shortest_period(false);
        // Zero the live channel count so the RX page shows "no output device" rather
        // than the previous device's grid. Clear render groups + decode slots so no
        // decoding continues, and zero all incoming peak cells so meters read silence.
        // recv_routing is NOT cleared — the routing intent is preserved so it restores
        // when an output device is selected again.
        self.live_out_ch.store(0, std::sync::atomic::Ordering::Relaxed);
        self.render_groups.lock().clear();
        self.buffer_snaps.write().unwrap_or_else(|e| e.into_inner()).clear();
        self.dec_slots.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.bump_slots_epoch();
        // Clear the pending new-channel mailbox too. Without this, NewChannelMsg pushed
        // by receive() just before/around the loss can survive and desync against the
        // cleared dec_slots — leaving some channels (those mid-creation at loss time)
        // unable to re-create on reactivation until manually re-routed.
        self.new_channels.lock().unwrap_or_else(|e| e.into_inner()).clear();
        for arr in self.incoming_peaks.read().unwrap_or_else(|e| e.into_inner()).values() {
            for cell in arr.iter() {
                cell.store(0u32, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// BUILD the new output stream WITHOUT starting it, and store it. Call
    /// `rebuild_output_start` to begin playback.
    ///
    /// The caller has already stopped and disposed the previous unit — §5.2 sequences a
    /// rebuild as stop both, settle, dispose both, build both — so this expects an empty
    /// stream slot and only builds. The take-and-drop below is a fallback for a caller that
    /// did not, not the normal path.
    ///
    /// The unit is REBUILT, never reconfigured in place (CASCADE_AUDIO_RECEIVE_SPEC §5.2):
    /// every configure creates a fresh component instance, and properties are never
    /// re-applied to a live one.
    ///
    /// BOTH directions are prepared before either is started, then input is started, then
    /// output (CASCADE_AUDIO_SEND_SPEC §3). That ordering is the load-bearing part: it
    /// keeps the receive buffer on its setpoint across the change, because nothing drains
    /// while the other side is stopped.
    pub fn rebuild_output_prepare(&self, oc: &mut OutputControl, device: Option<Device>,
                          send_frame_samples: Option<usize>) -> Result<()> {
        // A device change (vs a frame-size-only change, where device == None) requires
        // resetting the receive render state — see the clear after the old stream stops.
        let device_changed = device.is_some();
        // Switch the output device if a new one was given; otherwise keep the current.
        if let Some(d) = device {
            *oc.current_output_device.borrow_mut() = d;
            // A different device: what the last one did with requested periods says nothing
            // about this one.
            self.out_period.reset();
        }
        // Callback request: change it if given, else keep the current — never shorter than
        // what the assigned devices can run (`audio::shortest_request`), and stored as
        // requested so the reconciler compares like with like.
        let send_frame_samples = match send_frame_samples {
            Some(f) => f,
            None => oc.current_send_frame.get(),
        }.max(super::shortest_request());
        oc.current_send_frame.set(send_frame_samples);
        // Stop the old stream's callback before building the new one. stop() halts the
        // audio unit deterministically; the 20ms settle wait is performed by the CALLER
        // so the async executor
        // is not blocked. We return the paused stream so the caller owns the drop timing.
        let had_stream = {
            let old = oc._stream.borrow_mut().take();
            let present = old.is_some();
            if let Some(mut old) = old {
                crate::audio::backend::detach_faults(&mut old);
                let _ = crate::audio::backend::stop(&old);
                drop(old);
            }
            present
        };
        // On an output DEVICE change, the per-channel render state was warmed for the OLD
        // device's pull timing and channel layout: the jitter rings + zita resamplers held
        // in each PeerGroup, plus the decode slots. Carrying them onto the new device
        // produces garbled ("robotic") audio (the symptom that re-doing the RX crosspoints
        // cleared by hand). Drop both here, inside the paused window (no callback running),
        // so incoming packets rebuild them fresh against the new device: channels
        // auto-repopulate from the decode path on the next packet, and the limiter /
        // gain-share are recreated by the stream rebuild. A frame-size-only change
        // (device == None) keeps the same device, so the state stays valid and is kept.
        if device_changed {
            // Mark render dead for the swap window so receive() early-returns and doesn't
            // decode into the cleared group. The new stream's callback sets it true again.
            self.render_alive.store(false, std::sync::atomic::Ordering::Relaxed);
            self.render_groups.lock().clear();
            self.buffer_snaps.write().unwrap_or_else(|e| e.into_inner()).clear();
            self.dec_slots.lock().unwrap_or_else(|e| e.into_inner()).clear();
            self.bump_slots_epoch();
            // Clear the pending new-channel mailbox to avoid orphaned messages desyncing
            // against the cleared dec_slots (see stop_output for the same reasoning).
            self.new_channels.lock().unwrap_or_else(|e| e.into_inner()).clear();
            // Zero all incoming peak cells so meters for channels that lose their route
            // on the new (smaller) device read silence immediately, rather than holding
            // their last value until the next packet re-evaluates the route.
            for arr in self.incoming_peaks.read().unwrap_or_else(|e| e.into_inner()).values() {
                for cell in arr.iter() {
                    cell.store(0u32, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        let stream = {
            let device_ref = oc.current_output_device.borrow();
            match (oc.output_builder)(&device_ref, send_frame_samples) {
                Ok(s) => s,
                // Our own previous unit was stopped and dropped above, before this build.
                // An exclusive backend (ALSA `hw:`, WASAPI exclusive) can still report busy
                // for a moment while the kernel side of that release completes, so a busy
                // open when we did hold the device gets one retry after the release settle.
                // A second busy is genuinely someone else's hold and is returned.
                Err(e) if crate::audio::backend::is_device_busy(&e) && had_stream => {
                    warn!("Output '{}': device busy on rebuild — retrying after the \
                           release settle", crate::audio::device_name(&device_ref));
                    std::thread::sleep(crate::audio::encode::DEVICE_RELEASE_SETTLE);
                    (oc.output_builder)(&device_ref, send_frame_samples)?
                }
                Err(e) => return Err(e),
            }
        };
        // `open_output` returns a prepared, stopped stream, so this is belt-and-braces
        // rather than load-bearing: "prepared" must mean genuinely stopped, and
        // rebuild_output_start plays it in Phase 2 (input first, then output). A stream
        // that rendered during PREPARE drained the receive ring by ~100ms and collapsed
        // the buffer, so the guarantee is worth asserting twice.
        let _ = crate::audio::backend::stop(&stream);
        *oc._stream.borrow_mut() = Some(stream);   // built + paused → NOT running until start
        info!("Output '{}': rebuilt (frame {}, buffer {}) — prepared",
              crate::audio::device_name(&oc.current_output_device.borrow()),
              send_frame_samples,
              send_frame_samples.min(crate::audio::encode::IO_BUF_CAP_FRAMES));

        // On the same device the new stream can be granted a different period, which moves
        // the setpoint floor. A device change needs nothing here: it cleared every channel
        // above, and each is recreated against the new floor.
        #[cfg(not(target_os = "macos"))]
        if !device_changed {
            self.apply_period_floor();
        }

        // After an output device change the render groups + decode slots were cleared
        // above, so the live render channels must be rebuilt from the (unclamped)
        // recv_routing table against the NEW device's channel count. recv_routing itself
        // is preserved across device changes (full routing intent), and set_recv_routing
        // clamps the live render mask to live_out_ch — so growing the device back restores
        // the wider routing and shrinking it drops the unplayable channels, both without
        // mutating the stored table. Incoming packets recreate decode slots on the next
        // frame via the receive() path; this re-apply primes the render-side masks.
        if device_changed {
            let snapshot: Vec<(String, Vec<crate::audio::routing::RouteEntry>)> = {
                let lock = self.recv_routing.lock().unwrap_or_else(|e| e.into_inner());
                lock.iter().map(|(peer, pr)| {
                    let routes = pr.slot_to_outs.iter()
                        .flat_map(|(&src, dsts)| dsts.iter().map(move |&dst|
                            crate::audio::routing::RouteEntry { src, dst, value: 1 }))
                        .collect();
                    (peer.clone(), routes)
                }).collect()
            };
            for (peer, routes) in snapshot {
                self.set_recv_routing(&peer, &routes);
            }
            // Final clear of decode slots + mailbox AFTER the new stream and render groups
            // are in place. Any slots that receive() recreated during the teardown→rebuild
            // window (e.g. between a device-loss stop_output and this rebuild) are dropped
            // here, so EVERY incoming channel re-creates cleanly against the fresh render
            // group on its next packet — not just the ones that happened to be mid-creation.
            // Without this, channels that sent a packet during the window stayed in dec_slots
            // but were absent from the rebuilt render group (decoding to nowhere), which
            // showed as some channels silent until manually re-routed.
            self.dec_slots.lock().unwrap_or_else(|e| e.into_inner()).clear();
            self.bump_slots_epoch();
            self.new_channels.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }

        Ok(())
    }

    /// START the prepared output stream (play). Called after BOTH units are prepared and
    /// after the input unit is started. The input-then-output order is fixed.
    pub fn rebuild_output_start(&self, oc: &mut OutputControl) -> Result<()> {
        if let Some(s) = oc._stream.borrow().as_ref() { crate::audio::backend::start(s)?; }
        Ok(())
    }


    /// Receive half of the shared callback period, in samples (CASCADE_AUDIO_RECEIVE_SPEC
    /// §13.2): the minimum, over ENABLED remotes, of §13.2's table applied to each remote's
    /// configured receive buffer — 5 ms gives 120, 10 ms gives 240, anything else 480. 480
    /// when no remote is enabled.
    pub fn min_output_period(&self, enabled: &std::collections::HashSet<String>) -> usize {
        // The detected incoming frame size is NOT an input here, at all. §13.2's minimum is
        // over `bufferSize` — the user-configured receive buffer — which is separate from the
        // per-channel setpoint the auto-grow mechanism widens, with no path between them. So a
        // sender switching frame size mid-stream must NOT move the callback period; tying the
        // two together rebuilds the audio units on an event that should be handled silently.
        //
        // §13.2's table is exact-match, not a formula — 5 gives 120, 10 gives 240, and every
        // other value falls through to 480. There is no general mapping to derive.
        //
        // §13.2's minimum is taken over channels that are ENABLED — that gate and no other:
        //
        //     minBufferSize = min(channel.bufferSize for channel in allChannels
        //                          if channel.isEnabled)
        //
        // `enabled` comes from config (the caller holds it). One disabled remote left at 5ms
        // must not hold the callback at 120 samples for every active peer on the machine.
        //
        // There is deliberately NO second "is this peer actually decoding yet" gate. Adding
        // one makes this value depend on connection state, and connection state is not a
        // settings change: the period would then read 480 at startup (nothing decoding), move
        // to its real value once a peer connected, and rebuild both units to get there. That
        // rebuild stops the render callback for ~130ms while packets keep arriving, so the
        // ring overshoots setpoint by roughly one rebuild's worth of audio — and if the
        // overshoot lands inside a channel's first 3 seconds it is what §2.3's reference
        // calibrates against, leaving the servo armed against a depth the link never actually
        // runs at. The buffer setting is known from config before any audio flows, so this
        // value is correct from the first seed and never has to move.
        let bufs  = self.peer_buffer_ms.lock().unwrap_or_else(|e| e.into_inner());
        let mut min_period = 480usize;
        for (peer, &buf_ms) in bufs.iter() {
            if !enabled.contains(peer.as_str()) {
                continue;
            }
            // §13.2 is an EXACT-MATCH table, not a formula: 5 gives 120, 10 gives 240, and
            // every other value — including 7, 15, and anything above 20 — falls through to
            // the 480 default. Deriving this from the setpoint instead (`setpoint / 2`,
            // clamped) agrees at 5, 10 and 20-and-above but not between: it turns 7 into 120
            // and 15 into 240, holding the callback far shorter than §13.2 specifies for
            // a buffer the dropdown never offers but the API and a hand-edited config both
            // accept.
            min_period = min_period.min(super::channel_sync::receive_half_for_buffer(buf_ms));
        }
        min_period
    }

    /// The setpoint floor the callback period imposes, in samples:
    /// `channel_sync::period_floor_samples` of the longer of the period the output stream
    /// was granted and the shortest period both assigned devices can run
    /// (`audio::agreed_period`).
    ///
    /// A driver on Windows or Linux can grant a longer period than the one requested — some
    /// grant their own fixed period whatever they are asked — and a setpoint no deeper than
    /// one period is emptied by every render callback. The floor raises every channel's
    /// setpoint above that, whatever the buffer setting. The agreed period keeps it at the
    /// floor the web UI's buffer list uses.
    ///
    /// 0 with no output and no learned period, and always on macOS, where the setpoint
    /// follows the buffer setting and incoming frame size alone.
    pub fn period_floor(&self) -> usize {
        #[cfg(target_os = "macos")]
        { 0 }
        #[cfg(not(target_os = "macos"))]
        { super::channel_sync::period_floor_samples(
              self.live_out_period.load(std::sync::atomic::Ordering::Relaxed)
                  .max(super::agreed_period())) }
    }

    /// Bring every channel's period floor up to date with `period_floor`, and refill each
    /// channel whose setpoint moves as a result (`refill_at_current_geometry`). Called when
    /// the output is rebuilt on the same device and when the agreed period changes. A
    /// channel whose ring has not been sized yet only records the floor: its first packet
    /// sizes the ring with it.
    ///
    /// The slot handles are collected before any of them is locked, so this never holds the
    /// map lock and a slot lock at the same time.
    #[cfg(not(target_os = "macos"))]
    pub fn apply_period_floor(&self) {
        let floor = self.period_floor();
        let slots: Vec<DecodeSlot> = self.dec_slots.lock().unwrap_or_else(|e| e.into_inner())
            .values().map(Arc::clone).collect();
        let mut refilled = 0usize;
        for slot in slots {
            let mut s = slot.lock();
            if s.period_floor == floor { continue; }
            s.period_floor = floor;
            if s.expected_frame_samples == 0 { continue; }
            let target = super::channel_sync::target_samples_floored(
                s.buffer_ms, s.expected_frame_samples, floor);
            if target != s.setpoint {
                refill_at_current_geometry(&mut s);
                refilled += 1;
            }
        }
        if refilled > 0 {
            info!("receive setpoint floor now {} ms — {} channel(s) re-buffered at their new \
                   setpoint", floor / 48, refilled);
        }
    }

    /// Smallest receive setpoint (in samples) across channels that are enabled AND have an
    /// active decode pipeline. The decode-pipeline gate is right HERE and wrong in
    /// `min_output_period`: this is a runtime check against rings that actually exist, so a
    /// peer with no decode slots has no ring to underrun and belongs out of the minimum.
    ///
    /// Returns `None` when no channel qualifies, which is the "nothing is decoding yet"
    /// case rather than a setpoint of zero. Callers must treat that as "no constraint".
    ///
    /// This exists for one comparison: the callback period must stay strictly BELOW the
    /// setpoint. If a render cycle is as long as the buffer is deep, that cycle consumes
    /// the entire buffer and the ring empties every time — continuous underrun, with the
    /// jitter buffer providing no protection at all because it is drained as fast as it
    /// fills. For every buffer the settings offer, §13.2's table keeps the period at or below
    /// half the setpoint, so the constraint is violated only by a backend that grants a
    /// larger period than the one requested — or by a buffer between the table's entries
    /// that only the API or a hand-edited config can set (7 ms maps to a 480 period against a
    /// 240 setpoint). CoreAudio honours requests. ALSA rounds to its nearest supported
    /// period, and some WASAPI drivers grant their own fixed period (Dante Virtual Soundcard
    /// grants 512 whatever is asked), so both can.
    ///
    /// The setpoint includes `period_floor`, as the channels' own setpoints do. On Windows
    /// and Linux that floor is at least twice the granted period, so the comparison cannot
    /// fail there; it reports the cases the floor does not cover, which are on macOS.
    pub fn min_active_setpoint(&self, enabled: &std::collections::HashSet<String>)
        -> Option<usize>
    {
        let floor = self.period_floor();
        let slots = self.dec_slots.lock().unwrap_or_else(|e| e.into_inner());
        let bufs  = self.peer_buffer_ms.lock().unwrap_or_else(|e| e.into_inner());
        let mut min_sp: Option<usize> = None;
        for (peer, &buf_ms) in bufs.iter() {
            if !enabled.contains(peer.as_str()) { continue; }
            if !slots.keys().any(|(p, _)| p == peer) { continue; }
            let sp = super::channel_sync::target_samples_for_buffer(buf_ms).max(floor);
            min_sp = Some(min_sp.map_or(sp, |m: usize| m.min(sp)));
        }
        min_sp
    }

    /// Called at startup from RemoteConfig.buffer_ms.
    ///
    /// §4 of CASCADE_AUDIO_RECEIVE_SPEC: when Sync is enabled AT THE MOMENT the
    /// buffer-size setting is applied, the requested value is clamped to a minimum of
    /// 20ms — anything smaller is silently treated as 20ms. With Sync disabled the
    /// requested value is used as given. The clamp is conditional on Sync state, not a
    /// universal minimum, and it is evaluated when the SETTING is applied — toggling
    /// Sync afterwards does not retroactively raise an already-applied buffer.
    ///
    /// The 5ms sync-off floor below is ours, not the spec's: a guard against absurd
    /// values (5ms is 2× the smallest frame, 2.5ms). The exact per-frame floor
    /// (2×incoming frame) is applied downstream by target_samples_floored, as is the
    /// output-period floor (`period_floor`).
    pub fn set_peer_buffer(&self, peer_name: &str, ms: u32) {
        let sync_on = self.sync_flags.lock().ok()
            .and_then(|sf| sf.get(peer_name).map(|f| f.load(Ordering::Relaxed)))
            .unwrap_or(false);
        let floor = if sync_on { 20 } else { 5 };
        let clamped = ms.clamp(floor, 10000);
        if clamped != ms {
            debug!("peer '{}': buffer {}ms → {}ms ({})", peer_name, ms, clamped,
                   if sync_on { "sync-on 20ms minimum" } else { "range guard" });
        }
        self.peer_buffer_ms.lock().unwrap_or_else(|e| e.into_inner())
            .insert(peer_name.to_string(), clamped);
    }

    /// Per-peer receive-buffer health for the monitor page: peer →
    /// (depth_ms, target_ms, holding), depth being the mean across active channels and
    /// min/max the interval's range around it. `holding` = at least
    /// one channel has the prebuffer hold armed. Lock-free reads
    /// of the atoms published by render().
    /// Per-peer buffer readout over the interval since the last call, in ms:
    /// `(peer, mean, min, max, target, holding)`. `mean` is the display figure; `min`/`max`
    /// are the spread it moved through.
    ///
    /// CONSUMING: reading resets the fold, so the next call describes the next interval.
    /// Call it from one place only — the stats tick — or intervals get split between
    /// callers and each sees part of the picture.
    ///
    /// A peer with no active channel, or none that rendered since the last call, is absent
    /// rather than reported at zero.
    pub fn buffer_report(&self) -> Vec<(String, f32, f32, f32, f32, bool)> {
        let snaps = self.buffer_snaps.read().unwrap_or_else(|e| e.into_inner());
        snaps.iter().filter_map(|(peer, snap)| {
            let (mean, min, max, target) = snap.take()?;
            let holding = snap.holding.load(std::sync::atomic::Ordering::Relaxed) > 0;
            Some((peer.clone(), mean as f32 / 48.0, min as f32 / 48.0, max as f32 / 48.0,
                  target as f32 / 48.0, holding))
        }).collect()
    }

    /// Per-channel ring depths, per peer — diagnostic only, behind `CASCADE_DEBUG_API`.
    ///
    /// `try_lock`, never `lock`. `render_groups` is taken by the render callback with a
    /// blocking lock, and §10 requires the buffer display to be read "without touching the
    /// real-time audio thread's own execution — no blocking, no lock contention". A poll
    /// that arrives mid-render returns `None` and the caller keeps its previous value; for
    /// a diagnostic sampled once a second that is invisible, and it makes contention with
    /// the audio thread structurally impossible rather than merely unlikely.
    pub fn channel_depth_report(&self) -> Option<Vec<(String, Vec<(u8, usize)>)>> {
        let groups = self.render_groups.try_lock()?;
        Some(groups.iter().map(|(p, g)| (p.clone(), g.channel_depths())).collect())
    }

    /// Snapshot the live INCOMING frame size (in samples) per peer, as measured by the
    /// decode path (peer_frame_size.swap on each frame-size change). 0/absent until the
    /// first packet from that peer is decoded. The UI uses 2× this as the receive-buffer
    /// floor; peers not yet decoding are simply
    /// absent and the UI falls back to its default floor.
    pub fn peer_frame_report(&self) -> Vec<(String, usize)> {
        let pfs = self.peer_frame_sizes.lock().unwrap_or_else(|e| e.into_inner());
        pfs.iter()
            .map(|(peer, fs)| (peer.clone(), fs.load(std::sync::atomic::Ordering::Relaxed)))
            .filter(|(_, samples)| *samples > 0)
            .collect()
    }

    /// Live receive-buffer change for a connected peer. The SPSC ring is sized from the
    /// buffer, so a target change alone can't grow past the existing ring — that left audio
    /// silenced on an increase. Instead each of the peer's channels gets a fresh ring at the
    /// new geometry through its swap inbox, with the prebuffer re-armed: a brief re-buffer
    /// gap, then resume. Decoders, resamplers and routing are untouched, so audio returns on
    /// the same crosspoints with no re-route. `ms` must already carry the phase-lock floor.
    pub fn retarget_peer_buffer(&self, peer_name: &str, ms: u32) {
        let clamped = ms.clamp(5, 10000);
        self.peer_buffer_ms.lock().unwrap_or_else(|e| e.into_inner())
            .insert(peer_name.to_string(), clamped);

        // Resize each of this peer's channels IN PLACE rather than dropping them and
        // letting arriving packets rebuild them. The end state is the same either way —
        // fresh ring at the new geometry, boxcar cleared, prebuffer re-armed — but a
        // teardown also destroys the Opus decoder and the resampler, and neither is part
        // of the ring. The decoder would restart mid-stream on a link that never lost a
        // packet, and the resampler would lose its filter history and phase accumulator,
        // both of which stay valid across a buffer change because the audio timeline
        // itself is continuous: only the depth we are holding it at has moved.
        //
        // The slot handles are collected before any of them is locked, so this never holds
        // the map lock and a slot lock at the same time.
        let targets: Vec<super::pool::DecodeSlot> = {
            let slots = self.dec_slots.lock().unwrap_or_else(|e| e.into_inner());
            slots.iter()
                .filter(|((p, _), _)| p.as_str() == peer_name)
                .map(|(_, slot)| Arc::clone(slot))
                .collect()
        };
        for slot in targets {
            let mut s = slot.lock();
            s.buffer_ms = clamped;
            refill_at_current_geometry(&mut s);
        }

        // No epoch bump: the slots themselves survive, so every cached handle stays valid.
        // A channel that joins this peer LATER builds its ring from the group's copy of the
        // setting, so that copy has to move too or the newcomer would size against the old
        // buffer and sit at a different depth from its siblings.
        if let Some(group) = self.render_groups.lock().get_mut(peer_name) {
            group.set_buffer_ms(clamped);
        }
    }

    /// Remove all state for a disabled or removed peer.
    pub fn remove_peer(&self, peer: &str) {
        self.recv_routing.lock().unwrap_or_else(|e| e.into_inner()).remove(peer);
        self.peer_buffer_ms.lock().unwrap_or_else(|e| e.into_inner()).remove(peer);
        self.peer_frame_sizes.lock().unwrap_or_else(|e| e.into_inner()).remove(peer);
        // Drop the render group, decode slots, and buffer-display snapshot too. Nothing
        // reaps idle PeerGroups (writer_idle_renders only gates is_active_for_display, it
        // never deletes), so a stale group would otherwise linger with warmed=true. On
        // re-enable, receive() would then find existed==true, skip re-registering a fresh
        // buffer_snaps handle, and reuse the warmed group whose latch stays released and
        // never re-accumulates — so audio resumes at near-empty ring depth (the 0-10ms
        // buffer-on-re-enable bug). Removing them here forces re-enable down the !existed
        // path: fresh ring, warmed=false, fresh snapshot → warms to target like boot.
        {
            let mut s = self.dec_slots.lock().unwrap_or_else(|e| e.into_inner());
            s.retain(|(p, _), _| p.as_str() != peer);
            self.bump_slots_epoch();
        }
        // Channel announcements the render callback has not picked up yet go with them. Left
        // in the mailbox, the next callback would rebuild a render group for a peer that no
        // longer exists — the same stale group the removals above are there to prevent.
        self.new_channels.lock().unwrap_or_else(|e| e.into_inner())
            .retain(|m| m.peer != peer);
        self.render_groups.lock().remove(peer);
        self.buffer_snaps.write().unwrap_or_else(|e| e.into_inner()).remove(peer);
        // The remaining per-peer runtime state. None of it is a setting in its own right —
        // every entry is re-derived from the peer's config block when it is enabled again
        // (the boot loop in main.rs and the peer-spawn path apply phase lock, buffer
        // and routing), so dropping it here leaves nothing
        // for a re-enable to inherit. Keeping it would: the flags and the peaks array are
        // shared by Arc with the receive path, so a stale entry would be adopted by the
        // rebuilt channels instead of the freshly applied value.
        // stat_acc is deliberately NOT removed. The receive thread caches this peer's
        // Arc<StatAccumulator> in ArrivalStats::accs on first sight and never invalidates
        // it, so dropping the map entry orphans that cached Arc: arrivals keep landing in
        // the old accumulator while the peer task drains a fresh empty one, and jitter and
        // loss read zero for the rest of the process. The accumulator is three atomics
        // keyed by peer name, and its stats belong to the peer's identity rather than to
        // one session, so keeping it across a disable is also the correct behaviour.
        self.sync_flags.lock().unwrap_or_else(|e| e.into_inner()).remove(peer);
        self.incoming_peaks.write().unwrap_or_else(|e| e.into_inner()).remove(peer);
        // dec_queues is a memo cache of (peer, channel) → queue handle. The queues live in
        // the process-wide scheduler pool and are shared by channel index across peers, so
        // this releases map entries and Arc clones, not queues or threads.
        self.dec_queues.lock().unwrap_or_else(|e| e.into_inner())
            .retain(|(p, _), _| p.as_str() != peer);
    }

    /// Set receive routing for a peer from a parsed route list. Called at startup and on
    /// remote enable (from the saved matrix), from the web UI's receive matrix, and after an
    /// output device change (to re-clamp the live masks to the new device).
    pub fn set_recv_routing(&self, peer: &str,
                            routes: &[crate::audio::routing::RouteEntry]) {
        // Store the FULL routing unclamped. Routes to output slots beyond the current
        // device are kept (dormant) rather than discarded — so growing the device back
        // (e.g. 2ch → 8ch) restores the wider routing without the user re-toggling, and
        // this table never diverges from the config matrix. Clamping happens only when
        // building the live render mask below.
        let nch = self.live_out_ch.load(std::sync::atomic::Ordering::Relaxed) as u8;

        // Store the new routing table (unclamped — full intent preserved).
        {
            let mut lock = self.recv_routing.lock().unwrap_or_else(|e| e.into_inner());
            if routes.is_empty() {
                // No routes → remove the entry so the slot resolves to an empty
                // output list = dropped (policy: nothing routed unless specified).
                lock.remove(peer);
            } else {
                lock.insert(peer.to_string(),
                            PeerReceiveRouting::new(routes, self.num_out_ch));
            }
        }

        // Apply LIVE to existing channels — update their output masks in place rather
        // than tearing them down, so a routing edit never re-warms a buffer or restarts a
        // resampler under audio the user is listening to.
        // The mask is CLAMPED to the live device channel count: a route to a slot the
        // device can't play contributes no mask bit, so a channel only exists — and so
        // decodes and meters — while it has at least one PLAYABLE output.
        let lookup = {
            let lock = self.recv_routing.lock().unwrap_or_else(|e| e.into_inner());
            lock.get(peer).cloned()
        };
        let clamp = |outs: Vec<u8>| -> Vec<u8> {
            if nch > 0 { outs.into_iter().filter(|&d| d < nch).collect() } else { outs }
        };
        // Resolve every slot to its output mask BEFORE taking render_groups.
        //
        // `render_groups` is taken by the RT render callback with a blocking `lock()`, so
        // work done while holding it stalls audio directly — and resolving allocates a
        // `Vec<u8>` per slot (`resolve()`, then `clamp`'s `collect()`).
        //
        // `None` means "not routed anywhere" and drops the channel; an absent `lookup`
        // leaves every entry `None`. Indexed by slot over the same 0..128 range the rest of
        // the group uses (`has_channel`, the meter cells), with a checked `get` so a slot
        // outside it drops rather than panicking.
        let mut slot_masks: [Option<u128>; 128] = [None; 128];
        if let Some(r) = &lookup {
            for slot in 0u8..128 {
                let outs = clamp(r.resolve(slot));
                if !outs.is_empty() {
                    slot_masks[slot as usize] =
                        Some(super::channel_sync::outputs_to_mask(&outs));
                }
            }
        }
        {
            let mut groups = self.render_groups.lock();
            if let Some(group) = groups.get_mut(peer) {
                group.reroute_channels(
                    |slot| slot_masks.get(slot as usize).copied().flatten());
            }
        }
        // Drop decode slots for this peer whose render channel was removed (now
        // unrouted), so a future re-route recreates them cleanly. Channels that are
        // still routed keep their decode slot and ring intact (no re-warm).
        {
            let in_group: std::collections::HashSet<u8> = {
                let groups = self.render_groups.lock();
                groups.get(peer)
                    .map(|g| (0u8..128).filter(|&s| g.has_channel(s)).collect())
                    .unwrap_or_default()
            };
            // dec_slots BEFORE new_channels — the order resolve_channel takes them in.
            let mut s = self.dec_slots.lock().unwrap_or_else(|e| e.into_inner());
            // A channel announced in the mailbox but not yet picked up by the render callback
            // has a live decode slot and no render channel YET. It is still present: judging
            // it by the render group alone removed its decode slot, the next packet created a
            // second one, and the channel the callback then built from the first announcement
            // read a ring nothing wrote. Apply the new routing to those announcements too —
            // re-masked if still routed, withdrawn if not.
            let pending: std::collections::HashSet<u8> = {
                let mut mb = self.new_channels.lock().unwrap_or_else(|e| e.into_inner());
                mb.retain_mut(|m| {
                    if m.peer != peer { return true; }
                    match slot_masks.get(m.channel as usize).copied().flatten() {
                        Some(mask) => { m.out_mask = mask; true }
                        None => false,
                    }
                });
                mb.iter().filter(|m| m.peer == peer).map(|m| m.channel).collect()
            };
            let before = s.len();
            s.retain(|(p, slot), _| p.as_str() != peer
                                    || in_group.contains(slot) || pending.contains(slot));
            // If we dropped any decode slot, bump the epoch so the recv hot-path handle
            // cache (net::udp chan_cache) flushes, as every other teardown site does.
            // Otherwise a re-enabled crosspoint keeps getting a cache HIT on the dropped
            // slot's stale handle and never calls resolve_channel — so its render channel
            // is never re-created and the audio stays silent.
            if s.len() != before {
                drop(s);
                self.bump_slots_epoch();
            }
        }
        debug!("Peer '{}': receive routing updated live ({} routes)", peer, routes.len());
    }


    /// Resolve incoming (peer_name, slot) → LIST of local output channels.
    ///
    /// POLICY: nothing is routed unless explicitly specified. No RX routing entry
    /// for a peer, or no route for this slot → EMPTY list (dropped), NOT default
    /// round-robin. A slot may route to MULTIPLE outputs (one-to-many fan-out).
    fn recv_out_chs(&self, peer_name: &str, slot: u8) -> Vec<u8> {
        let lock = self.recv_routing.lock().unwrap_or_else(|e| e.into_inner());
        match lock.get(peer_name) {
            Some(routing) => routing.resolve(slot),
            None => Vec::new(), // no routing configured → drop (no audio out)
        }
    }

    fn buffer_for(&self, peer_name: &str) -> u32 {
        self.peer_buffer_ms.lock().unwrap_or_else(|e| e.into_inner())
            .get(peer_name).copied().unwrap_or(self.buffer_ms)
    }

    /// Whether the output render callback is running. Gates the receive hot path: audio is
    /// dropped until the device callback is up, which is `resolve_channel`'s precondition.
    pub fn render_alive(&self) -> bool {
        self.render_alive.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Current decode-slot epoch. The recv hot path compares this against its last-seen
    /// value and flushes its ChannelHandle cache on a change (a slot teardown).
    pub fn slots_epoch(&self) -> u64 {
        self.slots_epoch.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Bump after any structural dec_slots teardown so cached recv-side handles are
    /// invalidated. Release-ordered so the recv side sees a consistent post-teardown view.
    fn bump_slots_epoch(&self) {
        self.slots_epoch.fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// Resolve (get-or-create) the per-(peer,channel) decode handle. Takes the engine-wide
    /// map locks (dec_slots, sync_flags, stat_acc, dec_queues) — this is the ONLY place
    /// that does. A routed channel resolves once and the caller caches the handle; an
    /// unrouted one is not cached, so it comes back here on every packet and must stay cheap.
    /// Returns `None` when the slot has no playable route (nothing to decode). Caller must
    /// have already checked `render_alive`.
    pub fn resolve_channel(&self, peer_name: &str, channel: u8) -> Option<ChannelHandle> {
        let key = (peer_name.to_string(), channel);

        // ── Get-or-create decode slot ──────────────────────────────────────
        let slot = {
            let mut slots = self.dec_slots.lock().unwrap_or_else(|e| e.into_inner());
            if !slots.contains_key(&key) {
                // ROUTING FIRST, before anything is built. An unrouted channel lands here
                // for every packet it sends, and building its ring, decoder and shared state
                // only to discard them on finding no route cost a full slot construction per
                // packet per unrouted channel.
                //
                // Still under the dec_slots lock, deliberately: set_recv_routing updates the
                // table BEFORE taking this lock, so a routing change either lands before this
                // read or finds whatever this call inserts and corrects it.
                //
                // Empty list when no route for this slot — nothing is created (nothing routed
                // unless specified). Clamped to the live device channel count: a slot routed
                // ONLY to outputs the current device can't play must not decode (e.g. routed
                // to Out3 on a 2ch device). The unclamped route stays in recv_routing so it
                // reactivates if the device grows back.
                let nch = self.live_out_ch.load(std::sync::atomic::Ordering::Relaxed) as u8;
                let outs: Vec<u8> = self.recv_out_chs(peer_name, channel)
                    .into_iter().filter(|&d| nch == 0 || d < nch).collect();
                if outs.is_empty() {
                    trace!("Peer '{}' slot{}: no playable receive route, dropping",
                           peer_name, channel);
                    // Zero this slot's incoming peak cell so the meter reads silence
                    // rather than holding the last value from when it was routed (e.g.
                    // after an output device shrank and this channel lost its route).
                    if let Some(arr) = self.incoming_peaks.read()
                        .unwrap_or_else(|e| e.into_inner()).get(peer_name)
                    {
                        if let Some(cell) = arr.get(channel as usize) {
                            cell.store(0u32, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    return None;
                }

                // ── Ensure sync flag exists for this peer ──────────────────
                let sync = {
                    let mut sf = self.sync_flags.lock().unwrap_or_else(|e| e.into_inner());
                    sf.entry(peer_name.to_string())
                      .or_insert_with(|| Arc::new(AtomicBool::new(false)))
                      .clone()
                };
                let buf_ms = self.buffer_for(peer_name);
                let hint_frame = super::channel_sync::INITIAL_FRAME_SAMPLES;
                // Read with the slot map locked, so a concurrent `apply_period_floor` either
                // finds this slot or has already published the period this reads.
                let period_floor = self.period_floor();
                let cap_samples = ring_capacity_floored(buf_ms, hint_frame, period_floor);
                let (tx, rx) = spsc::channel(cap_samples);
                let swap_inbox: SwapInbox = Arc::new(parking_lot::Mutex::new(None));
                // Per-peer frame-size tracker — shared across all channels of this peer. Starts
                // at 0, "nothing decoded yet": the first channel to decode a frame then logs the
                // peer's incoming frame size at info whatever it is, and the frame report stays
                // empty for a peer that has not sent any audio.
                let peer_frame_size = {
                    let mut pfs = self.peer_frame_sizes.lock()
                        .unwrap_or_else(|e| e.into_inner());
                    pfs.entry(peer_name.to_string())
                        .or_insert_with(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
                        .clone()
                };
                // Shared per-channel common-mode drift direction: written by this
                // channel's decode worker, read+summed by render. One atomic = the
                // entire cross-thread surface of the corrector.
                let cm_dir_shared: super::pool::DriftDir =
                    Arc::new(std::sync::atomic::AtomicI8::new(0));
                // §6.6's adjusting flag, shared for the same reason. The two are
                // adjacent fields cleared together by every reset of the averaging
                // window, so they have to be reachable from the same side.
                let skew_flag: super::pool::DriftDir =
                    Arc::new(std::sync::atomic::AtomicI8::new(0));
                // Shared prebuffer-hold flag (spec §4.2) — armed at creation, so the
                // channel holds (fresh join) until it has filled to setpoint once.
                let prebuffer_hold: super::pool::PrebufferHold =
                    Arc::new(std::sync::atomic::AtomicBool::new(true));
                // §6.5 reset request line, and the one per-channel §2.3/§6.6 reference
                // both the decode and render sides read and write.
                let boxcar_reset = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let skew_ref     = Arc::new(std::sync::atomic::AtomicI64::new(0));
                // Per-incoming peak array for this peer (created once, shared with the
                // PeerGroup + the API). Fetched HERE — before the decode slot — because the
                // §9 meter is now updated on the decode/write side (see ChannelDecodeSlot
                // .meter_peaks), so the slot needs the array at construction. 128 slots
                // covers the channel range.
                let peaks = {
                    let mut ip = self.incoming_peaks.write().unwrap_or_else(|e| e.into_inner());
                    ip.entry(peer_name.to_string())
                      .or_insert_with(|| Arc::new(
                          (0..128).map(|_| std::sync::atomic::AtomicU32::new(0)).collect()))
                      .clone()
                };
                // Gates the §9.3 meter switch — see ChannelDecodeSlot::meter_live.
                let meter_live = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let dec_slot: DecodeSlot = Arc::new(ParkingMutex::new(
                    ChannelDecodeSlot::new(tx,
                                          0,   // expected_frame_samples: 0 = UNKNOWN until the
                                               // first packet sizes it. Sizing from the
                                               // packet rather than a default avoids the
                                               // boot-default→resize+re-arm disturbance.
                                          buf_ms,
                                          period_floor,
                                          Arc::clone(&swap_inbox),
                                          peer_frame_size,
                                          Arc::clone(&cm_dir_shared),
                                          Arc::clone(&skew_flag),
                                          Arc::clone(&prebuffer_hold),
                                          Arc::clone(&boxcar_reset),
                                          Arc::clone(&skew_ref),
                                          Arc::clone(&sync),
                                          Arc::clone(&peaks),
                                          channel as usize,
                                          Arc::clone(&meter_live))));

                slots.insert(key.clone(), dec_slot);

                let out_mask = crate::audio::channel_sync::outputs_to_mask(&outs);
                let mut mb = self.new_channels.lock().unwrap_or_else(|e| e.into_inner());
                // `peaks` (per-incoming peak array) was created above, before the decode
                // slot, and shared into it for the §9 decode-side meter. The PeerGroup gets
                // the same Arc so the render group and API read the identical cells.
                mb.push(NewChannelMsg {
                    peer:      peer_name.to_string(),
                    channel,
                    rx,
                    out_mask,
                    sync:      sync.clone(),
                    buffer_ms: buf_ms,
                    frame_samples: hint_frame,
                    swap_inbox,
                    cm_dir_shared,
                    skew_flag,
                    prebuffer_hold,
                    boxcar_reset,
                    skew_ref,
                    peaks,
                });
                debug!("Audio: new channel slot{} → outputs {:?} buffer={}ms",
                       channel, outs, self.buffer_for(peer_name));
            }
            slots[&key].clone()
        };

        // Ensure the peer's stats accumulator exists. The receive thread creates it too
        // (stat_acc_for), so this is belt-and-braces for a peer that resolves a channel
        // before its first audio packet is accounted.
        let _ = self.stat_acc_for(peer_name);

        // ── Get-or-create the per-channel decode queue ─────────────────────
        // A serial queue over a shared worker pool on both platforms: a GCD
        // USER_INITIATED queue targeting the global concurrent queue on macOS, a queue
        // over the process-wide pool on Linux. Neither owns a thread.
        let queue = {
            let mut dq = self.dec_queues.lock().unwrap_or_else(|e| e.into_inner());
            dq.entry(key.clone())
              .or_insert_with(|| {
                  let ch_idx = key.1 as usize;
                  static SCHEDULER: std::sync::OnceLock<std::sync::Arc<dyn super::scheduler::Scheduler>> =
                      std::sync::OnceLock::new();
                  let sched = SCHEDULER.get_or_init(|| make_scheduler(0, 128));
                  sched.decode_queue(ch_idx)
              })
              .clone()
        };

        // Lift the meter-live flag out once, here on the rare resolve path, so the cached
        // hot path can read it without touching the slot mutex.
        let meter_live = Arc::clone(&slot.lock().meter_live);

        Some(ChannelHandle { slot, queue, meter_live })
    }


    /// Dispatch a decode onto the channel's serial queue. This is the HOT path: it takes
    /// NO engine-map locks — only the cached handle's `Arc`s — dispatching straight to the
    /// channel's own queue, so the socket-readable handler does no lookup. The decode runs
    /// in the closure on the per-channel serial queue (GCD on macOS, the shared worker
    /// pool on Linux).
    pub fn dispatch_decode(&self, handle: &ChannelHandle, seq: u16, ts: u32,
                           codec: u8, opus_frame: &[u8], sender_restart: bool) {
        let slot  = handle.slot.clone();
        // Opus single-frame maximum is 1275 bytes (RFC 6716 §3.4). 1276 = spec max rounded
        // up; matches the encoder's pkt_buf sizing so encode and decode agree.
        //
        // Raw PCM is far larger: 20ms mono is 960 samples, so 1920 bytes at 16-bit
        // and 2880 at 24-bit. The staging buffer therefore
        // sizes to the raw worst case, not Opus's.
        const MAX_OPUS:    usize = 1276;
        const MAX_PAYLOAD: usize = super::spsc::FRAME_MAX * 3;   // 2880 = 24-bit @ 20ms
        let cap  = if codec == crate::net::protocol::CODEC_OPUS { MAX_OPUS } else { MAX_PAYLOAD };
        let plen = opus_frame.len().min(cap);
        let mut payload_buf = [0u8; MAX_PAYLOAD];
        payload_buf[..plen].copy_from_slice(&opus_frame[..plen]);
        handle.queue.dispatch(Box::new(move || {
            let mut s = slot.lock(); // parking_lot: infallible

            // ── Sequence-gap detection ──
            // The gap comes from the 16-bit packet SEQUENCE, not from timestamps: a packet is
            // in-order iff seq == last_seq+1 (gap 0); a forward gap is seq-(last_seq+1); a
            // duplicate (seq == last_seq) or a behind/reordered seq is dropped without
            // decoding. A sequence counter is immune to the sender-timestamp irregularity
            // that a ts-delta gap would mis-read as loss. seq_gap is in FRAMES (one seq =
            // one frame) and is used directly as the conceal count.
            //
            // §3.1: the decoder is flushed on the FIRST packet of a channel's life, and
            // whenever the sender says it has restarted. Both are the same event from the
            // decoder's point of view — the stream on the far side is discontinuous with
            // anything already in its state.
            //
            // A restart also suppresses the gap: the sequence jump across a sender restart
            // is not loss, so concealing it would run FEC against a packet from a different
            // encoder generation and pad a hole that no one dropped.
            let first_packet = s.last_seq.is_none() || sender_restart;
            let seq_gap: u32 = match s.last_seq {
                _ if sender_restart => 0,                    // announced restart: not loss
                None => 0,                                   // first packet: no gap, anchor
                Some(last) => {
                    let d = seq.wrapping_sub(last) as u16;   // 16-bit wrap-aware delta
                    if d == 0 {
                        return;                              // duplicate — drop, no decode
                    }
                    // `d` is the mod-65536 forward distance. d in [1,64000] is a genuine
                    // forward gap; only d in [64001,65535] — behind by 1 to 1535 — counts as
                    // behind/restart. A tighter threshold such as 0x8000 would wrongly reject
                    // large-but-genuinely-forward gaps, e.g. a session restart where d runs
                    // into the tens of thousands.
                    //
                    // gap_size = (seq - expectedSeq) where expectedSeq = lastSeenSeq + 1, so
                    // gap_size is d - 1, and the discard condition is gap_size > 64000. That
                    // is d > 64001, making d == 64002 the first discarded value — so the
                    // test is `d >= 0xfa02`, NOT `>= 0xfa01`, which would discard a
                    // gap_size of exactly 64000 that must still be accepted.
                    if d >= 0xfa02 {
                        // Behind or reordered packet. The RECEIVED seq is stored as the new
                        // baseline BEFORE bailing, which lets the next consecutive packet
                        // from a new session pass through. Without it, a sender whose
                        // sequence restarts near 0 (while last_seq is, say, 9600) would have
                        // every later packet read as 'behind' and dropped.
                        s.last_seq = Some(seq);
                        return;                              // behind/reordered — drop
                    }
                    (d - 1) as u32                           // forward gap in FRAMES (0 = in-order)
                }
            };
            s.last_seq = Some(seq);
            if first_packet {
                let _ = s.decoder.reset_state();
            }

            // The discontinuity flag is set from DEPTH, never from sequence-gap size. The
            // 2×setpoint bound applies to depth; the gap and that bound only look related
            // because they share a constant. Do NOT re-derive this flag from a gap's sample
            // span.
            //
            // ── Loss accounting (CASCADE_SESSION_STATS_SPEC §2.3) ──

            // Loss and jitter are NOT measured here. CASCADE_SESSION_STATS_SPEC §2.3/§2.4
            // require both to be accumulated in the packet-ARRIVAL dispatcher, "entirely
            // before any routing check" — see ArrivalStats in net/udp.rs. Timing a packet
            // here, inside the per-channel decode worker, measured the dispatch hop and the
            // slot lock as well as the network, so scheduling latency was reported as
            // network jitter; and an unrouted channel never reaches this closure at all, so
            // it accumulated no statistics whatsoever.
            //
            // `now` is still needed on this side for the discrete splice's own 0.5s rate
            // limit (CASCADE_SYNC_MECHANISM_SPEC §2.1), which is a decode-side concern.
            let now = Instant::now();

            let payload = &payload_buf[..plen];

            // ── Decode with loss concealment (CASCADE_AUDIO_RECEIVE_SPEC §3.1) ──
            // On a gap of 1-4 frames there are exactly TWO decode calls into ONE contiguous
            // buffer, back to back, and their counts are SUMMED:
            //
            //   fec_count    = decode(payload, buf[0..],         frame_size =
            //                         single_frame_samples * gap_size, decode_fec = TRUE)
            //   normal_count = decode(payload, buf[fec_count..], frame_size =
            //                         DECODE_MAX - fec_count,          decode_fec = FALSE)
            //   frame_length = fec_count + normal_count
            //
            // The FEC call is asked for the whole gap in one shot. Inside it Opus conceals
            // the earlier missing frames with PLC and rebuilds the last one from THIS
            // packet's in-band FEC (all PLC when the packet carries none). ORDER IS
            // LOAD-BEARING: the FEC call must precede the normal decode, which then appends
            // the current frame directly after it. `single_frame_samples` is read from the
            // packet's own TOC byte, so the request is sized correctly even across a sender
            // frame-size change.
            //
            // `gap_size` is the MISSING-frame count (0 = in-order) from the sequence number
            // (seq_gap above). A gap of 5 frames or more is neither concealed nor padded
            // (§5.4) — concealing a long outage would be invented audio.
            const DECODE_MAX: usize = 4800;   // decode output bound (§3.1): 100ms at 48kHz
            let mut pcm = [0f32; DECODE_MAX];
            let gap_size = seq_gap as usize;

            // ── Raw PCM: direct extraction, no Opus involved at all ───────────────────
            // Raw audio arrives on OPCODE 11 with the codec in the flags byte — 1 = Raw16
            // (a 20ms payload is exactly 1920 bytes = 960x2) and 2 = Raw24 (2880 = 960x3).
            // Opcode 11 marks raw PCM; it is not a sequence-wraparound marker.
            //
            // Samples are LITTLE-ENDIAN.
            //
            // Raw carries no in-band redundancy, so there is no FEC/PLC pass and no
            // concealment on a gap — a lost raw packet is a hole. fec_count is therefore
            // 0 by construction here, and everything downstream — the §5 tree, the boxcar,
            // the write, the splice — is shared with the Opus path unchanged.
            let raw_bytes = match codec {
                crate::net::protocol::CODEC_RAW16 => 2usize,
                crate::net::protocol::CODEC_RAW24 => 3usize,
                _ => 0usize,
            };
            let (fec_count, normal_count, lost_samples) = if raw_bytes > 0 {
                let n = (plen / raw_bytes).min(DECODE_MAX);
                if n == 0 { return; }
                if raw_bytes == 2 {
                    for (i, c) in payload[..n * 2].chunks_exact(2).enumerate() {
                        pcm[i] = i16::from_le_bytes([c[0], c[1]]) as f32 / 32_767.0;
                    }
                } else {
                    // The three bytes are the TOP three of a 32-bit sample scaled by
                    // INT32_MAX, little-endian — algebraically a plain 24-bit LE sample over
                    // 2^23, which is what this computes.
                    for (i, c) in payload[..n * 3].chunks_exact(3).enumerate() {
                        pcm[i] = i32::from_le_bytes([0, c[0], c[1], c[2]]) as f32 / 2_147_483_647.0;
                    }
                }
                // Raw carries no in-band redundancy, so a gap is never concealed and
                // never padded: `lost` stays zero and §5.4 inserts nothing.
                (0usize, n, 0usize)
            } else {
            // ── Pre-decode sanity check (CASCADE_AUDIO_RECEIVE_SPEC §3.1) ─────
            // opus_packet_get_samples_per_frame — pure TOC-byte arithmetic, no decoder
            // state and no audio produced. Reject anything claiming more than the
            // protocol's own maximum (960 = 20ms @ 48kHz) before touching the decoder.
            // The value also sizes the FEC request below.
            let single_frame_samples = match opus::packet::get_nb_samples(payload, 48_000) {
                Ok(n) if n <= super::spsc::FRAME_MAX => n,
                _ => return,                                     // malformed / >960
            };
            // §5.4: what the gap actually COST, and therefore how much silence it earns.
            // Non-zero in exactly one case — concealment was attempted and failed. A gap
            // FEC/PLC concealed has lost nothing needing replacement, and a gap of 5 frames
            // or more is never concealed and never padded.
            let mut lost_samples = 0usize;
            let fec_count = if (1..=4).contains(&gap_size) {
                let want = (single_frame_samples * gap_size).min(DECODE_MAX);
                match s.decoder.decode_float(payload, &mut pcm[..want], true) {
                    Ok(n) => n,
                    // FEC failure does NOT drop the packet (§3.1): fec_count = 0 and
                    // we fall through to the same normal decode, whose arguments then adjust
                    // themselves — it writes at offset 0 with frame_size = DECODE_MAX - 0. Only
                    // the concealed audio is lost; the arriving packet's own audio is unaffected.
                    // That loss is what §5.4's silence then stands in for.
                    Err(_) => { lost_samples = single_frame_samples * gap_size; 0 }
                }
            } else {
                0
            };
            let normal_count = match s.decoder
                .decode_float(payload, &mut pcm[fec_count..DECODE_MAX], false)
            {
                Ok(n) => n,
                Err(_) => return,
            };
            (fec_count, normal_count, lost_samples)
            };
            // Total span produced this cycle: recovered frames (if any) + the current frame.
            // This IS §5.4's frame_length — there is no separate fec_recovered term to add.
            let decoded = fec_count + normal_count;
            // Frame-size detection (§3.2) keys off the CURRENT packet's own decoded count,
            // not the concealment-inflated total. Valid sizes are {120,240,480,960}.
            if !matches!(normal_count, 120 | 240 | 480 | 960) {
                return;
            }

            // ── §9 post-buffer meter (CASCADE_AUDIO_RECEIVE_SPEC §9/§4.1) ──
            // Update the running peak HERE, at the decode/write side — the moment decoded
            // audio is produced for the ring buffer — NOT on the render/read side:
            // metering at the read side would lag by the buffer's setpoint depth and put
            // routed (this meter) and unrouted (§9.2 pre-decode) channels out of step. Metered over the
            // freshly decoded frame, before any splice/clamp mutates `pcm`, so its value
            // matches §9.2's independent decode of the same packet. Accumulate-then-snapshot
            // (fetch_max; drained by the meter snapshot task, api::spawn_meter_publisher)
            // holds the peak between snapshots.
            {
                let mut pk = 0.0f32;
                for &sm in &pcm[..decoded] { let a = sm.abs(); if a > pk { pk = a; } }
                if let Some(a) = s.meter_peaks.get(s.meter_slot) {
                    a.fetch_max(pk.to_bits(), std::sync::atomic::Ordering::Relaxed);
                    // This source is now live, so the §9.3 switch may point at it.
                    s.meter_live.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }

            // Hot ring resize on a frame-size change. Setpoint and capacity are both
            // FLOORED — to 2×frame and to the output-period floor (target_samples_floored /
            // ring_capacity_floored) — so a frame-size change can move both. A resize
            // re-anchors the timeline.
            if normal_count != s.expected_frame_samples {
                let first_sizing = s.expected_frame_samples == 0;   // 0 = unknown until now
                // Auto-adapt trigger (CASCADE_AUDIO_RECEIVE_SPEC §3.2). The setpoint is
                // reconfigured in BOTH directions, and each direction has its own test
                // against the CURRENT STORED setpoint:
                //
                //   GROW   — the detected frame exceeds half the setpoint, so the buffer
                //            genuinely isn't large enough to hold it comfortably.
                //
                //   SHRINK — the frame is now comfortably under half the setpoint AND the
                //            setpoint is still standing above what the configured buffer
                //            and the period floor give without any frame. That second half
                //            is what makes this a restore rather than a general shrink: the
                //            only way the setpoint gets above that target is a previous
                //            grow, so this undoes one and nothing else.
                //
                // Without the shrink arm a link that briefly carries a large frame keeps
                // the enlarged buffer for the rest of the session — the frame size comes
                // back down and the latency does not.
                //
                // The shrink test needs no separate `configured <= 39ms` guard: at 40ms and
                // above the configured target is already at least 1920, and the largest
                // frame (960) floors to exactly 1920, so the setpoint can never stand above
                // the target there and the condition is unreachable by arithmetic.
                //
                // Both arms take the same path below — a resize IS a reallocation and a
                // full re-anchor, not an in-place setpoint edit.
                let new_target = super::channel_sync::target_samples_floored(
                    s.buffer_ms, normal_count, s.period_floor);
                let configured = super::channel_sync::target_samples_for_buffer(s.buffer_ms)
                    .max(s.period_floor);
                let grow   = normal_count > s.setpoint / 2;
                let shrink = normal_count < s.setpoint / 2 && s.setpoint > configured;
                if first_sizing || grow || shrink {
                    let cap_samples = ring_capacity_floored(s.buffer_ms, normal_count, s.period_floor);
                    let (new_tx, new_rx) = super::spsc::channel(cap_samples);
                    s.producer = new_tx;
                    s.expected_frame_samples = normal_count;
                    // The ONLY place the setpoint changes, and it changes THIS channel's
                    // alone: it is written on the channel the call landed on, never
                    // looping over siblings (see ChannelDecodeSlot::setpoint).
                    s.setpoint = new_target;
                    // Delivered with the ring so the consumer re-floors on swap.
                    *s.swap_inbox.lock() = Some((new_rx, new_target));
                    s.last_pushed_ts = None;   // new ring (see ChannelDecodeSlot::last_pushed_ts)

                    let frame_ms = match normal_count { 120=>"2.5", 240=>"5", 480=>"10", _=>"20" };
                    // This closure runs per channel, but every channel of a peer detects the
                    // same size. The shared peer_frame_size swap lets only the FIRST channel
                    // to see a new size log at info/warn; the rest (prev == normal_count)
                    // drop to debug.
                    let prev = s.peer_frame_size.swap(normal_count, Ordering::Relaxed);
                    // NOTHING is notified here, deliberately. A detected incoming frame-size
                    // change is an internal, silent, per-connection adaptation — it widens
                    // this channel's own ring and setpoint and stops there. It is NOT a
                    // buffer-setting change and must never reach the CoreAudio reconcile.
                    //
                    // The separation is structural: the value the callback-period
                    // computation minimises over is the user's `bufferSize` setting, while
                    // auto-adapt resizes this channel's own ring and setpoint in either
                    // direction. There is no path between them, so a sender changing frame
                    // size mid-stream cannot move the callback period, and there is no
                    // rebuild on this path.
                    //
                    // Firing the shared period notify here would wake the SAME reconcile the
                    // UI's buffer-size control uses, rebuilding both CoreAudio units on every
                    // sender frame-size change — and the rebuild window costs receive depth
                    // that §5.1 has no mechanism to recover, stranding the buffer far below
                    // setpoint after e.g. a 20ms -> 2.5ms switch.
                    //
                    // Genuine buffer-setting changes still reconcile, from their own path
                    // (main.rs's set_buffer handler), which is the only thing that should.
                    if prev == normal_count {
                        tracing::debug!(
                            "incoming frame size {}ms — ring resized to {} samples (dup channel)",
                            frame_ms, cap_samples);
                    } else if first_sizing {
                        tracing::info!(
                            "incoming frame {} ms ({} samples) — prebuffering to {:.0} ms \
                             setpoint (buffer setting {} ms)",
                            frame_ms, normal_count, new_target as f32 / 48.0, s.buffer_ms);
                        let _ = cap_samples;
                    } else {
                        // The one frame-change case that interrupts audio: the ring is
                        // reallocated and the prebuffer re-arms, so there is a refill gap.
                        tracing::warn!(
                            "incoming frame {}ms ({} samples): ring → {} samples, \
                             setpoint → {:.0}ms, prebuffer re-armed",
                            frame_ms, normal_count, cap_samples, new_target as f32 / 48.0);
                    }
                } else {
                    // Setpoint unchanged (buffer-dominated): the ring geometry still fits, so
                    // no reallocation and no flush — just track the new frame size.
                    s.expected_frame_samples = normal_count;
                    let prev = s.peer_frame_size.swap(normal_count, Ordering::Relaxed);
                    if prev != normal_count {
                        // Once per peer-level change (this closure runs per channel;
                        // the shared peer_frame_size swap dedupes so only the first
                        // channel to see the new size logs it).
                        let frame_ms = match normal_count { 120=>"2.5", 240=>"5", 480=>"10", _=>"20" };
                        let prev_ms  = match prev    { 120=>"2.5", 240=>"5", 480=>"10", 960=>"20", _=>"?" };
                        // Ring geometry only — the callback period never depends on the
                        // incoming frame size (see the resize arm above).
                        tracing::info!(
                            "incoming frame size {}ms → {}ms: ring kept ({:.0}ms setpoint, \
                             buffer setting {}ms)",
                            prev_ms, frame_ms, s.setpoint as f32 / 48.0, s.buffer_ms);
                    }
                }
            }

            // baseTS = header_ts − decoded_count.
            let base_ts = ts.wrapping_sub(decoded as u32);

            // ── Post-decode buffer management (CASCADE_AUDIO_RECEIVE_SPEC §5) ──
            // Runs after each successful decode, before the frame is committed.
            // This is the mechanism that keeps the buffer at its target depth under
            // both clean and lossy conditions.
            // STORED setpoint — not re-derived here, so it cannot drift away from the
            // consumer's own target on a frame change that doesn't resize the ring.
            let target = s.setpoint;
            let overrun_ceiling = target * 2;
            let depth = s.producer.samples_buffered();
            s.depth_high_water = s.depth_high_water.max(depth);

            // Exactly one of three branches runs per packet:
            //
            //   HOLD      the prebuffer hold is armed, or the ring has drained to empty
            //             (which re-arms it): pad a gap, fill toward the setpoint keeping
            //             the tail, and release on reaching it.
            //   RECOVERY  an overrun is pending: bail while depth is at or above the
            //             setpoint; below it, pad a gap, write the frame whole, clear the
            //             overrun and discard the averaging history.
            //   NORMAL    everything else: bail on a new overrun, pad a gap, then measure,
            //             run the relay, splice and write.
            //
            // Only NORMAL feeds the §2.2 window, and only NORMAL can splice. A held packet,
            // the packet that releases the hold and the packet that recovers from an overrun
            // are written without being measured; the last two also reset the window, so the
            // next measurement starts a clean history.

            if s.prebuffer_hold.load(std::sync::atomic::Ordering::Relaxed) || depth == 0 {
                // ── HOLD ──
                if !s.prebuffer_hold.load(std::sync::atomic::Ordering::Relaxed) {
                    // Buffer genuinely drained to empty — active recovery: re-arm the
                    // prebuffer hold and treat everything from here as a fresh join.
                    s.depth_high_water = 0;
                    s.discontinuity = false;
                    s.prebuffer_hold.store(true, std::sync::atomic::Ordering::Relaxed);
                    // Rate-limited: one line per second per channel, carrying the count. See
                    // ChannelDecodeSlot::drain_count for why per-event logging is harmful here.
                    s.drain_count += 1;
                    let due = s.drain_logged.map_or(true, |t| now.duration_since(t).as_secs_f64() >= 1.0);
                    if due {
                        tracing::debug!("buffer drained to empty x{} — prebuffer hold re-armed \
                                         (silent refill to target)", s.drain_count);
                        s.drain_logged = Some(now);
                        s.drain_count = 0;
                    }
                }
                // A gap is padded exactly as on the normal path: only what concealment failed
                // to recover, capped at the setpoint's headroom (§5.4, `gap_silence`). With no
                // loss this is nothing, and the buffer refills from real frames alone.
                let silence = super::channel_sync::gap_silence(target, depth, decoded, lost_samples);
                if silence > 0 {
                    // Contiguous with the timeline, immediately before this frame.
                    let pad_ts = base_ts.wrapping_sub(silence as u32);
                    s.producer.write_silence(silence, pad_ts);
                }
                // §5 Path A: the prebuffer fill stops ON the setpoint, and it is the TAIL of
                // the frame that is kept — the opposite end from every other path. See
                // `prebuffer_keep`.
                //
                // The base timestamp is passed UNADJUSTED even though the retained samples
                // begin `skip` into the frame. Per-sample timestamps drive the sync read
                // cursor (§4.1), and advancing the base here would move that cursor during
                // a prebuffer fill — a correction applied to a channel that is not being
                // read yet. The inaccuracy is bounded by one frame and is gone as soon as
                // the hold releases.
                let depth = s.producer.samples_buffered();
                let keep = super::channel_sync::prebuffer_keep(target, depth, decoded);
                let skip = decoded - keep;
                s.producer.write_samples(&pcm[skip..decoded], base_ts, keep);
                s.last_pushed_ts = Some(base_ts.wrapping_add(decoded as u32));

                // ── Gate 1 release (CASCADE_AUDIO_RECEIVE_SPEC §4.2 / §3.1) ──
                // Release when accumulated depth has reached setpoint; it then clears the hold
                // together with the averaging window's running sum and circular index (§3.1),
                // so a joining channel's post-release averaging starts clean rather than
                // carrying pre-release history into the §2.3 deadband relay.
                //
                // Gate 1 governs BOTH read paths (CASCADE_SYNC_MECHANISM_SPEC §3.2): a
                // sync-on join holds until setpoint exactly as a sync-off one does.
                //
                // That shared release is what produces cross-channel alignment: every channel
                // of a source shares the sender's clock, so releasing them all at the same
                // depth relative to that clock puts their read positions — and their
                // merge timestamps — at the same point the moment they release. The servo
                // then has drift to correct, not a join offset. Implementing a separate,
                // threshold-free join for the sync path is what the spec warns against.
                if s.producer.samples_buffered() >= target {
                    s.prebuffer_hold.store(false, std::sync::atomic::Ordering::Relaxed);
                    s.cm.reset();
                    // Both direction flags go with the window. They are consumed on opposite
                    // sides — the depth relay's here, §6.6's on the render side — but they are
                    // both derived from the history being discarded, so leaving either armed
                    // would let a decision taken against the old average survive into the new
                    // one.
                    s.cm_dir_shared.store(0, std::sync::atomic::Ordering::Relaxed);
                    s.skew_flag.store(0, std::sync::atomic::Ordering::Relaxed);
                    tracing::debug!("gate1 released at depth {} (setpoint {}) — boxcar window reset",
                                    s.producer.samples_buffered(), target);
                }
                return;
            }

            if s.discontinuity {
                // ── RECOVERY — pending correction from an earlier overrun (§5) ──
                // The threshold here is SETPOINT, not the ceiling that originally set the
                // flag — the correction waits for depth to come all the way back down to
                // target, not merely back inside the ceiling.
                if depth >= target {
                    // Bail: no write of any kind this cycle, and the flag is deliberately
                    // NOT cleared, so the next packet re-checks the same condition. That is
                    // what makes this repeat every cycle until depth genuinely drops below
                    // setpoint — each bail discards a packet's worth of supply while the
                    // render side keeps draining, which is the mechanism that walks depth
                    // back down. Clearing here (or writing anyway) would strand the buffer
                    // in overrun with nothing scheduled to reduce it.
                    s.last_pushed_ts = Some(base_ts.wrapping_add(decoded as u32));
                    return;
                }
                // Depth has dropped below the setpoint. The frame is written whole, preceded
                // only by padding for a gap — what concealment failed to recover, capped at the
                // headroom — exactly as on the normal path. Nothing tops the buffer up toward
                // the setpoint: depth rebuilds from real audio.
                let silence = super::channel_sync::gap_silence(target, depth, decoded, lost_samples);
                if silence > 0 {
                    let pad_ts = base_ts.wrapping_sub(silence as u32);
                    s.producer.write_silence(silence, pad_ts);
                }
                s.producer.write_samples(&pcm[..decoded], base_ts, decoded);
                s.last_pushed_ts = Some(base_ts.wrapping_add(decoded as u32));
                s.discontinuity = false;
                // The averaging history is discarded along with the overrun that produced
                // it. Every depth in the window was recorded while the buffer was over the
                // ceiling, so carrying them forward would leave the average reading an
                // inflated buffer for three seconds after the buffer itself had recovered
                // — and §2.3 re-latches its reference on the next fill, so that stale
                // history is what the reference would be calibrated against.
                //
                // The direction is cleared with it: a relay armed against the pre-overrun
                // depth has nothing left to act on. Both direction flags go with the window.
                s.cm.reset();
                s.cm_dir_shared.store(0, std::sync::atomic::Ordering::Relaxed);
                s.skew_flag.store(0, std::sync::atomic::Ordering::Relaxed);
                return;
            }

            // ── NORMAL ──
            if depth >= overrun_ceiling {
                // Overrun: a same-cycle DISCARD, not a deferred correction (§5). Set the
                // flag and return immediately — no write helper runs for this packet at
                // all. The flag is for the NEXT packet's own bail check above, which uses the
                // lower setpoint threshold.
                //
                // This bail — not the §2.1 splice — is what regulates depth at frame sizes
                // where the splice cannot fire (§12). Without it, depth ran to the full
                // ring allocation: 2280 samples resting, 47.5ms on a 5ms setting.
                s.discontinuity = true;
                // Discarded, not un-sent: the recorded end timestamp still advances, as on
                // the write paths.
                s.last_pushed_ts = Some(base_ts.wrapping_add(decoded as u32));
                return;
            }
            if seq_gap >= 1 {
                // GAP-GATED silence (CASCADE_AUDIO_RECEIVE_SPEC §5.4): silence is never a
                // reaction to depth alone — it runs only when a real sequence gap was
                // detected this packet, so jitter with no loss inserts nothing. It replaces
                // what the gap actually lost (`lost_samples`, non-zero only when the FEC call
                // failed), capped by the setpoint headroom left after this cycle's `decoded`
                // span — the FEC-recovered frames and the current frame, already summed
                // above (§3.1). See `gap_silence`.
                //
                // No second bound against the overrun ceiling: the silence can never
                // exceed the setpoint headroom, and the setpoint is below the ceiling.
                let silence = super::channel_sync::gap_silence(
                    target, depth, decoded, lost_samples);
                if silence > 0 {
                    let pad_ts = base_ts.wrapping_sub(silence as u32);
                    s.producer.write_silence(silence, pad_ts);
                    tracing::debug!(
                        "gap recovery: seq_gap={} (fec-recovered {} of {} decoded), {} samples \
                         lost, inserted {} silence into a {}ms buffer (depth was {})",
                        seq_gap, fec_count, decoded, lost_samples, silence, s.buffer_ms, depth);
                }
            }

            // ── Packet-arrival boxcar + deadband/hysteresis (spec §2.2/§2.3) ──
            // PRODUCER SIDE: feed the window (keyed to the SETPOINT — 1200/600/300/150
            // entries at a 2.5/5/10/20ms setpoint, three seconds' worth in each case, §2.2),
            // compare against the reference with the state-selected deadband, and publish the
            // direction for render to sum. Computed on every normal packet regardless of
            // sync — only APPLICATION is sync-gated.
            //
            // It runs BEFORE the splice below, so the splice acts on the direction this very
            // packet's measurement produced.
            //
            // §6.5 reset_averaging: render requests a reset when the ladder is correcting
            // while the common mode is disengaged. It is honoured here, on the normal path
            // only, and the packet that honours it is not measured — the refilled window
            // starts with the next one.
            let sync_on = s.sync.load(std::sync::atomic::Ordering::Relaxed);
            if s.boxcar_reset.swap(false, std::sync::atomic::Ordering::Relaxed) {
                s.cm.reset();
                // Both direction flags go with the window. They are consumed on opposite
                // sides — the depth relay's here, §6.6's on the render side — but they are
                // both derived from the history being discarded, so leaving either armed
                // would let a decision taken against the old average survive into the new
                // one.
                s.cm_dir_shared.store(0, std::sync::atomic::Ordering::Relaxed);
                s.skew_flag.store(0, std::sync::atomic::Ordering::Relaxed);
            } else {
                // §2.2: the window is fed `depth + frame_len` — depth after any gap padding,
                // plus the frame AS DECODED, before any splice — not the ring's content after
                // the write. The two differ when the write is truncated, and a splice's own
                // change is folded back in by `note_splice` once it has happened.
                //
                // This distinction is the whole mechanism. An average built from post-write
                // depth is bounded by the ring, so on a small buffer it could never reach the
                // DRAIN threshold (`reference + min(deadband, setpoint/2)`) — the deadband
                // would sit above a ceiling the value can never cross, and the Sync-off
                // splice, the ONLY correction Path B has, could never fire. Feeding the
                // unbounded sum makes it reachable.
                let measured = s.producer.samples_buffered() + decoded;
                // §2.2: keyed to the SETPOINT, so this is a no-op on every packet whose
                // setpoint has not moved. Keying it to the incoming frame size instead resets
                // the window on a frame-size switch, which re-arms §2.3's latch and lets the
                // reference be re-captured at the depth the switch disturbed it to — see
                // `set_window`. A re-key clears both direction flags, like every other reset.
                if s.cm.set_window(target) {
                    s.skew_flag.store(0, std::sync::atomic::Ordering::Relaxed);
                }
                // Deadband base (spec §2.3): ±12ms with Sync on, ±4ms with Sync off.
                // Clamped to setpoint/2 inside update().
                let band = if sync_on {
                    super::channel_sync::DEADBAND_SYNC_ON
                } else {
                    super::channel_sync::DEADBAND_SYNC_OFF
                };
                let dir = s.cm.update(measured, target, band);
                s.cm_dir_shared.store(dir, std::sync::atomic::Ordering::Relaxed);
            }

            // ── Mechanism B: producer-side zero-crossing splice (sync-off only,
            // CASCADE_SYNC_MECHANISM_SPEC §2.1) — rate-limited to one per 0.5s,
            // driven by the direction the relay just set. TIMELINE: the ring's timestamps
            // stay anchored to the sender's clock regardless of splice — they advance by the
            // ORIGINAL decoded count; only the written count changes.
            let orig_decoded = decoded;
            let mut pcm = pcm;
            let mut written = decoded;
            let throttle_ok = match s.last_splice_time {
                None    => true,
                Some(t) => now.duration_since(t).as_secs_f64() >= 0.5,
            };
            if throttle_ok && !sync_on {
                let dir = s.cm.dir();
                if dir != 0 {
                    written = super::channel_sync::zero_crossing_splice(&mut pcm, decoded, dir);
                    // §2.1: a splice counts — restarting the 0.5s timer, and folded out of the
                    // window — only when it changed the length and the window is full. The
                    // relay cannot be armed with the window unfilled, so the second test
                    // never refuses one here; it states the rule rather than guarding a case.
                    //
                    // The magnitude is the absolute change: DRAIN shortens the frame and
                    // FILL lengthens it, and the window correction takes the size of the
                    // change with the direction supplied separately.
                    if written != decoded && s.cm.filled() {
                        let moved = written.abs_diff(decoded);
                        s.last_splice_time = Some(now);
                        s.cm.note_splice(moved, dir);
                        // DIAGNOSTIC (temporary): every splice, with what it moved.
                        tracing::debug!(
                            "SPLICE ch{} {} {} samples ({:.2}ms) | ring before {:.1}ms  \
                             avg {:.1}ms  calref {:?}",
                            s.meter_slot, if dir > 0 { "added" } else { "dropped" }, moved,
                            moved as f64 / 48.0,
                            s.producer.samples_buffered() as f64 / 48.0,
                            s.cm.avg() as f64 / 48.0,
                            s.cm.calref().map(|c| (c as f64 / 48.0 * 10.0).round() / 10.0));
                    }
                }
            }

            // ── Commit the decoded frame, bounded by the RING ──
            // The write is bounded by physical ring CAPACITY, never by the setpoint. On
            // overflow the FRONT of the frame is what survives: the ring helper writes the
            // first `free_room` samples and discards the rest. The head is kept and the TAIL
            // is truncated.
            //
            // Bounding at the setpoint instead does two harmful things at once:
            //
            //   1. It DESTROYS AUDIO. Depth could never exceed the setpoint, so any two
            //      writes landing between two reads discard a whole frame. Each discarded
            //      frame is supply permanently lost, which starves the ring a cycle later
            //      and starts a cascade.
            //
            //   2. It SUPPRESSES THE CORRECTION that exists to prevent exactly that.
            //      DRAIN arms at `reference + band`; with depth pinned at the setpoint the
            //      boxcar average could never sustain a value up there, so the splice
            //      would never engage. DRAIN is meant to sit armed continuously — active
            //      per-packet trimming is what holds the level, and a hard bound cannot
            //      do that job.
            //
            // So depth is free to rise above the setpoint, the boxcar sees it, DRAIN
            // engages, and the splice trims. That is the intended steady state.
            //
            // `orig_decoded` is the length the sender transmitted, captured before the
            // splice. It differs from `written` only when the splice changed the frame,
            // and the ring uses it to stop a fill's extra samples carrying the per-sample
            // timeline past what was actually sent. The recorded end timestamp follows the
            // SENDER clock too. See ChannelDecodeSlot::last_pushed_ts.
            s.producer.write_samples(&pcm[..written], base_ts, orig_decoded);
            s.last_pushed_ts = Some(base_ts.wrapping_add(orig_decoded as u32));

            // ── DIAGNOSTIC (temporary): drift-servo state, 1/s per channel ────────
            // Shows whether DRAIN is arming and against what. Everything is printed in
            // ms so it lines up with the buffer figure in the UI. Remove with the
            // sync-servo investigation.
            {
                let due = s.diag_last.map_or(true, |t| now.duration_since(t).as_secs_f64() >= 1.0);
                if due {
                    s.diag_last = Some(now);
                    let ms = |v: i64| v as f64 / 48.0;
                    let dir = s.cm.dir();
                    let avg = s.cm.avg();
                    // The EFFECTIVE band: update() clamps the base to setpoint/2, so at
                    // small buffers the base alone overstates it (5ms setpoint clamps
                    // ±4ms down to ±2.5ms).
                    let band = if sync_on {
                        super::channel_sync::DEADBAND_SYNC_ON
                    } else {
                        super::channel_sync::DEADBAND_SYNC_OFF
                    }.min(target as i64 / 2);
                    match s.cm.calref() {
                        Some(cr) => tracing::debug!(
                            "SERVO ch{} sync={} state={} | avg {:.1}ms  calref {:.1}ms  \
                             band ±{:.1}ms  arm≥{:.1}ms  release<{:.1}ms | setpoint {:.1}ms  \
                             ring {:.1}ms",
                            s.meter_slot, if sync_on {"on "} else {"off"},
                            match dir { -1 => "DRAIN", 0 => "idle ", _ => "fill " },
                            ms(avg), ms(cr), ms(band), ms(cr + band), ms(cr),
                            ms(target as i64), ms(s.producer.samples_buffered() as i64)),
                        None => tracing::debug!(
                            "SERVO ch{} sync={} state=GATED (window {}filled, no calref yet) | \
                             avg {:.1}ms  setpoint {:.1}ms  ring {:.1}ms",
                            s.meter_slot, if sync_on {"on "} else {"off"},
                            if s.cm.filled() {""} else {"not "},
                            ms(avg), ms(target as i64),
                            ms(s.producer.samples_buffered() as i64)),
                    }
                }
            }
        }));
    }


    /// Return a cloneable handle to the stat accumulator map.
    /// Peer tasks hold this Arc directly — a direct handle rather than routing
    /// through Arc<AudioEngine>.
    pub fn stat_acc_handle(&self)
        -> Arc<Mutex<HashMap<String, Arc<StatAccumulator>>>> {
        Arc::clone(&self.stat_acc)
    }

    /// Get-or-create a peer's stats accumulator.
    ///
    /// Exists so the receive thread can resolve the accumulator ONCE per peer and cache the
    /// Arc: CASCADE_SESSION_STATS_SPEC §2.3/§2.4 require loss and jitter to be accumulated
    /// in the packet-arrival dispatcher, which runs per packet and must not take this map's
    /// lock at that rate.
    pub fn stat_acc_for(&self, peer_name: &str) -> Arc<StatAccumulator> {
        self.stat_acc.lock().unwrap_or_else(|e| e.into_inner())
            .entry(peer_name.to_string())
            .or_insert_with(StatAccumulator::new)
            .clone()
    }

    /// Set the per-peer Sync (phase-lock) flag. Writes an AtomicBool shared with the render
    /// closure and the decode slots — no lock needed on the audio paths.
    pub fn set_phase_lock(&self, peer_name: &str, enabled: bool) {
        if let Ok(mut sf) = self.sync_flags.lock() {
            // Create the flag if this peer has no entry yet — there is none until the
            // first packet, and enabling phase lock before audio flows must not be lost.
            // The receive path uses entry().or_insert_with() and so SHARES whatever
            // flag we create here, so the desired state survives until audio starts.
            let flag = sf.entry(peer_name.to_string())
                .or_insert_with(|| Arc::new(AtomicBool::new(false)));
            let was = flag.swap(enabled, Ordering::Relaxed);
            if was != enabled {
                debug!("phase lock '{}': {}", peer_name,
                       if enabled { "on" } else { "off" });
            }
        }
    }

    /// Live count of received channels actually being DECODED for CONNECTED peers — i.e.
    /// routed to at least one playable output AND from a peer still connected. dec_slots holds
    /// every routed+decoding channel (receive() drops unrouted ones before slot creation), but
    /// a slot is NOT torn down the moment its peer disconnects (the peer just stops sending;
    /// the jitter buffer is deliberately left warm so a brief blip resumes seamlessly). So we
    /// filter by the connected set here rather than tearing the decode state down on
    /// disconnect — the count drops immediately, the warm buffer survives for fast reconnect.
    /// This is the RX counterpart to the capture engine's live_send_streams (TX), which is
    /// likewise connected-gated. Summed across connected peers.
    pub fn active_recv_channels(&self, connected: &std::collections::HashSet<String>) -> usize {
        self.dec_slots.lock().unwrap_or_else(|e| e.into_inner())
            .keys().filter(|(peer, _)| connected.contains(peer)).count()
    }
}



