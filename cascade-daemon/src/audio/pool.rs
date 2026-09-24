//! Decode slot and stats accumulator.
//!
//! ChannelDecodeSlot owns the opus decoder and the SPSC producer for one incoming
//! channel. Held under Arc<parking_lot::Mutex>, so one decode is active per channel at
//! a time and there is no contention.
//!
//! Decode logic lives in engine.rs (`dispatch_decode`). Loss concealment for a 1-4 frame
//! gap uses Opus FEC/PLC rather than silence: one decode with decode_fec=1 sized to the whole
//! gap, then the normal decode (CASCADE_AUDIO_RECEIVE_SPEC §3.1). Silence replaces only
//! concealment that failed, and a gap of 5 frames or more is neither concealed nor padded
//! (§5.4). baseTS = header_ts − decoded_count directly, with no ts_step inference.

use parking_lot::Mutex as ParkingMutex;
use std::sync::Arc;
use std::time::Instant;

use super::spsc;

// ── Decode slot ───────────────────────────────────────────────────────────────

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::AtomicI8;
use super::channel_sync::SwapInbox;

/// Shared per-peer frame-size tracker. Lets all channels of a peer
/// coordinate: the first channel to detect a change logs at WARN,
/// the rest at DEBUG (they all still resize their own ring independently).
pub type PeerFrameSize = Arc<AtomicUsize>;

/// Shared per-channel common-mode drift direction, written by the decode worker
/// (producer side, per packet) and read+summed by the render callback
/// (consumer side). -1 = drain (depth
/// high), +1 = fill (depth low), 0 = within deadband. One AtomicI8 is the ENTIRE
/// cross-thread surface of the drift corrector; the window itself stays private
/// to the decode slot (single-threaded per channel under the slot mutex).
pub type DriftDir = Arc<AtomicI8>;
/// Shared prebuffer-hold flag (CASCADE_AUDIO_RECEIVE_SPEC §4.2): true = all
/// read-side consumers hold (no reads) until the buffer has filled to setpoint.
/// Armed at construction (fresh join), re-armed by the producer on depth==0
/// (§5's drained-to-empty branch) and by the consumer on a ring swap; cleared
/// by the consumer at release (depth ≥ setpoint).
pub type PrebufferHold = Arc<std::sync::atomic::AtomicBool>;

pub struct ChannelDecodeSlot {
    pub decoder:  opus::Decoder,
    pub producer: spsc::Producer,
    /// Incoming frame size in samples, as last observed on the wire. 0 until the first
    /// packet is decoded. Updated on each resize so the check fires at most once per
    /// sender change.
    pub expected_frame_samples: usize,
    /// Playback setpoint in SAMPLES — STORED state, not derived per packet
    /// (CASCADE_AUDIO_RECEIVE_SPEC §3.2), and PER-CHANNEL, not per-source.
    ///
    /// Per-channel is deliberate: a resize writes the setpoint on the one channel the call
    /// landed on, with no loop over siblings and no source-level structure touched. Two
    /// channels of one source receiving different frame sizes genuinely end up with
    /// different setpoints, and nothing brings them back
    /// together. CASCADE_SYNC_MECHANISM_SPEC §2.3's "every channel of a source uses the
    /// identical setpoint" therefore carries an implied qualifier — it holds while frame
    /// sizes match across the source's channels, which is the ordinary case, and the
    /// cross-channel alignment §2.3 derives from it holds under the same condition.
    ///
    /// Written ONLY where the ring is resized, and it must NOT be derived from
    /// (buffer_ms, expected_frame_samples) per packet. Deriving it moves the value on the
    /// "buffer is already large enough" path, where no resize happens — the producer's clamp
    /// target then drifts away from the consumer's own `target_samples`, which only updates
    /// on a ring swap. The two disagree and playback stalls.
    pub setpoint: usize,
    /// User's buffer_ms for this peer — needed to compute new ring capacity.
    pub buffer_ms: u32,
    /// The setpoint floor the callback period imposes, in samples — see
    /// `AudioEngine::period_floor`. Set when the slot is created and rewritten, with the
    /// ring resized if the setpoint moves, whenever that floor changes.
    pub period_floor: usize,
    /// Shared mailbox for hot ring-consumer swaps.
    pub swap_inbox: SwapInbox,
    /// Per-peer deduplication: only the first channel per peer to detect a
    /// frame-size change logs at WARN; others log at DEBUG.
    pub peer_frame_size: PeerFrameSize,
    /// Shared prebuffer-hold flag (spec §4.2) — see the PrebufferHold type doc.
    pub prebuffer_hold: PrebufferHold,
    /// Discontinuity flag (spec §4/§5): set when a sequence gap exceeds the FEC
    /// tolerance (>4 frames) or the overrun detector fires; the NEXT packet's
    /// buffer-management cycle handles it (calculated silence fill) and clears it.
    pub discontinuity: bool,
    /// Drain-to-empty events since the last log line, and when that line was emitted.
    /// The log is rate-limited because it fires per drain per channel from the decode
    /// worker: at a high drain rate the blocking stdout write delays the very decode that
    /// would have refilled the ring, causing another drain — the log amplifying itself.
    pub drain_count: u32,
    pub drain_logged: Option<Instant>,
    /// Running maximum of observed fill depth (spec §4). Reset when the buffer
    /// drains to empty (§5's depth==0 branch).
    pub depth_high_water: usize,
    /// Last pushed frame's END timestamp (ts + decoded_count) = the ts the NEXT
    /// contiguous frame should carry. None until the first frame is pushed, and after a
    /// ring replacement. Recorded only: loss gaps are measured from the packet sequence
    /// number, and nothing reads this.
    pub last_pushed_ts: Option<u32>,

    /// Last accepted packet SEQUENCE number (wire header bytes 2-3, u16). Used to drop
    /// duplicate packets: a repeat of the stored sequence bails before decoding. Without
    /// this a duplicate is re-decoded and re-written, over-advancing the ring on links that
    /// duplicate packets (some WiFi/AP setups). None until the first packet.
    pub last_seq: Option<u16>,

    // ── Common-mode drift corrector (producer side) ───────────────────────────
    // 150-sample BOXCAR window of buffer depth, sampled once per pushed packet (depth
    // AFTER this frame). Computed here in the decode worker, per channel, per packet.
    // PRIVATE to this slot (single-threaded under the slot mutex); only cm_dir_shared
    // crosses to the render thread.
    pub cm: super::channel_sync::DriftWindow,
    /// Cross-thread output: the current direction, read+summed by render.
    pub cm_dir_shared: DriftDir,
    /// §6.6's per-channel adjusting flag. Computed by the channel's resample job and
    /// consumed by render, but shared so this side can clear it: every reset of the
    /// averaging window clears BOTH direction flags, and the resets happen here.
    pub skew_flag: DriftDir,
    /// §6.5 reset_averaging request (render → producer): stored by the render thread on
    /// every Sync-on dispatch — true while the six-tier ladder is correcting and the
    /// common mode is disengaged. The producer takes it on its next normal-path packet,
    /// resets the boxcar window (both direction flags → 0), and does not measure that
    /// packet.
    pub boxcar_reset: Arc<std::sync::atomic::AtomicBool>,
    /// Peer Sync flag — gates the producer-side zero-crossing splice
    /// (mechanism B). The splice runs only when sync is OFF and the adjusting flag is set,
    /// rate-limited to once per 0.5 s. Under sync ON the splice's internal gate forces a
    /// plain copy and
    /// alignment is done by the ladder + resampler ratio instead. The depth window runs
    /// in both modes; only the splice application differs.
    pub sync: Arc<std::sync::atomic::AtomicBool>,
    /// Last time a zero-crossing splice counted — changed the frame's length with the
    /// averaging window full — for the 0.5 s rate limit (`now - last >= 0.5s`). None until
    /// the first. Producer-thread only.
    pub last_splice_time: Option<Instant>,

    /// §9 post-buffer peak meter (CASCADE_AUDIO_RECEIVE_SPEC §9/§4.1). The per-peer,
    /// slot-indexed peak array (bit-cast f32) shared with the render group + the API.
    /// The meter is updated HERE, at the decode/ring-buffer WRITE side — NOT on the
    /// render/read side — so a routed channel's meter fires at packet arrival, the same
    /// instant as §9.2's pre-decode meter for unrouted channels. Metering at the read
    /// side instead introduces a buffer-depth timing lag between routed and unrouted
    /// channels (§9's "critical timing clarification").
    pub meter_peaks: Arc<Vec<std::sync::atomic::AtomicU32>>,
    /// This channel's index into `meter_peaks`.
    pub meter_slot: usize,
    /// DIAGNOSTIC (temporary): last time the drift-servo state was logged, for the 1/s
    /// rate limit. Remove with the sync-servo investigation.
    pub diag_last: Option<Instant>,
    /// False until this channel's post-buffer meter has written at least once.
    ///
    /// The §9.3 read-time switch picks post-buffer for a routed channel and pre-decode for
    /// an unrouted one. Those two are published from DIFFERENT threads: `routed` is set
    /// synchronously on the receive thread the moment a decode is dispatched, while the
    /// post-buffer peak is written asynchronously on this channel's decode queue. So for at
    /// least one queue hop after a channel becomes routed, the selector points at a source
    /// that has no sample yet, and the meter reads zero for a poll cycle — a visible drop to
    /// silence at the routing moment.
    ///
    /// The receive path therefore gates `routed` on THIS flag rather than on the dispatch,
    /// so the switch happens when the new source is actually live. Set once, never cleared:
    /// a channel that stops being routed loses its whole slot, and a fresh slot starts false
    /// again, which is correct.
    pub meter_live: Arc<std::sync::atomic::AtomicBool>,
}

impl ChannelDecodeSlot {
    pub fn new(producer: spsc::Producer,
               expected_frame_samples: usize,
               buffer_ms: u32,
               period_floor: usize,
               swap_inbox: SwapInbox,
               peer_frame_size: PeerFrameSize,
               cm_dir_shared: DriftDir,
               skew_flag: DriftDir,
               prebuffer_hold: PrebufferHold,
               boxcar_reset: Arc<std::sync::atomic::AtomicBool>,
               skew_ref: Arc<std::sync::atomic::AtomicI64>,
               sync: Arc<std::sync::atomic::AtomicBool>,
               meter_peaks: Arc<Vec<std::sync::atomic::AtomicU32>>,
               meter_slot: usize,
               meter_live: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            decoder: opus::Decoder::new(48_000, opus::Channels::Mono)
                         .expect("opus decoder"),
            producer,
            expected_frame_samples,
            // Initial stored setpoint from the configured buffer, the frame hint (0 = frame
            // not yet observed, which the floor treats as "no 2×frame constraint yet") and the
            // output-period floor.
            setpoint: super::channel_sync::target_samples_floored(
                buffer_ms, expected_frame_samples, period_floor),
            buffer_ms,
            period_floor,
            swap_inbox,
            peer_frame_size,
            prebuffer_hold,
            discontinuity: false,
            drain_count: 0,
            drain_logged: None,
            depth_high_water: 0,
            boxcar_reset,
            last_pushed_ts:    None,
            last_seq:          None,
            cm:        super::channel_sync::DriftWindow::new(skew_ref),
            cm_dir_shared,
            skew_flag,
            sync,
            last_splice_time: None,
            meter_peaks,
            meter_slot,
            diag_last: None,
            meter_live,
        }
    }
}

pub type DecodeSlot = Arc<ParkingMutex<ChannelDecodeSlot>>;

// ── Stats accumulator ─────────────────────────────────────────────────────────

pub struct StatAccumulator {
    received:      std::sync::atomic::AtomicU64,
    lost:          std::sync::atomic::AtomicU64,
    jitter_max_us: std::sync::atomic::AtomicU64,
}

impl StatAccumulator {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            received:      std::sync::atomic::AtomicU64::new(0),
            lost:          std::sync::atomic::AtomicU64::new(0),
            jitter_max_us: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn record(&self, received: u64, lost: u64, jitter_ms: f64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.received.fetch_add(received, Relaxed);
        self.lost.fetch_add(lost, Relaxed);
        let jitter_us = (jitter_ms * 1000.0) as u64;
        let mut cur = self.jitter_max_us.load(Relaxed);
        while jitter_us > cur {
            match self.jitter_max_us.compare_exchange_weak(
                cur, jitter_us, Relaxed, Relaxed) {
                Ok(_)  => break,
                Err(v) => cur = v,
            }
        }
    }

    pub fn drain(&self) -> (u64, u64, f64) {
        use std::sync::atomic::Ordering::Relaxed;
        let r = self.received.swap(0, Relaxed);
        let l = self.lost.swap(0, Relaxed);
        let j = self.jitter_max_us.swap(0, Relaxed) as f64 / 1000.0;
        (r, l, j)
    }
}

