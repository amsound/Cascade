//! Lock-free single-producer / single-consumer ring — a dual per-sample array model.
//!
//! The ring is TWO parallel flat arrays indexed by a per-sample position:
//!   - `audio`: f32 sample, one slot per sample.
//!   - `ts`:    u32 sender timestamp, one slot per sample.
//! Both are `cap` long, where cap = 2*setpoint + 1920 samples. `write` and `read` are
//! monotonic per-sample positions; physical index = pos % cap.
//!
//! WHY two parallel arrays: indexing audio and timestamps by the SAME per-sample index
//! lets the reader take N samples from ANY offset, spanning any number of packet
//! boundaries, and know each sample's exact timestamp in O(1) — no frame-boundary
//! bookkeeping. That is what makes the read position a flat cursor, and what makes
//! channels from one peer align: every channel's cursor over a shared timestamp
//! timeline yields identical per-sample timestamps.
//!
//! WRITE: for each sample i of a packet whose base timestamp is base_ts,
//!   ts[(w+i)%cap]    = base_ts + i      (strictly +1 per sample)
//!   audio[(w+i)%cap] = input[i]
//! then write += count. Loss/gap padding writes the same way with audio = 0, so the
//! timestamp timeline stays contiguous with the sender clock across losses.
//!
//! Thread safety: exactly ONE producer and ONE consumer. Producer advances
//! `write`; consumer advances `read`. Disjoint slot ranges, never concurrent.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::cell::UnsafeCell;

/// Maximum Opus frame size at 48kHz (20ms). Used by callers that stage a decode
/// into a fixed scratch buffer before writing it to the ring.
pub const FRAME_MAX: usize = 960;

struct Inner {
    cap:    usize,                 // capacity in SAMPLES (flat, not rounded to a power of two)
    audio:  UnsafeCell<Vec<f32>>,  // len = cap
    ts:     UnsafeCell<Vec<u32>>,  // len = cap
    /// Monotonic per-sample positions; physical index = pos % cap.
    /// Single producer / single consumer.
    write:  AtomicU64,
    read:   AtomicU64,
}

unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

pub struct Producer { inner: Arc<Inner> }
pub struct Consumer { inner: Arc<Inner> }

/// Create a ring of exactly `cap_samples` samples — a flat, non-power-of-two capacity,
/// indexed modulo cap.
pub fn channel(cap_samples: usize) -> (Producer, Consumer) {
    let cap = cap_samples.max(2);
    let inner = Arc::new(Inner {
        cap,
        audio: UnsafeCell::new(vec![0.0; cap]),
        ts:    UnsafeCell::new(vec![0;   cap]),
        write: AtomicU64::new(0),
        read:  AtomicU64::new(0),
    });
    (Producer { inner: inner.clone() }, Consumer { inner })
}

impl Inner {
    #[inline] fn depth(&self) -> usize {
        self.write.load(Ordering::Acquire)
            .wrapping_sub(self.read.load(Ordering::Acquire)) as usize
    }
}

impl Producer {
    /// Write up to `samples.len()` audio samples with per-sample timestamps base_ts+i,
    /// advancing `write`. PARTIAL-WRITE-AWARE: writes whatever fits in the remaining
    /// room rather than discarding the whole write on a marginal overflow (see
    /// `write_inner`). Returns the count actually written, which may be less than
    /// samples.len().
    /// `original_count` is the length the SENDER transmitted, before any producer-side
    /// splice changed it. When a fill splice has lengthened the frame the extra samples
    /// must not carry the timeline past what was actually sent, so the tail timestamps are
    /// clamped against this rather than continuing to climb — see `write_inner`.
    pub fn write_samples(&self, samples: &[f32], base_ts: u32, original_count: usize) -> usize {
        self.write_inner(samples.len(), base_ts, Some(samples), original_count)
    }

    /// Write up to `n` SILENCE samples carrying timestamps base_ts+i (loss/gap pad).
    /// Keeps the timestamp timeline contiguous across loss so the read position stays
    /// anchored to the sender clock. PARTIAL-WRITE-AWARE, as `write_samples`. Returns
    /// the count actually written.
    pub fn write_silence(&self, n: usize, base_ts: u32) -> usize {
        // Silence is never spliced, so its own length IS the original count and the tail
        // clamp below can never engage.
        self.write_inner(n, base_ts, None, n)
    }

    /// Writes `min(n, free_room)` samples/silence — PARTIAL-WRITE-AWARE: a write that
    /// doesn't fully fit writes whatever DOES fit rather than being discarded whole.
    /// Returns the count actually written.
    ///
    /// Loss detection in engine.rs works from packet sequence numbers and never reads the
    /// ring, so a partial write only affects how much audible content survives a marginal
    /// near-overrun write.
    fn write_inner(&self, n: usize, base_ts: u32, samples: Option<&[f32]>,
                   original_count: usize) -> usize {
        let inner = &*self.inner;
        if n == 0 { return 0; }
        // The write is bounded by the ring's physical CAPACITY, per
        // CASCADE_AUDIO_RECEIVE_SPEC §12.2:
        //
        //     freeSpace = capacity - (writePos - readPos)
        //     if freeSpace >= requestedLength: write the full requested length
        //
        // The setpoint plays no part in this bound. Depth is bounded at 2×setpoint by the
        // overrun bail in engine.rs, which discards the whole packet before this is ever
        // reached. This helper's only jobs are to not overrun the allocation, and to write
        // the partial that fits rather than refusing the write outright.
        let write_cap = inner.cap;
        let free_room = write_cap.saturating_sub(inner.depth());
        let n = n.min(free_room);
        if n == 0 { return 0; }
        let cap = inner.cap;
        let w = inner.write.load(Ordering::Relaxed);
        let audio = unsafe { &mut *inner.audio.get() };
        let ts    = unsafe { &mut *inner.ts.get() };
        for i in 0..n {
            let p = ((w as usize).wrapping_add(i)) % cap;
            audio[p] = match samples { Some(s) => s[i], None => 0.0 };
            ts[p]    = base_ts.wrapping_add(i as u32);
        }
        // ── Tail timestamps are bounded by the ORIGINAL count ──
        // A fill splice writes more samples than the sender sent. Left alone, their
        // timestamps would keep climbing and the ring's timeline would run ahead of the
        // sender's — and that timeline is what the read cursor reports as
        // merge_time_stamp, so every downstream comparison would inherit the error.
        //
        // The final `n - original_count` slots are rewritten instead, walking BACKWARD
        // from the last one with values descending from `base_ts + original_count - 1`.
        // The highest timestamp anywhere in the ring is therefore exactly the last one the
        // sender transmitted, whatever the splice did to the frame's length.
        //
        // This runs before the Release store below, so a consumer never observes the
        // uncorrected values.
        if n > original_count {
            let mut stamp = base_ts.wrapping_add(original_count as u32).wrapping_sub(1);
            for k in 0..n - original_count {
                let p = (w as usize).wrapping_add(n - 1 - k) % cap;
                ts[p] = stamp;
                stamp = stamp.wrapping_sub(1);
            }
        }
        inner.write.store(w.wrapping_add(n as u64), Ordering::Release);
        n
    }

    /// Buffered depth in samples (write − read).
    pub fn samples_buffered(&self) -> usize { self.inner.depth() }

}

impl Consumer {
    /// Timestamp at the current read position = ts[read % cap]. Returns None when empty.
    ///
    /// The merge timestamp is this value minus the resampler's carried-over input count
    /// (CASCADE_SYNC_MECHANISM_SPEC §6.4), applied in channel_sync, not here.
    pub fn read_ts(&self) -> Option<u32> {
        let inner = &*self.inner;
        if inner.depth() == 0 { return None; }
        let r = inner.read.load(Ordering::Relaxed) as usize;
        let ts = unsafe { &*inner.ts.get() };
        Some(ts[r % inner.cap])
    }

    /// Sample-accurate flat read: copy exactly `out.len()` samples from the current
    /// read position, advancing `read`. Returns the count copied (< out.len() only on
    /// underrun). No frame boundaries — a flat cursor over the per-sample array.
    pub fn read_into(&self, out: &mut [f32]) -> usize {
        let inner = &*self.inner;
        let cap = inner.cap;
        let avail = inner.depth();
        let take = out.len().min(avail);
        if take == 0 { return 0; }
        let r = inner.read.load(Ordering::Relaxed) as usize;
        let audio = unsafe { &*inner.audio.get() };
        for i in 0..take {
            out[i] = audio[(r + i) % cap];
        }
        inner.read.store((r as u64).wrapping_add(take as u64), Ordering::Release);
        take
    }

    /// A read-only view of this ring's positions, for a thread that needs its depth or
    /// write position but must not read samples. The consumer itself can then live with
    /// whichever thread does the reading, and the probe stays with everyone else.
    pub fn probe(&self) -> RingProbe { RingProbe { inner: Arc::clone(&self.inner) } }

}

/// Positions only — depth and write position, both plain atomic loads. It cannot read
/// or advance anything, so holding one alongside the consumer never breaks the
/// single-consumer rule.
pub struct RingProbe { inner: Arc<Inner> }

impl RingProbe {
    /// Available samples (write − read).
    pub fn samples_available(&self) -> usize { self.inner.depth() }

    /// Absolute write position (monotonic sample counter, pre-modulo). Used by the render
    /// layer to detect whether the WRITER is still advancing (a live feed) vs frozen
    /// (sender removed the channel). Not a ring depth — a position that only moves when
    /// the producer writes. Wrapping arithmetic on the caller side.
    pub fn write_pos(&self) -> u64 { self.inner.write.load(Ordering::Acquire) }
}

/// The two write-side rules that are easy to get backwards: which end of an oversized
/// frame survives, and what bounds the timeline when a splice has lengthened one.
#[cfg(test)]
mod write_tests {
    use super::channel;

    /// An oversized write keeps the FRONT of the frame. The tail is what gets dropped —
    /// the newest audio is discarded, not the oldest, so the samples that do land stay
    /// contiguous with everything already in the ring.
    #[test]
    fn an_overrun_keeps_the_head_and_truncates_the_tail() {
        let (tx, rx) = channel(8);
        let frame: Vec<f32> = (0..12).map(|i| i as f32).collect();
        assert_eq!(tx.write_samples(&frame, 1000, frame.len()), 8, "only the room available");

        let mut out = [0.0f32; 8];
        assert_eq!(rx.read_into(&mut out), 8);
        assert_eq!(out, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
                   "the first 8 samples must survive, not the last 8");
    }

    /// A fill splice writes more than the sender sent. The extra samples must not carry the
    /// ring's timeline past the sender's own last timestamp, because that timeline is what
    /// the read cursor reports downstream.
    #[test]
    fn a_lengthened_frame_cannot_push_the_timeline_past_the_sender() {
        let (tx, rx) = channel(64);
        let spliced: Vec<f32> = (0..12).map(|i| i as f32).collect();   // 10 sent, 12 written
        tx.write_samples(&spliced, 500, 10);

        // Walk the ring and collect every timestamp the consumer can observe.
        let mut seen = Vec::new();
        for _ in 0..12 {
            seen.push(rx.read_ts().expect("ring is not empty"));
            let mut one = [0.0f32; 1];
            rx.read_into(&mut one);
        }
        let highest = *seen.iter().max().unwrap();
        assert_eq!(highest, 509, "the sender's last timestamp is base + original - 1");
        assert_eq!(&seen[..10], &[500, 501, 502, 503, 504, 505, 506, 507, 508, 509],
                   "the sent samples keep their own timestamps");
    }

    /// With nothing spliced, the clamp must not engage: every sample is stamped in sequence.
    #[test]
    fn an_unspliced_frame_is_stamped_straight_through() {
        let (tx, rx) = channel(64);
        let frame = vec![0.0f32; 6];
        tx.write_samples(&frame, 900, frame.len());
        for expect in 900..906 {
            assert_eq!(rx.read_ts(), Some(expect));
            let mut one = [0.0f32; 1];
            rx.read_into(&mut one);
        }
    }
}
