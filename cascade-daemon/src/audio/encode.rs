/// Transmit path: parallel per-stream encode + direct send.
///
/// Device callback → one `dispatch` hop onto the handoff serial queue → `encode_apply`
/// fans the frame's send streams — one per (channel, frame size, mode) in use — across the
/// scheduler's worker pool → a serial pass emits each stream's packets in fixed channel
/// order. All channels complete within ~300-700µs.
///
/// One pipeline on every platform. Only the scheduler underneath differs: GCD serial
/// queues over the global pool on macOS, serial queues over the shared worker pool on
/// Linux (`audio/scheduler/`).
///
/// The parallelism is the point, and CASCADE_AUDIO_SEND_SPEC §2 requires it: encoding
/// channels sequentially serialises the whole frame behind the slowest channel and costs
/// milliseconds of send jitter. Fanning them out means per-frame channel sends complete
/// in a random order, which is the observable signature that they really are running
/// concurrently.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use crate::audio::{Device, Stream};
use super::scheduler::make_scheduler;
use tracing::{info, warn, debug};
use anyhow::{Result, anyhow};

pub const SAMPLE_RATE: u32  = 48_000;
/// Maximum supported Opus frame size (20ms at 48kHz). Used as a buffer ceiling.
/// Actual outgoing frame size is per remote (`RemoteConfig::frame_ms`) at runtime.
pub const FRAME_MAX:   usize = 960;
/// Both audio-unit buffers are capped at 480 frames (10 ms @ 48 kHz) and otherwise follow
/// the **local (outgoing/send) frame size**: buffer = min(frame_samples, 480). Input and
/// output both follow the local send frame; the incoming stream's frame size affects
/// neither. Shared by the input (here) and output (engine.rs) paths.
pub const IO_BUF_CAP_FRAMES: usize = 480;

/// Fixed ceiling for the outgoing source channels. This sizes the per-channel arrays
/// (accumulators, channel plan, streams) — it is NOT the number of channels actually sent,
/// which always follows the live input device. It must never be derived from the startup
/// device: a ceiling taken from a 1-channel boot device would hold the whole session to 1,
/// so a later 8-channel device could only ever send channel 1. A high constant ceiling no
/// real device exceeds removes that coupling; the arrays are cheap when unused.
pub const OUTGOING_CHANNELS_MAX: usize = 128;

// ── Per-channel encoder ────────────────────────────────────────────────────

pub struct ChannelEncoder {
    pub encoder:       opus::Encoder,
    pub seq:           u16,
    /// Outgoing frame size in samples — fixed at construction: a stream's frame size and
    /// mode are its identity (`StreamKey`, CASCADE_AUDIO_SEND_SPEC §5), so another pair is
    /// another encoder. 120=2.5ms, 240=5ms, 480=10ms, 960=20ms.
    pub frame_samples: usize,
    /// Result of THIS frame's encode (seq, opus_byte_count), stashed by the
    /// parallel encode worker for the serial send pass to pick up.
    /// None = not encoded this frame (unrouted, or encode failed).
    pub pending: Option<(u16, usize)>,
    /// Pre-allocated packet buffer. Reused every frame — zero heap allocation per packet.
    /// Opus payload written to pkt_buf[HEADER_LEN..], header filled by caller.
    pub pkt_buf:    Vec<u8>,
}

impl ChannelEncoder {
    fn new(bitrate_kbps: u32,
           mode: crate::config::AudioMode, frame_samples: usize) -> Result<Self> {
        // Application mode. Voice → VOIP, Audio → AUDIO — with the sub-20ms
        // override (CASCADE_AUDIO_SEND_SPEC §6): for the three faster frame sizes the
        // encoder is forced into OPUS_APPLICATION_AUDIO regardless of the requested
        // mode; only at 20ms does the requested mode apply unmodified.
        //
        // Complexity: not set — the libopus default (9) applies.
        let is_voice = mode == crate::config::AudioMode::Voice;
        let app = if frame_samples < FRAME_MAX {
            opus::Application::Audio
        } else if is_voice {
            opus::Application::Voip
        } else {
            opus::Application::Audio
        };
        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, app)
            .map_err(|e| anyhow!("Opus encoder: {}", e))?;
        // Bandpass pinned to FULLBAND on every encoder, unconditionally — not left to
        // OPUS_AUTO. The two only agree at the bitrates where auto would pick fullband
        // anyway; below that auto narrows the band and spends the bits on a cleaner,
        // darker signal, while pinning keeps the full spectrum and accepts the artefacts.
        // Same setting, audibly different output, so this is not a free choice.
        enc.set_bandwidth(opus::Bandwidth::Fullband)
           .map_err(|e| anyhow!("set_bandwidth: {}", e))?;
        // VBR on, CONSTRAINED — the libopus default for both, set explicitly so the
        // intent is visible. Constrained VBR bounds the rate over a sliding window and
        // holds the average tightly to the target; unconstrained VBR lets peaks run
        // freely and overshoots the configured bitrate by roughly 15%.
        enc.set_vbr(true)
           .map_err(|e| anyhow!("set_vbr: {}", e))?;
        enc.set_vbr_constraint(true)
           .map_err(|e| anyhow!("set_vbr_constraint: {}", e))?;
        // DRED state: the vendored libopus 1.6.1 includes DRED (decoder-side neural tools
        // _celt_decode_with_ec_dred, _opus_decoder_dred_decode, _silk_LoadOSCEModels).
        //
        // DRED ENCODING is opt-in: requires OPUS_SET_DRED_DURATION_REQUEST(>0).
        // We never call this, so DRED encoding is off by default. The opus Rust crate
        // does not expose this CTL yet (audiopus_sys TODO), and the confirmed symbols
        // are all decoder-side, so no encoder-side neural features are active.
        //
        // The visible neural symbols (OSCE, DRED decode) affect the RECEIVE path only —
        // they are decoder post-processing features, not encoder rate-control.
        if bitrate_kbps > 0 {
            enc.set_bitrate(opus::Bitrate::Bits((bitrate_kbps * 1000) as i32))
               .map_err(|e| anyhow!("set_bitrate: {}", e))?;
        }
        // FEC gate (CASCADE_AUDIO_SEND_SPEC §6.1): BOTH conditions must hold — the
        // 20ms frame size specifically AND OPUS_APPLICATION_VOIP specifically.
        // packet_loss_perc is the hardcoded literal 1. Bitrate has no bearing.
        // (At sub-20ms the app override above forces AUDIO, so VOIP can only occur
        // at 20ms — the frame check states the spec's condition explicitly anyway.)
        if frame_samples == FRAME_MAX && app == opus::Application::Voip {
            enc.set_inband_fec(true).map_err(|e| anyhow!("set_fec: {}", e))?;
            enc.set_packet_loss_perc(1).map_err(|e| anyhow!("set_loss: {}", e))?;
        }
        let pkt_buf = vec![0u8; crate::net::protocol::HEADER_LEN + 1276];
        Ok(Self {
            encoder: enc, seq: 0,
            frame_samples,
            pkt_buf,
            pending: None,
        })
    }

    /// Encode one mono frame, stamping the caller-supplied SHARED capture timestamp
    /// `shared_ts`.
    ///
    /// The timestamp is deliberately not the encoder's own. Channels created at different
    /// times, or that independently skip a frame, would otherwise carry DIFFERENT timestamp
    /// origins, which the receive-side phase-lock reads as a cross-channel deviation as large
    /// as the difference between them — pinning its resampler ladder at the limit, so the
    /// channels never lock. The capture callback advances one sample clock per frame and
    /// passes it here, so every channel of a frame carries the same timestamp and a late
    /// joiner is born aligned.
    ///
    /// Returns (seq, ts, opus_byte_count). `seq` stays per-encoder: it is the packet
    /// sequence number, independent per stream; only the media timestamp is shared.
    pub fn encode(&mut self, interleaved_hw: &[f32], shared_ts: u32) -> Result<(u16, u32, usize)> {
        // PCM is already extracted and mono-sequential by the caller.
        let pcm: &[f32] = interleaved_hw;

        // Encode directly into pkt_buf after the header slot.
        // No heap allocation — pkt_buf pre-allocated at ChannelEncoder::new().
        let opus_out = &mut self.pkt_buf[crate::net::protocol::HEADER_LEN..];
        let n = self.encoder.encode_float(&pcm, opus_out)
            .map_err(|e| anyhow!("encode: {}", e))?;
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        // Media timestamp is the SHARED capture clock — identical across all
        // channels in this frame.
        Ok((seq, shared_ts, n))
    }
}

/// Time one second of Opus encode and decode for one channel, and return it as text.
///
/// Reached by `cascade --selftest`, so it runs from a copied binary with no toolchain.
///
/// The encoder is built by `ChannelEncoder::new`, the same call the audio path uses, so the
/// configuration measured is the configuration that ships: application AUDIO, bandwidth
/// FULLBAND, VBR constrained, complexity at the libopus default, bitrate unset (auto).
///
/// Figures are the fraction of one CPU core one channel consumes at 48 kHz. Multiply by the
/// channel count for the process total. That unit is the same on every platform, unlike
/// Task Manager (a share of all cores) or Activity Monitor (a share of one).
pub fn self_test() -> String {
    use std::fmt::Write as _;
    let mut r = String::new();
    let _ = writeln!(r, "opus — cost of one channel at 48 kHz, on this machine");
    let _ = writeln!(r, "(run with nothing else busy; each line is one second of audio)\n");

    // A mildly noisy tone rather than a pure sine: silence and pure tones both encode far
    // cheaper than real programme material and would flatter the result.
    let make_pcm = |n: usize, seed: &mut u32| -> Vec<f32> {
        (0..n).map(|i| {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (*seed >> 8) as f32 / 8_388_608.0 - 1.0;
            (i as f32 * 0.03).sin() * 0.4 + noise * 0.02
        }).collect()
    };

    for (label, frame) in [("20 ms frames", FRAME_MAX), ("2.5 ms frames", 120usize)] {
        let frames_per_sec = SAMPLE_RATE as usize / frame;
        let mut enc = match ChannelEncoder::new(0, crate::config::AudioMode::default(), frame) {
            Ok(e) => e,
            Err(e) => { let _ = writeln!(r, "  {label}: encoder unavailable ({e})"); continue; }
        };
        let mut seed = 0x1234_5678u32;
        let pcm = make_pcm(frame, &mut seed);

        // Encode, keeping the last packet so the decode pass has something real to work on.
        let mut packets: Vec<Vec<u8>> = Vec::with_capacity(frames_per_sec);
        let t0 = std::time::Instant::now();
        for i in 0..frames_per_sec {
            if let Ok((_, _, n)) = enc.encode(&pcm, i as u32) {
                packets.push(enc.pkt_buf[crate::net::protocol::HEADER_LEN..][..n].to_vec());
            }
        }
        let enc_s = t0.elapsed().as_secs_f64();

        let mut dec = match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono) {
            Ok(d) => d,
            Err(e) => { let _ = writeln!(r, "  {label}: decoder unavailable ({e})"); continue; }
        };
        let mut out = vec![0.0f32; frame];
        let t1 = std::time::Instant::now();
        for pkt in &packets { let _ = dec.decode_float(pkt, &mut out, false); }
        let dec_s = t1.elapsed().as_secs_f64();

        let bytes: usize = packets.iter().map(|p| p.len()).sum();
        let kbps = bytes as f64 * 8.0 / 1000.0;
        let _ = writeln!(r, "  {label:14} encode {:>6.2} ms = {:>6.3}%/ch ({:>5.2}% for 16ch)",
                         enc_s * 1000.0, enc_s * 100.0, enc_s * 1600.0);
        let _ = writeln!(r, "  {:14} decode {:>6.2} ms = {:>6.3}%/ch ({:>5.2}% for 16ch)",
                         "", dec_s * 1000.0, dec_s * 100.0, dec_s * 1600.0);
        let _ = writeln!(r, "  {:14} {:.0} kbit/s produced, {} packets\n",
                         "", kbps, packets.len());
    }
    r
}

/// Put the CURRENT thread into the audio-adjacent priority class: `USER_INITIATED` (0x19)
/// on macOS, `nice -10` on Linux.
///
/// Windows takes the MMCSS "Audio" class, the platform equivalent of both.
///
/// Applies to the threads that carry audio work but are not the device callback: the
/// select loop and e.receive, the cascade-recv socket-drain thread, the tokio workers, and
/// the scheduler's encode/decode workers. At the OS default priority these are descheduled
/// mid-receive under system load for milliseconds at a time.
///
/// USER_INITIATED is deliberately the ceiling here: no real-time or FIFO scheduling, and
/// no entitlements required.
///
/// Decode/encode workers and the receive thread take the SAME class, making them peers.
/// Raising decode above receive would let it preempt the receive loop mid-packet, which
/// costs more than the decode gains.
/// The only thread that takes a real-time class is the device callback, via
/// `scheduler::pool::elevate_audio_callback_thread` on Linux; on macOS CoreAudio applies a
/// time-constraint policy to that thread itself, and on Windows the WASAPI backend
/// registers its I/O thread with MMCSS.
///
/// CASCADE_AUDIO_SEND_SPEC §2 requires `USER_INITIATED` "or the platform equivalent …
/// at a comparable priority" for the encode/transmit stage; `nice -10` is that
/// equivalent, and SCHED_FIFO is not — it is a different scheduling class entirely.
pub fn set_qos_user_initiated() {
    #[cfg(target_os = "linux")]
    {
        unsafe {
            // Linux: `PRIO_PROCESS` with `who == 0` addresses the CALLING THREAD, not the
            // process. Threads are scheduling entities here and carry their own nice
            // value, which is what makes this the per-thread analogue of a QoS class.
            //
            // setpriority returns -1 both on failure and on a legitimate -1 result, so
            // errno has to be cleared first to tell them apart.
            *libc::__errno_location() = 0;
            let rc = libc::setpriority(libc::PRIO_PROCESS, 0, -10);
            let failed = rc == -1 && *libc::__errno_location() != 0;
            let tname = std::thread::current().name().unwrap_or("<main>").to_string();
            if failed {
                // WARN, not debug, and once. A refusal does not fail the process — the
                // thread simply runs at normal priority — so the only symptom is receive
                // and decode slipping under load, which the servo reads as ordinary
                // load-induced drift. That is the hardest class of fault to attribute.
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        "nice(-10) refused for audio threads ('{}') — they run at normal \
                         priority and will slip under load. Grant CAP_SYS_NICE (systemd: \
                         AmbientCapabilities=CAP_SYS_NICE).", tname);
                }
            } else {
                tracing::debug!("thread '{}' nice → -10", tname);
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        // MMCSS "Audio" — the Windows analogue of QOS_CLASS_USER_INITIATED and nice -10.
        // NOT "Pro Audio": that is reserved for the WASAPI I/O thread, which has the hard
        // deadline (see audio/backend/wasapi_backend.rs). These are the receive, decode and
        // encode threads, which must be above normal but must not outrank the device
        // thread they feed.
        //
        // Unlike SCHED_FIFO this needs no privilege, so a refusal is genuinely unexpected
        // rather than the ordinary unprivileged-run case — hence WARN, once.
        use std::cell::RefCell;
        use windows::core::w;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Threading::{
            AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
        };

        /// Holds the registration for the life of the thread and reverts it on exit, so a
        /// rebuilt worker does not leak an MMCSS task association.
        struct Mmcss(HANDLE);
        impl Drop for Mmcss {
            fn drop(&mut self) {
                let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
            }
        }
        thread_local! {
            static REG: RefCell<Option<Mmcss>> = const { RefCell::new(None) };
        }

        REG.with(|r| {
            // Once per thread: a second registration would leak the first handle.
            if r.borrow().is_some() { return; }
            let tname = std::thread::current().name().unwrap_or("<main>").to_string();
            let mut task_index: u32 = 0;
            match unsafe { AvSetMmThreadCharacteristicsW(w!("Audio"), &mut task_index) } {
                Ok(h) => {
                    *r.borrow_mut() = Some(Mmcss(h));
                    tracing::debug!("thread '{}' MMCSS → Audio", tname);
                }
                Err(e) => {
                    static WARNED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        tracing::warn!(
                            "MMCSS refused for audio threads ('{}': {e}) — they run at normal \
                             priority and will slip under load. The only symptom is receive \
                             and decode slipping, which the servo reads as ordinary drift.",
                            tname);
                    }
                }
            }
        });
    }
    #[cfg(target_os = "macos")]
    {
        const QOS_CLASS_USER_INITIATED: u32 = 0x19;
        extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
            fn pthread_get_qos_class_np(
                thread: libc::pthread_t, qos_class: *mut u32, rel_pri: *mut i32) -> i32;
        }
        unsafe {
            let rc = pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0);
            let tname = std::thread::current().name().unwrap_or("<main>").to_string();
            // Read back the ACTUAL effective QoS so we confirm it took, not just that the
            // set call returned 0.
            let mut got: u32 = 0;
            let mut rp: i32 = 0;
            pthread_get_qos_class_np(libc::pthread_self(), &mut got, &mut rp);
            if rc == 0 {
                tracing::debug!("thread '{}' QoS set; readback=0x{:x} (want 0x19)", tname, got);
            } else {
                tracing::warn!("pthread_set_qos_class_self_np('{}') failed rc={rc} readback=0x{:x}",
                               tname, got);
            }
        }
    }
}

/// Current thread's CONSUMED CPU time in nanoseconds (CLOCK_THREAD_CPUTIME_ID).
/// Pair with a wall-clock Instant around a span: if wall elapsed >> the thread-CPU
/// delta, the thread was DESCHEDULED (not running) for the difference — i.e. scheduling
/// latency, not slow work. If wall ≈ cpu, the thread was actually on-CPU the whole time
/// (real work or a spin). This is the definitive descheduling discriminator.
pub fn thread_cpu_nanos() -> u64 {
    // POSIX, not Darwin-only: CLOCK_THREAD_CPUTIME_ID is in the standard and Linux has it.
    // Gating this on macOS left the descheduling discriminator returning 0 on Linux, where
    // a descheduled receive thread then looked identical to a quiet one — which is exactly
    // the case it exists to tell apart.
    #[cfg(unix)]
    unsafe {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        if libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) == 0 {
            return ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64;
        }
        0
    }
    // Windows: GetThreadTimes reports kernel and user time separately, both as FILETIME —
    // 100 ns units. Their sum is this thread's consumed CPU time; the creation and exit
    // times are wall-clock and are not what is wanted here.
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if GetThreadTimes(GetCurrentThread(), &mut created, &mut exited,
                          &mut kernel, &mut user).is_ok() {
            let to_ns = |f: FILETIME| -> u64 {
                (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) * 100
            };
            return to_ns(kernel) + to_ns(user);
        }
        0
    }
    #[cfg(not(any(unix, windows)))]
    {
        0
    }
}

// ── Public engine ──────────────────────────────────────────────────────────

pub struct CaptureEngine {
    /// Input device stream. `Option` so a live rebuild can drop the old stream
    /// (releasing the device) before building the replacement. `None` only
    /// transiently inside `rebuild_input`.
    _stream:      Option<Stream>,
    /// Re-invokable builder for the input stream (macOS). Captures clones of the
    /// persistent shared state; called by `rebuild_input` to recreate the device
    /// stream with a new frame size (buffer = min(frame,480)) and/or a new input
    /// device, while continuing the shared capture clock. The device is a per-call
    /// parameter (not captured) so a live device switch can rebuild on a new device.
    /// `None` on platforms where live rebuild isn't wired.
    input_builder: Option<Box<dyn Fn(&Device, usize) -> anyhow::Result<Stream>>>,
    pub num_out_ch: usize,
    /// num_out_ch + TONE_LEGS — the length of every per-source array (accumulators,
    /// channel plan, streams). Hardware channels occupy
    /// `0..num_out_ch`; the tone legs occupy the two above.
    pub total_ch: usize,
    /// Raw SIGNAL routes per peer, exactly as the TX matrix sent them. Kept apart from
    /// the composed `send_routing` because the UI delivers signal and tone through
    /// separate messages: each must be able to change without erasing the other. The
    /// composition (signal first, then tone, so tone wins a contested slot) happens in
    /// rebuild_send_routing.
    signal_routes: Arc<RwLock<HashMap<String, Vec<crate::audio::routing::RouteEntry>>>>,
    /// Live hardware input-channel count, updated by the stream builder on every build
    /// (so it reflects the current device after a hot switch). The API reads this to size
    /// the routing UI's source channels (seeded with the startup device's count).
    pub live_in_ch: Arc<std::sync::atomic::AtomicUsize>,
    /// Whether the input device honours requested periods — see `audio::PeriodTracker`.
    pub in_period: Arc<crate::audio::PeriodTracker>,
    /// The input device currently in use. Held so a live device switch
    /// (`rebuild_input` with a new device) can rebuild the stream on it; a frame-size
    /// rebuild reuses the current device. A `Device` is a cheap handle to clone.
    current_input_device: Device,
    /// Ownership anchor for the scheduler. Never read: its only job is to keep the
    /// per-channel queues (GCD serial queues on macOS, serial queues over the shared
    /// worker pool on Linux) alive for the
    /// engine's lifetime. Clones live in the capture callbacks, but this is the owner
    /// that does not depend on a stream existing. Underscore-prefixed so the compiler
    /// does not flag it, and so nobody "cleans it up" and silently kills the queues.
    _scheduler: Arc<dyn super::scheduler::Scheduler>,
    pub send_routing: Arc<RwLock<HashMap<String, crate::audio::routing::PeerSendRouting>>>,
    peer_addrs_ref: Arc<RwLock<HashMap<String, SocketAddr>>>,
    /// Each source channel's send streams, with their destinations — rebuilt off the audio
    /// thread whenever routing, preferences, addresses or status change; the capture
    /// callback loads it lock-free into each job.
    cached_per_ch: Arc<arc_swap::ArcSwap<Vec<Vec<SendStream>>>>,
    pub tx_atomics: Arc<RwLock<HashMap<String, Arc<std::sync::atomic::AtomicU64>>>>,
    /// Per-peer numeric connection status (CASCADE_SESSION_STATS_SPEC §1): 0 = down,
    /// 1 = connected with address mismatch, 2 = connected clean. Written by the stats
    /// pump; read per DESTINATION at send time for §3.1's connection gate. The Arc is
    /// stable across rebuilds while the value stays live, so the gate is re-evaluated
    /// fresh every cycle rather than being a stateful enable/disable transition.
    pub peer_status: Arc<RwLock<HashMap<String, Arc<std::sync::atomic::AtomicU8>>>>,
    /// Per-remote link indices, stamped onto each destination's audio header.
    links: crate::net::LinkMap,
    /// The live encoders, one per send stream (CASCADE_AUDIO_SEND_SPEC §5): keyed by
    /// (source channel, frame size, mode). A stream exists exactly while at least one
    /// remote sends that channel at that frame size and mode; remotes that agree on both
    /// share it. When the last one stops, the encoder is dropped, and a stream needed again
    /// later starts from a fresh encoder — new Opus state, sequence from 0.
    enc_live: Arc<Mutex<HashMap<StreamKey, Arc<Mutex<ChannelEncoder>>>>>,
    /// Per-input-channel peak levels (bit-cast f32). Updated during deinterleave.
    pub input_peaks: Arc<Vec<std::sync::atomic::AtomicU32>>,
    /// Line-up tone peak levels (bit-cast f32): [0]=L leg, [1]=R leg. Tapped where
    /// the tone is generated for send; 0 when no tone is currently routed.
    pub tone_peaks: Arc<Vec<std::sync::atomic::AtomicU32>>,
    /// Encoder construction parameters, retained so routing reconciliation can build
    /// a ChannelEncoder on demand. Interior-mutable so hot bitrate/mode/frame changes
    /// update the template used for future lazily-created encoders.
    enc_params: Arc<RwLock<EncoderParams>>,
    /// Per-remote encode preferences: (frame_samples, mode). Fed from config
    /// (remote.frame_ms / remote.mode) via set_peer_enc_prefs; each remote is sent every
    /// routed channel at exactly its own pair (CASCADE_AUDIO_SEND_SPEC §4.1).
    peer_enc_prefs: Arc<RwLock<HashMap<String, (usize, crate::config::AudioMode)>>>,
    /// Resolved channel plan read by the capture callback (see ChannelPlan).
    /// Rebuilt by reconcile_encoders, never on the audio thread.
    channel_plan: Arc<arc_swap::ArcSwap<ChannelPlan>>,
    pub tone_dests: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    /// Number of active encoders (routed source channels + tone streams), maintained
    /// by reconcile_encoders. Read by the API for the monitor page.
    pub active_streams: Arc<std::sync::atomic::AtomicUsize>,
    /// Current outgoing Opus frame size (samples), read live by the capture callback.
    /// rebuild_input_prepare() updates this so a frame-size change takes effect immediately and
    /// the buffer-resize rebuild cannot cause frame-size flapping (see start()).
    frame_samples_shared: Arc<std::sync::atomic::AtomicUsize>,
    /// Confirmed sample rate the input stream is running at (Hz). Written by the stream
    /// builder after a successful build; 0 before the first build completes.
    pub live_in_rate: Arc<std::sync::atomic::AtomicU32>,
    /// Actual device name the input stream is running on. Written by the stream
    /// builder; may differ from config if startup fell back to the default device.
    pub live_in_device: Arc<std::sync::Mutex<String>>,
    /// Set by the device-fault callback when the input device is lost (unplugged).
    pub input_device_lost: Arc<std::sync::atomic::AtomicBool>,
    /// See AudioEngine::output_stream_invalid — same signal for the capture stream.
    pub input_stream_invalid: Arc<std::sync::atomic::AtomicBool>,
    /// See AudioEngine::output_rebuilding — same echo suppression for the capture stream.
    pub input_rebuilding: Arc<std::sync::atomic::AtomicBool>,
}

/// Default encoder parameters — used for any destination without its own
/// per-remote preference, and as the template for hot global changes.
#[derive(Clone, Copy)]
struct EncoderParams {
    bitrate_kbps:  u32,
    mode:          crate::config::AudioMode,
    frame_samples: usize,
}

// ── Frame-size buckets and per-bucket wire timestamps ──────────────────────────
//
// Four frame-size buckets (2.5/5/10/20ms = 120/240/480/960 samples), each with ONE
// global timestamp counter shared across every stream currently using that bucket
// (CASCADE_WIRE_PROTOCOL_SPEC §2.2). The counters live for the process lifetime,
// initialised to zero exactly once at startup — never reset on connect, reconnect,
// or engine rebuild. Increment by exactly 1 per bucket readiness cycle; multiply by
// the frame size only at the point the value is written onto the wire. The raw
// (pre-multiplication) counter wraps at ⌊2³²/960⌋ − 1 = 4,473,923 — this specific
// threshold, shared across all four buckets (sized for the worst case).
pub const TS_COUNTER_WRAP: u32 = 4_473_923;

static BUCKET_TS: [std::sync::atomic::AtomicU32; 4] = [
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
];

fn bucket_index(frame_samples: usize) -> usize {
    match frame_samples { 120 => 0, 240 => 1, 480 => 2, _ => 3 }
}

/// Advance a bucket's counter by one cycle and return the WIRE timestamp
/// (counter × frame size). Every stream in the bucket draining this cycle stamps
/// the identical value — the shared-timeline property the receive-side group
/// sync depends on.
fn bucket_next_wire_ts(frame_samples: usize) -> u32 {
    let b = bucket_index(frame_samples);
    let mut next = 0u32;
    let _ = BUCKET_TS[b].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
        next = if c >= TS_COUNTER_WRAP { 0 } else { c + 1 };
        Some(next)
    });
    next.wrapping_mul(frame_samples as u32)
}

// ── Channel plan (read by the capture callback each cycle) ─────────────────────
//
// The frame sizes each source channel is buffered at, rebuilt off the audio thread by
// reconcile_encoders whenever routing or per-remote preferences change. The
// capture callback clones the current Arc each cycle (same pattern as
// cached_per_ch) and uses it for buffering/readiness only — no encoding.
/// The line-up tone occupies two PSEUDO source channels immediately above the
/// hardware ones: `num_out_ch` = L (GLITS-interrupted), `num_out_ch + 1` = R
/// (steady). They are ordinary sources in every respect — same streams, same routing
/// table, same per-remote frame size and mode (§4.1) — differing only in that their PCM
/// comes from the generator rather than the input device. That is what makes a
/// slot's encoder identity depend solely on which source is routed there, so a
/// tone→signal handover cannot swap Opus state underneath the receiver.
pub const TONE_LEGS: usize = 2;

/// Source-channel index of a tone leg. `leg` is 1 = L, 2 = R — the values the TX
/// matrix already sends in `tone_routing.slots`.
#[inline]
pub fn tone_channel(num_out_ch: usize, leg: u8) -> usize { num_out_ch + (leg as usize - 1) }

/// Keep one source channel's accumulator on its bucket's frame grid (CASCADE_AUDIO_SEND_SPEC
/// §3): the invariant is `acc.len() ≡ total_captured (mod frame)`, so that every channel in a
/// bucket completes a frame on the same callback and "same timestamp" means "same samples".
///
/// Whenever the frame size differs from the one the accumulator was primed at — including
/// the unrouted → routed edge (`prev_frame == 0`) — the accumulator restarts primed with
/// `total_captured % frame` samples of silence. Returns whether it re-primed. `frame == 0`
/// (unrouted) only records the state.
///
/// Priming on the routed edge alone was not enough. A smaller grid divides a larger one but
/// not the other way round, so after a 10 → 20 ms change the accumulator still sat on the
/// 10 ms grid, and a channel routed afterwards — primed onto the 20 ms grid — could land a
/// whole 10 ms off its siblings while carrying the same timestamps.
#[inline]
fn keep_on_frame_grid(acc: &mut Vec<f32>, prev_frame: &mut usize, frame: usize,
                      total_captured: u64) -> bool {
    let changed = frame != 0 && frame != *prev_frame;
    if changed {
        acc.clear();
        acc.resize((total_captured % frame as u64) as usize, 0.0);
    }
    *prev_frame = frame;
    changed
}

#[derive(Default)]
pub struct ChannelPlan {
    /// Per source channel, the frame sizes it is buffered at: bit `b` set = bucket `b`
    /// (`bucket_index`) is in use by at least one send stream. 0 = not routed.
    pub per_ch_buckets: Vec<u8>,
}

/// Frame size in samples of each bucket, indexed by `bucket_index`.
const BUCKET_FRAMES: [usize; 4] = [120, 240, 480, 960];

/// A send stream's identity (CASCADE_AUDIO_SEND_SPEC §5): source channel, frame size in
/// samples, encoder mode. Every remote routing a source channel is served by the stream for
/// its OWN frame size and mode; remotes that agree on both share one.
type StreamKey = (usize, usize, crate::config::AudioMode);

/// One live send stream: an encoder and every (remote, slot) destination it feeds.
#[derive(Clone)]
pub struct SendStream {
    pub frame:   usize,
    pub mode:    crate::config::AudioMode,
    pub encoder: Arc<Mutex<ChannelEncoder>>,
    pub dests:   Vec<SendDest>,
}

// ── The off-real-time-thread send job ──────────────────────────────────────────
//
// The capture callback's ONLY downstream action is packaging ready frames into a
// SendJob and dispatch_async-ing it to a dedicated serial handoff queue
// (CASCADE_AUDIO_SEND_SPEC §2: capture/buffering/metering on the real-time thread;
// one async hop; encode and transmit entirely off it). The handoff queue is serial,
// so jobs — and therefore packets per stream — stay in capture order.
/// One send destination for a source channel: a (remote, wire slot) pair.
#[derive(Clone)]
pub struct SendDest {
    pub addr:   SocketAddr,
    /// Channel number written at 0x12 — the remote's slot this source is routed to.
    pub slot:   u8,
    /// The remote's TX byte counter.
    pub tx:     Arc<std::sync::atomic::AtomicU64>,
    pub peer:   String,
    /// The remote's connection status (§3.1 gate): 0 = down.
    pub status: Arc<std::sync::atomic::AtomicU8>,
    /// The remote's link indices, written at 0x0B/0x11.
    pub link:   Arc<crate::net::LinkIndices>,
}

pub struct SendJob {
    /// Ready frames, signal and tone legs alike: (source channel, frame size in samples,
    /// mono PCM, wire timestamp). A channel buffered at several frame sizes contributes one
    /// entry per size that completed.
    pub entries: Vec<(usize, usize, Vec<f32>, u32)>,
    /// Snapshot of each source channel's send streams.
    pub streams: Arc<Vec<Vec<SendStream>>>,
    /// Where each frame's buffer goes when the job is done — see `FramePool`.
    pool: FramePool,
}

impl Drop for SendJob {
    /// Return every frame buffer to the pool. In Drop rather than at the end of
    /// `encode_and_send_job`, so a job that returns early — shutdown, a dropped job on a
    /// disconnected instance — recycles its buffers exactly like one that sent.
    fn drop(&mut self) {
        for (_, _, pcm, _) in self.entries.drain(..) {
            self.pool.give(pcm);
        }
    }
}

/// Frame buffers recycled between the capture callback and the encode workers.
///
/// A ready frame has to outlive the callback that produced it: it is handed to a worker that
/// encodes and sends it. Allocating one per frame puts an allocation per channel per frame on
/// the device thread — 6,500 a second at 64 channels and 10 ms frames — and an allocator
/// stall there lands on the capture deadline. The buffers are taken from here instead and
/// returned when the job is dropped, so the steady state allocates nothing.
///
/// A bounded queue, not a lock: the callback takes with a non-blocking receive and a worker
/// returns with a non-blocking send. An empty pool allocates a buffer rather than blocking or
/// dropping audio, and a full pool drops the returned buffer — so a miscount costs the old
/// behaviour and never a glitch.
#[derive(Clone)]
pub struct FramePool {
    free: crossbeam_channel::Receiver<Vec<f32>>,
    ret:  crossbeam_channel::Sender<Vec<f32>>,
}

impl FramePool {
    /// `buffers` pre-allocated at the largest frame size, so no frame size has to grow one.
    fn new(buffers: usize) -> Self {
        let (ret, free) = crossbeam_channel::bounded(buffers);
        for _ in 0..buffers {
            let _ = ret.try_send(Vec::with_capacity(FRAME_MAX));
        }
        FramePool { free, ret }
    }

    /// An empty buffer for one frame. Allocates only when the pool is empty.
    fn take(&self) -> Vec<f32> {
        match self.free.try_recv() {
            Ok(mut b) => { b.clear(); b }
            Err(_) => Vec::with_capacity(FRAME_MAX),
        }
    }

    /// Hand a buffer back. Dropped if the pool is full, which only happens if more were
    /// allocated than it holds.
    fn give(&self, mut b: Vec<f32>) {
        b.clear();
        let _ = self.ret.try_send(b);
    }
}

/// Frames a channel can have in flight at once: the job being encoded and sent, plus the one
/// the next callback fills. The pool is this many per channel.
const FRAMES_IN_FLIGHT: usize = 2;

/// dispatch_apply context for the parallel encode over a job's entries.
/// Lives on the handoff worker's stack for the (synchronous) dispatch_apply_f.
struct JobApplyCtx<'a> {
    /// One encode per (entry, stream): the entry's index and the stream's encoder.
    tasks:   &'a [(usize, Arc<Mutex<ChannelEncoder>>)],
    entries: &'a [(usize, usize, Vec<f32>, u32)],
}

unsafe extern "C" fn job_apply_work(ctx_raw: *mut std::ffi::c_void, idx: usize) {
    // GCD C frame — a Rust panic must not unwind across it; catch_unwind contains it
    // and .get() degrades a length desync to a skipped entry rather than UB.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = &*(ctx_raw as *const JobApplyCtx);
        let (ei, enc) = match ctx.tasks.get(idx) { Some(t) => t, None => return };
        let (ch, _f, pcm, ts) = match ctx.entries.get(*ei) { Some(e) => e, None => return };
        let mut e = match enc.lock() { Ok(g) => g, Err(_) => return };
        if pcm.len() != e.frame_samples { e.pending = None; return; }
        match e.encode(pcm, *ts) {
            Ok((seq, _ts, n)) => { e.pending = Some((seq, n)); }
            Err(e2) => { e.pending = None; tracing::debug!("enc ch{}: {e2}", ch); }
        }
    }));
}

#[allow(clippy::too_many_arguments)]
/// Transmit one already-built plaintext audio packet to one destination, applying
/// the per-remote encryption gate (CASCADE_ENCRYPTION_SPEC §5). Returns the wire
/// byte count for tx accounting, or 0 if the packet was dropped (encryption
/// requested but the handshake is not yet complete — never sent in the clear).
/// `pkt` is `[header(HEADER_LEN) | opus(n)]`; `scratch` is reused for the sealed
/// packet so no per-call allocation beyond the ciphertext itself.
fn transmit_audio(
    socket:  &std::net::UdpSocket,
    crypto:  Option<&Arc<crate::net::crypto::PeerCrypto>>,
    pkt:     &[u8],
    n:       usize,
    addr:    &SocketAddr,
    scratch: &mut Vec<u8>,
) -> u64 {
    use crate::net::crypto::SealResult;
    let hl = crate::net::protocol::HEADER_LEN;
    match crypto.map(|c| c.seal(&pkt[hl..hl + n])) {
        None | Some(SealResult::Plaintext) => {
            let _ = socket.send_to(&pkt[..hl + n], addr);
            (hl + n) as u64 + 28
        }
        Some(SealResult::Drop) => 0,   // handshake pending — drop, never plaintext
        Some(SealResult::Sealed(blob)) => {
            // Same header, encrypted flag set, length field = sealed payload length.
            scratch.clear();
            scratch.extend_from_slice(&pkt[..hl]);
            scratch.extend_from_slice(&blob);
            scratch[8] |= crate::net::protocol::FLAG_ENCRYPTED;   // 0x08 bit 7
            let blen = blob.len() as u16;                          // 0x13-0x14 LE
            scratch[0x13] = (blen & 0xff) as u8;
            scratch[0x14] = (blen >> 8) as u8;
            let _ = socket.send_to(&scratch[..hl + blob.len()], addr);
            (hl + blob.len()) as u64 + 28
        }
    }
}

/// Encode and transmit one job — runs on the serial handoff queue, never on the
/// real-time capture thread. Parallel encode (dispatch_apply across every (entry, stream)
/// pair), then a serial send pass in fixed order (deterministic emission); tone legs are
/// ordinary entries on their pseudo source channels.
fn encode_and_send_job(
    job:           SendJob,
    socket:        &std::net::UdpSocket,
    crypto:        &crate::net::CryptoMap,
    scheduler:     &dyn super::scheduler::Scheduler,
) {
    // QUIESCE ON SHUTDOWN. While audio streams these workers are inside `send_to`
    // continually, and `std::process::exit` terminates a thread exactly where it stands.
    // On Windows a thread killed inside a socket call orphans the endpoint: the port stays
    // registered to the dead process, cannot be re-bound by its successor — refused, not
    // merely busy — and does not clear for over a minute. Skipping the send once shutdown
    // is requested means no worker is inside a socket call when the process goes.
    //
    // The frames dropped here are the last few milliseconds before exit, which are not
    // going anywhere regardless.
    if crate::lifecycle::is_stopping() { return; }
    // Snapshot the per-remote crypto handles once for this job (cheap Arc clones),
    // so the send loops below take no crypto lock per destination.
    let crypto_snap: HashMap<String, Arc<crate::net::crypto::PeerCrypto>> =
        crypto.read().unwrap_or_else(|e| e.into_inner()).clone();
    let mut enc_scratch: Vec<u8> = Vec::new();
    // Each entry is encoded once by every stream on its channel at its frame size — one
    // per distinct mode among the remotes sending that channel at that size.
    let mut tasks: Vec<(usize, Arc<Mutex<ChannelEncoder>>)> = Vec::new();
    for (ei, (ch, f, _, _)) in job.entries.iter().enumerate() {
        if let Some(list) = job.streams.get(*ch) {
            for st in list.iter().filter(|st| st.frame == *f) {
                tasks.push((ei, Arc::clone(&st.encoder)));
            }
        }
    }
    // ── Parallel encode ──
    if !tasks.is_empty() {
        let ctx = JobApplyCtx { tasks: &tasks, entries: &job.entries };
        let ctx_ptr = std::ptr::addr_of!(ctx) as *mut std::ffi::c_void;
        scheduler.encode_apply(tasks.len(), ctx_ptr, job_apply_work);
        // dispatch_apply barrier: all encodes complete; no worker holds a lock.
    }

    // ── Serial send pass (deterministic emission, fixed channel order) ──
    for (ch, f, _pcm, ts) in &job.entries {
        let list = match job.streams.get(*ch) { Some(l) => l, None => continue };
        for st in list.iter().filter(|st| st.frame == *f) {
            let mut e = match st.encoder.lock() { Ok(g) => g, Err(_) => continue };
            let (enc_seq, n) = match e.pending.take() { Some(v) => v, None => continue };
            // One entry per (remote, slot) this stream feeds: same encoded payload, a
            // different channel number in the header per slot.
            for SendDest { addr, slot: remote_slot, tx: tx_atom, peer, status, link } in st.dests.iter() {
                // §3.1 connection gate — PER DESTINATION, re-read fresh every cycle. Only the
                // fully-down state (0) gates sending; the address-mismatch state (1) still
                // sends. Nothing is torn down: the encoder, routing and cached destination all
                // stay intact, so transmission resumes the instant the peer reconnects.
                if status.load(std::sync::atomic::Ordering::Relaxed) == 0 { continue; }
                // Sequence belongs to the ENCODER: one counter per stream, i.e. per
                // (channel, frame size, mode), advanced once per encode and stamped
                // identically onto every destination this frame fans out to.
                crate::net::protocol::build_audio_into(
                    &mut e.pkt_buf, enc_seq, *ts, *remote_slot, n);
                // This remote's link indices at 0x0B/0x11 (net::LinkIndices).
                link.stamp(&mut e.pkt_buf);
                let pc = crypto_snap.get(peer);
                let wire = transmit_audio(socket, pc, &e.pkt_buf, n, addr, &mut enc_scratch);
                if wire > 0 {
                    tx_atom.fetch_add(wire, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

}


impl CaptureEngine {
    /// Start the capture engine with parallel per-channel encoding.
    ///
    /// Spawns no threads of its own. The device callback thread is the backend's; the
    /// encode work runs on the scheduler's shared pool, which is created once for the
    /// process on first dispatch.
    pub fn start(
        device:            &Device,
        outgoing_channels: usize,
        tone_level_db:     f32,
        bitrate_kbps:      u32,
        mode:              &crate::config::AudioMode,
        frame_ms:          f32,
        socket:            crate::net::udp::SharedSocket,
        peer_addrs:        Arc<RwLock<HashMap<String, SocketAddr>>>,
        any_connected:     Arc<AtomicBool>,
        _tx_bytes:         Arc<AtomicU64>,  // replaced by per-peer tx_atomics
        event_tx:          tokio::sync::broadcast::Sender<String>,
        crypto_map:        crate::net::CryptoMap,
    ) -> Result<Self> {
        let _ = &event_tx; // reserved for future per-stream events; currently unused
        let frame_samples = crate::config::frame_ms_to_samples(frame_ms);
        // Asked for its shortest callback period first, while nothing holds it: it bounds the
        // callback request and the outgoing frame sizes (`audio::shortest_request`,
        // `audio::shortest_frame`). The answer is kept only once the stream is built — see
        // `audio::LearnedPeriod`.
        let learned = crate::audio::LearnedPeriod::learn(device, true);
        let cfg = crate::audio::backend::find_config(
            device, crate::audio::backend::Dir::Input,
            frame_samples.min(IO_BUF_CAP_FRAMES))?;
        // .max(1): a 0-channel input config would make the capture loop's
        // chunks_exact(num_hw_ch) panic (chunks_exact requires size > 0) and the
        // deinterleave's (num_hw_ch - 1) underflow. A backend should never report 0 input
        // channels for a real device, but the cost of the guard is one comparison.
        let num_hw_ch = (cfg.channels() as usize).max(1);

        // Never send more channels than the input device physically has. The old
        // behaviour duplicated the last hardware channel into the surplus slots —
        // a development-time hangover from testing with a single-channel input device. Cap it so the
        // wire only ever carries real input channels.
        // Ceiling only — do NOT cap to the boot device's channel count. The per-channel
        // arrays are sized to the caller's ceiling (OUTGOING_CHANNELS_MAX); the number of channels
        // actually deinterleaved and encoded each callback follows the LIVE device
        // (num_hw_ch), so a hot-swap to a device with more channels works. (The old
        // `.min(num_hw_ch)` froze the ceiling at the startup device — a single-channel input
        // development hangover — which clamped swap-up to 1 channel.)
        let outgoing_channels = outgoing_channels.max(1);
        // Every per-source array covers the hardware channels PLUS the two tone legs.
        let total_channels = outgoing_channels + TONE_LEGS;

        info!("Input '{}': {} ch, outgoing frame {} ms ({} samples)",
              crate::audio::device_name(device), num_hw_ch, frame_ms, frame_samples);

        // No encoder exists until a channel is routed: reconcile_encoders creates one per
        // send stream on demand (see `enc_live`).
        let enc_params = Arc::new(RwLock::new(EncoderParams {
            bitrate_kbps,
            mode: *mode,
            frame_samples,
        }));

        // ── Scheduler ─────────────────────────────────────────────────────
        // encode_apply (parallel per-stream encode) + ONE serial encode queue used as
        // the handoff queue — the single async hop that takes encode and transmit off
        // the real-time capture thread (CASCADE_AUDIO_SEND_SPEC §2). The encode workers
        // call sendto() inline; there is no separate send thread.
        let scheduler = make_scheduler(1, 0);
        let handoff_q = scheduler.encode_queue(0);
        // One pool for the life of the engine, sized for every channel to have its frames in
        // flight: the job being encoded and sent, and the one the next callback fills.
        let frame_pool = FramePool::new(total_channels * FRAMES_IN_FLIGHT);

        // ── Input stream ──────────────────────────────────────────────
        //
        // The callback dispatches straight onto the handoff queue on every platform.
        // The dispatch itself is non-blocking and costs ~50ns, so it is safe to make
        // from a real-time callback.
        //
        // What the callback thread is differs by platform, and only the priority setup
        // follows from that. macOS: coreaudiod's AUHAL I/O thread, with
        // THREAD_TIME_CONSTRAINT_POLICY already applied by the OS. Linux: an ordinary
        // backend thread, elevated to SCHED_FIFO on its first callback by
        // `scheduler::pool::elevate_audio_callback_thread`. Windows: the WASAPI I/O thread,
        // which registers itself with MMCSS "Pro Audio" for its whole lifetime.

        // Shared send routing — defined here so it's in scope for both the
        // platform-specific capture path and the CaptureEngine return value.
        let send_routing_shared: Arc<RwLock<HashMap<String,
            crate::audio::routing::PeerSendRouting>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let tx_atomics_shared: Arc<RwLock<HashMap<String,
            Arc<std::sync::atomic::AtomicU64>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        // Wire timestamps come from the per-bucket counters (BUCKET_TS, module level,
        // process-lifetime — CASCADE_WIRE_PROTOCOL_SPEC §2.2), not from a capture clock.
        //
        // The callback period requested for the input stream, in samples — the backend
        // buffer-size knob (buffer = min(knob, 480)), never shorter than
        // `audio::shortest_request` was when it was built. Written by rebuild_input; the
        // per-channel frame sizes the callback actually drains at come from the ChannelPlan.
        let input_request = frame_samples.max(crate::audio::shortest_request());
        let frame_samples_shared = Arc::new(std::sync::atomic::AtomicUsize::new(input_request));
        // Live hardware input-channel count, written by the stream builder on every build
        // (so it tracks the current device after a hot switch) and read by the API to drive
        // the routing UI's source-channel count. Seeded with the startup device's count.
        let live_in_ch_shared = Arc::new(std::sync::atomic::AtomicUsize::new(num_hw_ch));
        let in_period_shared = Arc::new(crate::audio::PeriodTracker::default());
        let input_peaks_shared: Arc<Vec<std::sync::atomic::AtomicU32>> = Arc::new(
            // Sized to the channel count, so metering covers every channel of any device.
            (0..outgoing_channels).map(|_| std::sync::atomic::AtomicU32::new(0)).collect());
        // Tone meter cells: [L, R].
        let tone_peaks_shared: Arc<Vec<std::sync::atomic::AtomicU32>> = Arc::new(
            (0..2).map(|_| std::sync::atomic::AtomicU32::new(0)).collect());
        let tone_dests_cell: Arc<std::sync::Mutex<Option<Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>>>> =
            Arc::new(std::sync::Mutex::new(None));

        // Shared per-channel address cache — before platform cfg blocks.
        let peer_status_shared: Arc<RwLock<HashMap<String, Arc<std::sync::atomic::AtomicU8>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let enc_live_shared: Arc<Mutex<HashMap<StreamKey, Arc<Mutex<ChannelEncoder>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let cached_per_ch_shared: Arc<arc_swap::ArcSwap<Vec<Vec<SendStream>>>> =
            Arc::new(arc_swap::ArcSwap::from_pointee(
                vec![vec![]; total_channels]));
        // Per-remote encode preferences (frame_samples, mode) — fed from config by
        // main via set_peer_enc_prefs; consumed by reconcile_encoders (§4.1).
        let peer_enc_prefs_shared: Arc<RwLock<HashMap<String, (usize, crate::config::AudioMode)>>> =
            Arc::new(RwLock::new(HashMap::new()));
        // Resolved channel plan, read by the capture callback each cycle. Starts
        // empty (nothing routed yet) — populated by the first reconcile.
        let channel_plan_shared: Arc<arc_swap::ArcSwap<ChannelPlan>> =
            Arc::new(arc_swap::ArcSwap::from_pointee(ChannelPlan {
                per_ch_buckets: vec![0; total_channels],
            }));
        // Confirmed input sample rate and device name — written by the stream builder.
        let live_in_rate_shared = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let live_in_device_shared = Arc::new(std::sync::Mutex::new(String::new()));
        let input_device_lost_shared = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let input_stream_invalid_shared = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let input_rebuilding_shared = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Capture clock, persistent across stream rebuilds (see its use in the builder).
        let total_captured_shared = Arc::new(std::sync::atomic::AtomicU64::new(0));

        // ONE capture pipeline for every platform. Everything it rests on is portable:
        // the backend's callback, the scheduler abstraction, and accumulators that tolerate
        // whatever buffer size the device delivers. Keep it that way — a second
        // platform-specific pipeline means every send-side fix has to be made twice, and
        // the routing table, §2.1 per-wire-channel sequencing, tone, encryption and
        // frame-grid phase priming all live here.
        let (stream, input_builder) = {
            // (Per-stream locals — acc, peak scratch, channel_arcs, frame_target,
            // input_buf_frames — are created inside the re-invokable builder below,
            // since they depend on frame_samples and must be fresh on each rebuild.)

            // ── create-once persistent infrastructure (NOT rebuilt per stream) ──
            // Address-cache watcher thread, encoder/tone Arcs, shared cells, and the
            // tone generator/dest state hold live state that must survive a stream
            // rebuild, so they are created exactly once here — OUTSIDE the builder.
            let cached_per_ch: Arc<arc_swap::ArcSwap<Vec<Vec<SendStream>>>> =
                Arc::clone(&cached_per_ch_shared);
            // The per-channel destination cache rebuild is event-driven rather than
            // polled: net::peer fires rebuild_tx whenever a peer's learned
            // address changes (connect / NAT rebind), and main's rebuild_rx arm calls
            // engine.rebuild_cached_per_ch(). No polling, no extra thread, fires only on
            // a real change.
            let tone_dests_cap = Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::<String,Vec<u8>>::new()));
            let tone_gen_cap  = Arc::new(std::sync::Mutex::new(
                crate::audio::ebu_tone::EbuToneGenerator::new(tone_level_db)));
            *tone_dests_cell.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&tone_dests_cap));

            // ── re-invokable per-stream builder ─────────────────────────────────
            // Builds ONLY the input stream + its callback. Called once now, and
            // again by rebuild_input_prepare() on a live frame-size change (new buffer =
            // min(frame,480)). Captures clones of the persistent state above; each
            // call re-clones them into a fresh callback and CONTINUES the shared
            // capture clock. Originals stay in start() for the struct return.
            // (device is a per-call parameter below, not captured — supports switch.)
            let any_b          = any_connected.clone();
            let cached_b       = Arc::clone(&cached_per_ch);
            let socket_b       = Arc::clone(&socket);
            let scheduler_b    = Arc::clone(&scheduler);
            let handoff_b      = Arc::clone(&handoff_q);
            // Shared across rebuilds: buffers in flight when a stream is rebuilt come back
            // to the same pool (see `FramePool`).
            let frame_pool_b   = frame_pool.clone();
            let plan_b         = Arc::clone(&channel_plan_shared);
            let peaks_b        = Arc::clone(&input_peaks_shared);
            let tone_peaks_b   = Arc::clone(&tone_peaks_shared);
            let tone_gen_bld   = Arc::clone(&tone_gen_cap);
            let crypto_b       = crate::net::CryptoMap::clone(&crypto_map);
            let live_in_ch_b   = Arc::clone(&live_in_ch_shared);
            let in_period_b = Arc::clone(&in_period_shared);
            let live_in_rate_b = Arc::clone(&live_in_rate_shared);
            let live_in_device_b = Arc::clone(&live_in_device_shared);
            let input_device_lost_b = Arc::clone(&input_device_lost_shared);
            let input_stream_invalid_b = Arc::clone(&input_stream_invalid_shared);
            let input_rebuilding_b = Arc::clone(&input_rebuilding_shared);
            let n_out_b        = outgoing_channels;
            let total_ch_b     = total_channels;

            let build_input_stream: Box<dyn Fn(&Device, usize) -> anyhow::Result<Stream>> =
                Box::new(move |device: &Device, frame_samples: usize| -> anyhow::Result<Stream> {
            let input_buf_frames = frame_samples.min(IO_BUF_CAP_FRAMES);
            let cfg = crate::audio::backend::find_config(
                device, crate::audio::backend::Dir::Input, input_buf_frames)?;
            // .max(1): see the build-time derivation above — guards the capture
            // loop's chunks_exact(num_hw_ch) and the deinterleave underflow against a
            // device that reports 0 input channels (e.g. after a hot-swap).
            let num_hw_ch = (cfg.channels() as usize).max(1);
            // Publish the current device's channel count, confirmed sample rate, and name.
            live_in_ch_b.store(num_hw_ch, std::sync::atomic::Ordering::Relaxed);
            // The rate we successfully OPENED at. Since 0.18 the build path sets the device's
            // physical format and our patch verifies the hardware clock actually reached it, so
            // a stream existing means this value is real — but it describes THIS stream only,
            // and is zeroed when the stream is invalidated (see main.rs).
            live_in_rate_b.store(cfg.sample_rate(), std::sync::atomic::Ordering::Relaxed);
            let device_name = crate::audio::device_name(device);
            if let Ok(mut g) = live_in_device_b.lock() { *g = device_name.clone(); }
            let err_device_name = device_name.clone();
            let err_lost_flag = Arc::clone(&input_device_lost_b);
            let err_invalid_flag = Arc::clone(&input_stream_invalid_b);
            let err_rebuilding_flag = Arc::clone(&input_rebuilding_b);
            let outgoing_channels = n_out_b;
            let total_channels    = total_ch_b;
            let input_buf_frames = frame_samples.min(IO_BUF_CAP_FRAMES);
            let mut ch_peaks_scratch: Vec<f32> = Vec::with_capacity(outgoing_channels);
            // Per-(channel, frame size) mono accumulators — the spec's per-channel buffering
            // (CASCADE_AUDIO_SEND_SPEC §3), one per bucket, because remotes sending the same
            // channel at different frame sizes each need their own frames of it. Each
            // accumulator buffers until its frame size is reached; readiness is tracked per
            // bucket. Streams that share a frame size share its accumulator: their frames
            // are identical, only the encoder differs.
            // The frame size each accumulator was last primed at, 0 while its bucket is not
            // in use. Coming into use is what primes the frame grid below.
            let mut prev_frame: Vec<[usize; 4]> = vec![[0; 4]; total_channels];
            let mut mono_acc: Vec<[Vec<f32>; 4]> = (0..total_channels)
                .map(|_| std::array::from_fn(|b| Vec::with_capacity(BUCKET_FRAMES[b] * 2)))
                .collect();
            // The capture clock, persistent across stream rebuilds — see the shared handle
            // created outside the builder. This is the clock the frame-grid phase priming is
            // computed against (`total_captured % frame_samples`), and it must describe one
            // continuous capture timeline. Declaring it here reset it to zero on every
            // rebuild, so a channel routed before a rebuild and one routed after computed
            // their grid phase against different origins.
            let total_captured_cell = Arc::clone(&total_captured_shared);
            let mut total_captured: u64 =
                total_captured_cell.load(std::sync::atomic::Ordering::Relaxed);
            // per-call clones consumed (moved) by the audio callback below
            let any_conn        = any_b.clone();
            let cached_cap      = Arc::clone(&cached_b);
            let socket_cap      = Arc::clone(&socket_b);
            let scheduler_cap   = Arc::clone(&scheduler_b);
            let handoff_cap     = Arc::clone(&handoff_b);
            let frame_pool_cap  = frame_pool_b.clone();
            let plan_cap        = Arc::clone(&plan_b);
            let peaks_cap       = Arc::clone(&peaks_b);
            let tone_peaks_cap  = Arc::clone(&tone_peaks_b);
            let tone_gen_cap    = Arc::clone(&tone_gen_bld);
            let crypto_cap      = crate::net::CryptoMap::clone(&crypto_b);

            // The processing callback is written ONCE, in f32. Integer formats are
            // converted into a preallocated scratch buffer and handed to this same
            // closure, so there is one capture code path on every platform and in every
            // format. On macOS F32 is always selected, so this is bit-identical to the
            // path that was here before.
            let f32_cb = move |data: &[f32]| {
                    // Real-time class for the device callback, once per callback thread.
                    // See the matching call on the output side.
                    #[cfg(target_os = "linux")]
                    crate::audio::scheduler::pool::elevate_audio_callback_thread();
                    // One-shot: log actual CoreAudio buffer size on first callback.
                    static LOGGED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    // Exception only — see the matching note on the output side.
                    if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        let actual = data.len() / num_hw_ch;
                        if actual != input_buf_frames {
                            warn!("Input device delivered {} frames ({:.2} ms), not the \
                                   requested {} — accumulators compensating",
                                  actual, actual as f32 / 48.0, input_buf_frames);
                        }
                    }
                    // This callback does ONLY capture, per-channel buffering,
                    // bucket-readiness checks, and peak metering. Encode and
                    // transmit happen off-thread via the handoff queue
                    // (CASCADE_AUDIO_SEND_SPEC §2).
                    // Every channel the live device delivers, up to the outgoing maximum.
                    let n_ch = outgoing_channels.min(num_hw_ch);
                    // Lock-free atomic load on the RT capture thread (CASCADE_AUDIO_SEND_SPEC
                    // §2.1) — reconcile_encoders stores a fresh Arc; the callback never blocks.
                    let plan = plan_cap.load_full();
                    let k = data.len() / num_hw_ch;   // samples per channel this callback

                    // ── Deinterleave into per-channel accumulators + peak metering ──
                    ch_peaks_scratch.clear();
                    ch_peaks_scratch.resize(n_ch, 0.0f32);
                    for ch in 0..n_ch {
                        let buckets = plan.per_ch_buckets.get(ch).copied().unwrap_or(0);
                        // ── Frame-grid phase priming ──────────────────────────────────
                        // A channel that starts accumulating mid-frame completes its
                        // frames offset from every other channel's by that amount — yet
                        // the drain loop stamps every channel ready in a round with the
                        // SAME bucket timestamp. The result is frames carrying identical
                        // timestamps whose CONTENT is offset by up to a full frame.
                        //
                        // The receiver aligns on those timestamps, so it sees perfect
                        // alignment (deviation 0), the §6.2 servo correctly does nothing,
                        // and the offset is permanent — heard as comb filtering across
                        // channels that should be sample-locked. Routing several channels
                        // in one action hides it, because they all start together.
                        //
                        // Priming with `total_captured % f` samples of silence lands the
                        // first frame boundary exactly on the shared grid, so a channel
                        // routed at any instant drains in the same rounds as its siblings.
                        //
                        // It runs per frame size, whenever a size comes into use on the
                        // channel — see `keep_on_frame_grid`. A frame size coming into use is
                        // a fresh stream at the receiver regardless (new encoder, new
                        // sequence), so starting its grid here costs nothing audible: it is
                        // the fresh encoder's initial fill (§5).
                        for b in 0..4 {
                            let f = if buckets & (1 << b) != 0 { BUCKET_FRAMES[b] } else { 0 };
                            keep_on_frame_grid(&mut mono_acc[ch][b], &mut prev_frame[ch][b], f,
                                               total_captured);
                        }
                        let mut peak = 0.0f32;
                        let mut i = ch;
                        while i < data.len() {
                            let a = data[i].abs();
                            if a > peak { peak = a; }
                            i += num_hw_ch;
                        }
                        ch_peaks_scratch[ch] = peak;
                        for b in 0..4 {
                            if buckets & (1 << b) == 0 { continue; }
                            let accv = &mut mono_acc[ch][b];
                            let mut i = ch;
                            while i < data.len() { accv.push(data[i]); i += num_hw_ch; }
                        }
                    }
                    // A routed channel above the live device width takes no samples while it
                    // is out of range, so its accumulators fall off the grid. Forgetting their
                    // frame sizes makes them re-prime when the device grows back to cover it.
                    for f in prev_frame.iter_mut().take(outgoing_channels).skip(n_ch) {
                        *f = [0; 4];
                    }
                    // Running MAX, drained by the meter snapshot task (accumulate-then-
                    // snapshot, CASCADE_AUDIO_SEND_SPEC §9) — bit-pattern max is valid for
                    // the non-negative peak values.
                    let pub_n = n_ch.min(peaks_cap.len());
                    for ch in 0..pub_n {
                        peaks_cap[ch].fetch_max(ch_peaks_scratch[ch].to_bits(), Ordering::Relaxed);
                    }

                    // ── Tone generation (every callback so meters read even when
                    // unrouted; the generator free-runs, one shared clock) ──
                    let (tl, tr) = tone_gen_cap.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .next_lr_frames(k);
                    let pl = tl.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                    let pr = tr.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                    tone_peaks_cap[0].fetch_max(pl.to_bits(), Ordering::Relaxed);
                    tone_peaks_cap[1].fetch_max(pr.to_bits(), Ordering::Relaxed);
                    // Feed the two pseudo source channels. Generation is unconditional
                    // (above) so the meters read whether or not anything is routed; only
                    // the accumulate is gated, exactly as it is for a hardware channel.
                    for (leg, pcm) in [(1u8, &tl), (2u8, &tr)] {
                        let tch     = tone_channel(outgoing_channels, leg);
                        let buckets = plan.per_ch_buckets.get(tch).copied().unwrap_or(0);
                        // Same frame-grid priming as the hardware channels above, on the same
                        // trigger: a frame size coming into use.
                        for b in 0..4 {
                            let f = if buckets & (1 << b) != 0 { BUCKET_FRAMES[b] } else { 0 };
                            keep_on_frame_grid(&mut mono_acc[tch][b], &mut prev_frame[tch][b], f,
                                               total_captured);
                            if f != 0 {
                                mono_acc[tch][b].extend_from_slice(pcm);
                            } else if !mono_acc[tch][b].is_empty() {
                                mono_acc[tch][b].clear();
                            }
                        }
                    }

                    total_captured = total_captured.wrapping_add(k as u64);
                    total_captured_cell.store(total_captured, std::sync::atomic::Ordering::Relaxed);

                    // ── Drain rounds: bucket readiness → one job per round ──
                    // Each round, every bucket that has at least one ready stream
                    // advances its counter ONCE; all of that bucket's ready streams
                    // stamp the identical wire timestamp (shared timeline).
                    loop {
                        let mut ts_val: [Option<u32>; 4] = [None; 4];
                        let mut entries: Vec<(usize, usize, Vec<f32>, u32)> = Vec::new();
                        // Hardware channels are bounded by the live device width; the tone
                        // legs sit above it and are always eligible. Frame sizes not in use
                        // on a channel have empty accumulators and are skipped here.
                        for ch in (0..n_ch).chain(outgoing_channels..total_channels) {
                            let buckets = plan.per_ch_buckets.get(ch).copied().unwrap_or(0);
                            for (b, &f) in BUCKET_FRAMES.iter().enumerate() {
                                if buckets & (1 << b) == 0 || mono_acc[ch][b].len() < f {
                                    continue;
                                }
                                let ts = *ts_val[b]
                                    .get_or_insert_with(|| bucket_next_wire_ts(f));
                                // Recycled, not allocated — see `FramePool`.
                                let mut frame = frame_pool_cap.take();
                                frame.extend(mono_acc[ch][b].drain(..f));
                                entries.push((ch, f, frame, ts));
                            }
                        }
                        if entries.is_empty() { break; }

                        let job = SendJob {
                            entries,
                            streams: cached_cap.load_full(),   // lock-free load (§2.1)
                            pool: frame_pool_cap.clone(),
                        };
                        // Not connected anywhere: the timeline advanced (counters are
                        // never reset — wire-spec §2.2) but the frames are dropped. Dropping
                        // the job is what returns their buffers to the pool.
                        if !any_conn.load(Ordering::Relaxed) { drop(job); continue; }
                        let sock  = Arc::clone(&socket_cap);
                        let cryp  = Arc::clone(&crypto_cap);
                        let sched = Arc::clone(&scheduler_cap);
                        handoff_cap.dispatch(Box::new(move || {
                            // Load once per job, not per packet: a job covers every channel
                            // in this callback, and the socket cannot change mid-job.
                            let s = sock.load();
                            encode_and_send_job(job, &s, &cryp, sched.as_ref());
                        }));
                    }

                    // ── Backlog guard: bound every accumulator to 2 frames ──
                    //
                    // This runs AFTER the drain, and the ordering is load-bearing. Run BEFORE
                    // it, a callback delivering more than two frames' worth would have the
                    // excess truncated away before the drain could packetise it. CoreAudio
                    // grants exactly the period requested, which is never larger than the frame
                    // size; a backend that rounds the period UP does not — ALSA rounds to its
                    // nearest supported period, and a driver pinned to 2048 frames against a
                    // 2.5 ms setting (f = 120, cap = 240) would lose 1808 of every 2048 samples.
                    //
                    // After the drain the truncation cannot fire on any channel the drain
                    // covers, because the drain loops until every such channel holds fewer than
                    // f samples. What the guard does here is bound channels the drain does NOT
                    // iterate: a frame size not in use gets cleared, and a
                    // channel sitting above the live device width `n_ch` after the device
                    // narrows is held at 2 frames instead of retaining stale audio
                    // indefinitely.
                    for ch in 0..total_channels {
                        let buckets = plan.per_ch_buckets.get(ch).copied().unwrap_or(0);
                        for (b, &f) in BUCKET_FRAMES.iter().enumerate() {
                            let accv = &mut mono_acc[ch][b];
                            if buckets & (1 << b) == 0 {
                                if !accv.is_empty() { accv.clear(); }
                                continue;
                            }
                            let cap = 2 * f;
                            if accv.len() > cap {
                                let drop = accv.len() - cap;
                                accv.copy_within(drop.., 0);
                                accv.truncate(cap);
                            }
                        }
                    }
            };

            let err_cb = move |fault: crate::audio::backend::StreamFault| {
                    use crate::audio::backend::StreamFault as SF;
                    match fault {
                        // The hardware actually disappeared — drive the device-lost path.
                        SF::DeviceLost => {
                            warn!("Input '{}': device no longer available (disconnected)",
                                  err_device_name);
                            // Set the lost flag only; the stats tick sends a single device_lost.
                            err_lost_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        // The stream is invalid but the DEVICE is present, so the remedy is a
                        // rebuild rather than the device-lost teardown. Rebuilding re-runs the
                        // config path, which re-asserts 48 kHz and verifies the hardware clock
                        // actually moved — so switching the device to 44.1 kHz externally now
                        // pulls it back rather than going permanently silent.
                        SF::Invalidated => {
                            // Quiet while a rebuild is already under way: setting the rate
                            // back is itself a rate change, so this is the ECHO of our own
                            // correction, not a new fault. Warning twice about one event
                            // reads as two separate problems.
                            if err_rebuilding_flag.load(std::sync::atomic::Ordering::Relaxed) {
                                debug!("Input device '{}': invalidation echo during rebuild",
                                       err_device_name);
                            } else {
                                warn!("Input '{}': device rate changed — reconfiguring", err_device_name);
                            }
                            err_invalid_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        // Recoverable glitch: tearing the device down would be a destructive
                        // response.
                        SF::Transient(what) => {
                            warn!("Input '{}': {} (transient — device retained)",
                                  err_device_name, what);
                        }
                    }
            };

            // The capture body is written ONCE, against f32. Whatever the device's own
            // sample format is, the backend converts at its boundary — see
            // `backend::open_input`. The stream comes back PREPARED BUT NOT STARTED, which
            // suits the configure-both-then-start order directly.
            let stream = crate::audio::backend::open_input(device, &cfg, f32_cb, err_cb)?;
            // `input_buf_frames`, NOT `frame_samples` — the request is capped at
            // IO_BUF_CAP_FRAMES (480) because §13.3 maps both the 10ms and 20ms send
            // settings to a 480-sample period. Comparing against the uncapped frame size
            // made a correct 20ms configuration look like a backend substitution and
            // aborted capture at boot.
            let granted = crate::audio::verify_period(
                &stream, input_buf_frames, &format!("input '{}'", crate::audio::device_name(device)));
            in_period_b.record(input_buf_frames, granted);
            Ok(stream)
                });

            let stream = build_input_stream(device, input_request)?;
            crate::audio::backend::start(&stream)?;   // boot: build+start immediately
            (stream, Some(build_input_stream))
        };




        learned.keep();
        let tone_dests_for_return = tone_dests_cell.lock().unwrap_or_else(|e| e.into_inner())
            .take().expect("tone_dests_cell not populated — platform cfg block failed");

        Ok(Self {
            _stream:      Some(stream),
            input_builder,
            num_out_ch:   outgoing_channels,
            total_ch:     total_channels,
            signal_routes: Arc::new(RwLock::new(HashMap::new())),
            current_input_device: device.clone(),
            live_in_ch:   live_in_ch_shared,
            in_period: in_period_shared,
            _scheduler: scheduler,
            send_routing:    send_routing_shared,
            peer_addrs_ref:  peer_addrs,
            cached_per_ch:   cached_per_ch_shared,
            tx_atomics:      tx_atomics_shared,
            peer_status:     peer_status_shared,
            links:           crate::net::LinkMap::default(),
            enc_live:        enc_live_shared,
            input_peaks:     input_peaks_shared,
            tone_peaks:      tone_peaks_shared,
            enc_params,
            peer_enc_prefs: peer_enc_prefs_shared,
            channel_plan: channel_plan_shared,
            tone_dests: tone_dests_for_return,
            active_streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            frame_samples_shared,
            live_in_rate: live_in_rate_shared,
            live_in_device: live_in_device_shared,
            input_device_lost: input_device_lost_shared,
            input_stream_invalid: input_stream_invalid_shared,
            input_rebuilding: input_rebuilding_shared,
        })
    }

    /// STOP the input unit — `AudioOutputUnitStop` — and hand the stopped stream back.
    ///
    /// The capture side of `pause_output_for_rebuild`, and narrow for the same reason: the
    /// device is being reopened, not removed, so the live device name, channel count and
    /// meters all stay as they are. Dropping the returned stream is the dispose.
    pub fn pause_input_for_rebuild(&mut self) -> Option<Stream> {
        let mut stream = self._stream.take()?;
        crate::audio::backend::detach_faults(&mut stream);
        let _ = crate::audio::backend::stop(&stream);
        Some(stream)
    }

    /// Stop and dispose of the input stream and clear the running device: name, rate,
    /// channel count and meters. Used when the user selects "— none —" in Settings and when
    /// the device manager stops input.
    pub fn stop_input(&mut self) {
        if let Some(mut s) = self._stream.take() {
            crate::audio::backend::detach_faults(&mut s);
            let _ = crate::audio::backend::stop(&s);
            drop(s);
        }
        if let Ok(mut g) = self.live_in_device.lock() { g.clear(); }
        self.live_in_rate.store(0, std::sync::atomic::Ordering::Relaxed);
        self.in_period.reset();
        crate::audio::forget_shortest_period(true);
        // Zero the live channel count so the TX page shows "no input device" rather
        // than the previous device's channel grid, and clear the per-channel peak
        // meters so they don't latch their last value.
        self.live_in_ch.store(0, std::sync::atomic::Ordering::Relaxed);
        for cell in self.input_peaks.iter() {
            cell.store(0u32, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// BUILD the new input stream WITHOUT starting it, and store it — for a new outgoing
    /// frame size, a new input device, or both. `device = None` keeps the current device;
    /// `frame_samples = None` keeps the current size (buffer = min(frame, 480)). Encoders,
    /// routing, tone state and the shared capture clock all carry on: the outgoing wire
    /// timestamp is never reset across a rebuild. Call `rebuild_input_start` to begin
    /// capture.
    ///
    /// The caller has already stopped and disposed the previous unit, so this expects an
    /// empty stream slot and only builds. The take-and-drop below is a fallback for a
    /// caller that did not, not the normal path.
    ///
    /// The capture side of `rebuild_output_prepare`, and rebuilt for the same reason —
    /// CASCADE_AUDIO_RECEIVE_SPEC §5.2 specifies a fresh instance on every configure. BOTH directions are prepared before either is started, then input, then
    /// output, which keeps the receive buffer on its setpoint across the change.
    pub fn rebuild_input_prepare(&mut self, device: Option<Device>,
                         frame_samples: Option<usize>) -> anyhow::Result<()> {
        if self.input_builder.is_none() {
            anyhow::bail!("rebuild_input: live input rebuild unsupported on this target");
        }
        // Switch the input device if a new one was given; otherwise keep the current.
        if let Some(d) = device {
            self.current_input_device = d;
            // A different device: what the last one did with requested periods says nothing
            // about this one.
            self.in_period.reset();
        }
        // Callback request: publish to the shared atomic FIRST (it is the backend buffer-size
        // knob, read back on a device-only change). When None (device-only change), keep the
        // current one. Never shorter than what the assigned devices can run
        // (`audio::shortest_request`), and stored as requested so the reconciler compares
        // like with like.
        let frame_samples = match frame_samples {
            Some(f) => f,
            None => self.frame_samples_shared.load(std::sync::atomic::Ordering::Relaxed),
        }.max(crate::audio::shortest_request());
        self.frame_samples_shared.store(frame_samples, std::sync::atomic::Ordering::Relaxed);
        // Fallback: STOP and dispose of any stream still here before building the new one,
        // so the two never run concurrently on the device.
        let had_stream = if let Some(mut old) = self._stream.take() {
            crate::audio::backend::detach_faults(&mut old);
            let _ = crate::audio::backend::stop(&old);
            drop(old);
            true
        } else {
            false
        };
        let build = self.input_builder.as_ref().unwrap();
        let stream = match build(&self.current_input_device, frame_samples) {
            Ok(s) => s,
            // A stream this call dropped above was stopped and disposed before this build.
            // An exclusive backend (ALSA `hw:`, WASAPI exclusive) can still report busy for
            // a moment while the kernel side of that release completes, so a busy open when
            // we did hold the device gets one retry after the release settle. A second busy
            // is genuinely someone else's hold and is returned.
            Err(e) if crate::audio::backend::is_device_busy(&e) && had_stream => {
                warn!("Input '{}': device busy on rebuild — retrying after the release \
                       settle", crate::audio::device_name(&self.current_input_device));
                std::thread::sleep(DEVICE_RELEASE_SETTLE);
                build(&self.current_input_device, frame_samples)?
            }
            Err(e) => return Err(e),
        };
        // Every backend returns streams prepared and stopped, so "prepared" is already the
        // state we want and this stop() is a no-op. It is kept because it makes the
        // build-then-start contract explicit at the call site: the spec requires both
        // endpoints configured before either runs, and a backend that ever started on
        // build would break that silently rather than loudly.
        let _ = crate::audio::backend::stop(&stream);
        self._stream = Some(stream);
        info!("Input '{}': rebuilt (frame {}, buffer {}) — prepared",
              crate::audio::device_name(&self.current_input_device),
              frame_samples, frame_samples.min(IO_BUF_CAP_FRAMES));
        Ok(())
    }

    /// START the prepared input stream (play). Called after BOTH units are prepared, and
    /// BEFORE the output unit is started. The input-then-output order is fixed.
    pub fn rebuild_input_start(&self) -> anyhow::Result<()> {
        if let Some(s) = self._stream.as_ref() { crate::audio::backend::start(s)?; }
        Ok(())
    }

    /// Change the encoder bitrate for all active channels without restarting the stream.
    /// 0 = VBR auto. Takes effect on the next encoded frame per channel.
    ///
    /// Every active encoder is attempted: one that refuses the value does not stop the rest
    /// from taking it. Returns an error naming how many refused, and the first reason.
    pub fn set_bitrate_hot(&self, bitrate_kbps: u32) -> anyhow::Result<()> {
        // Update the template so future lazily-created encoders use the new bitrate.
        self.enc_params.write().unwrap_or_else(|e| e.into_inner()).bitrate_kbps = bitrate_kbps;
        let bitrate = if bitrate_kbps == 0 {
            opus::Bitrate::Auto
        } else {
            opus::Bitrate::Bits((bitrate_kbps * 1000) as i32)
        };
        let mut applied = 0usize;
        let mut refused = 0usize;
        let mut first_err: Option<opus::Error> = None;
        let live: Vec<Arc<Mutex<ChannelEncoder>>> = self.enc_live.lock()
            .unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
        for enc_arc in live {
            if let Ok(mut enc) = enc_arc.lock() {
                match enc.encoder.set_bitrate(bitrate) {
                    Ok(()) => applied += 1,
                    Err(e) => {
                        refused += 1;
                        first_err.get_or_insert(e);
                    }
                }
            }
        }
        info!("Bitrate → {} kbps, {} active encoder(s)",
              if bitrate_kbps == 0 { "VBR auto".to_string() } else { format!("{}", bitrate_kbps) },
              applied);
        match first_err {
            Some(e) => Err(anyhow::anyhow!("set_bitrate: {} encoder(s) refused ({})", refused, e)),
            None => Ok(()),
        }
    }

    /// Hot reinitialise encoders with new frame_ms and/or mode.
    /// Pass frame_ms < 0 to keep existing, mode = None to keep existing.
    pub fn reinit_encoders(&self, frame_ms: f32, mode: Option<&str>) -> anyhow::Result<()> {
        use crate::config::AudioMode;
        // Global (UI-driven) change: applies to the default template AND every
        // registered per-remote preference, then reconciles. Reconcile touches only the
        // streams whose (frame, mode) actually changed — a stream still in use keeps its
        // live encoder untouched (CASCADE_AUDIO_SEND_SPEC §5.2).
        // When frame_ms <= 0 (sentinel — mode-only change), the frame size is kept.
        let frame_samples = if frame_ms > 0.0 {
            crate::config::frame_ms_to_samples(frame_ms)
        } else {
            self.enc_params.read().unwrap_or_else(|e| e.into_inner()).frame_samples
        };
        let mode_cfg = mode.map(|m| {
            if m == "voice" { AudioMode::Voice } else { AudioMode::Audio }
        });
        {
            let mut p = self.enc_params.write().unwrap_or_else(|e| e.into_inner());
            if frame_ms > 0.0 { p.frame_samples = frame_samples; }
            if let Some(mc) = mode_cfg { p.mode = mc; }
        }
        {
            let mut prefs = self.peer_enc_prefs.write().unwrap_or_else(|e| e.into_inner());
            for v in prefs.values_mut() {
                if frame_ms > 0.0 { v.0 = frame_samples; }
                if let Some(mc) = mode_cfg { v.1 = mc; }
            }
        }
        self.reconcile_encoders();
        info!("Encoders reconciled: frame={}ms mode={}",
              frame_samples as f32 / 48.0, mode.unwrap_or("(unchanged)"));
        Ok(())
    }

    /// Register (or update) a remote's encode preferences from its config:
    /// frame_ms → frame samples, plus its Opus application mode. Both are per remote —
    /// there is no instance-wide value for either. Triggers a reconcile so per-channel
    /// latency resolution (§4.1) picks the change up immediately.
    pub fn set_peer_enc_prefs(&self, peer: &str, frame_ms: f32,
                              mode: crate::config::AudioMode) {
        let frame = crate::config::frame_ms_to_samples(frame_ms);
        self.peer_enc_prefs.write().unwrap_or_else(|e| e.into_inner())
            .insert(peer.to_string(), (frame, mode));
        self.reconcile_encoders();
    }

    /// Tone routing per destination slot: 0 = off, 1 = Tone L (EBU ident,
    /// interrupted), 2 = Tone R (continuous) — the wire format the TX matrix already
    /// sends, kept unchanged so the UI needs no rework.
    ///
    /// The slots are translated into ordinary routes on the two tone pseudo source
    /// channels (see TONE_LEGS) and composed with this peer's signal routes. Mutual
    /// exclusivity is not a special case: rebuild_send_routing applies tone
    /// after signal, and PeerSendRouting's one-source-per-slot rule evicts whatever
    /// signal source held a contested slot.
    pub fn set_tone_routing(&self, peer: &str, slots: &[u8]) {
        {
            let mut td = self.tone_dests.lock().unwrap_or_else(|e| e.into_inner());
            if slots.is_empty() || slots.iter().all(|&b| b == 0) {
                td.remove(peer);
            } else {
                td.insert(peer.to_string(), slots.to_vec());
            }
        }
        self.rebuild_send_routing(peer);
    }

    /// Compose this peer's SIGNAL routes and TONE routes into one send table.
    ///
    /// Order is load-bearing: signal first, then tone, so a tone leg dropped onto an
    /// occupied slot displaces the signal source there — matching what the TX matrix
    /// shows, and the reason no separate conflict-resolution pass is needed.
    fn rebuild_send_routing(&self, peer: &str) {
        use crate::audio::routing::{PeerSendRouting, RouteEntry};
        let mut routes: Vec<RouteEntry> = self.signal_routes
            .read().unwrap_or_else(|e| e.into_inner())
            .get(peer).cloned().unwrap_or_default();
        {
            let td = self.tone_dests.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slots) = td.get(peer) {
                for (ci, &t) in slots.iter().enumerate() {
                    // §8: 128 is the structural channel ceiling on the wire.
                    if t == 0 || ci > 127 { continue; }
                    routes.push(RouteEntry {
                        src:   tone_channel(self.num_out_ch, t) as u8,
                        dst:   ci as u8,
                        value: 1,
                    });
                }
            }
        }
        let empty = routes.is_empty();
        {
            let mut sr = self.send_routing.write().unwrap_or_else(|e| e.into_inner());
            if empty { sr.remove(peer); } else {
                sr.insert(peer.to_string(), PeerSendRouting::new(&routes));
            }
        }
        self.rebuild_cached_per_ch();
        self.reconcile_encoders();
    }

    /// Forget all tone routing across every peer. Called when going ON AIR so a
    /// line-up tone can never be left feeding the wire during a live transmission.
    pub fn clear_tone(&self) {
        let peers: Vec<String> = {
            let mut td = self.tone_dests.lock().unwrap_or_else(|e| e.into_inner());
            if td.is_empty() { return; }
            let peers = td.keys().cloned().collect();
            td.clear();
            peers
        };
        // Rebuild from the SIGNAL routes alone. Nothing is reinstated: a signal route
        // that a tone leg displaced was removed by the UI when the tone was placed, so
        // dropping tone leaves the slot unrouted rather than restoring what was there.
        for peer in peers { self.rebuild_send_routing(&peer); }
    }

    pub fn set_send_routing(&self, peer: &str,
                            routes: &[crate::audio::routing::RouteEntry]) {
        // Store the signal routes as given; tone is composed on top in
        // rebuild_send_routing, where one-source-per-slot settles any contest.
        self.signal_routes.write().unwrap_or_else(|e| e.into_inner())
            .insert(peer.to_string(), routes.to_vec());
        self.rebuild_send_routing(peer);
    }

    /// Remove ALL per-peer capture state for a removed/disabled remote — the capture-side
    /// mirror of AudioEngine::remove_peer. Drops the send routing and encode preferences
    /// (with encoder reconcile), the TX byte-counter and connection-status atomics, and any
    /// tone-destination slots. Keeping all of it here means the capture engine owns its own
    /// teardown in one place, so the main-loop teardown can't drift.
    pub fn remove_peer(&self, peer: &str) {
        self.tx_atomics.write().unwrap_or_else(|e| e.into_inner()).remove(peer);
        // Drop the status atomic too — otherwise a removed/disabled remote leaves a
        // stale entry behind, and re-adding the same name would briefly inherit its old
        // connection status before the stats pump refreshes it.
        self.peer_status.write().unwrap_or_else(|e| e.into_inner()).remove(peer);
        // Encoders are not per remote: the reconcile below releases any stream this remote
        // was the last user of.
        self.tone_dests.lock().unwrap_or_else(|e| e.into_inner()).remove(peer);
        self.signal_routes.write().unwrap_or_else(|e| e.into_inner()).remove(peer);
        self.peer_enc_prefs.write().unwrap_or_else(|e| e.into_inner()).remove(peer);
        // send_routing last — it triggers the encoder reconcile, which should run against
        // the already-cleaned state.
        self.send_routing.write().unwrap_or_else(|e| e.into_inner()).remove(peer);
        self.rebuild_cached_per_ch();
        self.reconcile_encoders();
    }

    /// The smallest frame size anything is sent at, in samples, or `None` when no channel
    /// is routed: the minimum over every remote that routes at least one channel of its own
    /// frame size (each raised to `audio::shortest_frame`, as `stream_params` does). The
    /// shared device callback is driven from this (§13.3), so every stream's frames
    /// complete on callback boundaries.
    pub fn min_resolved_send_frame(&self) -> Option<usize> {
        let p     = *self.enc_params.read().unwrap_or_else(|e| e.into_inner());
        let sr    = self.send_routing.read().unwrap_or_else(|e| e.into_inner());
        let prefs = self.peer_enc_prefs.read().unwrap_or_else(|e| e.into_inner());
        sr.iter()
            .filter(|(_, routing)| !routing.channel_to_slot.is_empty())
            .map(|(peer, _)| stream_params(&prefs, &p, peer).0)
            .min()
    }

    /// Re-resolve every stream after the shortest frame size changed
    /// (`audio::shortest_frame`), which moves any remote sent below it.
    pub fn apply_frame_floor(&self) {
        self.reconcile_encoders();
    }

    /// Reconcile the live encoders against current routing and per-remote preferences.
    /// Runs OFF the audio thread (on routing and encoder-setting changes only), so
    /// opus_encoder_create never touches the realtime path.
    ///
    /// One stream per (source channel, frame size, mode) that some remote is sent at
    /// (CASCADE_AUDIO_SEND_SPEC §4.1, §5). A stream still needed keeps its encoder
    /// untouched; one no longer needed has its encoder dropped; a new one gets a fresh
    /// encoder. No encoder exists for a channel that is not routed.
    fn reconcile_encoders(&self) {
        let p = *self.enc_params.read().unwrap_or_else(|e| e.into_inner());

        // The send streams needed (CASCADE_AUDIO_SEND_SPEC §4.1/§5): for every remote, one
        // per channel it routes, at that remote's own frame size and mode. Remotes that
        // agree on both share a stream; nothing is merged across remotes that differ.
        let desired: std::collections::HashSet<StreamKey> = {
            let sr    = self.send_routing.read().unwrap_or_else(|e| e.into_inner());
            let prefs = self.peer_enc_prefs.read().unwrap_or_else(|e| e.into_inner());
            let mut d = std::collections::HashSet::new();
            for (peer, routing) in sr.iter() {
                let (pf, pm) = stream_params(&prefs, &p, peer);
                for &src in routing.channel_to_slot.keys() {
                    if (src as usize) < self.total_ch { d.insert((src as usize, pf, pm)); }
                }
            }
            d
        };

        // Live encoders follow the desired set exactly: a stream still needed keeps its
        // encoder untouched (state and sequence continue), one no longer needed is dropped,
        // and a new one gets a fresh encoder.
        {
            let mut live = self.enc_live.lock().unwrap_or_else(|e| e.into_inner());
            live.retain(|k, _| {
                let keep = desired.contains(k);
                if !keep { debug!("encoder ch{}: released ({} samples, {:?})", k.0, k.1, k.2); }
                keep
            });
            for &k in &desired {
                if live.contains_key(&k) { continue; }
                match ChannelEncoder::new(p.bitrate_kbps, k.2, k.1) {
                    Ok(e) => {
                        debug!("encoder ch{}: created ({} samples, {:?})", k.0, k.1, k.2);
                        live.insert(k, Arc::new(Mutex::new(e)));
                    }
                    Err(e) => warn!("encoder ch{} ({} samples, {:?}): {}", k.0, k.1, k.2, e),
                }
            }
        }

        // Publish the channel plan for the capture callback: the frame sizes each channel
        // is buffered at.
        {
            let mut per_ch_buckets = vec![0u8; self.total_ch];
            for &(ch, f, _) in &desired { per_ch_buckets[ch] |= 1 << bucket_index(f); }
            // Atomic store — the RT capture thread load_full()s this without a lock (§2.1).
            self.channel_plan.store(Arc::new(ChannelPlan { per_ch_buckets }));
        }
        // And the streams with their destinations.
        self.rebuild_cached_per_ch();

        // Publish: routed source streams + tone streams = active outgoing stream count.
        // COMBINED ACROSS ALL REMOTES (per-destination): a source channel routed to N peers is
        // N outgoing streams, matching how tone is counted per-(peer, slot). This is the
        // "what's leaving me" total. NOTE distinct from the ENCODER count (unique source
        // channels) — Opus encodes a source once and fans the packet to each peer; the stream
        // count is per-destination, the encoder count is per-source. We want streams here.
        let source_stream_count: usize = {
            let sr = self.send_routing.read().unwrap_or_else(|e| e.into_inner());
            sr.values().map(|routing| routing.stream_count()).sum()
        };
        // Tone is an ordinary source now, so its streams are already counted above.
        self.active_streams.store(source_stream_count, std::sync::atomic::Ordering::Relaxed);
    }

    /// Live outgoing stream count to CONNECTED peers only — the real "what's leaving me" TX
    /// total. Per peer: every (source, slot) pair in send_routing — which now includes the
    /// tone legs. Summed across only the peers in `connected`.
    /// Engine truth (not config): reflects the routing actually applied. A disconnected peer's
    /// routing persists in send_routing (so it restores on reconnect) but audio send to it has
    /// stopped, so it must be excluded here — hence the connected-set filter.
    pub fn live_send_streams(&self, connected: &std::collections::HashSet<String>) -> usize {
        let sr = self.send_routing.read().unwrap_or_else(|e| e.into_inner());
        sr.iter().filter(|(peer, _)| connected.contains(*peer))
            .map(|(_, routing)| routing.stream_count()).sum()
    }

    /// Adopt an externally-owned peer-status map. The stats pump is spawned before this
    /// engine exists, so main creates the map first and hands it over here; both then share
    /// one set of live atomics. Rebuilds the cached destination table so existing entries
    /// pick up their real status Arc instead of the default (down) placeholder.
    pub fn adopt_peer_status_map(
        &mut self,
        m: Arc<RwLock<HashMap<String, Arc<std::sync::atomic::AtomicU8>>>>,
    ) {
        self.peer_status = m;
        self.rebuild_cached_per_ch();
    }

    /// Adopt the process-wide per-remote link indices (net::LinkIndices), which every
    /// audio packet sent to a remote carries at 0x0B/0x11.
    pub fn adopt_link_map(&mut self, m: crate::net::LinkMap) {
        self.links = m;
        self.rebuild_cached_per_ch();
    }

    /// Register a per-peer TX atomic. Called from main.rs when creating each peer task.
    /// The same Arc is passed to the peer task so poke_tick can read+reset it.
    pub fn register_tx_atomic(&self, peer: &str, atom: Arc<std::sync::atomic::AtomicU64>) {
        self.tx_atomics.write().unwrap_or_else(|e| e.into_inner()).insert(peer.to_string(), atom);
        self.rebuild_cached_per_ch();
    }

    /// The live input (capture) callback period — the shared min(incoming, outgoing)
    /// last applied to this unit as its CoreAudio buffer size. Used by the reconciler to
    /// skip a no-op rebuild. (frame_samples_shared is the input-buffer knob, distinct from
    /// the per-channel encode frame in channel_plan, which is unaffected by resizing it.)
    pub fn current_input_frames(&self) -> usize {
        self.frame_samples_shared.load(std::sync::atomic::Ordering::Relaxed)
            .min(IO_BUF_CAP_FRAMES)
    }

    pub fn rebuild_cached_per_ch(&self) {
        let p  = *self.enc_params.read().unwrap_or_else(|e| e.into_inner());
        let pa = self.peer_addrs_ref.read().unwrap_or_else(|e| e.into_inner());
        let sr = self.send_routing.read().unwrap_or_else(|e| e.into_inner());
        let tx = self.tx_atomics.read().unwrap_or_else(|e| e.into_inner());
        let st = self.peer_status.read().unwrap_or_else(|e| e.into_inner());
        let prefs = self.peer_enc_prefs.read().unwrap_or_else(|e| e.into_inner());
        let live  = self.enc_live.lock().unwrap_or_else(|e| e.into_inner());
        // total_ch, not num_out_ch: the tone legs need streams too.
        let built = build_streams(&pa, &sr, &tx, &st, &self.links, &prefs, &p, &live,
                                  self.total_ch);
        // Atomic store — the RT capture thread load_full()s this without a lock (§2.1).
        self.cached_per_ch.store(Arc::new(built));
    }
}

/// The frame size and mode a remote is sent at: its own preference (the template's when it
/// has none), never shorter than the assigned devices' callback (`audio::shortest_frame`) —
/// a shorter setting is sent at that size.
fn stream_params(prefs: &HashMap<String, (usize, crate::config::AudioMode)>,
                 p: &EncoderParams, peer: &str) -> (usize, crate::config::AudioMode) {
    let (f, m) = prefs.get(peer).copied().unwrap_or((p.frame_samples, p.mode));
    (f.max(crate::audio::shortest_frame()), m)
}

/// Each source channel's send streams: one per live encoder on that channel, carrying every
/// (remote, slot) destination sent at that stream's frame size and mode. Streams on a
/// channel are ordered by (frame size, mode) and destinations by remote name, so emission
/// order is deterministic.
#[allow(clippy::too_many_arguments)]
fn build_streams(
    pa: &HashMap<String, SocketAddr>,
    sr: &HashMap<String, crate::audio::routing::PeerSendRouting>,
    tx: &HashMap<String, Arc<std::sync::atomic::AtomicU64>>,
    st: &HashMap<String, Arc<std::sync::atomic::AtomicU8>>,
    lm: &crate::net::LinkMap,
    prefs: &HashMap<String, (usize, crate::config::AudioMode)>,
    p: &EncoderParams,
    live: &HashMap<StreamKey, Arc<Mutex<ChannelEncoder>>>,
    n_ch: usize,
) -> Vec<Vec<SendStream>> {
    let mut out: Vec<Vec<SendStream>> = (0..n_ch).map(|_| Vec::new()).collect();
    for (&(ch, f, m), enc) in live.iter() {
        if let Some(list) = out.get_mut(ch) {
            list.push(SendStream { frame: f, mode: m, encoder: Arc::clone(enc), dests: vec![] });
        }
    }
    for list in out.iter_mut() {
        list.sort_by_key(|s| (s.frame, s.mode == crate::config::AudioMode::Audio));
    }
    // No send routing at all → NOTHING is sent. (Policy: nothing routed unless specified —
    // same rule as RX.) There is deliberately no fallback to every known peer address:
    // disabling a remote removes its routing entry while its address can still be cached,
    // and a fallback would keep streaming to it.
    let default_atom = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let default_stat = Arc::new(std::sync::atomic::AtomicU8::new(0));   // 0 = down
    let mut peers: Vec<&String> = sr.keys().collect();
    peers.sort();
    for peer in peers {
        let routing = &sr[peer];
        // A remote with no known address yet has nowhere to send.
        let Some(&addr) = pa.get(peer) else { continue };
        let (pf, pm) = stream_params(prefs, p, peer);
        let atom = tx.get(peer).map(Arc::clone).unwrap_or_else(|| Arc::clone(&default_atom));
        // Status Arc defaults to 0 (down) for a peer with no entry yet, so a
        // not-yet-known peer is gated OFF rather than sent to blindly.
        let stat = st.get(peer).map(Arc::clone).unwrap_or_else(|| Arc::clone(&default_stat));
        let link = crate::net::link_for(lm, peer);
        for ch in 0..n_ch {
            let slots = routing.resolve(ch as u8);
            if slots.is_empty() { continue; }
            let Some(stream) = out[ch].iter_mut().find(|s| s.frame == pf && s.mode == pm)
                else { continue };
            // One destination per slot: a source feeding several slots on this remote
            // yields several entries, each carrying its own channel number.
            for &remote_slot in slots {
                // §8: 128 is the structural channel ceiling — a destination slot above
                // 127 is not addressable on the wire, so reject it here rather than
                // emitting an out-of-range channel byte.
                if remote_slot > 127 { continue; }
                stream.dests.push(SendDest {
                    addr, slot: remote_slot, tx: Arc::clone(&atom), peer: peer.clone(),
                    status: Arc::clone(&stat), link: Arc::clone(&link),
                });
            }
        }
    }
    out
}

/// How long to let an exclusive device settle after closing it, before reopening.
///
/// Dropping a `Stream` joins the backend's worker and closes the device synchronously, so
/// by the time the error surfaces the handle is already gone. This is margin for the kernel
/// side of the release, which that guarantee does not cover. Cold path only.
pub(crate) const DEVICE_RELEASE_SETTLE: std::time::Duration =
    std::time::Duration::from_millis(20);

/// The frame-grid invariant across frame-size changes (CASCADE_AUDIO_SEND_SPEC §3).
/// The frame-buffer pool: recycling, and the two fallbacks that keep it safe.
#[cfg(test)]
mod frame_pool_tests {
    use super::{FramePool, FRAME_MAX};

    #[test]
    fn a_returned_buffer_comes_back_empty_and_keeps_its_capacity() {
        let pool = FramePool::new(2);
        let mut b = pool.take();
        b.extend([0.5f32; 480]);
        assert_eq!(b.capacity(), FRAME_MAX, "pre-allocated at the largest frame size");
        pool.give(b);
        let again = pool.take();
        assert!(again.is_empty(), "handed out empty");
        assert_eq!(again.capacity(), FRAME_MAX, "and without having to grow");
    }

    /// Every buffer of a full pool can be held at once, which is what sizing it per channel
    /// is for.
    #[test]
    fn the_whole_pool_can_be_in_flight() {
        let pool = FramePool::new(3);
        let held: Vec<_> = (0..3).map(|_| pool.take()).collect();
        assert_eq!(held.len(), 3);
        for b in held { pool.give(b); }
        assert_eq!(pool.take().capacity(), FRAME_MAX, "all of them came back");
    }

    /// An empty pool allocates rather than blocking or dropping a frame.
    #[test]
    fn an_empty_pool_still_yields_a_buffer() {
        let pool = FramePool::new(1);
        let first = pool.take();
        let extra = pool.take();
        assert!(extra.is_empty());
        assert!(extra.capacity() >= FRAME_MAX);
        drop((first, extra));
    }

    /// Returning more than the pool holds drops the surplus instead of growing without
    /// bound: memory stays at the size it was built with.
    #[test]
    fn a_full_pool_drops_the_surplus() {
        let pool = FramePool::new(1);
        pool.give(Vec::with_capacity(FRAME_MAX));
        pool.give(Vec::with_capacity(FRAME_MAX));
        assert!(pool.take().is_empty());
        // The pool held one, so the second give was dropped and this take allocates.
        assert!(pool.take().is_empty());
    }
}

#[cfg(test)]
mod frame_grid_tests {
    use super::keep_on_frame_grid;

    /// One simulated capture callback of `k` samples, followed by the drain. Each captured
    /// sample's value is its absolute capture index, so the last sample of a drained frame
    /// says exactly where that frame ends. Returns, per channel, the end index of every frame
    /// completed this callback.
    fn callback(accs: &mut [Vec<f32>], prevs: &mut [usize], frames: &[usize],
                tc: &mut u64, k: usize) -> Vec<Vec<u64>> {
        for ch in 0..accs.len() {
            keep_on_frame_grid(&mut accs[ch], &mut prevs[ch], frames[ch], *tc);
            if frames[ch] != 0 {
                accs[ch].extend((0..k).map(|i| (*tc + i as u64) as f32));
            }
        }
        *tc += k as u64;
        accs.iter_mut().zip(frames).map(|(acc, &f)| {
            let mut ends = Vec::new();
            while f != 0 && acc.len() >= f {
                let frame: Vec<f32> = acc.drain(..f).collect();
                ends.push(frame[f - 1] as u64);
            }
            ends
        }).collect()
    }

    /// A channel whose frame size grows while routed, and a channel routed after the change,
    /// must complete their frames on the same samples. On the routed edge alone the first
    /// channel stayed on its old 10 ms grid and the two ran exactly 10 ms apart.
    #[test]
    fn a_frame_size_increase_keeps_later_channels_on_the_same_grid() {
        let mut accs = vec![Vec::new(), Vec::new()];
        let mut prevs = vec![0usize, 0];
        let mut tc: u64 = 1_000;          // deliberately not on any grid
        let k = 480;
        let mut both_drained = 0;
        for cb in 0..12 {
            // Channel 0: 10 ms frames, then 20 ms from the fourth callback.
            // Channel 1: unrouted, then routed at 20 ms from the sixth.
            let frames = [if cb < 3 { 480 } else { 960 }, if cb < 5 { 0 } else { 960 }];
            let ends = callback(&mut accs, &mut prevs, &frames, &mut tc, k);
            for ch in 0..2 {
                let f = frames[ch] as u64;
                if f != 0 {
                    assert_eq!(accs[ch].len() as u64 % f, tc % f,
                               "callback {cb}: channel {ch} is off the {f}-sample grid");
                }
            }
            if !ends[0].is_empty() && !ends[1].is_empty() {
                assert_eq!(ends[0], ends[1],
                           "callback {cb}: frames sharing a round must end on the same sample");
                both_drained += 1;
            }
        }
        assert!(both_drained > 0, "the two channels never drained in the same round");
    }

    /// An unchanged frame size must leave the accumulator alone — re-priming on every
    /// callback would throw audio away continuously.
    #[test]
    fn an_unchanged_frame_size_is_left_alone() {
        let mut acc = vec![0.5f32; 300];
        let mut prev = 480;
        assert!(!keep_on_frame_grid(&mut acc, &mut prev, 480, 12_345));
        assert_eq!(acc.len(), 300);
    }

    /// Unrouting forgets the grid, so routing again primes afresh.
    #[test]
    fn unrouting_forgets_the_grid() {
        let mut acc = vec![0.5f32; 100];
        let mut prev = 960;
        assert!(!keep_on_frame_grid(&mut acc, &mut prev, 0, 0));
        assert_eq!(prev, 0);
        assert!(keep_on_frame_grid(&mut acc, &mut prev, 960, 1_000));
        assert_eq!(acc.len(), 1_000 % 960);
    }
}
