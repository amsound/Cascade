//! Incoming-signal metering support (CASCADE_AUDIO_RECEIVE_SPEC §9.2/§9.3): who is
//! watching, the pre-decode meter worker, and the published snapshot every viewer reads.
//!
//! Nothing here runs on an audio thread, and the receive thread's part is bounded to one
//! atomic read per packet plus, for a packet that is actually metered, one copy into a
//! bounded queue it never waits on.
//!
//! - **Watch**: every meter poll names what it is showing. Each poll pushes a deadline
//!   LEASE into the future — for the named remote, and for "any meter at all". Any number
//!   of viewers keep the same deadlines alive; when the last stops polling they lapse by
//!   themselves, so a viewer that vanishes without a word cannot leave work running.
//! - **Worker**: the pre-decode meter's throwaway decode runs on its own thread, fed only
//!   packets for unrouted channels of a remote being watched.
//! - **Snapshot**: while any meter is watched, a fixed-rate task drains every peak
//!   accumulator into one published snapshot. Viewers read it without resetting anything,
//!   so every viewer sees the same values however many there are.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// How long one poll keeps its remote (and the snapshot task) live. Viewers poll at 25 Hz;
/// this rides out a slow tab or a dropped request without letting the work outlive the
/// viewer by more than a moment.
pub const LEASE: Duration = Duration::from_millis(1500);

/// Snapshot cadence while anything is watched — the viewers' own poll rate.
pub const SNAPSHOT_PERIOD: Duration = Duration::from_millis(40);

/// Channel slots per remote.
pub const SLOTS: usize = 128;

/// Pre-decode meter jobs the worker may have queued. Beyond this the receive thread drops
/// the job rather than wait: a meter missing one packet's peak is invisible, a stalled
/// receive thread is not.
const QUEUE: usize = 1024;

/// A throwaway decoder unused for this long is dropped (its remote stopped being watched,
/// or its channel was routed).
const DECODER_IDLE: Duration = Duration::from_secs(3);

/// Who is watching. Deadlines are milliseconds since `epoch`; a deadline in the past means
/// nobody is.
pub struct Watch {
    epoch: Instant,
    any:   AtomicU64,
    peers: RwLock<HashMap<String, Arc<AtomicU64>>>,
}

impl Watch {
    fn new() -> Self {
        Watch { epoch: Instant::now(), any: AtomicU64::new(0), peers: RwLock::new(HashMap::new()) }
    }

    fn now_ms(&self) -> u64 { self.epoch.elapsed().as_millis() as u64 }

    /// A viewer polled: extend the "any meter" deadline, and the named remote's if it
    /// named one.
    pub fn touch(&self, peer: Option<&str>) {
        let until = self.now_ms() + LEASE.as_millis() as u64;
        self.any.fetch_max(until, Ordering::Relaxed);
        if let Some(p) = peer {
            self.handle(p).fetch_max(until, Ordering::Relaxed);
        }
    }

    /// A remote's deadline cell, created on first use. Cells are never removed, so a
    /// holder's copy stays the live one for that name for the life of the process.
    pub fn handle(&self, peer: &str) -> Arc<AtomicU64> {
        if let Some(h) = self.peers.read().unwrap_or_else(|e| e.into_inner()).get(peer) {
            return Arc::clone(h);
        }
        Arc::clone(self.peers.write().unwrap_or_else(|e| e.into_inner())
            .entry(peer.to_string()).or_default())
    }

    /// Whether the remote behind `deadline` is being watched right now.
    pub fn is_live(&self, deadline: &AtomicU64) -> bool {
        deadline.load(Ordering::Relaxed) > self.now_ms()
    }

    /// Whether any meter is being watched right now.
    pub fn any_live(&self) -> bool { self.is_live(&self.any) }
}

/// One packet for the pre-decode meter: its (already decrypted) payload and where its
/// peak goes.
pub struct MeterJob {
    pub peer:    Arc<str>,
    pub channel: u8,
    pub codec:   u8,
    pub frame:   Vec<u8>,
    pub cells:   Arc<Vec<AtomicU32>>,
}

/// Everything the metering needs, shared by the receive thread (feeds it), the API (touches
/// the watch, reads the snapshot) and the snapshot task (drains the accumulators).
pub struct Meters {
    pub watch: Watch,
    /// Pre-decode peaks per remote, SLOTS cells each (bit-cast f32, running max, drained by
    /// the snapshot task). Entries are never removed, for the same reason as the watch.
    pre: RwLock<HashMap<String, Arc<Vec<AtomicU32>>>>,
    /// The latest snapshot, as the JSON `/api/peaks` returns.
    snapshot: RwLock<Arc<serde_json::Value>>,
    jobs: crossbeam_channel::Sender<MeterJob>,
}

impl Meters {
    /// Create the metering state and start its worker thread.
    pub fn start() -> Arc<Self> {
        let (tx, rx) = crossbeam_channel::bounded::<MeterJob>(QUEUE);
        std::thread::Builder::new()
            .name("cascade-meter".into())
            .spawn(move || worker(rx))
            .expect("spawn meter worker");
        Arc::new(Meters {
            watch: Watch::new(),
            pre: RwLock::new(HashMap::new()),
            snapshot: RwLock::new(Arc::new(serde_json::json!({
                "input": [], "output": [], "incoming": {}, "tone": []
            }))),
            jobs: tx,
        })
    }

    /// A remote's pre-decode peak cells, created on first use.
    pub fn pre_cells(&self, peer: &str) -> Arc<Vec<AtomicU32>> {
        if let Some(c) = self.pre.read().unwrap_or_else(|e| e.into_inner()).get(peer) {
            return Arc::clone(c);
        }
        Arc::clone(self.pre.write().unwrap_or_else(|e| e.into_inner())
            .entry(peer.to_string())
            .or_insert_with(|| Arc::new((0..SLOTS).map(|_| AtomicU32::new(0)).collect())))
    }

    /// Every remote's pre-decode cells, for the snapshot task.
    pub fn all_pre_cells(&self) -> Vec<(String, Arc<Vec<AtomicU32>>)> {
        self.pre.read().unwrap_or_else(|e| e.into_inner()).iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v))).collect()
    }

    /// Hand a packet to the meter worker. Never blocks: a full queue drops the job.
    pub fn submit(&self, job: MeterJob) {
        let _ = self.jobs.try_send(job);
    }

    pub fn snapshot(&self) -> Arc<serde_json::Value> {
        Arc::clone(&self.snapshot.read().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn publish(&self, v: serde_json::Value) {
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(v);
    }
}

/// The pre-decode meter worker (§9.2): peak of each packet, into its cell as a running max.
/// Raw PCM is read straight off the bytes; Opus goes through a throwaway decoder per
/// (remote, channel), whose output is discarded once its peak is taken.
fn worker(rx: crossbeam_channel::Receiver<MeterJob>) {
    let mut decoders: HashMap<(Arc<str>, u8), (opus::Decoder, Instant)> = HashMap::new();
    let mut scratch = vec![0.0f32; crate::audio::spsc::FRAME_MAX];
    let mut last_sweep = Instant::now();
    loop {
        let job = match rx.recv_timeout(DECODER_IDLE) {
            Ok(j) => Some(j),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        };
        if let Some(j) = job {
            let pk = peak(&mut decoders, &mut scratch, &j);
            if let Some(cell) = j.cells.get(j.channel as usize) {
                cell.fetch_max(pk.to_bits(), Ordering::Relaxed);
            }
        }
        if last_sweep.elapsed() >= DECODER_IDLE {
            decoders.retain(|_, (_, used)| used.elapsed() < DECODER_IDLE);
            last_sweep = Instant::now();
        }
    }
}

/// Peak sample magnitude of one packet. Opus float output is already normalised to
/// [-1, 1]; a decode error meters as silence.
fn peak(decoders: &mut HashMap<(Arc<str>, u8), (opus::Decoder, Instant)>,
        scratch: &mut [f32], j: &MeterJob) -> f32 {
    match j.codec {
        crate::net::protocol::CODEC_RAW16 => {
            return j.frame.chunks_exact(2)
                .map(|c| (i16::from_le_bytes([c[0], c[1]]) as f32 / 32_767.0).abs())
                .fold(0.0, f32::max);
        }
        crate::net::protocol::CODEC_RAW24 => {
            return j.frame.chunks_exact(3)
                .map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) as f32
                          / 2_147_483_647.0).abs())
                .fold(0.0, f32::max);
        }
        _ => {}
    }
    let entry = match decoders.entry((Arc::clone(&j.peer), j.channel)) {
        std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => {
            match opus::Decoder::new(48_000, opus::Channels::Mono) {
                Ok(d) => v.insert((d, Instant::now())),
                Err(_) => return 0.0,
            }
        }
    };
    entry.1 = Instant::now();
    match entry.0.decode_float(&j.frame, scratch, false) {
        Ok(n) => scratch[..n].iter().fold(0.0f32, |m, s| m.max(s.abs())),
        Err(_) => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A poll makes its remote, and "any", live; an unnamed poll makes only "any" live.
    #[test]
    fn touch_sets_the_deadlines() {
        let w = Watch::new();
        let a = w.handle("a");
        assert!(!w.any_live() && !w.is_live(&a));
        w.touch(None);
        assert!(w.any_live() && !w.is_live(&a));
        w.touch(Some("a"));
        assert!(w.is_live(&a));
        assert!(!w.is_live(&w.handle("b")));
    }

    /// Two viewers of one remote share one deadline; handles for a name are one cell.
    #[test]
    fn handles_are_shared_per_name() {
        let w = Watch::new();
        let h1 = w.handle("a");
        let h2 = w.handle("a");
        assert!(Arc::ptr_eq(&h1, &h2));
    }

    /// Raw 16-bit peaks come straight off the bytes.
    #[test]
    fn raw16_peak() {
        let mut d = HashMap::new();
        let mut s = vec![0.0f32; 16];
        let frame: Vec<u8> = [0i16, -16_384, 8_192].iter()
            .flat_map(|v| v.to_le_bytes()).collect();
        let j = MeterJob { peer: Arc::from("a"), channel: 0,
                           codec: crate::net::protocol::CODEC_RAW16, frame,
                           cells: Arc::new(vec![]) };
        let p = peak(&mut d, &mut s, &j);
        assert!((p - 0.5).abs() < 1e-3, "{p}");
    }
}
