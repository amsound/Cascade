//! Audio receive pipeline.
//!
//! Ring: a flat per-sample circular buffer, not fixed-slot. The write index is a running
//!   sample index: each write puts exactly `decoded_count` samples in and advances by
//!   that amount. There are no fixed 960-sample slots.
//!
//! Timestamps: baseTS = header_ts − decoded_count, and per-sample ts = baseTS +
//!   sampleIndex. No ts_step state, and nothing is inferred.
//!
//! Loss concealment (CASCADE_AUDIO_RECEIVE_SPEC §3.1, §5.4): for a 1-4 frame gap, one
//!   decode_fec call sized to the whole gap (Opus runs PLC for the earlier missing frames
//!   and rebuilds the last from the packet's in-band FEC), then the current packet's own
//!   decode, both into one contiguous span. Silence stands in only for concealment that
//!   failed; a gap of 5 frames or more is neither concealed nor padded.
//!   The concealment lives in the decode path (engine.rs); this file's prebuffer hold
//!   handles underrun re-anchoring only, NOT gap concealment.
//!
//! Latency changes: a detected incoming frame size that no longer fits the setpoint grows
//!   it (and a later smaller frame restores it) per packet, reallocating that channel's ring:
//!     target = (buffer_ms // 20) * 960     [for buffer_ms ≥ 20]
//!   with sub-20ms tiers 120 / 240 / 480 below 5, 10 and 20 ms, floored at 2 × frame and,
//!   on Windows and Linux, at twice the output callback period rounded up to one of those
//!   buffer levels (`period_floor_samples`), which a new granted period also re-applies.
//!   Ring capacity = target*2 + 1920 samples (a 40ms margin).
//!
//! The user's buffer_ms is preserved across per-packet calls — the per-packet path reads
//!   it as a gate and never writes it. Only an explicit settings change moves it.
//!
//! Valid incoming frame sizes: {120, 240, 480, 960}. Anything else is dropped.
//!
//! Zita resampler: inp_count is the queued input — variable, not a fixed 960.
//!
//! SYNC (phase-lock): cross-channel mean of next-emit timestamps, unchanged.

use std::collections::BTreeMap;
use std::sync::{Arc, atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering}};
use tracing::debug;

use super::spsc;
use super::zita::ZitaResampler;

// ── Buffer geometry formulas (CASCADE_AUDIO_RECEIVE_SPEC §3.2/§4) ──────────────────────────────────

/// §13.2's receive half of the shared callback period, in samples, from one channel's
/// configured receive buffer.
///
/// An EXACT-MATCH table, not a formula. Only 5 and 10 map to anything other than the 480
/// default — 7, 15, 20, 120 and everything else fall through to it. There is no general
/// mapping to derive, and deriving one from the setpoint instead gets the in-between values
/// wrong in the direction that holds the callback far shorter than it should be.
///
/// The DETECTED INCOMING FRAME SIZE is not an input here and must never become one. The
/// ring auto-grows to accommodate whatever frame arrives, entirely within whatever callback
/// period is already active; the two mechanisms are independent, and tying them together
/// reconfigures the audio units on an event that should be handled silently.
pub fn receive_half_for_buffer(buffer_ms: u32) -> usize {
    match buffer_ms {
        5  => 120,
        10 => 240,
        _  => 480,
    }
}

/// Playback setpoint in samples for a configured receive buffer, before any floor:
/// (ms//20)*960 for ms ≥ 20, with fixed sub-20ms tiers. The prebuffer fills to it before
/// playback starts.
pub fn target_samples_for_buffer(buffer_ms: u32) -> usize {
    if buffer_ms < 5  { 120  }   // 2 × 2.5ms frames (hardcoded branch w2=2)
    else if buffer_ms < 10 { 240  }   // 2 × 5ms frames  (branch w2=5)
    else if buffer_ms < 20 { 480  }   // 2 × 10ms frames (branch w2=10)
    else { (buffer_ms as usize / 20) * 960 }  // else: (ms//20)*960
}

/// Playback setpoint with both HARD floors applied: 2×frame and the output-period floor.
///
/// The setpoint is raised to 2×frame_samples whenever the configured setpoint is below
/// that — so a 20ms (960-sample) frame can never run below 40ms (1920 samples), and a
/// buffer setting under 40ms has no effect at that frame size.
/// `target_samples_for_buffer` alone only coincides with this at the sub-20ms tier
/// boundaries and does NOT floor the 20ms-frame case, which is what this wraps.
/// frame_samples must be a real measured incoming frame ({120,240,480,960}); 0 (not yet
/// measured) leaves the tier target unchanged.
///
/// `period_floor` is the smallest setpoint the output device's callback period allows, in
/// samples — see `AudioEngine::period_floor`. Each render callback takes one period from
/// the ring, so a setpoint no deeper than the period is emptied by every callback. 0 means
/// no such floor.
pub fn target_samples_floored(buffer_ms: u32, frame_samples: usize, period_floor: usize) -> usize {
    let target = target_samples_for_buffer(buffer_ms);
    target.max(2 * frame_samples).max(period_floor)
}

/// The smallest setpoint an output callback period allows, in samples: twice the period,
/// rounded up to the next level a buffer setting itself produces — 120, 240 or 480, then
/// whole 20 ms steps of 960 (`target_samples_for_buffer`). 0 for a period of 0 (no stream).
///
/// Twice, because each render callback takes one period from the ring: at twice the period
/// a callback leaves a period's worth behind for the frames still on their way.
///
/// Rounded to a buffer level, so a raised buffer runs at a level the settings offer (a
/// 512-frame period gives 40 ms) and every frame that can arrive at it is
/// whole: those levels are all whole numbers of each frame size up to half their depth,
/// which the prebuffer fill needs to land exactly on the setpoint (`prebuffer_keep`).
///
/// A period of half a buffer level returns that level (120 → 240, 240 → 480, 480 → 960),
/// so the floor never raises the setpoint of a buffer setting whose requested period was
/// granted as asked.
///
/// Applied to setpoints on Windows and Linux only (`AudioEngine::period_floor`).
pub fn period_floor_samples(period: usize) -> usize {
    if period == 0 {
        return 0;
    }
    match 2 * period {
        need if need <= 120 => 120,
        need if need <= 240 => 240,
        need if need <= 480 => 480,
        need => need.div_ceil(960) * 960,
    }
}

/// Ring capacity sized off the FLOORED setpoint: the floors are applied first, then the
/// ring is sized as 2×setpoint + 1920. For a 20ms frame at a sub-40ms buffer the floored
/// setpoint exceeds the buffer_ms target, so the ring MUST be sized off the floor to keep
/// the two-frame headroom. frame_samples 0 (unmeasured) and period_floor 0 ⇒ the unfloored
/// size.
pub fn ring_capacity_floored(buffer_ms: u32, frame_samples: usize, period_floor: usize) -> usize {
    target_samples_floored(buffer_ms, frame_samples, period_floor) * 2 + 1920
}

/// The resampler's fixed read-ahead: each staging window is the render quantum plus this
/// many samples (CASCADE_SYNC_MECHANISM_SPEC §6.4's `frames + 6`).
const READ_AHEAD: usize = 6;

/// Frame size assumed for a channel's FIRST ring allocation, before any packet from that
/// peer has been decoded. 20ms is the largest frame Opus carries here, so a ring sized from
/// it holds every smaller frame. This decides start-up memory only, never behaviour: the
/// real frame size is read from the first decoded packet, and the ring is reallocated to it
/// then (§3.2) — so this first ring is never written.
pub const INITIAL_FRAME_SAMPLES: usize = 960;

// The receive half of the callback period is computed in engine.rs (min_output_period)
// from `receive_half_for_buffer` above, over enabled remotes' buffer settings. It never
// reads the incoming frame size, so it stays constant when a sender switches frame size
// mid-stream — a seamless switch, with no output-stream rebuild.

/// Shared mailbox for hot ring-consumer swaps.
/// Decode worker (GCD) writes a new Consumer when it detects an incoming
/// frame size mismatch. Render thread polls with try_lock (non-blocking).
/// If render misses a frame due to lock contention, it picks up next frame.
/// Swap payload on a frame-size change: the new ring consumer plus the new floored
/// playback target (samples). The target is recomputed at the producer with the live
/// frame size so the Channel re-floors to 2×frame on swap.
pub type SwapInbox = Arc<parking_lot::Mutex<Option<(spsc::Consumer, usize)>>>;

// ── Sync ratio ladder ─────────────────────────────────────────────────────────
// Two sets of ratios drive the Sync-on resampler: a seven-step ladder that aligns each
// channel with its siblings (§6.2), and three common-mode ratios — 1.002 / 1.0 / 0.998 —
// broadcast to every channel of a source when the buffer-depth relays call for it (§6.3).
// The constants for both follow further down.
//
// §6.6's own deadband, distinct from §2.3's ±12ms/±4ms pair: the half-width of the band
// the per-channel skew reference is held inside, measured from the SETPOINT. 1728 samples
// @48k and 1590 @44.1k are both 36ms; the daemon is 48k-only, so the sample figure is the
// constant and no rate scaling is needed.
const SKEW_BAND_SAMPLES: i64 = 1728;

/// §5.4: how much silence a sequence gap earns.
///
/// Silence REPLACES WHAT WAS LOST — it does not refill the buffer to its setpoint. The
/// two are easily confused because the setpoint appears in the expression, but only as a
/// ceiling: `lost` is the quantity, and the buffer's own headroom merely bounds it.
///
/// `lost` is non-zero in exactly one situation — concealment was attempted and failed.
/// A gap that FEC/PLC successfully concealed has lost nothing that needs replacing, and a
/// gap too large to conceal (5 frames or more) is not padded at all: the buffer is left
/// short and §2.3's relay walks it back, which is a correction spread over seconds rather
/// than a step inserted in one packet.
///
/// Topping up to the setpoint on every gap instead — padding a concealed gap, or
/// over-filling past what was actually missing — injects invented audio and adds latency
/// the link never asked for, and does it precisely when the network is already struggling.
///
/// One rule for every branch of §5 that pads a gap: the normal path, the prebuffer hold
/// (including the packet that re-arms it on a drain to empty), and the first packet after
/// an overrun. None of them fills toward the setpoint; each replaces only what was lost.
#[inline]
pub fn gap_silence(setpoint: usize, depth: usize, decoded: usize, lost: usize) -> usize {
    lost.min(setpoint.saturating_sub(depth + decoded))
}

/// §5 Path A (prebuffer fill): how many samples of an arriving frame to keep when the
/// prebuffer hold is armed — that is, while the ring is filling for the first time or
/// refilling after a drain to empty.
///
/// The fill stops exactly ON the setpoint. Nothing is reading the ring yet, so there is no
/// reason to carry more than the target, and the hold releases the moment depth reaches
/// it: overshooting here is latency the channel then keeps for its whole life.
///
/// It is the TAIL that survives — the caller drops `frame_len - kept` samples from the
/// FRONT. A prebuffer should begin playing the most recent audio it has; keeping the head
/// instead would start playback from the oldest samples and add their age to the latency.
/// This is the opposite end from the normal-path write, where the head is what survives.
///
/// In clean running this never binds: every setpoint is an exact multiple of every frame
/// size, so depth lands on the target precisely. It binds when FEC/PLC concealment makes a
/// single arrival worth several frames, which is most likely during exactly the
/// drain-to-empty refill that follows a burst of loss.
#[inline]
pub fn prebuffer_keep(setpoint: usize, depth: usize, frame_len: usize) -> usize {
    frame_len.min(setpoint.saturating_sub(depth))
}

/// §6.4: the front of the resampler's staged input window on the ring's timeline.
/// `cursor_ts` is the timestamp at the ring read position BEFORE the top-up copy moves
/// it; `leftover` is the resampler's live carried-over input count, which sits ahead of
/// that first newly copied sample.
#[inline]
fn merge_ts_at_copy(cursor_ts: u32, leftover: usize) -> u32 {
    cursor_ts.wrapping_sub(leftover as u32)
}

/// §6.1's advancement gate: has this channel's merge timestamp moved forward since the
/// value latched at its last dispatch? A plain unsigned comparison — equal or smaller
/// means no.
///
/// Not wrap-aware. When the u32 sample counter wraps (~24.9 hours at 48kHz), a channel
/// whose stamp has just wrapped compares as smaller than its latched value and sits out
/// that one cycle; it rejoins on the next, once both values are past the wrap.
#[inline]
fn ts_advanced(last: u32, cur: u32) -> bool {
    last < cur
}

/// §6.6: the three-state skew decision for one channel — `+1` fill (the reference is at
/// or below the band's lower edge), `-1` drain (at or above its upper edge), `0` inside.
/// Both edges are inclusive, and fill is tested first.
///
/// What is compared is the REFERENCE against the setpoint, not the buffer depth against
/// anything. That is what closes the loop: the flag returned here drives the adjustment
/// that moves `reference`, so the reference is held within one band of the setpoint
/// instead of being free to settle anywhere the first-fill latch happened to leave it.
fn skew_flag(reference: i64, setpoint: usize) -> i8 {
    let setpoint = setpoint as i64;
    let band = SKEW_BAND_SAMPLES.min(setpoint / 2);
    if reference <= setpoint - band { 1 } else if reference >= setpoint + band { -1 } else { 0 }
}

const RATIO_STRONG_UP:   f64 = 1.002;
const RATIO_UNITY:       f64 = 1.000;
const RATIO_STRONG_DOWN: f64 = 0.998;
// Fine per-channel ladder for cross-channel timestamp-deviation alignment. Deviation
// thresholds |dev| ≥ 11 / ≥ 2 / == 1 select strong / medium / gentle ratios, clustered
// just either side of 1.0. Applied per channel only while the common-mode correction is
// disengaged; while it is engaged, and on any cycle that falls through to the group
// decision, the whole array is overwritten by one broadcast ratio (§6.3).
const RATIO_MEDIUM_UP:   f64 = 1.001;
const RATIO_GENTLE_UP:   f64 = 1.0005;
const RATIO_GENTLE_DOWN: f64 = 0.9995;
const RATIO_MEDIUM_DOWN: f64 = 0.999;

// ── Common-mode (sender-clock) drift correction ───────────────────────────────
// The second sync mechanism. The cross-channel mean ladder above corrects RELATIVE skew
// between channels of one source but CANNOT correct the COMMON drift between the sender's
// and receiver's clocks — all channels drift together, so deviation-from-mean stays ~0.
// This controller does: each channel's producer relays its averaged buffer depth against
// that channel's own reference (§2.3), and the render tallies the relays and nudges one
// ratio across all channels.
//
//   ratio = 1.002 (tally > 0, fill) / 0.998 (tally < 0, drain) / 1.0 (tally 0)
//   Relay band = min(base, setpoint/2) either side of the REFERENCE, not the setpoint.
//   Hysteresis: arm at the band edge; release when the average crosses back past the
//   reference itself.
//
// Once a non-zero tally engages it, the correction LATCHES: every later cycle skips the
// per-channel ladder and broadcasts the tally's ratio, until a cycle whose tally is zero
// disengages it. See `PeerGroup::render`.
//
// Inert at zero drift: on loopback, depth sits at its reference, inside the deadband, so
// the ratio is 1.0 and this cannot disturb cross-channel behaviour. That also means
// loopback cannot EXERCISE it — it is unvalidated against a real two-machine link.
// Deadband bases (CASCADE_SYNC_MECHANISM_SPEC §2.3/§9): one mechanism, the base
// selected by state at evaluation time, always clamped to min(base, setpoint/2).
//   Sync ON  → ±12ms (576 samples): the servo's operative band.
//   Sync OFF → ±4ms (192 samples): what arms the Path B discrete splice.
pub const DEADBAND_SYNC_ON:  i64 = 576;    // ±12 ms @ 48 kHz
pub const DEADBAND_SYNC_OFF: i64 = 192;    // ±4 ms @ 48 kHz
const COMMON_RATIO_DRAIN: f64 = RATIO_STRONG_DOWN;  // 0.998 — backstop broadcast
const COMMON_RATIO_FILL:  f64 = RATIO_STRONG_UP;    // 1.002 — backstop broadcast
// Depth smoothing — a BOXCAR (equal-weight) moving average of buffer depth.
// avgDepth = accum/count while filling, then accum/N once full. Updated once per PUSHED
// PACKET, on the producer side.
//
// N is NOT fixed. It is keyed to the SETPOINT — 1200 at a 120-sample setpoint, 600 at 240,
// 300 at 480, 150 at 960 and above — which is three seconds' worth of packets while the
// sender's frame size matches the tier the setpoint came from. A flat 150 would be up to
// 8x too short at the small setpoints and would make the servo twitchy.
//
// Keyed to the setpoint and NOT to the incoming frame size, deliberately: re-keying resets
// the window, and a reset re-arms §2.3's reference latch, so a frame-size switch would
// re-latch the reference onto the depth disturbance the switch itself caused. See
// `set_window`.
const COMMON_WINDOW_DEFAULT: usize = 150;

/// Boxcar window length N (packets) for the current SETPOINT
/// (CASCADE_SYNC_MECHANISM_SPEC §2.2) — 150 at a 960-sample setpoint and above, 300 at
/// 480, 600 at 240, 1200 at 120. Three seconds' worth of packets while the sender's frame
/// size matches the tier the setpoint came from, which is the case this sizing is for.
///
/// Set in one place only — wherever the setpoint is set, alongside the ring reallocation
/// and the setpoint itself.
pub fn boxcar_window_for_setpoint(setpoint_samples: usize) -> usize {
    match setpoint_samples {
        120 => 1200,   // 2.5ms setpoint
        240 => 600,    // 5ms setpoint
        480 => 300,    // 10ms setpoint
        _   => 150,    // 20ms setpoint and above
    }
}

// ── Common-mode drift window (producer side) ─────────────────────────────────
// Encapsulates the per-channel depth average and its adjusting decision: a boxcar moving
// average of buffer depth, plus a deadband + hysteresis comparator yielding a direction
// (-1 drain / 0 / +1 fill). Lives on the PRODUCER side (decode worker), updated once per
// pushed packet. The render thread only reads the resulting direction, summed across
// channels. Inert at zero drift: depth sits at the setpoint, the average stays inside the
// deadband, direction is 0 and the ratio is unity.
#[derive(Clone)]
pub struct DriftWindow {
    /// Circular buffer of depth samples. Integers, like the running sum: the average is
    /// the sum divided by the entry count and truncated, and every threshold it is tested
    /// against is a whole number of samples.
    window: Vec<i64>,
    accum:  i64,        // running sum
    widx:   usize,      // write index
    filled: bool,       // window full → slide
    dir:    i8,         // hysteresis state: held direction
    n:      usize,      // boxcar window length N; set per floored setpoint
    /// §2.3's reference and §6.6's `skew_reference` are ONE per-channel value, not two:
    /// both name the same storage. It is 0 until the boxcar first fills, latched to the
    /// average at that moment (§2.3), and thereafter walked toward the setpoint by §6.6's
    /// servo whenever it strays more than ±36ms away. So it is neither a constant nor
    /// free-running: latched once per boxcar fill, then bounded.
    ///
    /// Shared as an atomic because the halves of that description live on different
    /// threads — the latch and the hysteresis comparison here on the producer, the ±36ms
    /// check in the channel's resample job, and the adjustment in `PeerGroup::render`.
    ///
    /// This has to be the reference because §2.2 feeds the boxcar the PRE-clamp sum
    /// `depth_before_write + frame_len`, not ring depth. In steady state that averages
    /// roughly `setpoint + frame_len/2` — at 20ms frames, setpoint + 480 samples (10ms).
    /// Centring the band on `setpoint` instead puts the resting average permanently ABOVE
    /// `setpoint + 192` (the ±4ms Sync-off band), so DRAIN arms and stays armed on a
    /// perfectly clean link with no drift at all, and the splice bleeds the buffer down
    /// until the average is dragged to the setpoint. Centring on the channel's own settled
    /// average puts it mid-band, where it belongs.
    skew_ref: Arc<AtomicI64>,
}

/// Lock-free buffer readout for the UI, folded over the display interval.
///
/// Written at the end of every `render()` (Relaxed), read and reset by the stats tick, so
/// the buffer display never takes the `render_groups` lock the RT callback holds.
///
/// The fold accumulates rather than overwrites. Depth is a sawtooth — each packet write
/// adds a frame, each render drains a period — so a value sampled at one instant lands
/// anywhere on that ramp, and the display tick samples at a phase unrelated to packet
/// arrival. Reading one point every couple of seconds therefore aliases a stable buffer
/// into a number that jumps by up to a frame.
///
/// The headline figure is the interval MEAN. The mean of a uniform ramp is its midpoint,
/// so the sawtooth cancels instead of showing as noise, and one bad render carries only
/// its 1/n share instead of defining the reading the way an extreme does. `min`/`max` are
/// still folded alongside it, but they describe the interval's spread for the tooltip —
/// they are not what the bar is drawn or coloured from.
pub struct BufferSnap {
    /// Running total of the depths seen since the last read, in samples. u64 because a
    /// slow display tick over a deep buffer would overflow u32 in seconds.
    pub sum: std::sync::atomic::AtomicU64,
    /// How many depths went into `sum`. 0 = nothing seen, which is also the "no active
    /// channel" case.
    pub count: std::sync::atomic::AtomicU32,
    /// Lowest depth seen since the last read, in samples. Tooltip spread only.
    pub min: std::sync::atomic::AtomicU32,
    /// Highest depth seen since the last read, in samples. Tooltip spread only.
    pub max: std::sync::atomic::AtomicU32,
    /// The setpoint those depths were measured against.
    pub target: std::sync::atomic::AtomicU32,
    /// How many of this peer's channels have the prebuffer hold armed — the §10 tap point.
    pub holding: std::sync::atomic::AtomicU32,
}

impl BufferSnap {
    pub fn new() -> Self {
        Self { sum:     std::sync::atomic::AtomicU64::new(0),
               count:   std::sync::atomic::AtomicU32::new(0),
               min:     std::sync::atomic::AtomicU32::new(u32::MAX),
               max:     std::sync::atomic::AtomicU32::new(0),
               target:  std::sync::atomic::AtomicU32::new(0),
               holding: std::sync::atomic::AtomicU32::new(0) }
    }

    /// Fold one render's worth of readings in. Four relaxed RMWs per callback — the two
    /// that carry the mean are plain adds, cheaper than the min/max beside them.
    pub fn observe(&self, depth: usize, target: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        self.sum.fetch_add(depth as u64, Relaxed);
        self.count.fetch_add(1, Relaxed);
        self.min.fetch_min(depth as u32, Relaxed);
        self.max.fetch_max(depth as u32, Relaxed);
        self.target.store(target as u32, Relaxed);
    }

    /// Take the interval's mean, its spread and the setpoint, and arm the next interval.
    /// `None` when nothing was observed — no active channel, or no render between two
    /// reads.
    ///
    /// Returns `(mean, min, max, target)`. `count` is swapped first so a render landing
    /// mid-take adds its depth to an interval whose divisor has already been claimed; that
    /// costs the next mean one sample of weight and cannot divide by zero, which is the
    /// right trade for a display against any synchronisation on the RT side.
    pub fn take(&self) -> Option<(u32, u32, u32, u32)> {
        use std::sync::atomic::Ordering::Relaxed;
        let count = self.count.swap(0, Relaxed);
        let sum   = self.sum.swap(0, Relaxed);
        let min   = self.min.swap(u32::MAX, Relaxed);
        let max   = self.max.swap(0, Relaxed);
        if count == 0 { return None; }
        let mean = (sum / count as u64) as u32;
        Some((mean, min.min(mean), max.max(mean), self.target.load(Relaxed)))
    }
}

impl Default for BufferSnap {
    fn default() -> Self { Self::new() }
}

impl DriftWindow {
    pub fn new(skew_ref: Arc<AtomicI64>) -> Self {
        Self { window: Vec::with_capacity(COMMON_WINDOW_DEFAULT), accum: 0,
               widx: 0, filled: false, dir: 0, n: COMMON_WINDOW_DEFAULT,
               skew_ref }
    }

    /// Set the boxcar window length from the SETPOINT — not from the incoming frame
    /// size, and not per packet.
    ///
    /// The window is re-keyed exactly when the setpoint moves, which is when the buffer
    /// setting changes or a frame-size change is large enough to resize the ring. An
    /// incoming frame-size change that keeps the ring must NOT reach here.
    ///
    /// That distinction is the whole point. Re-keying resets the window, and a reset
    /// re-arms §2.3's reference latch, so the reference is re-captured at whatever depth
    /// the change left behind. A frame-size switch disturbs depth — the sender's frame
    /// boundary moves, so its output pauses or flushes by up to one frame — and latching
    /// on that disturbance makes the servo defend it as the new target. With Sync off
    /// there is no §6.6 to bound the reference back to the setpoint, so the buffer stays
    /// wherever the switch left it, indefinitely.
    ///
    /// Keyed to the setpoint, the window survives such a change: the reference stays at
    /// the settled depth, the average falls below `reference - band`, FILL arms, and the
    /// splice walks the buffer back.
    ///
    /// If N does change, the window IS reset cold, the held direction with it — a re-keyed
    /// window has no meaningful history, and a direction decided against that history has
    /// nothing left to stand on. Returns true when that happened, so the caller can clear
    /// §6.6's flag in the same step, as every other reset does.
    pub fn set_window(&mut self, setpoint_samples: usize) -> bool {
        let new_n = boxcar_window_for_setpoint(setpoint_samples);
        if new_n != self.n {
            self.n = new_n;
            self.reset();
            return true;
        }
        false
    }

    /// Reset to the cold state. Used by the §6.5 reset_averaging condition (the
    /// six-tier ladder is actively correcting this channel while the backstop is
    /// neutral) and on underrun/idle. Also clears the direction (adjusting_flag=0).
    /// Zero the averaging window and the held direction. This also RE-ARMS the §2.3
    /// reference latch, because that latch is gated on the not-filled -> filled
    /// transition and nothing else. The reference keeps its old value in the meantime —
    /// it is only read once the window has filled again — and is re-captured at that
    /// point from whatever depth the channel has settled to.
    pub fn reset(&mut self) {
        self.window.clear(); self.accum = 0;
        self.widx = 0; self.filled = false; self.dir = 0;
    }

    /// §2.1: fold a completed splice back out of the boxcar history.
    ///
    /// A splice removes audio from the ring, so every depth already recorded in the window
    /// overstates the buffer by that amount. Subtracting it from each entry re-baselines
    /// the whole history at once, which is what lets DRAIN release on the very next packet
    /// instead of waiting for the removal to wash through a 3-second window. Without it the
    /// average still reads the pre-splice depth, the relay stays armed, and the 0.5s
    /// throttle lets five or six more splices fire before the measurement catches up with
    /// a correction that already happened.
    ///
    /// `dir` selects which way, and the two halves are deliberately not symmetric in
    /// their guards. A DRAIN leaves alone any entry that would go negative, so a splice
    /// larger than an early, shallow measurement cannot drive the history below zero; a
    /// FILL has no such hazard and adds to every entry unconditionally.
    ///
    /// Called after the splice, which runs AFTER this packet's own entry was recorded from
    /// the pre-splice count — so this corrects every entry, that one included, and the
    /// whole history then reads as though the splice had always been there.
    ///
    /// Only reached with the window full: the caller gates on `filled()`.
    pub fn note_splice(&mut self, delta: usize, dir: i8) {
        let d = delta as i64;
        if dir > 0 {
            for v in self.window.iter_mut() { *v += d; }
            self.accum += d * self.window.len() as i64;
        } else {
            for v in self.window.iter_mut() {
                if *v - d >= 0 { *v -= d; self.accum -= d; }
            }
        }
    }

    pub fn update(&mut self, depth: usize, setpoint: usize, deadband_base: i64) -> i8 {
        let inst = depth as i64;
        let mut just_filled = false;
        // The average is the integer sum over the integer entry count, truncated: the
        // relay's release edges sit on whole samples, so a fractional average just past
        // the reference does not yet count as past it.
        let avg = if !self.filled {
            self.window.push(inst);
            self.accum += inst;
            if self.window.len() >= self.n { self.filled = true; just_filled = true; }
            self.accum / self.window.len() as i64
        } else {
            self.accum -= self.window[self.widx];
            self.window[self.widx] = inst;
            self.accum += inst;
            self.widx = (self.widx + 1) % self.n;
            self.accum / self.n as i64
        };
        // §2.3: latch the reference on the packet where the window transitions from
        // not-filled to filled — not on any later wraparound. The gate is that transition
        // alone, so every reset re-arms it and the reference is re-established at the
        // depth the channel settles to next.
        //
        // The re-arm is what keeps the latch honest across a setpoint change. The setpoint
        // moves under a live channel whenever a frame-size change is large enough to
        // resize the ring (§3.2), and that path re-arms the prebuffer, which resets this
        // window. A reference held over from before would stay anchored to the old
        // operating point: the average would sit permanently outside the band, DRAIN could
        // never reach its release condition, and the splice would run continuously on a
        // link carrying no drift at all.
        if just_filled { self.skew_ref.store(avg, Ordering::Relaxed); }
        // `setpoint` has exactly one role in this calculation: sizing the deadband.
        // Every threshold below is relative to `calref`.
        let band = deadband_base.min(setpoint as i64 / 2);
        let reference = self.skew_ref.load(Ordering::Relaxed);
        // The hysteresis comparison does NOT run until the boxcar window has filled once.
        //
        // §2.3 gives the thresholds but not this gate, and without it the section reads as
        // though DRAIN merely arms "far more readily" during a channel's first 3 seconds.
        // It does worse than that: every threshold there is relative to the same reference,
        // so with the reference still 0 the release condition is `average < 0`, which a
        // boxcar of buffer depths can never satisfy. DRAIN would arm on the first packet
        // and be unable to clear until the reference latched — bleeding roughly 25ms out of
        // the buffer, per channel, every time one joined.
        //
        // The gate prevents that: while the window has not filled, the state machine is
        // skipped entirely and the adjusting flag is left untouched.
        //
        // `filled` gates the comparison and also gates the reference latch above, so the
        // two move together: after a reset the comparison stays suppressed until the
        // window has REFILLED, and the reference is re-captured at that same moment. The
        // first comparison after any reset therefore weighs a full three-second average
        // against a reference drawn from that same history, never a one- or two-sample
        // "average" against a reference inherited from before the reset.
        if !self.filled {
            self.dir = 0;
            return 0;
        }
        // §2.3 is a symmetric hysteresis relay about the reference, with the deadband on
        // BOTH sides. Each half is entered either by crossing its own edge or by already
        // being in that state, and both release on the same test: the average crossing back
        // past the reference itself.
        //
        // Arming is inclusive at the edge (`>=` high, `<=` low); release is strict, so the
        // reference is a single point both states fall through rather than a second band.
        // When neither half is entered the direction is LEFT ALONE — an armed state
        // persists until its own release fires, which is what makes this a relay and not a
        // per-packet comparison.
        self.dir = if avg >= reference + band || self.dir == -1 {
            if avg >= reference { -1 } else { 0 }
        } else if avg <= reference - band || self.dir == 1 {
            if avg > reference { 0 } else { 1 }
        } else {
            self.dir
        };
        self.dir
    }

    /// Current average — the same truncated value the relay compares; 0 if no samples yet.
    pub fn avg(&self) -> i64 {
        if self.window.is_empty() { 0 } else { self.accum / self.window.len() as i64 }
    }
    pub fn dir(&self) -> i8 { self.dir }
    /// DIAGNOSTIC: the latched §2.3 reference, or None while it is still unlatched.
    pub fn calref(&self) -> Option<i64> {
        if self.filled { Some(self.skew_ref.load(Ordering::Relaxed)) } else { None }
    }
    /// DIAGNOSTIC: has the boxcar window filled (the state-machine entry gate)?
    pub fn filled(&self) -> bool { self.filled }
}

/// Mechanism B — the producer-side discrete zero-crossing splice
/// (CASCADE_SYNC_MECHANISM_SPEC §2.1). Gating is at the CALL SITE (engine.rs):
/// Sync OFF, adjusting_flag non-zero, rate-limited to one splice per 0.5s.
///
/// The search finds a PAIR of consecutive positive-to-negative crossings whose
/// separation lies in [120, 240) — 2.5ms–5ms, the window bounds being
/// `sample_rate × 0.0025` and `sample_rate × 0.005` truncated toward zero — and scores
/// each candidate pair by mean energy over the segment between them, keeping the lowest
/// (a minimum-energy search, not a first-match; an exact tie keeps the earlier one). That winning segment
/// `(earlier, later)` is what the splice acts on:
///   DRAIN (dir<0): drop it — output = [0..earlier+1) + [later..len), shortening the
///   frame by the segment's own length.
///   FILL  (dir>0): repeat it — output = [0..later) + [earlier+1..len), lengthening
///   the frame by that same length. Falls back to a plain copy when the frame buffer
///   has no room to grow.
/// The two directions are mirror images spliced at the same two crossings, so both
/// ends sit on a zero crossing → click-free. No qualifying pair → plain copy,
/// unmodified.
// `pcm` is a slice (not a fixed FRAME_MAX array): after §3.1's two-call concealment the
// decode buffer holds the recovered frames plus the current one, so it can exceed one frame.
pub fn zero_crossing_splice(pcm: &mut [f32], len: usize, dir: i8) -> usize {
    if dir == 0 || len < 2 { return len; }

    // Window bounds (§2.1): 48000×0.0025 = 120 .. 48000×0.005 = 240, truncated toward
    // zero — exact at 48kHz, the only rate the daemon runs.
    const WIN_LO: usize = 120;
    const WIN_HI: usize = 240;

    // ── Phase 1: locate the first positive-to-negative crossing ────────────────────
    // A pure pre-scan from the start of the frame that establishes where the bounded
    // search below begins. It does not itself become part of the splice.
    let mut p1 = None;
    for i in 0..len - 1 {
        if pcm[i] >= 0.0 && pcm[i + 1] < 0.0 { p1 = Some(i + 1); break; }
    }
    let first_crossing = match p1 { Some(v) if v < len => v, _ => return len };

    // ── Phase 2: quietest segment between two CONSECUTIVE crossings ────────────────
    // The segment is measured from the PREVIOUS candidate, and both the segment origin
    // and the energy accumulator reset at every candidate — win or lose. That resetting
    // origin is what makes the winning segment's own length the amount spliced, and it is
    // why the live splice range matches the window bounds exactly.
    //
    // Measuring from a FIXED origin instead makes the qualifying window an absolute
    // region near the start of the frame: only crossings whose absolute position happens
    // to fall inside it can ever be scored, so a quieter segment later in the frame is
    // unreachable and a louder splice point gets chosen.
    //
    // Scoring is single precision. Every segment's energy starts with the square of its
    // own first sample — the first segment's included, seeded here from the sample just
    // after the pre-scan's crossing — and the running best starts at 2^63.
    let mut energy: f32 = pcm[first_crossing] * pcm[first_crossing];
    let mut best_score = f32::from_bits(0x5F00_0000);   // 2^63
    let mut best: Option<(usize, usize)> = None;   // (earlier, later)

    let mut seg_start = first_crossing;
    let mut earlier   = first_crossing.saturating_sub(1);
    let mut j = first_crossing;
    while j + 1 < len {
        let s1 = pcm[j + 1];
        if pcm[j] >= 0.0 && pcm[j + 1] < 0.0 {
            let cur     = j + 1;
            let seg_len = cur - seg_start;
            if seg_len >= WIN_LO && seg_len < WIN_HI {
                let score = energy / seg_len as f32;
                if score < best_score {
                    best_score = score;
                    best = Some((earlier, cur));
                }
            }
            // Restart the segment at this candidate, whether or not it won.
            energy    = s1 * s1;
            earlier   = cur - 1;
            seg_start = cur;
        } else {
            energy = add_square(energy, s1);
        }
        j += 1;
    }

    let (earlier, later) = match best { Some(v) => v, None => return len };

    if dir > 0 {
        // FILL: emit `[0, later)` and then re-emit `[earlier + 1, len)`, so the quiet
        // segment between the two winning crossings is played twice and the frame grows by
        // exactly that segment's length — the mirror of the DRAIN case below, spliced at
        // the same two zero crossings for the same reason.
        //
        // One `copy_within` does it: the destination begins at `later`, so the head is
        // untouched, and the overlapping source is handled by the move's own semantics.
        let gap = later - earlier - 1;
        if gap == 0 || len + gap > pcm.len() { return len; }
        pcm.copy_within(earlier + 1..len, later);
        len + gap
    } else {
        // DRAIN: keep `earlier + 1` samples unmodified, then resume from `later`, so the
        // quiet segment between the two winning crossings is never copied out. The amount
        // removed is `later − earlier − 1`, which is exactly the segment length that was
        // scored — and therefore always inside the search window.
        let keep = earlier + 1;
        let gap  = later - earlier - 1;
        if gap == 0 || later >= len { return len; }
        pcm.copy_within(later..len, keep);
        len - gap
    }
}

/// `acc + s²`, rounded the way this architecture's multiply-add rounds it: once on
/// aarch64, where the multiply and the add are one fused instruction, and twice elsewhere,
/// where the product is rounded before the sum. The two can differ in the last bit, and
/// only ever matter to a near-tie between two splice candidates.
#[inline(always)]
fn add_square(acc: f32, s: f32) -> f32 {
    #[cfg(target_arch = "aarch64")]
    { s.mul_add(s, acc) }
    #[cfg(not(target_arch = "aarch64"))]
    { acc + s * s }
}

// Active-channel threshold for the gain-share tally (a channel counts as active
// when its tracked level exceeds this).
const ACTIVE_THRESHOLD:  f32 = 0.001;

/// Map signed deviation (dev = channel_ts − cross-channel mean, in samples) to the fine
/// ratio:
///   dev ≥ +11 → 1.002 ; +2..+10 → 1.001 ; +1 → 1.0005 ; 0 → 1.0 ;
///   −1 → 0.9995 ; −2..−10 → 0.999 ; ≤ −11 → 0.998.
/// A channel AHEAD of the mean (dev>0) speeds up (ratio>1) to let the others catch the
/// shared timeline; BEHIND (dev<0) slows down.
fn fill_ratio(dev: i32) -> f64 {
    match dev {
        d if d >=  11 => RATIO_STRONG_UP,
        d if d >=   2 => RATIO_MEDIUM_UP,
        1            => RATIO_GENTLE_UP,
        0            => RATIO_UNITY,
        -1           => RATIO_GENTLE_DOWN,
        d if d >  -11 => RATIO_MEDIUM_DOWN,
        _            => RATIO_STRONG_DOWN,
    }
}

/// §6.2 for one channel, from its merge timestamp and the group mean taken as the
/// unsigned counters they are. The side comes from an unsigned comparison — at or above
/// the mean is ahead — and the size from the unsigned difference on that side, so a stamp
/// and a mean on opposite sides of the counter's wrap read as a large deviation.
fn ladder_ratio(ts: u32, mean: u32) -> f64 {
    if ts >= mean {
        fill_ratio((ts - mean).min(i32::MAX as u32) as i32)
    } else {
        fill_ratio(-((mean - ts).min(i32::MAX as u32) as i32))
    }
}

/// Samples of correction the resampler applies per render cycle at a given ratio
/// (CASCADE_SYNC_MECHANISM_SPEC §7.3): `render_period × (1/ratio − 1)`.
///
/// The sign follows the ratio: a DOWN ratio (r < 1, applied to a channel BEHIND the mean)
/// gives a positive rate, and an UP ratio (r > 1, a channel AHEAD) gives a negative one —
/// so the deviation moves toward zero by adding this each cycle, from either side. Used by
/// the trajectory model below; the live path never needs it, because the resampler applies
/// the ratio itself.
#[cfg_attr(not(test), allow(dead_code))]
pub fn correction_per_cycle(ratio: f64, render_period: usize) -> f64 {
    render_period as f64 * (1.0 / ratio - 1.0)
}

// ── Asynchronous resample slot (CASCADE_SYNC_MECHANISM_SPEC §7.2) ────────────
// The resample is handed to a GENUINE per-channel serial queue, not computed inline.
// Every render cycle dispatches one job per channel, capturing that cycle's frame count
// and freshly computed ratio. The job tops the staged window up from the ring, runs the
// resampler and publishes the result; the render thread mixes whatever a PRIOR cycle's
// job published. Output is therefore one cycle behind computation.
//
// That delay is not structurally fixed. A job still running when the next cycle reads is
// a slip, and the channel contributes nothing that cycle — skip, never re-emit the last
// result. The next job is dispatched regardless and queues behind the running one, so a
// slip costs one quantum of OUTPUT but no input: every job still takes its own quantum
// from the ring when it runs, and the channel stays on its siblings' timeline. Measured
// live: two of four channels slipped exactly one cycle and recovered ~1ms later,
// independently — per-channel, not a global stall.

struct RsInner {
    /// This channel's ring. Read only under this lock — by the jobs while Sync is on, and
    /// by the render thread's direct read while it is off. The render thread keeps a
    /// `RingProbe` for depth and write position, which need no lock.
    ring:      spsc::Consumer,
    resampler: ZitaResampler,
    /// The staged input window: `in_fill` carried samples at the front, topped up to the
    /// render quantum plus `READ_AHEAD` by each job.
    stage:     Vec<f32>,
    /// Samples carried at the front of `stage` from the previous job — the resampler's
    /// unconsumed input, silence padding included, since the resampler consumed the
    /// padding as input like any other sample.
    in_fill:   usize,
    /// The last job's output, and how much of it the resampler produced.
    out:       Vec<f32>,
    produced:  usize,
}

/// State shared between a channel's render-side half and its jobs.
struct ResampleSlot {
    inner: parking_lot::Mutex<RsInner>,
    /// Set by a job whose top-up recovered input from the ring; cleared by every dispatch.
    /// The render thread mixes `out` only while it is set. A job that ran on carried
    /// input or silence alone still advanced the resampler, but leaves this clear.
    ready: AtomicBool,
    /// §6.4 merge timestamp: the front of the staged window on the ring's timeline.
    /// Written by a job whose top-up recovered input, and by the Sync-off direct read.
    merge_ts: AtomicU32,
    /// Shared with the producer: the prebuffer hold (§4.2), the §2.3/§6.6 reference and
    /// §6.6's flag, all read or written by the job.
    hold:      super::pool::PrebufferHold,
    skew_ref:  Arc<AtomicI64>,
    skew_flag: super::pool::DriftDir,
    /// DIAGNOSTIC (temporary, read-only): jobs whose top-up recovered input, and so reached
    /// §6.6's check, against jobs where the carried input already covered the window.
    skew_checked: AtomicU64,
    skew_skipped: AtomicU64,
}

/// One dispatched job — the resample for one render cycle. Runs on the channel's serial
/// queue, so jobs for one channel run one at a time, in the order they were dispatched.
///
/// The window is always `frames + READ_AHEAD` samples: whatever the previous job carried,
/// topped up from the ring when that falls short. The top-up takes nothing while the
/// prebuffer hold is armed or the ring is empty, and whatever it cannot supply is silence.
/// A top-up that recovered at least one sample is what makes the result mixable, moves
/// the merge timestamp, and — once the reference has latched — re-evaluates §6.6's flag.
fn run_resample_job(slot: &ResampleSlot, frames: usize, ratio: f64, target: usize) {
    let mut guard = slot.inner.lock();
    let g = &mut *guard;
    let need = frames + READ_AHEAD;
    // A callback longer than the window reserved for this stream grows it here, off the
    // audio thread.
    if g.stage.len() < need { g.stage.resize(need, 0.0); }
    if g.out.len() < frames { g.out.resize(frames, 0.0); }

    let have = g.in_fill;
    let mut recovered = false;
    if need > have {
        // §6.6 compares the reference as it stood before this copy.
        let skew_ref = slot.skew_ref.load(Ordering::Relaxed);
        let (got, ts_at_copy) = if slot.hold.load(Ordering::Relaxed) {
            (0, None)
        } else {
            // The cursor timestamp is read BEFORE the copy advances it: the timestamp of
            // the first sample the copy takes.
            let ts = g.ring.read_ts();
            (g.ring.read_into(&mut g.stage[have..need]), ts)
        };
        g.stage[have + got..need].fill(0.0);
        if got > 0 {
            if let Some(t) = ts_at_copy {
                slot.merge_ts.store(merge_ts_at_copy(t, have), Ordering::Relaxed);
            }
            slot.skew_checked.fetch_add(1, Ordering::Relaxed);
            // `>= 1` asks only whether §2.3's first-fill latch has written the reference
            // yet. Until it has, the flag keeps whatever value it held.
            if skew_ref >= 1 {
                slot.skew_flag.store(skew_flag(skew_ref, target), Ordering::Relaxed);
            }
            recovered = true;
        }
    } else {
        slot.skew_skipped.fetch_add(1, Ordering::Relaxed);
    }

    let (produced, leftover) = {
        let RsInner { resampler, stage, out, .. } = &mut *g;
        // set_rratio: the phase step snaps instantly — no set_rrfilt, no smoothing.
        resampler.set_ratio(ratio);
        let produced = resampler.process(&stage[..need], &mut out[..frames], frames);
        if produced < frames { out[produced..frames].fill(0.0); }
        let leftover = resampler.inp_count;
        // The unconsumed tail of the window moves to the front for the next job. Anything
        // staged beyond `need` — possible only if the quantum shrank — is dropped.
        stage.copy_within(need - leftover..need, 0);
        (produced, leftover)
    };
    g.in_fill  = leftover;
    g.produced = produced;
    drop(guard);
    slot.ready.store(recovered, Ordering::Release);
}


/// Per-channel serial queue for the §7.2 dispatch. Separate scheduler instance from the
/// decode queues so a channel's resample never serialises behind its own decode.
///
/// Separate QUEUES, not separate threads: on both platforms every scheduler instance's
/// queues target the same process-wide pool (the global concurrent queue on macOS, the
/// worker pool on Linux), so this costs queue objects and no additional threads.
fn resample_queue(slot: u8) -> std::sync::Arc<dyn super::scheduler::ChannelQueue> {
    static SCHED: std::sync::OnceLock<std::sync::Arc<dyn super::scheduler::Scheduler>> =
        std::sync::OnceLock::new();
    let s = SCHED.get_or_init(|| super::scheduler::make_scheduler(0, 128));
    s.decode_queue(slot as usize)
}

// ── Per-channel render state ──────────────────────────────────────────────────
struct Channel {
    /// Depth and write position of this channel's ring. The consumer itself lives in the
    /// resample slot, with whichever side reads samples.
    ring_probe:    spsc::RingProbe,
    /// §7.2 async resample state, shared with this channel's dispatch queue.
    rs:            std::sync::Arc<ResampleSlot>,
    /// This channel's serial dispatch queue (§7.2 — one per channel).
    rs_queue:      std::sync::Arc<dyn super::scheduler::ChannelQueue>,
    /// Output destination bitmask — bit N set = this source is mixed into output N.
    /// 128 bits (outputs 0–127), so ONE incoming source can fan out to MULTIPLE outputs
    /// and routing can be updated live without tearing the channel down. render() reads
    /// it once per cycle with a single lock-free load that never tears; see AtomicMask128.
    out_mask: Arc<AtomicMask128>,
    /// The incoming slot number this channel carries — used for re-routing.
    source_slot:   u8,
    /// The slot's merge timestamp as it stood at this channel's last dispatch. A channel
    /// joins the cross-channel mean only if the stamp has moved past this since, so a
    /// stalled channel cannot corrupt the mean.
    last_merge_ts: u32,
    level:         f32,
    /// Pending ring-consumer swap from decode worker (populated on frame-size change).
    /// Polled non-blocking on every render frame via try_lock.
    swap_inbox:    SwapInbox,
    /// Shared prebuffer-hold flag (CASCADE_AUDIO_RECEIVE_SPEC §4.2). While set, all
    /// reads are refused regardless of how much data is present. Armed at creation
    /// (fresh join); re-armed by the PRODUCER on drain-to-empty (§5) and here on a
    /// ring swap; cleared here at release (depth ≥ setpoint) — a pure scheduling
    /// event, no flush, no snap (CASCADE_SYNC_MECHANISM_SPEC §3).
    hold:          super::pool::PrebufferHold,
    /// §6.5 reset_averaging request line to this channel's producer, stored on every
    /// Sync-on dispatch.
    boxcar_reset:  Arc<std::sync::atomic::AtomicBool>,
    /// §2.3/§6.6 reference for this channel — the same Arc the producer's
    /// `DriftWindow` latches into and the resample job reads for the ±36ms check. Adjusted
    /// by render's §6.6 servo; the producer reads it as its hysteresis centre.
    skew_ref:      Arc<AtomicI64>,
    /// §6.6 skew_adjusting_flag — three-state (-1 drain / 0 idle / +1 fill),
    /// recomputed by the resample job on cycles whose top-up recovered input, once the
    /// reference has latched; tallied and cleared by render.
    /// Shared with the decode side, which clears it whenever it resets the averaging
    /// window: the two direction flags are cleared together by every such reset, and
    /// those resets happen over there. Relaxed throughout — like `cm_dir_shared`, this
    /// is a one-value handoff with no ordering relationship to anything else.
    skew_adjusting_flag: super::pool::DriftDir,
    /// Writer-liveness tracking for the buffer DISPLAY (not the audio path). A channel
    /// whose writer (producer) has stopped advancing is parked/removed, and a removed
    /// channel sits at depth 0. We track writeIdx across renders; when it stops
    /// moving for several renders the channel is "idle" and excluded from buffer_fill so
    /// one parked channel doesn't drag the displayed buffer to 0. Brief jitter (writeIdx
    /// pauses 1-2 renders) does NOT mark it idle.
    last_write_pos:      u64,
    writer_idle_renders: u32,
    /// Target samples before warmup latch clears (from buffer_ms + frame size).
    /// Updated when incoming frame size changes.
    target_samples: usize,
    /// Last seen incoming frame size — detect changes to update target.
    last_frame_len: u16,
    /// Common-mode drift direction, written by this channel's decode worker
    /// (producer side) and read here in render. -1 drain / 0 / +1 fill. The render
    /// thread sums it across the channels that advanced this cycle to pick the common
    /// ratio. Read-only on the render side.
    cm_dir_shared:  super::pool::DriftDir,
}

/// A 128-bit routing mask, readable from the render thread without tearing.
///
/// §7.2 requires a routing update to be atomic against the mixing thread — "not
/// cell-by-cell interleaved with live mixing, which would risk torn reads producing
/// audible routing glitches". Holding one lock across the whole 128x128 update would
/// achieve it, but §2 forbids the real-time thread contending on a lock with
/// non-real-time code — a mix lock doing exactly that costs the majority of render time.
///
/// Two independent `AtomicU64` halves would NOT be atomic either: a writer storing `hi`
/// then `lo` as separate operations lets a concurrent reader observe the new `hi` with the
/// old `lo` — on a device with 64 or more outputs, one render cycle of audio sent to the
/// wrong output, which is exactly §7.2's "audible routing glitch".
///
/// `portable-atomic` supplies a lock-free 128-bit atomic on every Cascade target: x86_64
/// through `cmpxchg16b` (in the Apple and Windows targets' own baselines, and detected at
/// run time on Linux, where every x86-64-v2 CPU has it), aarch64 through `casp` or
/// `ldxp`/`stxp`. A
/// single atomic means no seqlock and no retry loop on the render thread: a reader observes
/// either the old mask or the new one, never a mixture. std's own `AtomicU128` is still
/// unstable, hence the crate.
struct AtomicMask128 {
    v: portable_atomic::AtomicU128,
}
impl AtomicMask128 {
    #[inline]
    fn new(v: u128) -> Self { Self { v: portable_atomic::AtomicU128::new(v) } }
    #[inline]
    fn load(&self, ord: std::sync::atomic::Ordering) -> u128 { self.v.load(ord) }
    #[inline]
    fn store(&self, v: u128, ord: std::sync::atomic::Ordering) { self.v.store(v, ord) }
}

/// Convert a list of output channels into a destination bitmask (bit N = output N).
/// Outputs >= 128 are ignored — the wire addresses channels 0-127 in a single byte.
#[inline]
pub fn outputs_to_mask(outs: &[u8]) -> u128 {
    let mut m = 0u128;
    for &o in outs { if o < 128 { m |= 1u128 << o; } }
    m
}

impl Channel {
    #[allow(clippy::too_many_arguments)]
    fn new(rx: spsc::Consumer, out_mask: u128, source_slot: u8, buffer_ms: u32,
           swap_inbox: SwapInbox, cm_dir_shared: super::pool::DriftDir,
           skew_flag: super::pool::DriftDir,
           hold: super::pool::PrebufferHold,
           boxcar_reset: Arc<std::sync::atomic::AtomicBool>,
           skew_ref: Arc<AtomicI64>,
           frame_samples: usize,
           window_len: usize) -> Self {
        let out_mask_atom = Arc::new(AtomicMask128::new(out_mask));
        // No period floor here: nothing is written to this first ring. The channel's first
        // decoded packet sizes it, and delivers a new ring and target through the swap inbox
        // with every floor applied (engine.rs, the frame-size resize).
        let target = target_samples_floored(buffer_ms, frame_samples, 0);
        let ring_probe = rx.probe();
        let rs = std::sync::Arc::new(ResampleSlot {
            inner: parking_lot::Mutex::new(RsInner {
                ring:      rx,
                resampler: ZitaResampler::new(),
                stage:     vec![0.0; window_len],
                in_fill:   0,
                out:       vec![0.0; window_len],
                produced:  0,
            }),
            ready:     AtomicBool::new(false),
            merge_ts:  AtomicU32::new(0),
            hold:      Arc::clone(&hold),
            skew_ref:  Arc::clone(&skew_ref),
            skew_flag: Arc::clone(&skew_flag),
            skew_checked: AtomicU64::new(0),
            skew_skipped: AtomicU64::new(0),
        });
        Self {
            ring_probe,
            rs,
            rs_queue: resample_queue(source_slot),
            out_mask: out_mask_atom,
            source_slot,
            last_merge_ts:  0,
            level:          0.0,
            swap_inbox,
            hold,
            boxcar_reset,
            skew_ref,
            skew_adjusting_flag: skew_flag,
            last_write_pos:      0,
            writer_idle_renders: 0,
            target_samples: target,
            last_frame_len: 0,
            cm_dir_shared,
        }
    }

    /// See `PeerGroup::reserve_for_period`. Takes the resampler lock with a blocking lock, so
    /// it must never run on the audio thread.
    #[cfg(not(target_os = "macos"))]
    fn reserve_for_period(&mut self, frames: usize) {
        let mut g = self.rs.inner.lock();
        if g.stage.len() < frames + READ_AHEAD { g.stage.resize(frames + READ_AHEAD, 0.0); }
        if g.out.len() < frames { g.out.resize(frames, 0.0); }
    }

    fn depth_samples(&self) -> usize {
        // Held buffer depth = writeIdx − readIdx.
        self.ring_probe.samples_available()
    }

    /// True when the prebuffer hold is clear (channel released, reads allowed).
    #[inline]
    fn released(&self) -> bool {
        !self.hold.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Should this channel contribute to the buffer DISPLAY at all?
    ///
    /// Two exclusions, and both are needed for the depth and the prebuffer indicator to
    /// agree with each other:
    ///
    ///   - **Unrouted** (`out_mask == 0`). Nobody is listening, so it has no place in a
    ///     display of what is being heard. (Render never reads such a channel — §7.1.)
    ///   - **Writer frozen** for several renders (sender removed it → parked). ~10 renders
    ///     (~100 ms) is well beyond normal packet jitter (one frame ≈ 10-20 ms = 1-2
    ///     renders), so a brief stall won't drop it from the display.
    fn is_displayed(&self) -> bool {
        const IDLE_RENDERS_LIMIT: u32 = 10;
        self.out_mask.load(std::sync::atomic::Ordering::Relaxed) != 0
            && self.writer_idle_renders < IDLE_RENDERS_LIMIT
    }

    /// Is this channel displayed AND past its prebuffer hold — i.e. contributing a real
    /// depth reading rather than still filling.
    fn is_active_for_display(&self) -> bool {
        self.is_displayed() && self.released()
    }

    /// Prebuffer hold (CASCADE_AUDIO_RECEIVE_SPEC §4.2). While the shared hold flag
    /// is set, reads are refused entirely; it clears once the buffer has filled to
    /// setpoint. Re-armed here on a ring swap (frame-size change — a full flush) and
    /// by the PRODUCER on drain-to-empty (§5). Release is a pure scheduling event
    /// (CASCADE_SYNC_MECHANISM_SPEC §3): no flush, no snap, no resampling — the
    /// channel begins emitting from wherever the setpoint crossing landed. (The fill write
    /// stops on the setpoint — see `prebuffer_keep` — and release is tested per packet
    /// against `depth >= setpoint`, so there is no backlog for a snap to drop anyway.)
    fn ready(&mut self) -> bool {
        // Non-blocking ring swap check. Decode worker populates swap_inbox when it
        // detects a qualifying incoming frame-size change (ring capacity change).
        //
        // Both locks are try_locked, so the render callback never blocks. The resampler's
        // bookkeeping is reset together with the ring, and a worker can be mid-resample
        // holding that lock: then the swap stays in the inbox for a later callback, and the
        // channel carries on from its current ring until it lands.
        if let Some(mut inbox) = self.swap_inbox.try_lock() {
            if inbox.is_some() {
                if let Some(mut g) = self.rs.inner.try_lock() {
                    if let Some((new_rx, new_target)) = inbox.take() {
                        self.ring_probe = new_rx.probe();
                        g.ring          = new_rx;
                        self.target_samples = new_target;   // re-floored 2×frame target
                        self.hold.store(true, std::sync::atomic::Ordering::Relaxed);
                        self.last_frame_len = 0;
                        // Drop any completed result, and the carried input: all of it was
                        // staged from the ring that has just been replaced. Jobs already
                        // queued run against the new ring, which the hold keeps them from
                        // reading until it has filled.
                        //
                        // The RESAMPLER itself is deliberately left alone. Its filter history
                        // and phase accumulator describe the audio stream, not the buffer
                        // holding it, and a ring swap changes only how much of that stream we
                        // keep queued — the sender's timeline runs on unbroken underneath.
                        // Resetting it would zero 64 taps of perfectly good history and
                        // restart the output from silence, turning a depth change into an
                        // audible one. None of the triggers that reset ring state touch it.
                        self.rs.ready.store(false, std::sync::atomic::Ordering::Relaxed);
                        g.produced = 0;
                        g.in_fill  = 0;     // carried samples belong to the old timeline
                        debug!("slot{}: ring swapped for new frame size, target→{}, hold re-armed",
                               self.source_slot, self.target_samples);
                    }
                }
            }
        }
        // The ring swap above — a frame-size or buffer-setting change — is the only thing
        // that clears the resampler's bookkeeping (in-flight result, staged and carried
        // input), because all of it was taken from the ring being replaced. A hold re-arm
        // with no swap (drain-to-empty) leaves that alone, and nothing resets the filter
        // history or phase.

        // Gate 1 (CASCADE_AUDIO_RECEIVE_SPEC §4.2) is READ here only. Its release is
        // evaluated on the WRITE side, after every packet (engine.rs) — this side never
        // clears it.
        //
        // Gate 1 governs BOTH read paths, not just the non-sync one
        // (CASCADE_SYNC_MECHANISM_SPEC §3.2). `ready()` returns this same flag and gates
        // the fan-out loop before `harvest`, so a held channel emits nothing on either
        // path. It does NOT gate the Sync-on dispatch: a held channel is still dispatched
        // every cycle, and it is the job's top-up that tests the hold and takes nothing.
        // The resampler then runs on the carried input and silence, so its filter history
        // is silence by the time the hold releases, not whatever it last held.
        //
        // Gate 2 is a different thing entirely: per-cycle readiness, i.e. whether a
        // dispatched resample recovered input. It does not replace this flag for the sync
        // path; both apply.
        !self.hold.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Writer-liveness for the buffer DISPLAY (not the audio path): has the producer's
    /// writeIdx advanced since the last render? A removed channel's writer freezes, so
    /// writeIdx stops; after a few idle renders `is_displayed` excludes it, which is what
    /// stops a parked channel at depth 0 dragging the displayed buffer down.
    ///
    /// **Called for EVERY channel, before the Gate 1 test, and deliberately not from
    /// `harvest`.** A channel inside its prebuffer hold is skipped before harvest, so while
    /// this lived there a held channel could never age out: `is_displayed` stayed true for
    /// the life of the session, `holding` never reached zero, and the buffer indicator was
    /// pinned on "buffering" with nothing able to release it — the writer had stopped, which
    /// is the very condition the counter exists to detect.
    fn note_writer_liveness(&mut self) {
        let wpos = self.ring_probe.write_pos();
        if wpos == self.last_write_pos {
            self.writer_idle_renders = self.writer_idle_renders.saturating_add(1);
        } else {
            self.writer_idle_renders = 0;
            self.last_write_pos = wpos;
        }
    }

    /// §6.1 Pass 1 retrieval. Returns false when the channel contributes NOTHING this
    /// cycle, in which case render skips its fan-out AND excludes it from the group
    /// average, the timestamp read and the advancement check.
    ///
    /// Sync on, it mixes the result a PRIOR cycle's job
    /// published, and only if that job recovered input. It never touches the ring.
    ///
    /// Sync off, it reads the ring directly, 1:1, under the slot lock.
    fn harvest(&mut self, frames: usize, sync: bool, out: &mut [f32]) -> bool {
        if !sync {
            // Sync off: pure 1:1 flat copy, no resampler. Not "the Gate 1 path" — Gate 1
            // is the prebuffer hold and applies to both paths; the caller has already
            // gated on it via `ready()`.
            //
            // The ring is read under the slot lock. The only thing that can hold it here is
            // a Sync-on job still running after Sync was switched off; that job owns the
            // ring until it finishes, and this cycle contributes nothing rather than read
            // alongside it. try_lock only — the render thread never blocks.
            //
            // merge_time_stamp is maintained on this path too, and it is the ring timestamp
            // at the read cursor with NO leftover term: nothing stands between the ring and
            // the mix here, so the front of the window IS the cursor. Read before the copy,
            // which is what moves that cursor.
            //
            // `last_merge_ts` is deliberately left alone. It is latched only by the
            // sync-on dispatch, so it holds the value from whenever sync was last on, while
            // the stamp keeps advancing underneath it. Switching sync back on therefore
            // finds a stamp pair that already reads as advanced, and the channel
            // participates on its first cycle instead of sitting out until the baseline
            // catches up.
            let g = match self.rs.inner.try_lock() { Some(g) => g, None => return false };
            if let Some(t) = g.ring.read_ts() {
                self.rs.merge_ts.store(t, Ordering::Relaxed);
            }
            let got = g.ring.read_into(&mut out[..frames]);
            drop(g);
            if got < frames { out[got..frames].fill(0.0); }
            self.track_level(&out[..got]);
            return true;
        }

        // ── §7.2 HARVEST: the result a PRIOR cycle's job published ──
        // Gate 2 (CASCADE_AUDIO_RECEIVE_SPEC §4.2): per-cycle readiness, nothing latched.
        // Not ready means this cycle's predecessor job either has not finished (a slip) or
        // recovered no input; either way the channel contributes nothing this cycle — not
        // silence written into the mix, and not a repeat of an earlier result.
        //
        // The flag is not cleared here; the dispatch that follows in this same cycle clears
        // it. A job running right now holds the lock, and try_lock turning it away is the
        // same outcome as not ready: its predecessor's result is being overwritten.
        if !self.rs.ready.load(Ordering::Acquire) { return false; }
        let g = match self.rs.inner.try_lock() { Some(g) => g, None => return false };
        let n = g.produced.min(frames);
        out[..n].copy_from_slice(&g.out[..n]);
        drop(g);
        if n < frames { out[n..frames].fill(0.0); }
        self.track_level(&out[..n]);
        true
    }

    /// §7.2 dispatch, run for EVERY
    /// channel every Sync-on cycle, after Pass 1 has harvested the previous result. Per
    /// §11 the harvest happens BEFORE this dispatch within a cycle, so each cycle's output
    /// consumes the PREVIOUS cycle's job — a structural one-cycle pipeline delay.
    ///
    /// Never skipped, and never conditional on the previous job having finished: the job
    /// is queued behind it on the channel's serial queue. The ring is read by the job, not
    /// here, so nothing on the render thread depends on how far the queue has got.
    fn dispatch(&mut self, frames: usize, ratio: f64, reset: bool) {
        // §6.5 reset_averaging, stored for this channel's producer: one value for the
        // whole group, written every cycle — a store, not a latch.
        self.boxcar_reset.store(reset, Ordering::Relaxed);
        // Latch the stamp Pass 1 compared against, before this cycle's job can move it.
        self.last_merge_ts = self.rs.merge_ts.load(Ordering::Relaxed);
        // Not ready again until this cycle's job says so.
        self.rs.ready.store(false, Ordering::Release);
        let slot = std::sync::Arc::clone(&self.rs);
        let target = self.target_samples;
        // The job captures the slot, this cycle's frame count and freshly computed ratio,
        // and runs OFF the audio thread. Its result is harvested by a LATER cycle.
        self.rs_queue.dispatch(Box::new(move || run_resample_job(&slot, frames, ratio, target)));
    }

    /// Peak-tracking level meter (instant attack, slow release), fed by the samples
    /// actually read from the ring this callback.
    fn track_level(&mut self, pcm: &[f32]) {
        let peak = pcm.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        self.level = if peak > self.level { peak } else { self.level * 0.98 + peak * 0.02 };
    }
}

/// The shape of one Sync-on cycle's Pass 2, decided from Pass 1's results.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CyclePlan {
    /// No channel advanced: every channel at 1.0, and nothing else runs.
    AllUnity,
    /// Each advanced channel its own §6.2 ladder ratio; every other channel 1.0.
    Ladder,
    /// The group decision (§6.3/§6.6): one ratio broadcast to every channel.
    Group,
}

/// Which way Pass 2 goes. The ladder runs only when the common-mode correction is
/// disengaged AND some advanced channel sits off the mean; once engaged, the common mode
/// owns the ratio even on cycles where channels disagree.
fn cycle_plan(advanced: u64, common_engaged: bool, any_off_mean: bool) -> CyclePlan {
    if advanced == 0 {
        CyclePlan::AllUnity
    } else if !common_engaged && any_off_mean {
        CyclePlan::Ladder
    } else {
        CyclePlan::Group
    }
}

/// §6.1: the group mean, rounded — the advanced channels' raw u32 stamps summed as u64,
/// plus half their count, divided by the count. Unsigned throughout, so the rounding is a
/// floor of `mean + 0.5`, and stamps on both sides of the counter's wrap average to a
/// value near neither. 0 for no channels.
fn group_mean(sum: u64, count: u64) -> u32 {
    if count == 0 { return 0; }
    ((sum + count / 2) / count) as u32
}

// ── Peer group ────────────────────────────────────────────────────────────────
pub struct PeerGroup {
    channels:          BTreeMap<u8, Channel>,
    sync:              Arc<AtomicBool>,
    rdiag:             u64,
    scratch:           Vec<f32>,
    /// §6.1 Pass 1 results: (slot, merge_time_stamp) for every channel whose retrieval
    /// SUCCEEDED this cycle and whose stamp ADVANCED — the channels that vote, are
    /// averaged, and receive ladder ratios and skew adjustments. Preallocated — Pass 2
    /// walks it instead of re-scanning the map, and it fixes each timestamp at the value
    /// read immediately after that channel's own retrieval, which is the whole point of the
    /// two-pass split.
    pass1:             Vec<(u8, u32)>,
    buffer_ms:         u32,
    last_sync_logged:  Option<bool>,
    /// §6.3's common-mode correction, the group's decision over the per-channel depth
    /// relays (which run on the producer side, `Channel::cm_dir_shared`).
    ///
    /// `common_engaged` is the latch: raised when the adjusting tally leaves zero, lowered
    /// when it returns to zero. While it is raised, every cycle takes the group decision
    /// and broadcasts one ratio — the per-channel ladder does not run. `common_dir` is
    /// the direction last applied (+1 fill / -1 drain / 0); the latch moves only when a
    /// tally's direction differs from it.
    common_engaged:    bool,
    common_dir:        i8,
    /// Per-incoming-channel peak levels (bit-cast f32), indexed by source slot.
    /// Written write-only at the end of render() (Relaxed); read by the API meter
    /// endpoint. Shared with the receive-side registry so the API can find it by peer.
    peaks:             Arc<Vec<std::sync::atomic::AtomicU32>>,
    /// Buffer readout for the UI, folded over the display interval. See `BufferSnap`.
    buffer_snap:       Arc<BufferSnap>,
    /// The largest callback period this group's buffers have been reserved for. See
    /// `reserve_for_period`.
    #[cfg(not(target_os = "macos"))]
    period_hint:       usize,
}

impl PeerGroup {
    pub fn new_with_sync(_num_out_ch: u8, sync: Arc<AtomicBool>, buffer_ms: u32,
                         peaks: Arc<Vec<std::sync::atomic::AtomicU32>>) -> Self {
        Self {
            channels:         BTreeMap::new(),
            sync,
            rdiag:            0,
            scratch:          Vec::with_capacity(spsc::FRAME_MAX * 2),
            pass1:            Vec::with_capacity(128),
            buffer_ms,
            last_sync_logged: None,
            common_engaged:   false,
            common_dir:       0,
            peaks,
            buffer_snap:      Arc::new(BufferSnap::new()),
            #[cfg(not(target_os = "macos"))]
            period_hint:      0,
        }
    }

    /// Length of a new channel's resampler staging and output windows.
    fn window_len(&self) -> usize {
        #[cfg(not(target_os = "macos"))]
        { (spsc::FRAME_MAX * 2).max(self.period_hint + READ_AHEAD) }
        #[cfg(target_os = "macos")]
        { spsc::FRAME_MAX * 2 }
    }

    /// Grow every buffer sized to a render quantum — the group's scratch and each channel's
    /// resampler staging and output windows — to hold a callback of `frames`, and remember it
    /// for channels that join later.
    ///
    /// Called with the period the output device granted, when the stream is built and before
    /// it runs, so the allocations happen off the audio thread. The fixed sizes hold 1914
    /// frames (`FRAME_MAX * 2` less the read-ahead); past that the render's scratch would
    /// grow on the audio thread, and each resample job would grow its own windows on its
    /// first run. Windows and Linux devices can impose such a period. On macOS a callback is capped at the output unit's maximum slice: the requested
    /// period, or the larger one the HAL grants when a device will not go that low
    /// (`configure` in the CoreAudio backend). Only a Mac device granting more than 1914
    /// frames could reach this there, and this growth is not built for macOS.
    #[cfg(not(target_os = "macos"))]
    pub fn reserve_for_period(&mut self, frames: usize) {
        self.period_hint = self.period_hint.max(frames);
        self.scratch.reserve(frames.saturating_sub(self.scratch.len()));
        for ch in self.channels.values_mut() {
            ch.reserve_for_period(frames);
        }
    }

    /// Lock-free handle to this group's buffer readout. The engine registers it once at
    /// group creation; the stats tick reads and resets it WITHOUT taking the
    /// render_groups lock.
    pub fn buffer_snap_handle(&self) -> Arc<BufferSnap> {
        Arc::clone(&self.buffer_snap)
    }

    pub fn sync_enabled(&self) -> bool { self.sync.load(Ordering::Relaxed) }

    /// Re-apply routing to every live channel. `routing_fn` returns a slot's new output
    /// mask, or `None` when the slot is no longer routed anywhere. A routed channel has
    /// its mask updated in place through the atomic — no teardown, no buffer re-warm — and
    /// an unrouted one is removed, its meter cell zeroed. The caller removes the matching
    /// decode slot, so a later route creates the channel afresh.
    ///
    /// It takes a MASK rather than a list of outputs deliberately. This runs while
    /// `render_groups` is held, and the RT render callback takes that same lock with a
    /// BLOCKING `lock()` — so anything slow in here stalls audio. The caller resolves every
    /// mask before taking the lock, so nothing allocates in here.
    pub fn reroute_channels<F>(&mut self, routing_fn: F)
    where F: Fn(u8) -> Option<u128>,
    {
        let mut drop_slots: Vec<u8> = Vec::new();
        for (&slot, ch) in self.channels.iter() {
            match routing_fn(slot) {
                None => drop_slots.push(slot),
                Some(mask) =>
                    ch.out_mask.store(mask, std::sync::atomic::Ordering::Relaxed),
            }
        }
        for slot in drop_slots {
            // Clear the meter cell so an unrouted channel reads silence, not the
            // last value it held (its render peak is no longer being written).
            if let Some(a) = self.peaks.get(slot as usize) {
                a.store(0u32, std::sync::atomic::Ordering::Relaxed);
            }
            self.channels.remove(&slot);
        }
    }

    /// True if this group currently carries the given incoming slot.
    pub fn has_channel(&self, slot: u8) -> bool { self.channels.contains_key(&slot) }

    /// Receive-buffer health: the MEAN (depth_samples, target_samples) across the group's
    /// active channels. None when it has none. Reads are an SPSC available-count (atomic)
    /// plus a Vec len, so this is safe to call from the API path.
    ///
    /// The interval's worst and best still reach the UI — `BufferSnap` keeps them — so a
    /// single channel dipping stays visible in the tooltip's spread rather than becoming
    /// the headline number.
    pub fn buffer_fill(&self) -> Option<(usize, usize)> {
        // Only ACTIVE channels (writer still feeding) count toward the displayed buffer.
        // A parked/removed channel sits at depth 0; including it would drag the readout to
        // 0 even though the live channels are healthy.
        //
        // The MEAN across those channels, matching the mean `BufferSnap` then takes over
        // TIME. Picking the lowest-ratio channel here would make the headline number the
        // lowest depth any channel reached at any instant in the window — an extreme of an
        // extreme, which moves far more than the buffer itself does. Averaged on both axes
        // instead, one channel dipping for one cycle carries 1/(channels x renders) of the
        // reading and the bar holds still. That dip is worth knowing, and it stays visible
        // in the min/max spread `BufferSnap` reports alongside the mean.
        //
        // §10 leaves presentation to the implementation. The one requirement is that
        // reading it never touches the render thread's execution, which is unchanged.
        let mut depth_sum = 0usize;
        let mut target_sum = 0usize;
        let mut n = 0usize;
        for ch in self.channels.values().filter(|c| c.is_active_for_display()) {
            depth_sum  += ch.depth_samples();
            target_sum += ch.target_samples.max(1);
            n += 1;
        }
        // Both averaged: a peer's channels normally share one target, but they can differ
        // for a cycle around a buffer-size change, and averaging both keeps the ratio the
        // caller derives meaningful through it.
        (n > 0).then(|| (depth_sum / n, (target_sum / n).max(1)))
    }

    /// Live buffer-setting change for this peer. Only affects channels that join AFTER
    /// it: the ones already present are resized by the producer, which hands each a new
    /// ring through its swap inbox.
    pub fn set_buffer_ms(&mut self, ms: u32) { self.buffer_ms = ms; }

    #[allow(clippy::too_many_arguments)]
    pub fn add_channel(&mut self, channel: u8, rx: spsc::Consumer,
                       out_mask: u128, swap_inbox: SwapInbox,
                       cm_dir_shared: super::pool::DriftDir,
                       skew_flag: super::pool::DriftDir,
                       prebuffer_hold: super::pool::PrebufferHold,
                       boxcar_reset: Arc<std::sync::atomic::AtomicBool>,
                       skew_ref: Arc<AtomicI64>,
                       frame_samples: usize) {
        // REPLACE any channel already on this slot. A second announcement for a slot only
        // happens when its decode slot was torn down and recreated, which leaves the existing
        // channel reading a ring no producer writes any more; the newest announcement is the
        // one wired to the live decode slot (the mailbox is drained in order). Keeping the
        // first instead left the slot silent, its prebuffer hold never released, until the
        // next routing or device change.
        let window_len = self.window_len();
        self.channels.insert(channel, Channel::new(
            rx, out_mask, channel, self.buffer_ms, swap_inbox,
            cm_dir_shared, skew_flag, prebuffer_hold, boxcar_reset, skew_ref,
            frame_samples, window_len));
    }

    pub fn render(&mut self, out: &mut [f32], frames: usize, nch: usize,
                  n_active: &mut [u32]) {
        let sync = self.sync.load(Ordering::Relaxed);
        if self.last_sync_logged != Some(sync) {
            debug!("[render] sync {} ({} ch ready)",
                if sync { "ON" } else { "OFF" },
                self.channels.values().filter(|c| c.released()).count());
            self.last_sync_logged = Some(sync);
            self.rdiag = 199;
        }

        // ── RESAMPLER RATIO MODEL (Sync on) ────────────────────────────────────────
        // Pass 1 mixes every channel's previous result and records which channels' merge
        // timestamps advanced. Pass 2 decides this cycle's ratios and dispatches EVERY
        // channel, in one of three ways:
        //
        //   no channel advanced      every channel at 1.0; nothing else runs this cycle.
        //   common mode disengaged,  each advanced channel takes its own §6.2 ladder
        //   and some advanced        ratio, every other channel 1.0, and the group's
        //   channel is off the mean  averaging windows are reset (§6.5).
        //   anything else            the group decision (§6.3/§6.6), one ratio for all:
        //                              skew tally non-zero: every advanced channel's
        //                              reference moves one render quantum toward its
        //                              setpoint, the ratio is 1.0, and the common-mode
        //                              state is left exactly as it was;
        //                              otherwise the adjusting tally's sign picks 1.002,
        //                              1.0 or 0.998, and engages or releases the latch.
        //
        // Once engaged, the common-mode correction therefore OWNS the ratio: the ladder
        // does not run again until a zero adjusting tally releases it.
        //
        // Do NOT "simplify" this to depth-only with the ratio pinned at 1.0. On a clean LAN
        // both correctors are no-ops anyway — deviation ~0 leaves the ladder idle, and
        // in-band depth gives a common ratio of 1.0 — so the result looks identical right
        // up until the moment either corrector is actually needed. The resampler still runs
        // for every channel every render; there is no 1.0 bypass.

        // ── §6.1 PASS 1 — retrieve, gate, accumulate: ONE loop over the channels ──
        // Per channel, in this order and with no second pass in between: call the
        // retrieval, and only if it SUCCEEDS read that channel's merge_time_stamp, apply
        // the advancement gate and contribute to the sum.
        //
        // A failed retrieval branches past the ENTIRE block: no timestamp read, no
        // advancement check, no contribution. A slipped channel therefore drops out of the
        // group consensus completely for that cycle, instead of pulling the average toward
        // a position it never actually played.
        //
        // Two gates, not one (§6.1): retrieval success, then genuine forward movement of
        // merge_time_stamp — the stamp as the harvested job left it, compared unsigned
        // against the value latched at the previous dispatch.
        self.pass1.clear();
        let mut sum: u64 = 0;
        for (&ch_n, ch) in self.channels.iter_mut() {
            // BEFORE Gate 1: a held channel still has to age out if its writer stops,
            // or the buffer indicator sticks on "buffering" forever. See
            // `note_writer_liveness`.
            ch.note_writer_liveness();

            // Gate 1 (§4.2): prebuffer hold. Metering stays on the decode/write side
            // (§9), so a held channel is simply skipped here. Pass 2 still dispatches it.
            if !ch.ready() { continue; }

            // §7.1: a channel routed nowhere is not touched AT ALL. In practice a channel
            // in the group always has a mask: an unrouted slot never gets a channel
            // (resolve_channel), and a routing change that unroutes one removes it
            // (reroute_channels). This check keeps §7.1 true regardless.
            let mask = ch.out_mask.load(std::sync::atomic::Ordering::Relaxed);
            if mask == 0 { continue; }

            self.scratch.resize(frames, 0.0);
            if !ch.harvest(frames, sync, &mut self.scratch) { continue; }

            // FAN-OUT: the decode/resample ran ONCE; accumulate into every output this
            // source is routed to. One-to-many costs only the extra accumulate.
            let mut m = mask;
            while m != 0 {
                let oc = m.trailing_zeros() as usize;
                m &= m - 1;
                // Routing is DECOUPLED from physical outputs. If this device lacks
                // that output the route stays valid but isn't mixed. MUST be
                // bounds-safe: render runs in the CoreAudio callback (non-unwinding),
                // where an out-of-range index aborts the process.
                if oc >= nch { continue; }
                // Sum at UNITY; the 1/N gain-share is applied once per output at the
                // engine level, with smoothing, so a source joining or leaving a mix
                // glides instead of stepping −6dB (which clicked).
                for i in 0..frames {
                    out[i * nch + oc] += self.scratch[i];
                }
            }

            if !sync { continue; }   // no servo on Path B
            let ts = ch.rs.merge_ts.load(Ordering::Relaxed);
            if ts_advanced(ch.last_merge_ts, ts) {
                sum += ts as u64;
                self.pass1.push((ch_n, ts));
            }
        }

        // ── §6.2–§6.6 PASS 2 — this cycle's ratios, then dispatch ────────────────
        // Runs for EVERY channel in the group, held or ready, advanced or not: a channel
        // that is never dispatched never produces a result, and never harvests again.
        if sync {
            let count = self.pass1.len() as u64;
            let mean = group_mean(sum, count);
            let any_off = self.pass1.iter().any(|&(_, ts)| ts != mean);
            let plan = cycle_plan(count, self.common_engaged, any_off);
            if plan == CyclePlan::AllUnity {
                for ch in self.channels.values_mut() {
                    if ch.out_mask.load(Ordering::Relaxed) == 0 { continue; }
                    ch.dispatch(frames, RATIO_UNITY, false);
                }
            } else {
                let broadcast = if plan == CyclePlan::Ladder {
                    None
                } else {
                    Some(self.group_decision(frames))
                };
                // §6.5, read AFTER the group decision, which may just have moved the latch.
                let reset = !self.common_engaged && any_off;

                if self.rdiag % 400 == 0 {
                    let devs: Vec<(u8, i64)> = self.pass1.iter()
                        .map(|&(slot, ts)| (slot, ts as i64 - mean as i64))
                        .collect();
                    tracing::debug!(
                        "LADDER {} | mean {} | {} of {} channels off the mean | devs {:?}",
                        if broadcast.is_none() { "ACTIVE" } else { "idle  " }, mean,
                        devs.iter().filter(|(_, d)| *d != 0).count(), devs.len(), devs);
                    // DIAGNOSTIC (temporary, read-only): is §6.6 alive? `checked` counts
                    // jobs whose top-up recovered input — the only ones that reach the
                    // deadband check — against `skipped`, where the carried input already
                    // covered the window. `ref-sp` is each reference measured from its
                    // setpoint, which §6.6 bounds to ±min(36ms, setpoint/2).
                    let (chk, skp) = self.channels.values().fold((0u64, 0u64), |(a, b), c| (
                        a + c.rs.skew_checked.load(Ordering::Relaxed),
                        b + c.rs.skew_skipped.load(Ordering::Relaxed)));
                    let refs: Vec<(u8, f64, i8)> = self.channels.iter()
                        .map(|(&n, c)| (n,
                            ((c.skew_ref.load(Ordering::Relaxed) - c.target_samples as i64)
                                as f64 / 48.0 * 10.0).round() / 10.0,
                            c.skew_adjusting_flag.load(Ordering::Relaxed)))
                        .collect();
                    tracing::debug!("SKEW  checked {} skipped {} | (ch, ref-setpoint ms, flag) {:?}",
                        chk, skp, refs);
                }

                // TRAJECTORY (CASCADE_SYNC_MECHANISM_SPEC §7.3): while the ladder is
                // correcting, log EVERY cycle — an episode lasts tens of cycles, so a
                // sampled view cannot show the decay curve. Silent otherwise.
                let mut traj: Vec<(u8, i64, f64)> = Vec::new();
                let mut p = 0usize;
                for (&ch_n, ch) in self.channels.iter_mut() {
                    if ch.out_mask.load(Ordering::Relaxed) == 0 { continue; }
                    let ratio = match broadcast {
                        Some(r) => r,
                        None => {
                            // pass1 is in ascending slot order, as the map iterates, so one
                            // advancing index pairs them without a lookup.
                            while p < self.pass1.len() && self.pass1[p].0 < ch_n { p += 1; }
                            match self.pass1.get(p) {
                                Some(&(slot, ts)) if slot == ch_n => {
                                    let r = ladder_ratio(ts, mean);
                                    traj.push((ch_n, ts as i64 - mean as i64, r));
                                    r
                                }
                                // Advanced channels only: any other takes 1.0.
                                _ => RATIO_UNITY,
                            }
                        }
                    };
                    ch.dispatch(frames, ratio, reset);
                }
                if !traj.is_empty() {
                    debug!("TRAJ n={} | {}", traj.len(), traj.iter()
                        .map(|(ch, dev, r)| format!("ch{ch}:{dev:+} @{r:.4}"))
                        .collect::<Vec<_>>().join("  "));
                }
            }
        }

        // Active-channel tally for gain-share. A source fanned to multiple outputs
        // counts toward each of those outputs' active tally.
        for ch in self.channels.values_mut() {
            if ch.ready() && ch.level > ACTIVE_THRESHOLD {
                let mask = ch.out_mask.load(std::sync::atomic::Ordering::Relaxed);
                let mut m = mask;
                while m != 0 {
                    let oc = m.trailing_zeros() as usize;
                    if oc < n_active.len() { n_active[oc] += 1; }
                    m &= m - 1; // clear lowest set bit
                }
            }
        }
        self.rdiag = self.rdiag.wrapping_add(1);

        // Fold the mean buffer reading into this display interval, lock-free
        // (same pattern as `peaks`). The stats tick reads these atomics instead of taking
        // the render_groups lock — which the RT callback (this function) holds — so the
        // buffer display can never stall render or the select loop.
        //
        // Nothing active leaves the interval unarmed, so the tick reports "no sample"
        // rather than a stale range.
        if let Some((depth, target)) = self.buffer_fill() {
            self.buffer_snap.observe(depth, target);
        }
        // §10 tap point: how many of this peer's channels have the prebuffer hold
        // armed (0 = all released). A gauge, read without consuming.
        // Same exclusions as the depth above: a channel that is not displayed is not
        // "buffering", it is absent. Counting them differently is what left the indicator
        // stuck on for channels the depth had already dropped.
        let holding = self.channels.values()
            .filter(|c| c.is_displayed() && !c.released())
            .count();
        self.buffer_snap.holding.store(holding as u32, std::sync::atomic::Ordering::Relaxed);
    }

    /// §6.3/§6.6 — the group decision: one ratio for every channel this cycle.
    ///
    /// Tallies the two relays over the channels that advanced this cycle (§6.1). A
    /// non-zero SKEW tally wins outright: every advanced channel's reference moves one
    /// render quantum in the tally's direction and its skew flag clears, the ratio is
    /// 1.0, and the adjusting tally — though counted — decides nothing, so the common-mode
    /// state is left exactly as it was. A zero skew tally hands the cycle to the ADJUSTING
    /// tally, whose sign alone picks the ratio and, when it differs from the direction
    /// last applied, moves the common-mode latch.
    ///
    /// A zero skew tally applies nothing AND clears nothing: the flags stand until a cycle
    /// that actually acts on them.
    fn group_decision(&mut self, frames: usize) -> f64 {
        let mut adjusting: i32 = 0;
        let mut skew:      i32 = 0;
        for &(slot, _) in self.pass1.iter() {
            if let Some(ch) = self.channels.get(&slot) {
                adjusting += ch.cm_dir_shared.load(Ordering::Relaxed) as i32;
                skew      += ch.skew_adjusting_flag.load(Ordering::Relaxed) as i32;
            }
        }
        if skew != 0 {
            // howMuch is the render cycle's own frame count.
            let delta = if skew > 0 { frames as i64 } else { -(frames as i64) };
            for &(slot, _) in self.pass1.iter() {
                if let Some(ch) = self.channels.get(&slot) {
                    ch.skew_ref.fetch_add(delta, Ordering::Relaxed);
                    ch.skew_adjusting_flag.store(0, Ordering::Relaxed);
                }
            }
            return RATIO_UNITY;
        }
        let (dir, ratio) = match adjusting.signum() {
            d if d < 0 => (-1, COMMON_RATIO_DRAIN),
            d if d > 0 => ( 1, COMMON_RATIO_FILL),
            _          => ( 0, RATIO_UNITY),
        };
        if self.common_dir != dir {
            self.common_dir = dir;
            self.common_engaged = dir != 0;
            debug!("[drift] common mode {} (tally {})",
                   match dir { -1 => "DRAIN 0.998", 1 => "FILL 1.002", _ => "released" },
                   adjusting);
        }
        ratio
    }

    pub fn num_channels(&self) -> usize { self.channels.len() }

    pub fn channel_depths(&self) -> Vec<(u8, usize)> {
        self.channels.iter().map(|(&c, ch)| (c, ch.depth_samples())).collect()
    }
}

// ── Smoothed gain-share ─────────────────────────────────────────────────────
// Applies the 1/N mix gain per output, but RAMPS toward the target instead of
// stepping. When a source joins or leaves a mixed output, N changes and the target
// gain jumps (e.g. 1.0 → 0.5); stepping that in one block is a -6dB discontinuity
// that clicks. Ramping over a few ms is inaudible. Outputs with N<=1 sit at gain
// 1.0 and, once settled, the apply is skipped entirely (bit-exact passthrough).
pub struct GainShare {
    gain: Vec<f32>,   // current (smoothed) gain per output
    // Per-block smoothing coefficient. gain += (target - gain) * coef each block.
    // At ~20ms blocks, coef 0.25 settles a 1.0→0.5 change in ~5 blocks (~100ms) —
    // fast enough to feel immediate, slow enough to avoid a click.
    coef: f32,
}

impl GainShare {
    pub fn new() -> Self { Self { gain: Vec::new(), coef: 0.25 } }

    /// Apply smoothed 1/N gain to the interleaved output buffer.
    /// `n_active[oc]` = number of active sources summed into output oc.
    #[inline]
    pub fn process(&mut self, samples: &mut [f32], nch: usize, n_active: &[u32]) {
        if nch == 0 { return; }
        if self.gain.len() != nch { self.gain.resize(nch, 1.0); }
        for oc in 0..nch {
            let n = n_active.get(oc).copied().unwrap_or(0).max(1);
            let target = 1.0 / n as f32;
            let g = &mut self.gain[oc];
            // Glide toward target.
            *g += (target - *g) * self.coef;
            // Snap when very close (avoids endless tiny multiplies / denormal drift).
            if (*g - target).abs() < 1e-4 { *g = target; }
            // Bit-exact passthrough when fully settled at unity.
            if *g >= 0.99999 { continue; }
            let gg = *g;
            let frames = samples.len() / nch;
            for f in 0..frames {
                samples[f * nch + oc] *= gg;
            }
        }
    }
}

// ── Per-channel output limiter ──────────────────────────────────────────────
// One independent limiter per output channel. Each tracks its OWN gain — a peak
// on one channel never affects another (no cross-channel ducking). When a channel
// is not over threshold its gain sits at exactly 1.0 and samples pass BIT-EXACT
// (we skip the multiply entirely in that case), so an un-limited channel gets zero
// extra processing. It only does work when it actually has to catch a peak.
//
// Design (per Alex's spec): a transparent per-channel catch limiter — never linked
// across channels, never colouring audio unless it engages. Instant attack (catch
// the peak immediately), slow release (smooth recovery). Threshold ≈ −1 dBFS.
pub struct Limiter {
    gain:         Vec<f32>,   // one gain per output channel
    threshold:    f32,
    release_coef: f32,
}

impl Limiter {
    pub fn new() -> Self {
        Self { gain: Vec::new(), threshold: 0.891, release_coef: 0.9998 }
    }

    /// Process an interleaved output buffer with `nch` channels. Each channel is
    /// limited independently. Channels whose gain is 1.0 and whose sample is within
    /// threshold pass through untouched (no multiply).
    #[inline]
    pub fn process(&mut self, samples: &mut [f32], nch: usize) {
        if nch == 0 { return; }
        if self.gain.len() != nch { self.gain.resize(nch, 1.0); }
        let frames = samples.len() / nch;
        for f in 0..frames {
            let base = f * nch;
            for c in 0..nch {
                let s = &mut samples[base + c];
                let g = &mut self.gain[c];
                let peak = s.abs();
                // Engage only if this sample would exceed threshold at current gain,
                // or if we're still recovering (gain < 1.0). Otherwise leave bit-exact.
                if peak > self.threshold {
                    let headroom = self.threshold / peak;
                    if headroom < *g { *g = headroom; }
                }
                if *g < 1.0 {
                    *s *= *g;
                    *g = (*g / self.release_coef).min(1.0);
                }
                // else: gain == 1.0 and sample within threshold → untouched (bit-exact).
            }
        }
    }
}

#[cfg(test)]
mod splice_tests {
    use super::zero_crossing_splice;

    /// A 200 Hz sine at 48kHz: 240 samples per cycle, so positive-to-negative
    /// crossings land ~240 apart — spaced to put candidates inside the 120..240 window.
    fn sine(len: usize, period: f32, amp: f32, phase: f32) -> Vec<f32> {
        (0..len).map(|i| amp * ((i as f32 / period + phase) * std::f32::consts::TAU).sin())
            .collect()
    }

    #[test]
    fn drain_removes_the_gap_between_two_crossings() {
        let mut pcm = sine(960, 150.0, 0.5, 0.0);
        let out = zero_crossing_splice(&mut pcm, 960, -1);
        assert!(out < 960, "drain must shorten the frame");
        let gap = 960 - out;
        assert!((120..240).contains(&gap), "gap {gap} outside the 120..240 window");
    }

    /// FILL is DRAIN's mirror: same search, same winning pair, opposite sign.
    #[test]
    fn fill_lengthens_the_frame_by_the_same_segment_drain_would_remove() {
        let mut a = sine(1920, 150.0, 0.5, 0.0);
        let mut b = a.clone();
        let drained = zero_crossing_splice(&mut a, 960, -1);
        let filled  = zero_crossing_splice(&mut b, 960, 1);
        assert!(filled > 960, "fill must lengthen the frame");
        assert_eq!(960 - drained, filled - 960,
                   "both directions must move by the winning segment's own length");
    }

    /// The output must be the input with one contiguous segment played twice — nothing
    /// resampled, nothing synthesised. Verified without assuming where the splice landed:
    /// the head that survives unchanged locates the seam, and the whole tail after it must
    /// then be the input replayed from `gap` samples earlier.
    #[test]
    fn fill_is_exactly_the_input_with_one_segment_repeated() {
        let src = sine(1920, 150.0, 0.5, 0.0);
        let mut pcm = src.clone();
        let out = zero_crossing_splice(&mut pcm, 960, 1);
        let gap = out - 960;
        assert!(gap > 0, "fill must have spliced something");

        let seam = (0..960).take_while(|&i| pcm[i] == src[i]).count();
        assert!(seam >= gap, "seam {seam} cannot precede the segment it repeats");
        assert_eq!(&pcm[seam..out], &src[seam - gap..960],
                   "everything after the seam must be the input replayed from `gap` back");
    }

    /// A frame with no headroom cannot grow, so it falls back to a plain copy rather than
    /// writing past the decode buffer.
    #[test]
    fn fill_falls_back_to_a_plain_copy_with_no_room_to_grow() {
        let mut pcm = sine(960, 150.0, 0.5, 0.0);
        assert_eq!(zero_crossing_splice(&mut pcm, 960, 1), 960,
                   "no spare capacity — the frame must be returned unmodified");
    }

    #[test]
    fn dir_zero_is_a_no_op() {
        let mut pcm = sine(960, 150.0, 0.5, 0.0);
        assert_eq!(zero_crossing_splice(&mut pcm, 960, 0), 960);
    }

    #[test]
    fn no_crossing_falls_through_to_plain_copy() {
        // Entirely positive: no positive-to-negative crossing exists anywhere.
        let mut pcm = vec![0.25f32; 960];
        assert_eq!(zero_crossing_splice(&mut pcm, 960, -1), 960);
    }

    #[test]
    fn drop_amount_is_the_segment_between_consecutive_crossings() {
        // The candidate is scored on its distance from the PREVIOUS candidate, and DRAIN
        // removes exactly that segment — never the crossing's absolute position. With
        // crossings every 150 samples from 75, every segment is 150 long, so the drop must
        // be 150. An absolute-position reading would remove 225 here (the winning
        // crossing's own offset), which is both a different amount and, at other phases,
        // one that can fall outside the search window entirely.
        let mut pcm = sine(960, 150.0, 0.9, 0.0);
        let out = zero_crossing_splice(&mut pcm, 960, -1);
        assert_eq!(960 - out, 150,
                   "DRAIN must remove the segment between two consecutive crossings, not \
                    the winning crossing's absolute position");
    }

    #[test]
    fn a_quieter_later_segment_wins() {
        // The origin resets at every candidate, so EVERY segment is eligible wherever it
        // sits in the frame — not just those near the start. Crossings at 75, 225, 375,
        // 525… are all 150 apart and all qualify, so quietening the 375..525 span must
        // make that segment win and change which samples are removed.
        //
        // A fixed origin would score these as 150, 300, 450 from the first crossing, so
        // only the 225 candidate could ever qualify and the quieter later span would be
        // unreachable — the output would be identical either way.
        let mut a = sine(960, 150.0, 0.9, 0.0);
        let mut b = a.clone();
        for v in b[375..525].iter_mut() { *v *= 0.01; }
        let out_a = zero_crossing_splice(&mut a, 960, -1);
        let out_b = zero_crossing_splice(&mut b, 960, -1);
        // Both remove one 150-sample segment; the point is WHICH one.
        assert_eq!(960 - out_a, 150);
        assert_eq!(960 - out_b, 150);
        assert_ne!(a, b, "the quieter later segment must be the one removed, so the \
                          spliced output must differ from the uniform case");
    }
}

#[cfg(test)]
mod buffer_snap_tests {
    use super::*;

    /// The headline figure is the interval mean, and one outlier render must not define
    /// it. A hundred renders at 5760 samples with a single dip to 480 has to read as
    /// essentially 5760, not as the dip — that difference is the whole point of folding a
    /// mean rather than a minimum.
    #[test]
    fn the_reading_is_the_mean_not_the_extreme() {
        let s = BufferSnap::new();
        s.observe(480, 5760);
        for _ in 0..99 { s.observe(5760, 5760); }
        let (mean, min, max, target) = s.take().expect("100 observations");
        assert_eq!(target, 5760);
        assert_eq!(min, 480, "the dip is still reported as the spread's floor");
        assert_eq!(max, 5760);
        // (480 + 99*5760)/100 = 5707 — 53 samples (~1.1ms) off, against the 5280 (110ms)
        // a minimum would have shown.
        assert_eq!(mean, 5707);
    }

    /// A uniform sawtooth must average to its midpoint, so the ramp cancels instead of
    /// appearing as movement in the bar.
    #[test]
    fn a_sawtooth_averages_to_its_midpoint() {
        let s = BufferSnap::new();
        for d in 0..=960 { s.observe(4800 + d, 5760); }
        let (mean, min, max, _) = s.take().unwrap();
        assert_eq!(mean, 5280);
        assert_eq!((min, max), (4800, 5760));
    }

    /// Nothing observed is absent, not zero — a peer with no active channel must not be
    /// reported as an empty buffer. Taking must also arm the next interval cleanly.
    #[test]
    fn an_unobserved_interval_is_absent_and_resets() {
        let s = BufferSnap::new();
        assert!(s.take().is_none(), "nothing observed yet");
        s.observe(2400, 4800);
        assert_eq!(s.take().unwrap().0, 2400);
        assert!(s.take().is_none(), "the fold is armed empty again after a take");
    }
}

#[cfg(test)]
mod add_channel_tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn group() -> PeerGroup {
        let peaks: Arc<Vec<AtomicU32>> = Arc::new((0..128).map(|_| AtomicU32::new(0)).collect());
        PeerGroup::new_with_sync(2, Arc::new(AtomicBool::new(false)), 20, peaks)
    }

    fn add(g: &mut PeerGroup, slot: u8, rx: spsc::Consumer) {
        g.add_channel(slot, rx, 1, Arc::new(parking_lot::Mutex::new(None)),
                      Arc::new(std::sync::atomic::AtomicI8::new(0)),
                      Arc::new(std::sync::atomic::AtomicI8::new(0)),
                      Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false)),
                      Arc::new(AtomicI64::new(0)), 480);
    }

    /// A second announcement for a slot is wired to the slot's live decode ring; the channel
    /// built from the first reads a ring nothing writes. The newest must win.
    #[test]
    fn a_newer_announcement_replaces_the_channel_on_that_slot() {
        let mut g = group();
        let (_dead_tx, dead_rx) = spsc::channel(256);
        add(&mut g, 5, dead_rx);
        let (live_tx, live_rx) = spsc::channel(256);
        live_tx.write_samples(&[0.25; 40], 1_000, 40);
        add(&mut g, 5, live_rx);
        assert_eq!(g.num_channels(), 1, "still one channel on the slot");
        assert_eq!(g.channel_depths(), vec![(5, 40)],
                   "the channel must read the ring the live decode slot writes");
    }
}

#[cfg(test)]
mod mask_tests {
    use super::*;
    use std::sync::atomic::Ordering::Relaxed;

    /// A mask spanning both 64-bit halves must survive a round trip intact — the case two
    /// independent AtomicU64 halves could tear (§7.2).
    #[test]
    fn mask128_round_trips_across_the_halves() {
        let m = AtomicMask128::new(0);
        for v in [1u128, 1u128 << 63, 1u128 << 64, 1u128 << 127,
                  (1u128 << 127) | (1u128 << 3), u128::MAX] {
            m.store(v, Relaxed);
            assert_eq!(m.load(Relaxed), v, "round trip failed for {v:#x}");
        }
    }

    /// Concurrent readers must never observe a half-updated mask: every value read has to
    /// be one of the two the writer actually published, never a mix of their halves.
    #[test]
    fn mask128_never_tears_under_concurrent_writes() {
        use std::sync::Arc;
        let a = (1u128 << 127) | 1;
        let b = (1u128 << 64) | (1u128 << 2);
        let m = Arc::new(AtomicMask128::new(a));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let w = { let m = Arc::clone(&m); let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Relaxed) { m.store(a, Relaxed); m.store(b, Relaxed); }
            })};
        for _ in 0..200_000 {
            let v = m.load(Relaxed);
            assert!(v == a || v == b, "torn read observed: {v:#x}");
        }
        stop.store(true, Relaxed);
        w.join().unwrap();
    }
}

#[cfg(test)]
mod drift_tests {
    use super::DriftWindow;
    use std::sync::{Arc, atomic::AtomicI64};

    fn win() -> DriftWindow { DriftWindow::new(Arc::new(AtomicI64::new(0))) }

    /// Run `n` packets at a fixed depth so the boxcar fills and settles there.
    fn settle(w: &mut DriftWindow, depth: usize, setpoint: usize, n: usize) -> i8 {
        let mut dir = 0;
        for _ in 0..n { dir = w.update(depth, setpoint, 192); }
        dir
    }

    /// §2.3: the reference is captured when the window fills, and a reset re-arms that
    /// capture. This is the regression that matters when the setpoint moves under a live
    /// channel — a frame-size change large enough to resize the ring (§3.2) re-arms the
    /// prebuffer, which resets this window, and the channel then settles at a new depth.
    ///
    /// Holding the old reference leaves the average permanently outside the band: DRAIN
    /// arms and can never reach its release condition, so the splice fires continuously
    /// on a link carrying no drift at all.
    #[test]
    fn a_reset_re_arms_the_reference_latch_at_the_new_depth() {
        let mut w = win();
        w.set_window(120);                    // 2.5ms setpoint → 1200-sample window

        // Settle at a 5ms setpoint, resting around 10ms of depth.
        let n = w.n;
        settle(&mut w, 480, 240, n);
        assert_eq!(w.calref(), Some(480), "reference latches at the settled depth");

        // The setpoint moves to 40ms and the window is reset, exactly as the resize path
        // leaves it. The channel now rests around 40ms of depth.
        w.reset();
        assert_eq!(w.calref(), None, "reset re-arms the latch");
        let dir = settle(&mut w, 1920, 1920, n);

        assert_eq!(w.calref(), Some(1920), "reference is re-captured at the new depth");
        assert_eq!(dir, 0, "a settled channel at the new depth must not be armed");
    }

    /// The counterpart: with the reference correctly re-latched, a genuine excursion past
    /// the deadband still arms DRAIN. The fix must not disarm the servo outright.
    #[test]
    fn a_genuine_excursion_still_arms_drain() {
        let mut w = win();
        w.set_window(120);
        let n = w.n;
        settle(&mut w, 1920, 1920, n);
        assert_eq!(w.calref(), Some(1920));

        // Deadband here is min(192, setpoint/2) = 192; push well past it.
        let dir = settle(&mut w, 1920 + 400, 1920, n);
        assert_eq!(dir, -1, "average above calref + band must arm DRAIN");
    }
}

#[cfg(test)]
mod skew_tests {
    use super::{skew_flag, DriftWindow};
    use std::sync::{Arc, atomic::{AtomicI64, Ordering}};

    // The 36ms base only binds once setpoint/2 exceeds it — i.e. at setpoints of 72ms or
    // more. Below that the clamp wins and the band IS half the setpoint, which covers most
    // real buffer settings. Both regimes are pinned here so neither can drift unnoticed.
    const SP_100MS: usize = 4800;  // setpoint/2 = 2400 > 1728, so the 36ms base applies
    const SP:       i64   = 4800;
    const BAND:     i64   = 1728;  // 36ms

    /// The band is measured from the SETPOINT, and a reference sitting strictly inside it
    /// is left alone — §6.6 bounds the reference, it does not drive it to the setpoint.
    #[test]
    fn a_reference_inside_the_band_is_not_flagged() {
        assert_eq!(skew_flag(SP, SP_100MS), 0);
        assert_eq!(skew_flag(SP + BAND - 1, SP_100MS), 0);
        assert_eq!(skew_flag(SP - BAND + 1, SP_100MS), 0);
    }

    /// Both edges belong to the band's OUTSIDE: a reference exactly on either edge is
    /// flagged, toward the setpoint.
    #[test]
    fn a_reference_on_an_edge_is_flagged() {
        assert_eq!(skew_flag(SP + BAND, SP_100MS), -1);
        assert_eq!(skew_flag(SP - BAND, SP_100MS), 1);
    }

    #[test]
    fn a_reference_outside_the_band_is_flagged_toward_the_setpoint() {
        // Above the band → drain (-1), which subtracts and brings it back down.
        assert_eq!(skew_flag(SP + BAND + 1, SP_100MS), -1);
        // Below → fill (+1), which adds.
        assert_eq!(skew_flag(SP - BAND - 1, SP_100MS), 1);
    }

    /// Same `min(base, setpoint/2)` clamp every other deadband uses. At every buffer
    /// setting below 72ms this is the term that decides the band, not the 36ms base.
    #[test]
    fn the_band_is_clamped_to_half_the_setpoint() {
        // 5ms setpoint → band clamps to 120.
        assert_eq!(skew_flag(240 + 120, 240), -1);
        assert_eq!(skew_flag(240 + 119, 240), 0);

        // 40ms — a common setting, and still in the clamped regime: band = 960, not 1728.
        assert_eq!(skew_flag(1920 + 960, 1920), -1, "band at 40ms is setpoint/2 = 960");
        assert_eq!(skew_flag(1920 + 959, 1920), 0);
    }

    /// §2.1: a splice re-baselines the whole history, so DRAIN can release on the next
    /// packet. Without it the average lags the correction by a full window.
    #[test]
    fn a_splice_is_folded_out_of_the_boxcar_immediately() {
        let cell = Arc::new(AtomicI64::new(0));
        let mut w = DriftWindow::new(Arc::clone(&cell));
        let n = w.n;

        // Settle at the setpoint first, so the reference latches there.
        for _ in 0..n { w.update(1920, 1920, 192); }
        assert_eq!(w.calref(), Some(1920));

        // Now the buffer deepens. Once the average clears calref + 192 the relay arms.
        let mut dir = 0;
        for _ in 0..n { dir = w.update(2400, 1920, 192); }
        assert_eq!(dir, -1, "an average above calref + band must arm DRAIN");
        let before = w.avg();

        // Splice out 240 samples. Every entry drops by that much, so the average does too.
        w.note_splice(240, -1);
        assert_eq!(w.avg(), before - 240,
                   "average must drop by the spliced amount at once, not over the window");
    }

    /// The relay is symmetric: the low side arms at `reference − band` and releases when
    /// the average climbs back past the reference, mirroring the high side exactly.
    #[test]
    fn an_average_below_the_band_arms_fill_and_releases_at_the_reference() {
        let cell = Arc::new(AtomicI64::new(0));
        let mut w = DriftWindow::new(Arc::clone(&cell));
        let n = w.n;

        for _ in 0..n { w.update(1920, 1920, 192); }
        assert_eq!(w.calref(), Some(1920));

        // Shallower than calref − 192: FILL arms.
        let mut dir = 0;
        for _ in 0..n { dir = w.update(1600, 1920, 192); }
        assert_eq!(dir, 1, "an average below calref − band must arm FILL");

        // Still below the reference — the relay holds rather than re-testing the edge.
        for _ in 0..n / 2 { dir = w.update(1900, 1920, 192); }
        assert_eq!(dir, 1, "FILL must hold until the average passes back over calref");

        // Strictly above the reference releases it.
        for _ in 0..n { dir = w.update(2000, 1920, 192); }
        assert_eq!(dir, 0, "FILL must release once the average clears calref");
    }

    /// Sitting inside the band on a fresh window arms nothing in either direction.
    #[test]
    fn an_average_inside_the_band_arms_neither_direction() {
        let cell = Arc::new(AtomicI64::new(0));
        let mut w = DriftWindow::new(Arc::clone(&cell));
        let n = w.n;
        for _ in 0..n { w.update(1920, 1920, 192); }
        let mut dir = 0;
        for _ in 0..n { dir = w.update(1850, 1920, 192); }
        assert_eq!(dir, 0, "a deviation inside the deadband must arm nothing");
    }

    /// A fill splice raises the recorded history at once, the mirror of a drain lowering
    /// it — and unlike the drain it has no floor to guard against.
    #[test]
    fn a_fill_splice_raises_the_average_immediately() {
        let cell = Arc::new(AtomicI64::new(0));
        let mut w = DriftWindow::new(Arc::clone(&cell));
        let n = w.n;
        for _ in 0..n { w.update(1600, 1920, 192); }
        let before = w.avg();
        w.note_splice(240, 1);
        assert_eq!(w.avg(), before + 240, "average must rise by the spliced amount at once");
    }

    /// The correction cannot drive the recorded history negative.
    #[test]
    fn a_splice_larger_than_an_entry_leaves_it_alone() {
        let cell = Arc::new(AtomicI64::new(0));
        let mut w = DriftWindow::new(Arc::clone(&cell));
        w.update(100, 1920, 192);            // one shallow entry
        w.note_splice(5000, -1);             // far more than it holds
        assert_eq!(w.avg(), 100, "an entry that would go negative is untouched");
    }

    /// The reference §2.3 latches and the reference §6.6 moves are ONE value. Latching
    /// must therefore be visible through the shared handle, and an adjustment made on the
    /// render side must be visible to the producer's hysteresis on the next packet.
    #[test]
    fn the_latch_and_the_servo_share_one_value() {
        let cell = Arc::new(AtomicI64::new(0));
        let mut w = DriftWindow::new(Arc::clone(&cell));
        assert_eq!(cell.load(Ordering::Relaxed), 0, "unlatched until the window fills");

        let n = w.n;
        for _ in 0..n { w.update(1920, 1920, 192); }
        assert_eq!(cell.load(Ordering::Relaxed), 1920, "latch is visible on the shared cell");

        // §6.6 adjusts from the other side; the producer must see it.
        cell.fetch_add(-480, Ordering::Relaxed);
        assert_eq!(w.calref(), Some(1440), "producer reads the adjusted reference");
    }

    /// The relay compares the TRUNCATED average. An exact average half a sample past the
    /// reference has not yet passed it, so FILL holds until the whole-sample average does.
    #[test]
    fn fill_releases_on_the_truncated_average() {
        let cell = Arc::new(AtomicI64::new(0));
        let mut w = DriftWindow::new(Arc::clone(&cell));
        let n = w.n;
        for _ in 0..n { w.update(1920, 1920, 192); }
        let mut dir = 0;
        for _ in 0..n { dir = w.update(1600, 1920, 192); }
        assert_eq!(dir, 1);

        // Half the window at the reference, half one sample above: exactly 1920.5.
        for k in 0..n { dir = w.update(if k % 2 == 0 { 1920 } else { 1921 }, 1920, 192); }
        assert_eq!(w.avg(), 1920);
        assert_eq!(dir, 1, "1920.5 truncates to the reference, which is not past it");

        for _ in 0..n { dir = w.update(1921, 1920, 192); }
        assert_eq!(dir, 0, "a whole sample past the reference releases FILL");
    }

    /// Re-keying the window is a reset like any other: the held direction goes with it.
    #[test]
    fn a_window_re_key_clears_the_direction() {
        let cell = Arc::new(AtomicI64::new(0));
        let mut w = DriftWindow::new(Arc::clone(&cell));
        let n = w.n;
        for _ in 0..n { w.update(1920, 1920, 192); }
        for _ in 0..n { w.update(2400, 1920, 192); }
        assert_eq!(w.dir(), -1);

        assert!(!w.set_window(1920), "same setpoint tier: nothing changes");
        assert_eq!(w.dir(), -1);
        assert!(w.set_window(240), "a new tier re-keys");
        assert_eq!(w.dir(), 0, "the direction is cleared with the window");
        assert!(!w.filled());
    }
}

/// §6.1/§6.4 — the two decisions behind a channel's participation in the group
/// consensus: where its merge timestamp sits, and whether that position moved.
#[cfg(test)]
mod participation_tests {
    use super::{merge_ts_at_copy, ts_advanced};

    /// The window starts where the leftover starts, not where the cursor sits. The
    /// leftover samples are already staged, ahead of the first sample this copy takes.
    #[test]
    fn window_front_sits_behind_the_cursor_by_the_leftover() {
        assert_eq!(merge_ts_at_copy(48_000, 6), 47_994);
        assert_eq!(merge_ts_at_copy(48_000, 0), 48_000);
    }

    /// The whole point of using the live leftover: two cycles at the same cursor but
    /// different resampler remainders are DIFFERENT positions. A constant lead reports
    /// them as identical and the ladder sees no deviation to correct.
    #[test]
    fn a_moving_leftover_moves_the_stamp() {
        let unity = merge_ts_at_copy(48_000, 6);
        let drain = merge_ts_at_copy(48_000, 71);
        assert_ne!(unity, drain);
        assert_eq!(unity.wrapping_sub(drain), 65);
    }

    /// The stamp itself wraps with the u32 sample counter. The advancement gate is a plain
    /// unsigned comparison, so a channel whose stamp has just wrapped sits out that one
    /// cycle and qualifies again on the next.
    #[test]
    fn a_just_wrapped_stamp_sits_out_one_cycle() {
        assert_eq!(merge_ts_at_copy(3, 10), u32::MAX - 6);
        assert!(!ts_advanced(u32::MAX - 6, 3), "wrapped stamp compares as smaller");
        assert!(ts_advanced(3, 483), "and the next cycle is ordinary again");
    }

    /// Forward movement qualifies; standing still does not. A channel whose resample
    /// recovered no input leaves the stamp exactly where the latch put it.
    #[test]
    fn only_forward_movement_qualifies() {
        assert!(ts_advanced(1000, 1480));
        assert!(!ts_advanced(1000, 1000));
        assert!(!ts_advanced(1000, 999));
    }

    /// A channel that fell back does not qualify.
    #[test]
    fn a_backward_step_is_not_read_as_advancement() {
        assert!(!ts_advanced(3_000_000_000, 2_999_999_520));   // one render quantum back
        assert!(ts_advanced(3_000_000_000, 3_000_000_480));
    }
}

/// §3.2's auto-adapt trigger, both directions. The conditions live in engine.rs but they
/// are pure arithmetic over these two helpers, so they are pinned here where the helpers
/// are — a regression in either one shows up as a resize that fires when it shouldn't.
#[cfg(test)]
mod sizing_tests {
    use super::{target_samples_for_buffer, target_samples_floored};

    /// The setpoint tier table (§3.2), including the sub-20ms special cases and the
    /// `(ms/20)*960` general form.
    #[test]
    fn the_setpoint_table_matches_every_tier() {
        assert_eq!(target_samples_for_buffer(2),  120);   // 2.5ms
        assert_eq!(target_samples_for_buffer(5),  240);
        assert_eq!(target_samples_for_buffer(10), 480);
        assert_eq!(target_samples_for_buffer(20), 960);
        assert_eq!(target_samples_for_buffer(40), 1920);
        assert_eq!(target_samples_for_buffer(100), 4800);
    }

    /// Every frame size floors to exactly twice itself, which is what makes the floor
    /// equivalent to "set the latency to twice the frame duration" (§3.2).
    #[test]
    fn the_floor_is_two_frames_at_every_frame_size() {
        for frame in [120usize, 240, 480, 960] {
            assert_eq!(target_samples_floored(5, frame, 0).max(2 * frame), 2 * frame.max(120),
                       "frame {frame}");
        }
        // A small buffer meeting a large frame is dominated by the frame.
        assert_eq!(target_samples_floored(5, 960, 0), 1920);
        // A large buffer meeting a small frame is dominated by the buffer.
        assert_eq!(target_samples_floored(40, 120, 0), 1920);
    }

    /// Grow then shrink must come back down and then STAY, rather than keeping the
    /// enlarged buffer for the rest of the session.
    ///
    /// The landing point is `max(configured, 2 x frame)`, not the bare configured value.
    /// §3.2 reaches it in two resizes — restore the configured latency, then let the grow
    /// arm re-fire against the new frame — and the floored target reaches the same value
    /// in one. `equivalent_to_the_two_step_restore` below proves
    /// those agree for every combination; one resize is simply one prebuffer re-arm
    /// instead of two.
    #[test]
    fn a_grow_then_shrink_comes_back_down_and_settles() {
        let buffer_ms = 5;
        let configured = target_samples_for_buffer(buffer_ms);   // 240
        let mut setpoint = configured;

        // A 20ms frame arrives: 960 > 240/2 → grow.
        let frame = 960;
        assert!(frame > setpoint / 2, "the grow arm must fire");
        setpoint = target_samples_floored(buffer_ms, frame, 0);
        assert_eq!(setpoint, 1920);

        // The sender drops back to 5ms frames: 240 < 1920/2 and 1920 > 240 → shrink.
        let frame = 240;
        assert!(frame < setpoint / 2 && setpoint > configured, "the shrink arm must fire");
        setpoint = target_samples_floored(buffer_ms, frame, 0);
        assert_eq!(setpoint, 480, "settles at two frames, well below the enlarged 1920");

        // Steady state: neither arm fires again, so this does not oscillate.
        assert!(!(frame > setpoint / 2), "grow must not re-fire");
        assert!(!(frame < setpoint / 2 && setpoint > configured), "shrink must not re-fire");
    }

    /// The one-step floored restore and §3.2's two-step restore land on the same
    /// setpoint for every buffer/frame pair, so collapsing them loses nothing.
    #[test]
    fn equivalent_to_the_two_step_restore() {
        for buffer_ms in [2u32, 5, 10, 20, 40, 60, 100] {
            let configured = target_samples_for_buffer(buffer_ms);
            for frame in [120usize, 240, 480, 960] {
                // Two-step: restore the configured latency, then the grow arm re-fires
                // if that leaves the frame above half the setpoint.
                let two_step = if configured / 2 < frame { 2 * frame } else { configured };
                assert_eq!(target_samples_floored(buffer_ms, frame, 0), two_step,
                           "buffer {buffer_ms}ms frame {frame}");
            }
        }
    }

    /// At 40ms and above the shrink arm is unreachable, which is why §3.2's
    /// `configured <= 39ms` guard needs no separate restatement.
    #[test]
    fn the_shrink_arm_cannot_fire_at_forty_milliseconds_or_more() {
        for buffer_ms in [40u32, 60, 100] {
            let configured = target_samples_for_buffer(buffer_ms);
            for frame in [120usize, 240, 480, 960] {
                let setpoint = target_samples_floored(buffer_ms, frame, 0);
                assert!(setpoint <= configured,
                        "buffer {buffer_ms}ms frame {frame}: setpoint can never exceed the target");
            }
        }
    }
}

/// §5 Path A — the prebuffer fill stops on the setpoint, and keeps the newest audio.
#[cfg(test)]
mod prebuffer_tests {
    use super::{period_floor_samples, prebuffer_keep, target_samples_floored};

    /// The whole frame survives while there is room for it.
    #[test]
    fn a_frame_that_fits_is_kept_whole() {
        assert_eq!(prebuffer_keep(1920, 0, 960), 960);
        assert_eq!(prebuffer_keep(1920, 960, 960), 960, "lands exactly on the setpoint");
    }

    /// Concealment can make one arrival worth several frames. The fill stops on the
    /// setpoint rather than carrying the overshoot for the channel's whole life.
    #[test]
    fn a_concealment_inflated_arrival_is_trimmed_to_the_setpoint() {
        // depth 960, setpoint 1920, a gap recovered 3 frames in one arrival.
        let keep = prebuffer_keep(1920, 960, 2880);
        assert_eq!(keep, 960, "only the room below the setpoint");
        assert_eq!(2880 - keep, 1920, "and 1920 samples are dropped from the FRONT");
    }

    /// No room means nothing is written, rather than a wrapping subtraction.
    #[test]
    fn no_room_keeps_nothing() {
        assert_eq!(prebuffer_keep(1920, 1920, 960), 0);
        assert_eq!(prebuffer_keep(1920, 2400, 960), 0, "depth already past the setpoint");
    }

    /// The clamp cannot bind on a clean fill: every setpoint is an exact multiple of every
    /// frame size that can arrive at it, so depth walks onto the target rather than stepping
    /// over it. Holds with an output-period floor in force too — including periods no
    /// request asks for, such as a driver's fixed 512.
    #[test]
    fn a_clean_fill_never_reaches_the_clamp() {
        for period in [0usize, 120, 144, 240, 256, 441, 480, 512, 1024, 2048] {
            let floor = period_floor_samples(period);
            for buffer_ms in [2u32, 5, 10, 20, 40, 60, 100] {
                for frame in [120usize, 240, 480, 960] {
                    let setpoint = target_samples_floored(buffer_ms, frame, floor);
                    assert_eq!(setpoint % frame, 0,
                               "period {period} buffer {buffer_ms}ms frame {frame}: \
                                setpoint is whole frames");
                    // Walk the fill one frame at a time; the clamp must never trim.
                    let mut depth = 0usize;
                    while depth < setpoint {
                        assert_eq!(prebuffer_keep(setpoint, depth, frame), frame,
                                   "trimmed at depth {depth} of {setpoint}");
                        depth += frame;
                    }
                    assert_eq!(depth, setpoint, "lands exactly on the target");
                }
            }
        }
    }
}

/// The output-period floor on the receive setpoint.
#[cfg(test)]
mod period_floor_tests {
    use super::{period_floor_samples, target_samples_floored, target_samples_for_buffer};

    /// Every setpoint a buffer setting can produce, up to 10 s.
    fn buffer_levels() -> Vec<usize> {
        let mut levels: Vec<usize> = (0u32..=10_000).map(target_samples_for_buffer).collect();
        levels.sort_unstable();
        levels.dedup();
        levels
    }

    #[test]
    fn no_period_means_no_floor() {
        assert_eq!(period_floor_samples(0), 0);
    }

    /// A period granted as requested floors to exactly the level that requested it, so the
    /// floor changes nothing for a device that honours the request.
    #[test]
    fn a_requested_period_floors_to_its_own_level() {
        assert_eq!(period_floor_samples(120), target_samples_for_buffer(5));
        assert_eq!(period_floor_samples(240), target_samples_for_buffer(10));
        assert_eq!(period_floor_samples(480), target_samples_for_buffer(20));
    }

    #[test]
    fn a_longer_period_rounds_up_to_a_buffer_level() {
        assert_eq!(period_floor_samples(144), 480);    // 10 ms
        assert_eq!(period_floor_samples(256), 960);    // 20 ms
        assert_eq!(period_floor_samples(512), 1920);   // 40 ms
        assert_eq!(period_floor_samples(960), 1920);   // 40 ms
        assert_eq!(period_floor_samples(1024), 2880);  // 60 ms
    }

    /// At least two periods, one of the buffer levels, and no smaller level would do.
    #[test]
    fn the_floor_is_the_smallest_buffer_level_holding_two_periods() {
        let levels = buffer_levels();
        for period in 1usize..=4096 {
            let floor = period_floor_samples(period);
            assert!(floor >= 2 * period, "period {period}: floor {floor} holds two periods");
            assert!(levels.contains(&floor), "period {period}: floor {floor} is a buffer level");
            assert!(!levels.iter().any(|&l| l >= 2 * period && l < floor),
                    "period {period}: a smaller buffer level holds two periods");
        }
    }

    /// The floor only ever raises the setpoint, and only when it exceeds the other floors.
    #[test]
    fn the_floor_raises_but_never_lowers_the_setpoint() {
        for buffer_ms in [5u32, 10, 20, 40, 100] {
            for frame in [120usize, 240, 480, 960] {
                let without = target_samples_floored(buffer_ms, frame, 0);
                let with = target_samples_floored(buffer_ms, frame, period_floor_samples(512));
                assert_eq!(with, without.max(1920), "buffer {buffer_ms}ms frame {frame}");
            }
        }
    }
}

/// §5.4 — silence replaces what a gap cost, it does not refill the buffer.
#[cfg(test)]
mod gap_silence_tests {
    use super::gap_silence;

    /// A gap that concealment covered has lost nothing, so nothing is inserted — even
    /// when the buffer is sitting well below its setpoint and there is room to fill.
    #[test]
    fn a_concealed_gap_earns_no_silence() {
        assert_eq!(gap_silence(1920, 100, 960, 0), 0);
        assert_eq!(gap_silence(1920, 0, 480, 0), 0, "plenty of headroom, still nothing");
    }

    /// Concealment failed: the lost samples are replaced, and only those.
    #[test]
    fn a_failed_concealment_replaces_exactly_what_was_lost() {
        // depth 100, frame 960, one 480-sample frame lost, headroom 1920-1060 = 860.
        assert_eq!(gap_silence(1920, 100, 960, 480), 480,
                   "the loss, not the 860 of headroom");
    }

    /// The setpoint bounds the replacement — it never becomes the target.
    #[test]
    fn headroom_caps_the_replacement() {
        // 2880 lost but only 860 of room below the setpoint.
        assert_eq!(gap_silence(1920, 100, 960, 2880), 860);
        assert_eq!(gap_silence(1920, 1920, 960, 960), 0, "no room at all");
        assert_eq!(gap_silence(1920, 1500, 960, 960), 0, "frame alone overshoots");
    }

    /// A gap too large to conceal is left short rather than papered over, so the relay
    /// walks it back over seconds instead of a step landing in one packet.
    #[test]
    fn an_unconcealable_gap_is_left_short() {
        // gap >= 5 never attempts concealment, so `lost` is zero by construction.
        assert_eq!(gap_silence(1920, 200, 960, 0), 0);
    }
}

/// §2.2 — the boxcar window is keyed to the setpoint, so a frame-size switch that keeps
/// the ring cannot reset it and cannot re-arm §2.3's reference latch.
#[cfg(test)]
mod window_key_tests {
    use super::{boxcar_window_for_setpoint, target_samples_floored};

    /// The four tier values, keyed on the setpoint (§2.2).
    #[test]
    fn the_window_tiers_match_each_setpoint() {
        assert_eq!(boxcar_window_for_setpoint(120), 1200);   // 2.5ms
        assert_eq!(boxcar_window_for_setpoint(240), 600);    // 5ms
        assert_eq!(boxcar_window_for_setpoint(480), 300);    // 10ms
        assert_eq!(boxcar_window_for_setpoint(1920), 150);   // 40ms
        assert_eq!(boxcar_window_for_setpoint(4800), 150);   // 100ms
    }

    /// The failure this fixes: at a 40ms buffer, every incoming frame size resolves to the
    /// SAME setpoint, so switching between them must not change the window — and therefore
    /// cannot reset it or re-latch the reference.
    #[test]
    fn switching_frame_size_at_a_fixed_buffer_does_not_re_key() {
        let buffer_ms = 40;
        let mut seen = None;
        for frame in [960usize, 120, 240, 480, 960] {
            let setpoint = target_samples_floored(buffer_ms, frame, 0);
            assert_eq!(setpoint, 1920, "frame {frame}: setpoint is unchanged");
            let n = boxcar_window_for_setpoint(setpoint);
            if let Some(prev) = seen {
                assert_eq!(n, prev, "frame {frame}: window must not be re-keyed");
            }
            seen = Some(n);
        }
    }

    /// It DOES re-key when the setpoint genuinely moves — a small buffer meeting a large
    /// frame is floored upward, which is a real resize and warrants a fresh window.
    #[test]
    fn a_real_setpoint_move_does_re_key() {
        let small = boxcar_window_for_setpoint(target_samples_floored(5, 120, 0));   // 240
        let grown = boxcar_window_for_setpoint(target_samples_floored(5, 960, 0));   // 1920
        assert_eq!(small, 600);
        assert_eq!(grown, 150);
        assert_ne!(small, grown, "a genuine resize re-keys the window");
    }
}

#[cfg(test)]
mod receive_half_tests {
    use super::receive_half_for_buffer;

    /// §13.2's table, including the values BETWEEN the two exact matches — which is the
    /// whole point of it being a table. A setpoint-derived formula agrees at 5, 10 and
    /// 20-and-above and disagrees everywhere else.
    #[test]
    fn is_an_exact_match_table() {
        assert_eq!(receive_half_for_buffer(5),  120);
        assert_eq!(receive_half_for_buffer(10), 240);
        // Between the matches, and below the smaller one: all default.
        for ms in [1, 4, 6, 7, 8, 9, 11, 15, 19] {
            assert_eq!(receive_half_for_buffer(ms), 480,
                       "buffer {ms} ms should fall through to the 480 default");
        }
        // At and above 20, also the default.
        for ms in [20, 40, 120, 500, 10_000] {
            assert_eq!(receive_half_for_buffer(ms), 480, "buffer {ms} ms");
        }
    }
}


#[cfg(test)]
mod ladder_trajectory {
    use super::{fill_ratio, correction_per_cycle};

    /// How a single channel's deviation from the group mean decays, cycle by cycle.
    ///
    /// The channel is one of `n` participants, so correcting it also moves the mean it is
    /// measured against: a correction of `c` samples changes the channel's own timestamp by
    /// `c` and the mean by `c/n`, leaving the deviation smaller by `c × (1 − 1/n)`. That
    /// feedback term is the whole point of the model — omitting it is what makes a
    /// prediction that assumes the mean stands still.
    fn cycles_to_settle(start_dev: f64, n: usize, render_period: usize) -> (usize, f64) {
        let mut dev = start_dev;
        let feedback = 1.0 - 1.0 / n as f64;
        for cycle in 1..=100_000 {
            let ratio = fill_ratio(dev.round() as i32);
            let step  = correction_per_cycle(ratio, render_period) * feedback;
            if step == 0.0 { return (cycle - 1, dev); }   // unity tier: settled
            dev += step;
            if dev.abs() < 0.5 { return (cycle, dev); }   // rounds to the unity tier
        }
        (usize::MAX, dev)
    }

    /// The naive prediction: the mid tier's span (11 → 2) divided by its own rate, with the
    /// mean held still and the correction stopping at the tier boundary. This is §7.3's
    /// 18.7, reproduced so the decomposition below is measured against it rather than
    /// against a number quoted from prose.
    fn naive_mid_tier(render_period: usize) -> f64 {
        let rate = correction_per_cycle(super::RATIO_MEDIUM_DOWN, render_period).abs();
        (11.0 - 2.0) / rate
    }

    /// §7.3 records 18.7 cycles predicted for the mid tier against 36 measured, and leaves
    /// the ~2x gap open. The prediction omits two things the mechanism actually does, and
    /// this measures how much each one accounts for:
    ///
    ///   1. The correction does not stop at the tier boundary. Below |dev| = 2 the fine
    ///      tier takes over at HALF the rate and runs until the deviation rounds to zero.
    ///   2. The group mean is not a fixed target. The corrected channel is one of its own
    ///      `n` contributors, so moving it by `c` moves the mean by `c/n` and closes the
    ///      deviation by only `c × (1 − 1/n)`. At n = 2 that halves the closing rate.
    ///
    /// Neither is a defect: the mechanism and its constants are right, and the prediction
    /// was simply too simple. Together they raise the predicted count into the range the
    /// measurement sits in.
    #[test]
    fn mid_tier_convergence_decomposed() {
        const PERIOD: usize = 480;   // §7.3's 10ms render cycle at 48kHz
        let naive = naive_mid_tier(PERIOD);
        println!("  naive (boundary-stop, fixed mean)      {naive:>6.1}");
        println!("  + runs on through the fine tier to zero, and the mean moves with it:");
        let mut counts = vec![];
        for n in [2usize, 3, 4, 8, 16] {
            let (cycles, end) = cycles_to_settle(11.0, n, PERIOD);
            println!("      {n:>2} participants  {cycles:>6} cycles (ends {end:+.2})");
            counts.push((n, cycles));
        }
        // Every participant count takes longer than the naive figure — that is the claim.
        for (n, c) in &counts {
            assert!(*c as f64 > naive,
                    "n={n} gave {c} cycles, which should exceed the naive {naive:.1}");
        }
        // And the measured 36 falls inside the range the corrected model spans.
        let (lo, hi) = (counts.last().unwrap().1, counts[0].1);
        assert!((lo..=hi).contains(&36),
                "36 measured should lie within the modelled range {lo}..={hi}");
    }

    /// The fine tier matched §7.3's prediction near-exactly (8.3 predicted, 8 measured),
    /// which the same model has to explain too — otherwise it is fitted to one data point.
    #[test]
    fn fine_tier_matches_the_measured_count_under_the_same_model() {
        const PERIOD: usize = 480;
        // The fine tier is entered at |dev| = 1 and settles once it rounds to 0.
        for n in [2usize, 8] {
            let (cycles, _) = cycles_to_settle(1.0, n, PERIOD);
            println!("    fine tier, {n:>2} participants → {cycles} cycles");
        }
        let (c8, _) = cycles_to_settle(1.0, 8, PERIOD);
        assert!(c8 <= 12, "fine tier should settle in a handful of cycles, got {c8}");
    }
}

/// §6.1–§6.6 — Pass 2's decision rules, each against the smallest case that tells the
/// rule from its nearest wrong alternative.
#[cfg(test)]
mod sync_decision_tests {
    use super::*;
    use std::sync::atomic::{AtomicI8, AtomicU32};

    #[test]
    fn no_advanced_channel_means_all_unity() {
        assert_eq!(cycle_plan(0, false, false), CyclePlan::AllUnity);
        assert_eq!(cycle_plan(0, true, true), CyclePlan::AllUnity);
    }

    /// Once engaged, the common mode owns the ratio even when channels disagree.
    #[test]
    fn the_ladder_runs_only_while_common_mode_is_disengaged() {
        assert_eq!(cycle_plan(4, false, true), CyclePlan::Ladder);
        assert_eq!(cycle_plan(4, true, true), CyclePlan::Group);
        assert_eq!(cycle_plan(4, false, false), CyclePlan::Group);
        assert_eq!(cycle_plan(4, true, false), CyclePlan::Group);
    }

    /// Unsigned, on the raw stamps: floor(mean + 0.5). A signed mean taken relative to the
    /// first stamp and divided with truncation lands one sample higher on the first case.
    #[test]
    fn the_mean_rounds_half_up_on_the_raw_stamps() {
        let b = 1_000_000u64;
        assert_eq!(group_mean(b + (b - 2) + (b - 3), 3), (b - 2) as u32, "b − 1.67 → b − 2");
        assert_eq!(group_mean(b + (b + 1), 2), (b + 1) as u32, "b + 0.5 → b + 1");
        assert_eq!(group_mean(b + (b - 1), 2), b as u32, "b − 0.5 → b");
    }

    /// Stamps either side of the counter's wrap average to a value near neither.
    #[test]
    fn stamps_either_side_of_the_wrap_average_to_neither() {
        let m = group_mean((u32::MAX as u64 - 9) + 5, 2);
        assert!(m > 0x7000_0000 && m < 0x9000_0000, "mean {m:#x}");
    }

    #[test]
    fn the_ladder_takes_its_side_from_an_unsigned_comparison() {
        assert_eq!(ladder_ratio(1011, 1000), RATIO_STRONG_UP);
        assert_eq!(ladder_ratio(1002, 1000), RATIO_MEDIUM_UP);
        assert_eq!(ladder_ratio(1001, 1000), RATIO_GENTLE_UP);
        assert_eq!(ladder_ratio(1000, 1000), RATIO_UNITY);
        assert_eq!(ladder_ratio(999, 1000), RATIO_GENTLE_DOWN);
        assert_eq!(ladder_ratio(998, 1000), RATIO_MEDIUM_DOWN);
        assert_eq!(ladder_ratio(989, 1000), RATIO_STRONG_DOWN);
        // A stamp just past the wrap, against a mean just before it, is far BELOW it.
        assert_eq!(ladder_ratio(3, u32::MAX - 3), RATIO_STRONG_DOWN);
    }

    struct Probe {
        cm:        Arc<AtomicI8>,
        skew:      Arc<AtomicI8>,
        reference: Arc<AtomicI64>,
        reset:     Arc<AtomicBool>,
    }

    /// A Sync-on group of `n` released, routed channels, every one listed in Pass 1.
    fn group_with(n: u8) -> (PeerGroup, Vec<Probe>) {
        let peaks: Arc<Vec<AtomicU32>> = Arc::new((0..128).map(|_| AtomicU32::new(0)).collect());
        let mut g = PeerGroup::new_with_sync(2, Arc::new(AtomicBool::new(true)), 40, peaks);
        let mut probes = Vec::new();
        for slot in 0..n {
            let (_tx, rx) = spsc::channel(4096);
            let pr = Probe {
                cm:        Arc::new(AtomicI8::new(0)),
                skew:      Arc::new(AtomicI8::new(0)),
                reference: Arc::new(AtomicI64::new(1920)),
                reset:     Arc::new(AtomicBool::new(false)),
            };
            g.add_channel(slot, rx, 1, Arc::new(parking_lot::Mutex::new(None)),
                          Arc::clone(&pr.cm), Arc::clone(&pr.skew),
                          Arc::new(AtomicBool::new(false)), Arc::clone(&pr.reset),
                          Arc::clone(&pr.reference), 960);
            g.pass1.push((slot, 1000));
            probes.push(pr);
        }
        (g, probes)
    }

    #[test]
    fn an_adjusting_tally_engages_the_latch_and_a_zero_tally_releases_it() {
        let (mut g, p) = group_with(3);
        p[0].cm.store(-1, Ordering::Relaxed);
        p[1].cm.store(-1, Ordering::Relaxed);
        assert_eq!(g.group_decision(480), RATIO_STRONG_DOWN);
        assert!(g.common_engaged);
        assert_eq!(cycle_plan(3, g.common_engaged, true), CyclePlan::Group,
                   "engaged: the next cycle broadcasts even with channels off the mean");

        p[0].cm.store(0, Ordering::Relaxed);
        p[1].cm.store(0, Ordering::Relaxed);
        assert_eq!(g.group_decision(480), RATIO_UNITY);
        assert!(!g.common_engaged, "a zero tally releases the latch");
    }

    #[test]
    fn a_skew_tally_takes_the_cycle_at_unity_and_leaves_the_latch_alone() {
        let (mut g, p) = group_with(3);
        p[0].cm.store(-1, Ordering::Relaxed);
        assert_eq!(g.group_decision(480), RATIO_STRONG_DOWN);
        assert!(g.common_engaged);

        // The adjusting tally now reads zero — which would release — but the skew relay
        // votes, and a skew cycle decides nothing else.
        p[0].cm.store(0, Ordering::Relaxed);
        p[1].skew.store(1, Ordering::Relaxed);
        p[2].skew.store(1, Ordering::Relaxed);
        assert_eq!(g.group_decision(480), RATIO_UNITY, "a skew cycle runs at 1.0");
        assert!(g.common_engaged, "the adjusting tally is not acted on");
        for pr in &p {
            assert_eq!(pr.reference.load(Ordering::Relaxed), 1920 + 480,
                       "every advanced channel moves one render quantum");
            assert_eq!(pr.skew.load(Ordering::Relaxed), 0, "and its flag clears");
        }
    }

    #[test]
    fn only_advanced_channels_vote_and_move() {
        let (mut g, p) = group_with(3);
        g.pass1.retain(|&(slot, _)| slot != 2);
        p[2].cm.store(1, Ordering::Relaxed);
        p[2].skew.store(-1, Ordering::Relaxed);
        assert_eq!(g.group_decision(480), RATIO_UNITY);
        assert!(!g.common_engaged, "a channel that did not advance does not vote");
        assert_eq!(p[2].reference.load(Ordering::Relaxed), 1920, "nor is it moved");
        assert_eq!(p[2].skew.load(Ordering::Relaxed), -1, "nor is its flag cleared");
    }

    /// Readies every channel as a finished job would leave it, with the given stamps, so
    /// one `render` takes them all through Pass 1 as advanced.
    fn arm(g: &mut PeerGroup, stamps: &[u32]) {
        g.pass1.clear();
        for (slot, &ts) in stamps.iter().enumerate() {
            let ch = g.channels.get_mut(&(slot as u8)).unwrap();
            ch.last_merge_ts = ts - 480;
            ch.rs.merge_ts.store(ts, Ordering::Relaxed);
            ch.rs.ready.store(true, Ordering::Relaxed);
        }
    }

    fn render_once(g: &mut PeerGroup) {
        let mut out = vec![0.0f32; 480 * 2];
        let mut n_active = vec![0u32; 2];
        g.render(&mut out, 480, 2, &mut n_active);
    }

    /// §6.5: reset while the ladder corrects and the common mode is disengaged — the
    /// latch read AFTER this cycle's group decision.
    #[test]
    fn the_ladder_cycle_resets_every_channel_s_averaging() {
        let (mut g, p) = group_with(3);
        arm(&mut g, &[10_000, 10_000, 10_011]);
        render_once(&mut g);
        assert!(p.iter().all(|pr| pr.reset.load(Ordering::Relaxed)),
                "a ladder cycle resets every channel, including those at the mean");
    }

    #[test]
    fn an_engaged_common_mode_suppresses_the_ladder_and_the_reset() {
        let (mut g, p) = group_with(3);
        g.common_engaged = true;
        g.common_dir = -1;
        for pr in &p { pr.cm.store(-1, Ordering::Relaxed); }
        arm(&mut g, &[10_000, 10_000, 10_011]);
        render_once(&mut g);
        assert!(g.common_engaged);
        assert!(p.iter().all(|pr| !pr.reset.load(Ordering::Relaxed)),
                "no reset while the common mode is engaged");
    }

    /// The cycle that RELEASES the latch reads it as released, so if channels disagree
    /// that same cycle resets — at the broadcast 1.0, not with ladder ratios.
    #[test]
    fn the_releasing_cycle_resets_when_channels_disagree() {
        let (mut g, p) = group_with(3);
        g.common_engaged = true;
        g.common_dir = -1;
        arm(&mut g, &[10_000, 10_000, 10_011]);
        render_once(&mut g);
        assert!(!g.common_engaged);
        assert!(p.iter().all(|pr| pr.reset.load(Ordering::Relaxed)));
    }

    /// The first segment is seeded with its own first sample, as every later one is.
    /// Here the only difference between two equal-length candidates is that first sample,
    /// so an unseeded first segment would look quieter than it is and win.
    #[test]
    fn the_first_splice_candidate_counts_its_first_sample() {
        let mut pcm = vec![0.0f32; 400];
        pcm[0]   = 0.1;
        pcm[1]   = -0.9;   // first segment: [1, 150)
        pcm[150] = -0.5;   // second segment: [150, 299)
        pcm[299] = -0.5;
        let out = zero_crossing_splice(&mut pcm, 400, -1);
        assert_eq!(out, 400 - 149);
        assert_eq!(pcm[1], -0.9, "the louder first segment is kept");
        assert_eq!(pcm[150], -0.5, "the quieter second one is dropped");
    }
}
