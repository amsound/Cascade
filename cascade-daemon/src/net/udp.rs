/// UDP engine — socket I/O and packet parsing.
///
/// Receives run on a dedicated BLOCKING std::thread rather than through async I/O: the OS
/// wakes it per packet and there is no readiness polling. An async receive path costs
/// kevent/epoll overhead proportional to packet rate, which at a few thousand packets a
/// second dominates the work actually being done.
///
/// AUDIO packets are handled INLINE on that thread via RecvContext — no handoff. CONTROL
/// packets (POKE/RSP/ACK/labels) go to the async main loop via `blocking_send`. Control
/// sends (one per two seconds) stay on a blocking send thread — same socket, negligible
/// at that rate.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{warn, debug};
use anyhow::Result;

use super::protocol::{
    parse_type, parse_timestamp, parse_sample_rate, parse_label_revision,
    parse_flags, payload_start,
    parse_sender_token, parse_channel, parse_opus_len, parse_sequence,
    PacketType, Token, HEADER_LEN, TOKEN_LEN,
    FLAG_ENCRYPTED, CODEC_OPUS, CODEC_RAW16, CODEC_RAW24,
};

const MAX_PACKET: usize = 8192;

#[derive(Debug)]
pub struct InboundPacket {
    pub from:         SocketAddr,
    pub ptype:        PacketType,
    pub ts:           u32,
    pub sender_token: Token,
    pub label_revision: u8,
    /// Raw control datagram — a pong is built by modifying the ping's own buffer (§2.1).
    pub raw:          Vec<u8>,
    pub label_meta:   u16,
    pub payload:      Vec<u8>,
}

pub struct OutboundPacket {
    pub to:   SocketAddr,
    pub data: Vec<u8>,
}

/// Recv-side-local probe for the handle-cache hot path: counts cache
/// hits vs (rare) resolves, tracks the worst wall-time sample, and emits a periodic
/// summary. Single-threaded (lives on the serial recv handler, like seen_audio_peers /
/// chan_cache), so no locking. The cached dispatch path takes NO engine-map locks, so a
/// non-trivial `worst` here means a genuine OS deschedule, not lock contention — that is
/// the thing this probe exists to distinguish.
struct HotPathProbe {
    hits:       u64,
    misses:     u64,
    worst_ms:   f64,
    window:     u64,   // packets since last summary
    win_worst:  f64,
    win_hits:   u64,
    win_misses: u64,
}

impl HotPathProbe {
    fn new() -> Self {
        HotPathProbe { hits: 0, misses: 0, worst_ms: 0.0,
                       window: 0, win_worst: 0.0, win_hits: 0, win_misses: 0 }
    }
    /// Record a wall-time sample (ms) for this packet.
    fn note(&mut self, ms: f64) {
        if ms > self.worst_ms  { self.worst_ms = ms; }
        if ms > self.win_worst { self.win_worst = ms; }
        self.window += 1;
    }
    /// Every ~2000 packets, return (win_hits, win_misses, win_worst) and reset the window.
    fn tick(&mut self) -> Option<(u64, u64, f64)> {
        if self.window >= 2000 {
            let out = (self.win_hits, self.win_misses, self.win_worst);
            self.window = 0;
            self.win_worst = 0.0;
            self.win_hits = 0;
            self.win_misses = 0;
            Some(out)
        } else {
            None
        }
    }
}

/// How long the receive loop spends BLOCKED in `recv_from`, which is the one segment of the
/// receive path nothing else measures.
///
/// `HotPathProbe` and the on-CPU/off-CPU discriminator both start their clock *after*
/// `recv_from` has returned, so they measure handling and are blind to the wait. That
/// matters because a receive thread that is not scheduled to read produces two symptoms at
/// once — arrival timestamps bunch up (reported as jitter) and nothing is written to the
/// rings (reported as the buffer dropping) — while every handling metric stays perfect.
///
/// The discriminator is the BURST LENGTH after a long wait, not the wait itself. Waiting
/// ~20 ms for the next frame is normal; what is not normal is waiting 40 ms and then
/// draining twice as many packets back to back, because that is a frame's worth of packets
/// that sat in the socket while we were not reading.
struct WaitProbe {
    /// Packets read with essentially no wait since the last real block — the burst length.
    burst:      u64,
    /// The block that preceded the current burst, in ms.
    burst_wait: f64,
    worst_ms:   f64,
    window:     u64,
    win_worst:  f64,
    win_bursts: u64,
    win_maxlen: u64,
}

impl WaitProbe {
    /// Under this, the datagram was already queued — we are draining, not waiting.
    const DRAINING_MS: f64 = 1.0;
    /// A frame is 20 ms at the usual sizes; half again is comfortably outside normal.
    const ALERT_MS: f64 = 30.0;

    fn new() -> Self {
        WaitProbe { burst: 0, burst_wait: 0.0, worst_ms: 0.0,
                    window: 0, win_worst: 0.0, win_bursts: 0, win_maxlen: 0 }
    }

    /// One `recv_from` completed after blocking `ms`. Returns `Some((wait, burst))` when a
    /// burst ends on a wait long enough to be worth reporting — `burst` is how many packets
    /// the PREVIOUS wait had queued up.
    fn note(&mut self, ms: f64) -> Option<(f64, u64)> {
        self.window += 1;
        if ms < Self::DRAINING_MS {
            self.burst += 1;
            if self.burst > self.win_maxlen { self.win_maxlen = self.burst; }
            return None;
        }
        // A real wait: the burst it was preceded by is now complete.
        let out = if self.burst_wait > Self::ALERT_MS || ms > Self::ALERT_MS {
            Some((ms, self.burst))
        } else {
            None
        };
        if ms > self.worst_ms  { self.worst_ms  = ms; }
        if ms > self.win_worst { self.win_worst = ms; }
        self.win_bursts += 1;
        self.burst_wait = ms;
        self.burst = 1;
        out
    }

    /// Every ~2000 packets: (bursts, longest burst, worst wait), and reset the window.
    fn tick(&mut self) -> Option<(u64, u64, f64)> {
        if self.window >= 2000 {
            let out = (self.win_bursts, self.win_maxlen, self.win_worst);
            self.window = 0; self.win_worst = 0.0; self.win_bursts = 0; self.win_maxlen = 0;
            Some(out)
        } else {
            None
        }
    }
}

/// Receive-thread handles for the §9.2 pre-decode meter (crate::meter): per remote, its
/// watch deadline, its peak cells, and its name as a shared string for the meter jobs.
/// Looked up once per remote; after that, deciding whether a packet is metered is one
/// atomic read. Handles are never invalidated — the meter keeps one cell set and one
/// deadline per remote name for the life of the process.
#[derive(Default)]
struct MeterCache {
    peers: std::collections::HashMap<String, MeterHandles>,
}

struct MeterHandles {
    deadline: Arc<std::sync::atomic::AtomicU64>,
    cells:    Arc<Vec<std::sync::atomic::AtomicU32>>,
    peer:     Arc<str>,
}

impl MeterCache {
    fn get(&mut self, meters: &crate::meter::Meters, peer: &str) -> &MeterHandles {
        if !self.peers.contains_key(peer) {
            self.peers.insert(peer.to_string(), MeterHandles {
                deadline: meters.watch.handle(peer),
                cells:    meters.pre_cells(peer),
                peer:     Arc::from(peer),
            });
        }
        &self.peers[peer]
    }
}

/// Shared context that lets `cascade-recv` process AUDIO packets INLINE — drain, demux and
/// handle on one serial context, rather than `blocking_send`-ing every audio packet to a
/// tokio worker. That cross-thread hop caused multi-millisecond select-loop deschedules: a
/// parked worker woken cross-thread per packet incurs wake latency that corrupts the
/// arrival-timestamp cadence and produces audible decode artifacts.
///
/// `cascade-recv` is spawned (in `start`) BEFORE the audio engine and peer maps exist, so
/// the context is injected late via `RecvCtxCell`: until main populates it, audio falls
/// through to `blocking_send` (correct at startup — there is nothing to receive into before
/// the engine exists, and the render-alive gate drops pre-render audio anyway).
///
/// Concurrency: the four maps are written ONLY by the select loop (peer setup / hot add /
/// remove) and read here per audio packet — single-writer / multi-reader, sound under
/// `RwLock`. Control/label packets still go to the select loop via `inbound_tx`.
/// The receive path is split into two planes:
///   - RecvAccounting: ALWAYS present once the daemon is up. Identifies the sender, counts
///     incoming bytes (RX rate), tracks the incoming sample-rate flag, and registers incoming
///     channels for the UI. None of this needs an output engine — receiving and accounting for
///     audio does not require being able to play it. This is why an enabled-but-unplayable
///     remote (no output device, or wrong name/password so we never connect) still shows an
///     incoming data rate + channels: the operator sees "data arriving but idle → misconfig".
///   - RecvDecode: OPTIONAL — present only when an output engine exists. Holds the engine and
///     is the only part that decodes/enqueues/renders. Attaches when an output device is
///     enabled (incl. late, post-boot), detaches when there is none.
/// RecvContext wraps both so the existing single RecvCtxCell delivery is unchanged.
pub struct RecvAccounting {
    pub by_addr:    Arc<std::sync::RwLock<std::collections::HashMap<SocketAddr, String>>>,
    pub rx_atomics: Arc<std::sync::RwLock<std::collections::HashMap<String, Arc<std::sync::atomic::AtomicU64>>>>,
    pub sr_atomics: Arc<std::sync::RwLock<std::collections::HashMap<String, Arc<std::sync::atomic::AtomicBool>>>>,
    /// Peers configured with phase_lock=true — for the first-packet one-shot apply.
    pub phase_lock_peers: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    /// Per-peer incoming channel registry (shared with the API) + the active-seen map.
    pub incoming_channels: Arc<std::sync::RwLock<std::collections::HashMap<String, Vec<crate::api::ChannelInfo>>>>,
    pub incoming_seen: Arc<std::sync::Mutex<std::collections::HashMap<(String, u8), std::time::Instant>>>,
    /// Per-remote crypto state, for decrypting flagged audio (CASCADE_ENCRYPTION_SPEC §7).
    pub crypto: crate::net::CryptoMap,
    /// Incoming-signal metering: who is watching, and the pre-decode meter worker.
    pub meters: Arc<crate::meter::Meters>,
    /// Per-remote link indices; inbound audio must carry the remote's pair (see
    /// `LinkIndices`).
    pub links: crate::net::LinkMap,
}

pub struct RecvDecode {
    pub engine: Arc<crate::audio::engine::AudioEngine>,
}

pub struct RecvContext {
    pub acct:   Arc<RecvAccounting>,
    pub decode: Option<Arc<RecvDecode>>,
}

/// Late-populated handle the recv thread polls per audio packet. `None` until main wires the
/// accounting plane in at boot; `decode` inside it is `Some` only when an output engine exists.
pub type RecvCtxCell = Arc<std::sync::RwLock<Option<Arc<RecvContext>>>>;


/// Recv-thread-local memo of what has already been published to the incoming-channel
/// registry, so the hot path can skip the shared write lock when nothing has changed.
///
/// The registration block below runs per audio packet — 3200/s at 8 channels x 2.5ms. Taking
/// `incoming_channels.write()` plus `incoming_seen.lock()` every time, with a String
/// allocation for each, would block against the API's /meters handler (the SAME write lock)
/// and almost always write values unchanged since the previous packet (`channel`, `active`,
/// `routed`).
///
/// Measured effect of NOT doing this: handler run time spiking to 14ms while the socket
/// backed up 48 packets deep — the receive path stalled inside our own code, not by the
/// scheduler. The socket handler should do a lookup and a dispatch, nothing resembling
/// per-packet UI bookkeeping.
#[derive(Default)]
struct RegistryCache {
    /// Keyed by peer, then indexed by channel. Deliberately NOT a `HashMap<(String, u8), _>`:
    /// a tuple key cannot be looked up from a `&str` without building the `String` first,
    /// which would put an allocation back on the very path this exists to keep clean.
    peers: std::collections::HashMap<String, Vec<Option<RegEntry>>>,
}

struct RegEntry {
    /// Last `routed` value published — a change must reach the registry (§9.3 selects the
    /// meter source from it).
    routed:    bool,
    /// When this channel's liveness was last stamped into `incoming_seen`.
    stamped:   std::time::Instant,
}

impl RegistryCache {
    /// Liveness only needs to be fresh enough for staleness detection, not per packet.
    const STAMP_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

    /// Does this packet require the shared registry to be touched at all?
    /// Allocation-free: `get(peer)` borrows the &str.
    fn needs_publish(&self, peer: &str, channel: u8, routed: bool) -> bool {
        match self.peers.get(peer).and_then(|v| v.get(channel as usize)).and_then(|e| e.as_ref()) {
            // First sight of this channel, or its routing changed — must publish.
            None => true,
            Some(e) => e.routed != routed || e.stamped.elapsed() >= Self::STAMP_EVERY,
        }
    }

    /// Allocates only on first sight of a peer (the 128-slot row) — never per packet.
    fn record(&mut self, peer: &str, channel: u8, routed: bool) {
        let row = match self.peers.get_mut(peer) {
            Some(r) => r,
            None => self.peers.entry(peer.to_string())
                        .or_insert_with(|| (0..128).map(|_| None).collect()),
        };
        if let Some(slot) = row.get_mut(channel as usize) {
            *slot = Some(RegEntry { routed, stamped: std::time::Instant::now() });
        }
    }
}

/// Packet-arrival loss and jitter accounting (CASCADE_SESSION_STATS_SPEC §2.3/§2.4).
///
/// Both specs are emphatic that these run "in the same outer packet-arrival dispatcher,
/// entirely before any routing check", and that "a channel being transmitted but never
/// routed to any output still accumulates real loss statistics the entire time".
///
/// It must not live in the per-channel decode closure: a timestamp taken there, after the
/// dispatch hop and the slot lock, reports decode scheduling latency as network jitter,
/// and an unrouted channel never reaches that closure at all.
///
/// Recv-thread-local and single-threaded, like `chan_cache`, so no locking. The per-peer
/// accumulator Arc is cached on first sight of a peer — resolving it per packet would take
/// the engine's stats map lock at the full packet rate.
#[derive(Default)]
struct ArrivalStats {
    chans: std::collections::HashMap<(String, u8), ChanArrival>,
    accs:  std::collections::HashMap<String, std::sync::Arc<crate::audio::pool::StatAccumulator>>,
}

struct ChanArrival {
    /// Arrival instant of the previous packet on this channel.
    last_arrival: std::time::Instant,
    /// Previous inter-arrival gap, in ms — jitter is the change in this (§2.4).
    last_gap_ms:  f64,
    /// Last sequence number seen, for gap analysis (§2.3).
    last_seq:     u16,
}

impl ArrivalStats {
    /// Account one arriving audio packet. Called for EVERY audio packet, routed or not.
    fn record(&mut self, engine: &crate::audio::engine::AudioEngine,
              peer: &str, channel: u8, seq: u16) {
        let now = std::time::Instant::now();

        let acc = match self.accs.get(peer) {
            Some(a) => a.clone(),
            None => {
                let a = engine.stat_acc_for(peer);
                self.accs.insert(peer.to_string(), a.clone());
                a
            }
        };

        let key = (peer.to_string(), channel);
        let (jitter_ms, lost) = match self.chans.get_mut(&key) {
            None => {
                // First packet on this channel: no previous arrival to difference against,
                // and no sequence baseline, so neither statistic is meaningful yet.
                self.chans.insert(key, ChanArrival {
                    last_arrival: now, last_gap_ms: 0.0, last_seq: seq,
                });
                (0.0, 0)
            }
            Some(st) => {
                // §2.4: jitter is the change between CONSECUTIVE inter-arrival gaps,
                // measured on the receiver's local clock only — not RFC 3550's estimate,
                // and not compared against any sender timestamp.
                //
                // Every gap counts, however long. A stream that stops and restarts — an
                // outage, a reconnect, transmit toggled off and on — reports the break as
                // one large jitter sample, which stands as the running max until the next
                // drain; the loss below counts whatever sequence gap the break left.
                let gap_ms = now.duration_since(st.last_arrival).as_secs_f64() * 1000.0;
                let jitter = (gap_ms - st.last_gap_ms).abs();
                st.last_arrival = now;
                st.last_gap_ms  = gap_ms;

                // §2.3: a forward gap of 1..=999 is that many lost packets. Larger gaps are
                // a stream reset (a sender reconnecting with a fresh counter), not loss.
                let d = seq.wrapping_sub(st.last_seq);
                let lost = if d == 0 { 0 } else {
                    let gap = (d as u32).wrapping_sub(1);
                    if (1..=999).contains(&gap) { gap as u64 } else { 0 }
                };
                st.last_seq = seq;
                (jitter, lost)
            }
        };

        acc.record(1, lost, jitter_ms);
    }
}

fn process_packet(
    data: &[u8],
    n: usize,
    from: SocketAddr,
    recv_ctx: &RecvCtxCell,
    seen_audio_peers: &mut std::collections::HashSet<String>,
    chan_cache: &mut std::collections::HashMap<(String, u8), crate::audio::engine::ChannelHandle>,
    chan_cache_epoch: &mut u64,
    chan_probe: &mut HotPathProbe,
    meter: &mut MeterCache,
    arrival: &mut ArrivalStats,
    registry: &mut RegistryCache,
    unhandled: &mut std::collections::HashSet<std::net::IpAddr>,
) -> Option<InboundPacket> {
    let ptype = parse_type(data)?;
    let ts   = parse_timestamp(data);
    let label_revision = parse_label_revision(data);
    let seq  = parse_sequence(data);   // wire bytes 2-3 (u16) — for duplicate drop

    if ptype == PacketType::Audio {
        if n < HEADER_LEN { return None }
        let hstart = payload_start(data);   // derived, not assumed (spec §2.1)
        // Flags byte (0x08): bit 7 = encrypted payload, bits 0-6 = codec
        // (CASCADE_WIRE_PROTOCOL_SPEC §2.1). The codec bits describe the PLAINTEXT
        // codec even when bit 7 is set (encrypted Opus → codec 0), so the codec
        // check below is unaffected by encryption; decryption is handled per-peer
        // in the decode plane once the sender is identified.
        let flags = parse_flags(data);
        let encrypted = flags & FLAG_ENCRYPTED != 0;
        // Codec is a first-class receive case for all three values. Raw PCM arrives on
        // opcode 11 (already an Audio alias) with a payload far larger than Opus's —
        // 1920 bytes at 16-bit, 2880 at 24-bit for a 20ms frame — which exceeds the path
        // MTU and therefore arrives IP-FRAGMENTED. That reassembly is the kernel's job and
        // `recvfrom` hands up the whole datagram, so nothing here needs to change for it;
        // MAX_PACKET (8192) has ample room. Worth knowing when reading a capture, though:
        // a raw stream looks like ~5 IP fragments per audio packet on the wire.
        let codec = flags & 0x7F;
        match codec {
            CODEC_OPUS | CODEC_RAW16 | CODEC_RAW24 => {}
            _ => return None,   // unknown codec — silently discard
        }
        // The sender's restart marker (§3.1): read here with the other header fields.
        let sender_restart = crate::net::protocol::parse_sender_restart(data);
        let channel  = parse_channel(data);
        // The protocol reserves channels 0..=127; the wire field is read as a SIGNED byte
        // and anything with the sign bit set is not a channel number. Dropped here, ahead
        // of the arrival accounting below, because a channel that cannot exist must not
        // contribute loss or jitter samples either: nothing downstream can route it (the
        // masks are 128 wide) and the channel registry's own rows are 128 long, so without
        // this the packet would be counted against a channel it then silently fails to
        // register, inflating the statistics for a channel the UI never shows.
        if channel >= 128 { return None }
        let opus_len = parse_opus_len(data) as usize;
        // Length gate (CASCADE_AUDIO_RECEIVE_SPEC §1.1): the datagram must hold EXACTLY the
        // header plus the declared payload — no more, no less — and the payload may not
        // exceed MAX_PACKET. A packet that fails it is still that remote's traffic: it is
        // counted and its channel still shows as active below, but it is never decrypted,
        // decoded or metered.
        let len_ok = n >= hstart && n - hstart == opus_len && opus_len <= MAX_PACKET;

        // ── INLINE AUDIO FAST-PATH ──
        // If the audio context is wired (post-startup), demux + handle the audio packet
        // HERE on the recv context — no blocking_send, no tokio worker wake. Falls through
        // to blocking_send only before the context is populated (startup).
        let ctx_opt = recv_ctx.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(ctx) = ctx_opt {
            // Audio packets carry no sender token → identify by source address, and the
            // header must carry that remote's link indices (`LinkIndices`); a packet that
            // does not is not treated as that remote's traffic.
            let peer = ctx.acct.by_addr.read().unwrap_or_else(|e| e.into_inner())
                .get(&from).cloned()
                .filter(|name| crate::net::link_accepts(&ctx.acct.links, name, data));
            let Some(name) = peer else {
                // Audio no configured remote claims. Logged once per sending host for the
                // life of the process — a misconfigured sender streams continuously, and
                // the first line says everything the rest would.
                if unhandled.insert(from.ip()) {
                    warn!("Unhandled audio packets received from address {}, port {}. \
                           Check stream name/password", from.ip(), from.port());
                }
                return None;
            };
            {
                // ── Accounting plane (ALWAYS — independent of any output engine) ──
                // RX byte accounting (poke_tick swaps to zero each 2s window).
                if let Some(atom) = ctx.acct.rx_atomics.read().unwrap_or_else(|e| e.into_inner()).get(&name) {
                    // Bytes on the wire: the whole datagram plus IP/UDP overhead.
                    atom.fetch_add(n as u64 + 28, std::sync::atomic::Ordering::Relaxed);
                }
                // 0x09 sample rate (spec §2.1): 1 = 48kHz, anything else = 44.1kHz.
                let is_44k = parse_sample_rate(data) != crate::net::protocol::SAMPLE_RATE_48K;
                if let Some(sr_atom) = ctx.acct.sr_atomics.read().unwrap_or_else(|e| e.into_inner()).get(&name) {
                    sr_atom.store(is_44k, std::sync::atomic::Ordering::Relaxed);
                }
                // ── Loss + jitter, at ARRIVAL (CASCADE_SESSION_STATS_SPEC §2.3/§2.4) ──
                // Deliberately here: in the packet-arrival dispatcher, before the routing
                // lookup below, so a transmitting-but-unrouted channel still accumulates
                // real statistics, and so the jitter figure reflects the network rather
                // than our own decode-dispatch scheduling.
                if let Some(dec) = ctx.decode.as_ref() {
                    arrival.record(&dec.engine, &name, channel, seq);
                }

                let is_new_peer = seen_audio_peers.insert(name.clone());
                // Whether this channel has a live decode/render channel this packet — the
                // §9.3 meter selector, published to the channel registry below.
                let mut ch_routed = false;
                // ── Decode plane (ONLY when an output engine is present) ──
                // No output device → no decode/enqueue/render, but accounting above + channel
                // registration below still run, so the UI shows the incoming rate + channels.
                if let Some(dec) = &ctx.decode {
                    // DROP non-48k audio (Cascade is 48k-only by design): still counted bytes +
                    // sr flag (above) + channel register (below), but skip decode/enqueue.
                    // A packet that failed the length gate above skips the decode plane
                    // entirely; it has already been counted and still registers below.
                    if !is_44k && len_ok {
                        // On-CPU vs off-CPU discriminator, measured on the recv context.
                        // Handling audio inline means there is no cross-thread wake, so
                        // off-CPU should be ~0. Off-CPU >> on-CPU means the recv context is
                        // still being descheduled.
                        let _rx_t0   = std::time::Instant::now();
                        let _rx_cpu0 = crate::audio::encode::thread_cpu_nanos();

                        let raw = &data[hstart..hstart + opus_len];
                        // ── Encryption gate + decrypt (CASCADE_ENCRYPTION_SPEC §7) ──
                        // The packet's encrypted flag must agree with this remote's
                        // encryption setting, in both directions:
                        // - encryption ON:  a plaintext packet is rejected; an encrypted one
                        //   is opened (combined = nonce||ct||tag, the AEAD recovers the
                        //   nonce from the first 12 bytes) and rejected if that fails
                        //   (wrong key, corruption, tampering — §7.1).
                        // - encryption OFF: an encrypted packet is rejected.
                        // A rejected packet is silent and goes no further than this: no
                        // decode, no meter. It has already been counted, and its channel
                        // still registers as active below. The opened `frame` is shared by
                        // the decode dispatch and the §9.2 pre-decode meter (one decrypt).
                        let pc = ctx.acct.crypto.read()
                            .unwrap_or_else(|e| e.into_inner()).get(&name).cloned();
                        let encrypting = pc.as_ref().is_some_and(|p| p.is_enabled());
                        let opened: Option<std::borrow::Cow<[u8]>> = match (encrypted, encrypting) {
                            (true, true)   => pc.and_then(|p| p.open(raw)).map(std::borrow::Cow::Owned),
                            (false, false) => Some(std::borrow::Cow::Borrowed(raw)),
                            _              => None,
                        };
                        if let Some(frame) = opened {
                            let frame: &[u8] = &frame;

                            // HOT PATH: dispatch via a cached per-(peer,channel) handle so the
                            // recv thread takes NO engine-map locks in steady state — the
                            // readable handler dispatches straight to the channel's own queue
                            // without a lookup. Only the
                            // first packet for a channel resolves (locked, once); render_alive is
                            // gated here so resolve_channel's precondition holds. A resolve that
                            // returns None (no playable route) is NOT cached — re-resolved next
                            // packet so a later routing change is picked up.
                            // `routed` = a real decode was dispatched → §9 meters it post-buffer,
                            // so the §9.2 meter below is skipped for this channel this packet.
                            let mut routed = false;
                            if dec.engine.render_alive() {
                                // Invalidate the handle cache if any slot teardown happened
                                // (routing/device change bumps the engine's slots_epoch). One
                                // relaxed-ish load; flush only on change.
                                let epoch = dec.engine.slots_epoch();
                                if epoch != *chan_cache_epoch {
                                    chan_cache.clear();
                                    *chan_cache_epoch = epoch;
                                }
                                let ckey = (name.clone(), channel);
                                // Track which branch ran so the probe can tell a stall on the
                                // LOCK-FREE cached path (should never happen) from a one-off
                                // resolve (cache miss: takes the engine-map locks, rare).
                                let _hit = if let Some(handle) = chan_cache.get(&ckey) {
                                    dec.engine.dispatch_decode(handle, seq, ts, codec, frame, sender_restart);
                                    chan_probe.hits += 1;
                                    chan_probe.win_hits += 1;
                                    // §9.3 selector: only claim this channel for the
                                    // post-buffer meter once that meter is actually live.
                                    routed = handle.meter_live();
                                    true
                                } else if let Some(handle) =
                                    dec.engine.resolve_channel(&name, channel)
                                {
                                    dec.engine.dispatch_decode(&handle, seq, ts, codec, frame, sender_restart);
                                    // Freshly resolved: the decode has only just been
                                    // dispatched, so the post-buffer meter cannot be live yet
                                    // and `routed` stays false for this packet. The pre-decode
                                    // meter carries the level until the first decode lands.
                                    routed = handle.meter_live();
                                    chan_cache.insert(ckey, handle);
                                    chan_probe.misses += 1;
                                    chan_probe.win_misses += 1;
                                    false
                                } else {
                                    // No playable route — nothing dispatched, not cached.
                                    false
                                };

                                // Hot-path probe. Splits wall time into on-CPU vs off-CPU so a
                                // remaining scheduler stall still shows as DESCHEDULED. The
                                // cached path takes NO engine-map locks, so a >1ms cached-path
                                // sample means a genuine OS deschedule, not lock contention.
                                // Diagnostic only:
                                // logged at debug level (silent at default INFO). Raise the log
                                // level (RUST_LOG=cascade::net::udp=debug) to see it.
                                let _rx_ms = _rx_t0.elapsed().as_secs_f64() * 1000.0;
                                chan_probe.note(_rx_ms);
                                // Lower threshold on the cached path (1ms) — it should be sub-µs;
                                // keep 3ms on the miss path (a resolve legitimately locks maps).
                                let thresh = if _hit { 1.0 } else { 3.0 };
                                if _rx_ms > thresh {
                                    let _cpu_ms = crate::audio::encode::thread_cpu_nanos()
                                        .saturating_sub(_rx_cpu0) as f64 / 1_000_000.0;
                                    let _off = _rx_ms - _cpu_ms;
                                    debug!("recv hot-path {:.2}ms (ch {}, {}) | on-CPU {:.2}ms, off-CPU {:.2}ms → {}",
                                        _rx_ms, channel,
                                        if _hit { "cache-hit" } else { "resolve" },
                                        _cpu_ms, _off,
                                        if _off > _cpu_ms { "DESCHEDULED" } else { "ON-CPU WORK" });
                                }
                                // Periodic hit-rate/worst summary (every ~2000 packets). Debug only.
                                if let Some((hits, misses, worst)) = chan_probe.tick() {
                                    debug!(
                                        "recv hot-path window: {} hits, {} misses, worst {:.2}ms | \
                                         lifetime {} hits / {} misses, worst {:.2}ms \
                                         (cached path lock-free — worst should stay sub-ms)",
                                        hits, misses, worst,
                                        chan_probe.hits, chan_probe.misses, chan_probe.worst_ms);
                                }
                            }
                            ch_routed = routed;
                            // ── §9.2 pre-decode meter (CASCADE_AUDIO_RECEIVE_SPEC §9.2/§9.3) ──
                            // Only for a channel with no live post-buffer meter, and only while
                            // its remote is being watched (crate::meter). The throwaway decode
                            // runs on the meter worker, never here: this thread's cost is one
                            // atomic read, plus a copy into the worker's queue when metered.
                            if !routed {
                                let h = meter.get(&ctx.acct.meters, &name);
                                if ctx.acct.meters.watch.is_live(&h.deadline) {
                                    ctx.acct.meters.submit(crate::meter::MeterJob {
                                        peer: Arc::clone(&h.peer), channel, codec,
                                        frame: frame.to_vec(), cells: Arc::clone(&h.cells),
                                    });
                                }
                            }
                        }
                    }
                    // First audio from this peer: apply its configured phase lock. Outside
                    // the gates above, so it holds whatever that first packet looked like.
                    if is_new_peer
                        && ctx.acct.phase_lock_peers.read().unwrap_or_else(|e| e.into_inner()).contains(&name) {
                        dec.engine.set_phase_lock(&name, true);
                    }
                    // NOTE: no teardown on the 48k→44.1k transition. The decode object is
                    // kept WARM and simply not fed non-48k frames (the decode is skipped
                    // above via `if !is_44k`). Tearing the render group down here
                    // would discard the warm jitter/phase state. The
                    // sr_mismatch flag (set in accounting above) still drives the UI "44.1k"
                    // badge, so the operator still sees the wrong-rate condition.
                }
                // ── Channel registration (UI incoming list, independent of decode) ──
                // Gated: the shared locks are taken only when this packet actually changes
                // something — first sight, a routing change, or the periodic liveness stamp.
                // See RegistryCache for why.
                if registry.needs_publish(&name, channel, ch_routed) {
                    let mut all_chans = ctx.acct.incoming_channels.write().unwrap_or_else(|e| e.into_inner());
                    let chans = all_chans.entry(name.clone()).or_insert_with(Vec::new);
                    let ch = channel as usize;
                    if chans.len() <= ch { chans.resize(ch + 1, crate::api::ChannelInfo::default()); }
                    chans[ch].channel = channel;
                    if chans[ch].label.is_empty() {
                        chans[ch].label = format!("Ch {}", channel + 1);
                    }
                    chans[ch].active = true;
                    chans[ch].routed = ch_routed;   // §9.3 meter selector
                    ctx.acct.incoming_seen.lock().unwrap_or_else(|e| e.into_inner())
                        .insert((name.clone(), channel), std::time::Instant::now());
                    drop(all_chans);
                    registry.record(&name, channel, ch_routed);
                }
            }
            // Handled inline — do NOT forward.
            return None;
        }

        // Context not yet wired (startup): forward to the async path.
        if !len_ok { return None }
        return Some(InboundPacket {
            from, ptype, ts, label_revision,
            raw: Vec::new(),   // audio path never builds a pong
            sender_token: Token::zero(),
            label_meta: 0,
            payload: data[hstart..hstart + opus_len].to_vec(),
        });
    }

    // ── Control/label/reconfig — forward to the async select loop ──
    let hstart = payload_start(data);   // derived, not assumed (spec §2.1)
    if n < hstart + TOKEN_LEN { return None }
    // 53-byte POKE = ordinary. 87-byte POKE = key-exchange extension appended
    // (CASCADE_ENCRYPTION_SPEC §3.2: byte 0x35 = 0, byte 0x36 = 32, sender's X25519
    // public key at 0x37-0x56). Both sizes carry the indicator at 0x0A and the two
    // identity hashes at 0x15/0x25 in the same positions. Both are forwarded whole; the
    // control dispatch drops a poke whose length disagrees with its remote's encryption
    // setting, and the peer task ingests the extension of the rest (§3.2).
    let sender_token = parse_sender_token(data).unwrap();
    let payload      = data[hstart + TOKEN_LEN..].to_vec();
    let label_meta   = if ptype == PacketType::Label {
        ((data[15] as u16) << 8) | (data[16] as u16)
    } else if ptype == PacketType::Poke {
        // Label-change indicator at byte 0x0A (CASCADE_WIRE_PROTOCOL_SPEC §2.1).
        data.get(0x0A).copied().unwrap_or(0) as u16
    } else { 0 };
    Some(InboundPacket {
        from, ptype, ts, label_revision,
        raw: data[..n].to_vec(),   // pong is built from the ping's own buffer (§2.1)
        sender_token, label_meta, payload,
    })
}

/// The audio socket, swappable in place.
///
/// Every holder keeps this cell rather than a socket, and loads at the point of use, so a
/// rebind is one store and no holder has to be told. The receive thread notices on its next
/// loop iteration; senders on their next send.
pub type SharedSocket = Arc<arc_swap::ArcSwap<std::net::UdpSocket>>;

/// Whether the receive and control-send paths are currently in a failing state.
///
/// Both log the TRANSITION into failure and back out of it, never the individual attempt:
/// a socket in a persistent error state returns immediately and an unreachable peer fails
/// every ping, so per-attempt logging buries everything else.
static RECV_FAILING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SEND_FAILING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Pause between receive attempts while the socket keeps failing. A socket stuck in an error
/// state returns immediately, so without it the receive thread spins a whole core at its
/// elevated priority. Only a repeat failure waits; a single transient error costs nothing.
const RECV_RETRY: std::time::Duration = std::time::Duration::from_millis(10);

pub struct UdpEngine {
    socket: SharedSocket,
}

/// Move the audio socket to a new bind address without restarting.
///
/// PORT AND INTERFACE ARE ONE ADDRESS: they are bound together, and a change to either
/// rebinds the socket in place rather than restarting anything. The setup is re-entrant for
/// that reason — it runs again on an already-bound socket.
///
/// NOTHING AUDIO-SIDE IS TOUCHED. Peers, decoders, jitter buffers and routing all survive:
/// the receive path re-arms its prebuffer on a genuine drain (`depth == 0`,
/// CASCADE_AUDIO_RECEIVE_SPEC §4.2) and has no socket-keyed trigger, so tearing state down
/// here would throw away warm buffers for no reason. The only exposure is TIME — if reads
/// stop for longer than a peer's buffer depth it drains and refills audibly — which is why
/// the old socket is woken explicitly rather than left to its receive timeout.
///
/// Two cases, and the difference is whether the port moves:
///
///   * PORT CHANGES — the new address is free, so bind it first and swap. A failed bind
///     leaves the old socket serving and the error is reportable.
///   * PORT IS THE SAME, INTERFACE MOVES — the old socket owns the port, so it must go
///     first. A throwaway socket is parked in the cell so holders always see a valid socket,
///     the old one is released, then the real socket is bound and swapped in. If that bind
///     fails the old address is restored, so a vanished interface cannot leave the daemon
///     with no socket at all.
///
/// Callers log the move in their own terms — a port change, an interface fallback, the
/// startup probe taking its port — so this logs only what they cannot see: failing to
/// reclaim the old address.
pub fn rebind(cell: &SharedSocket, addr: SocketAddr) -> std::io::Result<()> {
    let old = cell.load_full();
    let old_addr = old.local_addr()?;
    if old_addr == addr {
        return Ok(());
    }

    let open = |a: SocketAddr| -> std::io::Result<std::net::UdpSocket> {
        let s = std::net::UdpSocket::bind(a)?;
        s.set_nonblocking(false)?;
        disable_udp_conn_reset(&s);
        let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(100)));
        Ok(s)
    };

    if old_addr.port() != addr.port() {
        let fresh = open(addr)?;                    // free port — bind before releasing
        cell.store(Arc::new(fresh));
        wake(&old_addr);                            // return the blocked recv_from now
        drop(old);
        tracing::debug!("audio socket rebound {} → {}", old_addr, addr);
        return Ok(());
    }

    // Same port: the old socket has to be released before the new one can take it.
    let parked = open(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    cell.store(Arc::new(parked));
    wake(&old_addr);
    release(old);

    match open(addr) {
        Ok(fresh) => {
            cell.store(Arc::new(fresh));
            tracing::debug!("audio socket rebound {} → {}", old_addr, addr);
            Ok(())
        }
        Err(e) => {
            // Put the daemon back where it was rather than leaving it parked on loopback.
            match open(old_addr) {
                Ok(back) => {
                    cell.store(Arc::new(back));
                    tracing::debug!("rebind to {addr} failed ({e}) — stayed on {old_addr}");
                }
                Err(e2) => {
                    tracing::error!("rebind to {addr} failed ({e}) AND {old_addr} could not be \
                                     reclaimed ({e2}) — no audio socket until the next attempt");
                }
            }
            Err(e)
        }
    }
}

/// Return a socket's blocked `recv_from` immediately, instead of waiting out its receive
/// timeout.
///
/// The datagram goes to the address the socket is ACTUALLY bound to. A wildcard bind is
/// reachable on loopback, but a socket bound to one interface only receives datagrams
/// addressed to that interface — so a loopback wake would never reach it, and the rebind
/// would silently fall back to the receive timeout on exactly the interface-change case.
///
/// Best-effort by design: if it does not land the receive thread still wakes on its own
/// timeout, so the rebind is slower but never wrong.
fn wake(addr: &SocketAddr) {
    let ip = if addr.ip().is_unspecified() {
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    } else {
        addr.ip()
    };
    let to = SocketAddr::new(ip, addr.port());
    // The sender binds the wildcard: a loopback-bound sender cannot route to a
    // non-loopback address.
    if let Ok(s) = std::net::UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))) {
        let _ = s.send_to(&[0u8; 1], to);
    }
}

/// Drop the previous socket, waiting for the receive thread to let go of it first.
///
/// The receive thread loads the cell once per iteration and holds that reference across its
/// blocking call, so the socket does not actually close — and the port does not free — until
/// that reference is gone. Polling the strong count is what makes this observable rather
/// than a guessed sleep. The deadline is a backstop: if the count never falls, dropping
/// anyway is no worse than not trying.
fn release(old: Arc<std::net::UdpSocket>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    while Arc::strong_count(&old) > 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    drop(old);
}

/// Wrap a freshly bound socket, applying the settings every audio socket needs.
///
/// The RECEIVE TIMEOUT is set here rather than at the receive thread, and on every platform
/// rather than Windows alone, because a socket created by a later rebind needs it just as
/// much as the one bound at startup. It bounds how long the receive thread can sit in the
/// kernel: on shutdown it wakes and sees `lifecycle::is_stopping`, and on a rebind it wakes
/// and reloads this cell. Windows additionally requires it — a thread killed inside
/// `recvfrom` leaves the endpoint registered to the dead pid, which a successor cannot
/// rebind (WSAEACCES, not merely "in use").
///
/// It costs nothing in operation: packets arrive every 2.5-20 ms, so the timeout fires only
/// on an idle socket, where the wake is a flag check and a loop.
fn engine_from(socket: std::net::UdpSocket) -> UdpEngine {
    let _ = socket.set_read_timeout(Some(std::time::Duration::from_millis(100)));
    UdpEngine { socket: Arc::new(arc_swap::ArcSwap::from_pointee(socket)) }
}

/// Windows: stop ICMP port-unreachable from surfacing as a receive error.
///
/// A UDP socket is connectionless, but Windows still tracks the ICMP "port unreachable"
/// that comes back when a datagram is sent to a host that is not listening, and reports it
/// on the NEXT `recv_from` as WSAECONNRESET (10054) — an error about a previous SEND,
/// delivered to a RECEIVE, from an unrelated peer. Every poke to an absent remote therefore
/// costs one failed receive.
///
/// `SIO_UDP_CONNRESET` = false is the standard remedy for a UDP server: the socket stops
/// reporting these, and recv_from returns only real datagrams. Nothing is hidden that
/// Cascade acts on — an unreachable peer is detected by its pokes going unanswered
/// (CASCADE_WIRE_PROTOCOL_SPEC §4), never by this error.
///
/// No-op everywhere else; BSD and Linux never had the behaviour.
#[cfg(windows)]
fn disable_udp_conn_reset(socket: &std::net::UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{WSAIoctl, SIO_UDP_CONNRESET, SOCKET};

    let mut enable: u32 = 0;   // FALSE — stop reporting connection resets
    let mut returned: u32 = 0;
    // SAFETY: a well-formed WSAIoctl on a socket this function owns a borrow of. The input
    // buffer is a u32 that outlives the call; no output buffer is requested.
    let rc = unsafe {
        WSAIoctl(
            SOCKET(socket.as_raw_socket() as usize),
            SIO_UDP_CONNRESET,
            Some(&mut enable as *mut u32 as *mut std::ffi::c_void),
            std::mem::size_of::<u32>() as u32,
            None,
            0,
            &mut returned,
            None,
            None,
        )
    };
    if rc != 0 {
        tracing::debug!("SIO_UDP_CONNRESET not applied — recv_from may report 10054 for \
                         datagrams sent to an absent peer");
    }
}

#[cfg(not(windows))]
fn disable_udp_conn_reset(_socket: &std::net::UdpSocket) {}

impl UdpEngine {
    /// Bind the audio socket to `addr` as a blocking std::net::UdpSocket, exactly as std does
    /// it, on every platform. (Not tokio's UdpSocket: its readiness polling cost kevent/epoll
    /// work proportional to packet rate.)
    pub fn bind(addr: SocketAddr) -> Result<Self> {
        let socket = std::net::UdpSocket::bind(addr)?;
        socket.set_nonblocking(false)?;  // blocking mode — OS wakes us per packet
        disable_udp_conn_reset(&socket);
        tracing::info!("UDP engine bound to {}", addr);
        Ok(engine_from(socket))
    }

    /// Start receive and control-send threads.
    ///
    /// Returns:
    ///   inbound_rx  — incoming parsed packets (tokio mpsc, compatible with
    ///                 existing async main loop via .recv().await)
    ///   outbound_tx — channel for control packets (POKE, RSP, labels, ACKs)
    ///   socket      — the shared socket cell, for direct audio sends from the encode
    ///                 workers (no tokio wrapper overhead) and for in-place rebinds
    ///   recv_ctx    — the cell main populates to have audio handled on the receive thread
    pub fn start(self) -> (mpsc::Receiver<InboundPacket>, mpsc::Sender<OutboundPacket>, SharedSocket, RecvCtxCell) {
        let (inbound_tx, inbound_rx)       = mpsc::channel::<InboundPacket>(1024);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<OutboundPacket>(1024);

        // Late-populated audio context (see RecvContext). main fills this once the
        // engine + peer maps exist; until then audio falls through to blocking_send.
        let recv_ctx: RecvCtxCell = Arc::new(std::sync::RwLock::new(None));
        let recv_ctx_thread = Arc::clone(&recv_ctx);

        let recv_sock  = Arc::clone(&self.socket);

        // The receive timeout that bounds this thread's time in the kernel is set on the
        // socket itself by `engine_from`, so a socket installed by a later rebind carries it
        // too.
        let send_sock  = Arc::clone(&self.socket);
        let audio_sock = Arc::clone(&self.socket);

        // ── Receiver: dedicated blocking-recvfrom thread on every platform ──
        //
        // Not a GCD dispatch_source: a serial GCD queue does not own a thread — it borrows
        // one from the shared pool the per-channel decode and encode queues draw from — so
        // its handler can wait 8-100ms for a thread while packets that arrived on time sit
        // unread in the socket buffer.
        //
        // A dedicated thread always has one. QoS stays USER_INITIATED (0x19), never
        // USER_INTERACTIVE — this changes thread OWNERSHIP, not priority.
        {
            // Dedicated blocking-recvfrom thread (Linux has no native dispatch_source;
            // could move to epoll later). Runs the SAME process_packet.
            std::thread::Builder::new()
                .name("cascade-recv".into())
                .spawn(move || {
                    // Raise this thread into the audio-adjacent priority class:
                    // USER_INITIATED QoS (0x19) on macOS, nice -10 on Linux. The
                    // scheduler's decode workers take the same class, so this thread and
                    // decode are peers and neither preempts the other.
                    crate::audio::encode::set_qos_user_initiated();
                    let mut buf = vec![0u8; MAX_PACKET];
                    // Recv-thread-local audio state (first-packet phase-lock one-shot;
                    // 48k→44k edge teardown).
                    let mut seen_audio_peers: std::collections::HashSet<String> = std::collections::HashSet::new();
                    let mut chan_cache: std::collections::HashMap<(String, u8), crate::audio::engine::ChannelHandle> =
                        std::collections::HashMap::new();
                    let mut chan_cache_epoch: u64 = 0;
                    let mut chan_probe = HotPathProbe::new();
                    let mut meter = MeterCache::default();
                    let mut arrival = ArrivalStats::default();
                    let mut registry = RegistryCache::default();
                    // Hosts already reported for sending audio no remote claims.
                    let mut unhandled: std::collections::HashSet<std::net::IpAddr> =
                        std::collections::HashSet::new();
                    let mut wait_probe = WaitProbe::new();
                    loop {
                        // Checked EVERY iteration, not only when recv_from errors.
                        //
                        // The receive timeout below only fires on an IDLE socket. With audio
                        // arriving, recv_from returns Ok on every call, the error branch is
                        // never reached, and a stop check hidden there is never evaluated —
                        // so this thread would still be inside a blocking kernel recvfrom
                        // when ExitProcess terminated it, which is precisely what orphans the
                        // endpoint and blocks the successor from binding the port.
                        //
                        // In other words the fault only appeared while connected, which is
                        // the only state that matters.
                        if crate::lifecycle::is_stopping() { break; }

                        // Timed around the BLOCKING call, deliberately: every other probe on
                        // this path starts after it returns. See `WaitProbe`.
                        let _w0 = std::time::Instant::now();
                        let (n, from) = match recv_sock.load().recv_from(&mut buf) {
                            Ok(x) => {
                                if RECV_FAILING.swap(false, std::sync::atomic::Ordering::Relaxed) {
                                    tracing::info!("recv_from recovered");
                                }
                                x
                            }
                            Err(e) => {
                                // Shutting down: leave quietly rather than spinning on the
                                // error and filling the log on the way out. Dropping this
                                // thread's descriptor here is what stops the endpoint being
                                // orphaned — see the receive timeout in start().
                                if crate::lifecycle::is_stopping() { break; }
                                // An idle-socket timeout is not a fault; it is the
                                // mechanism that makes the check above reachable.
                                if matches!(e.kind(), std::io::ErrorKind::TimedOut
                                                    | std::io::ErrorKind::WouldBlock) {
                                    continue;
                                }
                                // TRANSITION, not attempt: a socket stuck in an error
                                // state returns immediately, so logging each one would
                                // fill the log as fast as the loop can spin. Say it once
                                // when it starts, and once when it clears.
                                if !RECV_FAILING.swap(true, std::sync::atomic::Ordering::Relaxed) {
                                    warn!("recv_from: {}", e);
                                } else {
                                    // Still failing: pace the retries (see RECV_RETRY). A
                                    // rebind swaps a working socket into the cell, and the
                                    // next attempt picks it up.
                                    std::thread::sleep(RECV_RETRY);
                                }
                                continue;
                            }
                        };
                        let _waited_ms = _w0.elapsed().as_secs_f64() * 1000.0;
                        if let Some((w, burst)) = wait_probe.note(_waited_ms) {
                            debug!("recv wait {:.2}ms then {} packet(s) back-to-back — \
                                    {} (a frame is ~20ms; a long wait followed by a \
                                    double-length burst is the socket queueing while we \
                                    were not reading)",
                                   w, burst,
                                   if burst > 1 { "DRAINED A BACKLOG" } else { "single" });
                        }
                        if let Some((bursts, longest, worst)) = wait_probe.tick() {
                            debug!("recv wait window: {} waits, longest burst {} packets, \
                                    worst wait {:.2}ms | lifetime worst {:.2}ms",
                                   bursts, longest, worst, wait_probe.worst_ms);
                        }
                        if let Some(pkt) = process_packet(
                            &buf[..n], n, from, &recv_ctx_thread,
                            &mut seen_audio_peers,
                            &mut chan_cache,
                            &mut chan_cache_epoch,
                            &mut chan_probe,
                            &mut meter,
                            &mut arrival,
                            &mut registry,
                            &mut unhandled,
                        ) {
                            if inbound_tx.blocking_send(pkt).is_err() { break; }
                        }
                    }
                    tracing::debug!("cascade-recv: exiting");
                })
                .expect("Failed to spawn recv thread");
        }

        // ── Control send thread — low rate (~1 poke/2s), blocking sends ───────
        // Uses blocking_recv() — no embedded tokio runtime needed.
        std::thread::Builder::new()
            .name("cascade-ctrl-send".into())
            .spawn(move || {
                // A configured-but-offline remote would otherwise spam the log on
                // every periodic poke. Log the expected "peer down/unreachable"
                // errno set once per destination at debug, then suppress repeats.
                //
                // NOTE: for UDP, send_to returning Ok does NOT mean the peer is
                // reachable — it only means the datagram entered the local send
                // buffer. EHOSTDOWN/EHOSTUNREACH surface intermittently from a
                // prior failed ARP/ICMP, so Ok and Err alternate for a dead host.
                // Reachability is therefore NOT inferred here; net::peer is the
                // source of truth for CONNECTED/disconnected (from received poke
                // responses), and logs those transitions itself.
                let mut logged_down: std::collections::HashSet<SocketAddr> =
                    std::collections::HashSet::new();
                while let Some(pkt) = outbound_rx.blocking_recv() {
                    // Same reason as the audio send path and the receive loop: a thread
                    // killed inside a socket call orphans the endpoint on Windows. Stop
                    // sending as soon as shutdown is requested, and leave.
                    if crate::lifecycle::is_stopping() { break; }
                    if let Err(e) = send_sock.load().send_to(&pkt.data, pkt.to) {
                        let expected = is_peer_offline(e.raw_os_error());
                        if expected {
                            if logged_down.insert(pkt.to) {
                                tracing::debug!(
                                    "ctrl send_to {}: {} (peer offline; suppressing repeats)",
                                    pkt.to, e);
                            }
                        } else if !SEND_FAILING.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            // TRANSITION, not attempt: an unreachable peer fails every
                            // ping, which would be a line every second or two, forever.
                            warn!("ctrl send_to {}: {}", pkt.to, e);
                        }
                    } else {
                        logged_down.remove(&pkt.to);
                        if SEND_FAILING.swap(false, std::sync::atomic::Ordering::Relaxed) {
                            tracing::info!("ctrl send recovered");
                        }
                    }
                }
                tracing::debug!("cascade-ctrl-send: exiting");
            })
            .expect("Failed to spawn ctrl-send thread");

        (inbound_rx, outbound_tx, audio_sock, recv_ctx)
    }
}

/// Is this `send_to` failure just "the peer is not reachable right now"?
///
/// These are the ordinary consequences of a peer being powered off or unplugged, not
/// faults: the send loop logs the first one per address and suppresses repeats. The two
/// families are genuinely different numbers rather than the same constants under another
/// name — Winsock's are in the 10000 range and are not exposed by `libc` on every Windows
/// target, so they are named here.
#[cfg(unix)]
fn is_peer_offline(err: Option<i32>) -> bool {
    matches!(
        err,
        Some(libc::EHOSTDOWN)    | Some(libc::EHOSTUNREACH)
      | Some(libc::ECONNREFUSED) | Some(libc::ENETUNREACH)
      | Some(libc::ENETDOWN)     | Some(libc::ETIMEDOUT)
    )
}

/// Winsock equivalents of the set above.
#[cfg(windows)]
fn is_peer_offline(err: Option<i32>) -> bool {
    const WSAENETDOWN:     i32 = 10050;
    const WSAENETUNREACH:  i32 = 10051;
    const WSAECONNREFUSED: i32 = 10061;
    const WSAETIMEDOUT:    i32 = 10060;
    const WSAEHOSTDOWN:    i32 = 10064;
    const WSAEHOSTUNREACH: i32 = 10065;
    matches!(
        err,
        Some(WSAEHOSTDOWN)    | Some(WSAEHOSTUNREACH)
      | Some(WSAECONNREFUSED) | Some(WSAENETUNREACH)
      | Some(WSAENETDOWN)     | Some(WSAETIMEDOUT)
    )
}

#[cfg(test)]
mod rebind_tests {
    use super::*;

    fn cell_on(addr: &str) -> SharedSocket {
        let s = std::net::UdpSocket::bind(addr).unwrap();
        let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(100)));
        Arc::new(arc_swap::ArcSwap::from_pointee(s))
    }

    /// Receive the next datagram that is not a rebind wake.
    ///
    /// `rebind` wakes the old socket with a single zero byte. In the same-port case that
    /// datagram can be delivered AFTER the old socket has closed and the new one has taken
    /// the port, so the new socket may see it first — measured at roughly one run in fifteen.
    /// The daemon's receive path discards it before parsing (`process_packet` rejects
    /// anything shorter than `HEADER_LEN`), so it is harmless there; the tests skip it here.
    fn recv_one(cell: &SharedSocket) -> Vec<u8> {
        let mut buf = [0u8; 64];
        loop {
            let (n, _) = cell.load().recv_from(&mut buf).unwrap();
            if buf[..n] != [0u8] {
                return buf[..n].to_vec();
            }
        }
    }

    /// A port move binds the new address before releasing the old, and the socket that lands
    /// in the cell is a working one.
    #[test]
    fn port_change_moves_the_socket_and_it_receives() {
        let cell = cell_on("127.0.0.1:0");
        let before = cell.load().local_addr().unwrap();
        let free = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();

        rebind(&cell, free).unwrap();

        let after = cell.load().local_addr().unwrap();
        assert_eq!(after, free, "cell holds the new address");
        assert_ne!(after, before);

        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.send_to(b"hello", after).unwrap();
        assert_eq!(recv_one(&cell), b"hello");
    }

    /// Keeping the port means the old socket must be released before the new one can take
    /// it — the case that needs the park-and-release sequence rather than bind-then-swap.
    #[test]
    fn same_port_interface_change_releases_and_retakes_the_port() {
        let cell = cell_on("127.0.0.1:0");
        let port = cell.load().local_addr().unwrap().port();
        let target = SocketAddr::from(([0, 0, 0, 0], port));

        rebind(&cell, target).unwrap();

        assert_eq!(cell.load().local_addr().unwrap(), target, "same port, new interface");
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.send_to(b"x", SocketAddr::from(([127, 0, 0, 1], port))).unwrap();
        assert_eq!(recv_one(&cell), b"x");
    }

    /// A rebind that cannot bind must leave a working socket behind, not a parked one — the
    /// interface-vanished case.
    #[test]
    fn failed_rebind_restores_the_old_address() {
        let cell = cell_on("127.0.0.1:0");
        let before = cell.load().local_addr().unwrap();
        let unbindable = SocketAddr::from(([192, 0, 2, 1], before.port())); // TEST-NET-1

        assert!(rebind(&cell, unbindable).is_err());
        assert_eq!(cell.load().local_addr().unwrap(), before, "back on the old address");

        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.send_to(b"still here", before).unwrap();
        assert_eq!(recv_one(&cell), b"still here");
    }
}
