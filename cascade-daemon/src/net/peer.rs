/// Per-peer state machine.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::collections::HashMap as StdHashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{debug, info, warn};
use serde::{Deserialize, Serialize};

use super::protocol::*;

/// Disconnect after 20s without a POKE_RSP. Distinct from the 10s DNS-resolution retry
/// and the 5s label-resend gate below — the three are easily conflated.
const TIMEOUT: Duration = Duration::from_secs(20);
const POKE_INTERVAL: Duration = Duration::from_millis(2000);
/// Label re-request interval: re-ask a peer for labels when an outstanding request has
/// been pending longer than this. It is what gives LABEL loss recovery.
const LABEL_REQUEST_RETRY: Duration = Duration::from_secs(5);

/// Minimum spacing between DNS re-resolution attempts for a single peer. The resolve
/// TRIGGER is the peer being silent for ≥ TIMEOUT; this is the floor that stops a
/// flapping link from resolving faster than once per 10s.
const RESOLVE_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// How often an unresolved-DNS banner event is re-broadcast. The re-fire exists only
/// so a browser that connects after the initial failure still sees the banner; it does
/// not need to repeat on every ~2s poke tick. 15s gives a late browser the banner
/// promptly without flooding the event channel.
const DNS_ERROR_REFIRE: Duration = Duration::from_secs(15);

/// Re-fire interval for the incoming-sample-rate-mismatch banner — same rationale as
/// DNS_ERROR_REFIRE: a late-connecting browser gets the banner within this window without
/// the event channel being flooded on every poke tick.
const SR_MISMATCH_REFIRE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState { Idle, Connecting, Connected }
// The label JSON is fragmented into 500-byte chunks, last segment = remainder, with the
// segment index and count at packet bytes 0x0F/0x10. Receivers read the segment length
// as a LE16 at bytes 0x13-0x14 and derive the payload length from it, so any chunk size
// up to the datagram cap is safe — provided that length field is written correctly. A
// byte-wrapped length here makes receivers compute a negative payload length and abort,
// which is why build_label writes a full LE16 (CASCADE_WIRE_PROTOCOL_SPEC §2.1).
const LABEL_FRAG_SIZE: usize  = 500;

/// The most entries a label push carries — one per channel slot.
pub const LABEL_SLOTS: u32 = 128;

/// How many entries every label push carries (CASCADE_WIRE_PROTOCOL_SPEC §3.3), fixed at
/// each label reload — the same points that advance the label revision: one per input
/// channel, up to LABEL_SLOTS, when an input was running at that reload; LABEL_SLOTS when
/// none was. Held until the next reload, so an input that stops later does not change it.
/// The TX routing grid is bounded by the same input channel count, so no routed slot falls
/// outside it.
pub static LABEL_ENTRIES: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(LABEL_SLOTS);


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelLabel {
    #[serde(rename = "c")] pub channel: u32,
    #[serde(rename = "l")] pub label: String,
}

/// Maximum stored length of a channel label, in characters (Unicode scalar values).
/// The protocol imposes no label-length limit, so a peer (or a local edit) can supply an
/// arbitrarily long string. Broadcast channel names are short ("Presenter 1", "Crowd L"),
/// so 64 is far more than any real label needs while keeping LABEL fragments, the
/// persisted config, and the UI bounded. A local robustness choice, not a wire-format
/// change: a capped label is still a valid label string.
pub const MAX_LABEL_CHARS: usize = 64;

/// Truncate a label to MAX_LABEL_CHARS on a character boundary (never splitting a
/// multi-byte UTF-8 sequence). Returns the input unchanged when already within bounds.
pub fn cap_label(s: &str) -> String {
    if s.chars().count() <= MAX_LABEL_CHARS {
        s.to_string()
    } else {
        s.chars().take(MAX_LABEL_CHARS).collect()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PeerStats {
    pub state:         String,
    /// Numeric connection status (CASCADE_SESSION_STATS_SPEC §1): 0 = down /
    /// disconnected (incl. connecting), 1 = connected with address mismatch,
    /// 2 = connected clean. Derived alongside `state`/`host_match`, which the
    /// web UI keys off; this is the spec-shaped value for machine consumers.
    pub status:        u8,
    pub latency_ms:    f64,
    pub tx_bps:        u64,
    pub rx_bps:        u64,
    /// Raw lost-packet count for the last stats window (spec §2.3 loss_count).
    pub loss_count:    u64,
    pub pct_lost:      f64,
    pub jitter_ms:     f64,
    pub phase_lock:    bool,
    /// True when this peer is Connected AND its learned source IP matches the
    /// resolved configured-host IP (port ignored), OR when there is no configured
    /// host to match against (passive remote). False when Connected but the source
    /// IP differs from the configured host — the amber/"connected on an unexpected
    /// address" case the monitor pill renders orange. Only meaningful while
    /// state=="connected"; the UI keys off it solely in that state.
    pub host_match:    bool,
    pub remote_token:  String,
    pub remote_labels: Vec<ChannelLabel>,
    /// Current unresolved-DNS error for this peer (None when resolving OK or connected).
    /// Published here so a newly-connected web client sees it in the on-connect state
    /// snapshot immediately, rather than waiting up to DNS_ERROR_REFIRE for the next
    /// peer_error WS re-fire — and so the UI can take the banner down when it clears.
    pub dns_error:     Option<String>,
    /// True when the most recent incoming AUDIO packet from this peer advertised a
    /// non-48k capture rate (byte 9 != 1 ⇒ 44.1k). Published in the stats snapshot so a
    /// newly-opened/refreshed web client re-derives the banner + 44.1k badge from current
    /// state, not from a one-shot event. Cleared on a clean 48k packet or on disconnect.
    pub sr_mismatch:   bool,
    /// Encryption enabled for this remote (the user setting).
    pub encryption:    bool,
    /// Encryption ACTIVE — enabled AND the X25519 handshake has completed, so audio
    /// is actually being sealed (CASCADE_ENCRYPTION_SPEC). Drives the UI lock badge.
    pub enc_active:    bool,
    /// Which peer task published this — unique per task, so a report from a task that has
    /// since been replaced under the same name (a re-add) can be told from the live one.
    #[serde(skip)]
    pub instance:      u64,
    /// The task's LAST report, sent as it shuts down. Everything it published about the peer
    /// is withdrawn, provided no newer task has taken the name over in the meantime.
    #[serde(skip)]
    pub ended:         bool,
}

/// Source of `PeerStats::instance`. Starts at 1 so the default of 0 never matches a task.
static NEXT_INSTANCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Clone)]
pub struct PeerConfig {
    pub name:         String,
    pub remote_name:  String,
    /// Raw hostname or IP string from config — preserved for DNS re-resolution on
    /// each reconnect cycle. May be empty for passive (receive-only) remotes.
    pub host:         String,
    pub port:         u16,
    pub remote_addr:  Option<SocketAddr>,
    pub our_token:    Token,
    pub password:     String,
    pub learned_addrs: Arc<RwLock<StdHashMap<String, SocketAddr>>>,
    /// Fired when this peer's learned address CHANGES (connect or NAT rebind) so the
    /// send path rebuilds its per-channel destination cache event-driven — replaces the
    /// old address-polling thread. try_send, never blocks.
    pub rebuild_tx: Option<tokio::sync::mpsc::Sender<()>>,
    pub rx_bytes_atomic: Arc<std::sync::atomic::AtomicU64>,
    /// TX byte counter — written by encode path per packet sent, read+reset by poke_tick.
    pub tx_bytes_atomic: Arc<std::sync::atomic::AtomicU64>,
    /// Incoming-sample-rate state: written by main.rs's audio receive loop on EVERY audio
    /// packet from this peer — `true` if byte 9 != 1 (44.1k advertised), `false` for 48k.
    /// The peer task reads it each poke tick to drive the sr_mismatch banner/badge. Using
    /// an atomic (not a command per packet) avoids flooding the command channel at the
    /// audio packet rate, matching the rx/tx byte-atomic pattern.
    pub sr_44k_atomic: Arc<std::sync::atomic::AtomicBool>,
    /// Stat accumulator handle — shared with AudioEngine decode path.
    /// Peer task's poke_tick drains this directly to get jitter+loss without
    /// any channel hop. Send+Sync (no raw pointers, unlike Arc<AudioEngine>).
    pub stat_acc: Option<Arc<std::sync::Mutex<std::collections::HashMap<
        String, Arc<crate::audio::pool::StatAccumulator>>>>>,
    /// Global label revision — one value for the whole app, sent to ALL peers in every
    /// POKE. Advanced by main.rs's `reload_labels` at every label reload: twice at launch,
    /// on label and routing edits, and on every audio restart. NEVER reset on disconnect —
    /// only zeroed at startup.
    /// Receive-side tracking (remote_label_change_indicator) stays per-peer.
    pub label_change_indicator: Arc<std::sync::atomic::AtomicU8>,
    /// Broadcast sender for WS push events (e.g. DNS failure). Cloned from AppState.
    pub event_tx: tokio::sync::broadcast::Sender<String>,
    /// Per-remote crypto state (CASCADE_ENCRYPTION_SPEC) — shared with the audio
    /// send/receive paths. The peer task drives the key exchange on poke/pong.
    pub crypto: std::sync::Arc<super::crypto::PeerCrypto>,
    /// This remote's link indices (net::LinkIndices): learned from each accepted pong,
    /// written on our config requests and config pushes, required on theirs.
    pub link: std::sync::Arc<super::LinkIndices>,
}

pub enum PeerCommand {
    Packet {
        from: SocketAddr, ptype: PacketType, ts: u32,
        label_revision: u8, raw: Vec<u8>, sender_token: Token, label_meta: u16, payload: Vec<u8>,
    },
    LabelsChanged(Vec<ChannelLabel>),
    Shutdown,
}

pub struct SendRequest {
    pub to:   SocketAddr,
    pub data: Vec<u8>,
}

pub struct PeerTask {
    config:                 PeerConfig,
    /// Last logged count of named channel labels, so the label refresh — which runs on a
    /// tick — reports only when it actually changes.
    last_named_count:       Option<usize>,
    state:                  PeerState,
    /// Whether name resolution for this peer is currently failing — see the DNS branch.
    dns_failing:            bool,
    our_token:              Token,
    remote_token:           Token,
    remote_addr:            Option<SocketAddr>,
    /// Any authenticated incoming traffic (POKE / POKE_RSP / ACK). Used for keepalive
    /// and rx bookkeeping only — it is NOT a connection-status signal.
    last_rx:                Option<Instant>,
    /// Result of the address-match check performed when the most recent pong
    /// ARRIVED (spec §1.1: evaluated per received pong, not per tick).
    pong_host_match:        bool,
    /// Last received POKE_RSP. This is the SOLE connection-status clock: status is
    /// `now − last_poke_rsp` against TIMEOUT, and a POKE_RSP only arrives if the peer
    /// authenticated OUR poke. An incoming POKE proves the peer is sending to us, NOT that
    /// it accepts us — authentication is directional, so connection MUST be gated on the
    /// round-trip proof (POKE_RSP), never on incoming POKEs.
    last_poke_rsp:          Option<Instant>,
    /// Shared global label-change indicator — same Arc across all peer tasks.
    label_change_indicator: Arc<std::sync::atomic::AtomicU8>,
    /// Per-peer: last indicator VALUE received from this specific peer.
    remote_label_change_indicator: Option<u8>,
    /// Some(when) while we are waiting for a label batch we asked for (via ACK), None
    /// when nothing is outstanding. Set when we send the
    /// ACK, re-stamped + re-ACK'd every LABEL_REQUEST_RETRY, cleared when the
    /// batch finishes reassembling. None = nothing outstanding.
    label_request_at: Option<Instant>,
    /// The version we asked for, so the timed re-ACK requests the same one.
    label_request_version: u8,
    local_labels:           Vec<ChannelLabel>,

    stats:                  PeerStats,
    // Label reassembly — STRICT-SEQUENCE accumulator (CASCADE_WIRE_PROTOCOL_SPEC
    // §3.1): an expected-segment counter (1-based) and one accumulating buffer.
    // Out-of-sequence segments are silently discarded — no reordering buffer, no
    // partial-data recovery; a lost segment drops the rest of that transfer and
    // recovery relies on the periodic re-request mechanisms. An incoming segment
    // 1 is the start of a fresh transfer (resets the accumulator and counter).
    label_buf:              Vec<u8>,
    label_expected_seg:     u8,
    /// When DNS resolution was last *attempted* for this peer. Re-resolution runs
    /// every RESOLVE_MIN_INTERVAL while the peer is not Connected
    /// (CASCADE_WIRE_PROTOCOL_SPEC §1/§7.2) — this stamps the periodic cadence.
    last_resolve_attempt:   Option<Instant>,
    /// Resolved address of the *configured* host (config.host), written ONLY by DNS
    /// resolution — never by a learned packet source. This is the reference for the
    /// amber/green host-match: amber when data arrives from an IP other than the
    /// configured one. `remote_addr` holds the LEARNED source (updated from every received
    /// packet for NAT correction), so it cannot be the match reference — the two are
    /// exactly what we compare. None for passive remotes (empty config.host) ⇒ no host to
    /// match ⇒ always green.
    resolved_host:          Option<SocketAddr>,
    /// Last DNS error for this peer, if any. Stored so it can be re-fired on each
    /// poke tick — a browser connecting after the initial failure sees it promptly.
    /// Cleared on successful DNS resolution, and when the peer is connected.
    dns_error:              Option<String>,
    /// When the dns_error banner event was last broadcast, so the re-fire (which
    /// exists only so a late-connecting browser sees the banner) is throttled rather
    /// than emitted on every ~2s poke tick.
    last_dns_error_emit:    Option<Instant>,
    /// Live incoming-sample-rate-mismatch flag: true while the most recent AUDIO packet
    /// from this peer advertised a non-48k rate (byte 9 != 1). Set/cleared off each audio
    /// packet; cleared on disconnect. Mirrors the dns_error model.
    sr_mismatch:            bool,
    /// Throttle for the sr-mismatch banner re-fire (like last_dns_error_emit) so a late
    /// browser gets the banner without flooding the event channel every poke tick.
    last_sr_mismatch_emit:  Option<Instant>,
    rx_bytes_window:        u64,
    tx_bytes_window:        u64,
    /// Audio packet counters for the current 2s stats window.
    pkts_rx_window:         u64,
    pkts_lost_window:       u64,
    /// Max |inter_arrival − 20ms| in the current 2s window.
    jitter_max_window:      f64,
    rx_bytes_atomic:        Arc<std::sync::atomic::AtomicU64>,
    tx_bytes_atomic:        Arc<std::sync::atomic::AtomicU64>,
    sr_44k_atomic:          Arc<std::sync::atomic::AtomicBool>,
    stat_acc: Option<Arc<std::sync::Mutex<std::collections::HashMap<
        String, Arc<crate::audio::pool::StatAccumulator>>>>>,
}

/// Boot-relative timestamp at 10 kHz (0.1 ms / 100 µs resolution), u32-wrapped.
/// This is the POKE round-trip latency clock. Boot-anchoring keeps wire values in the
/// same large-magnitude band as peers using an uptime-based clock.
/// macOS: CLOCK_UPTIME_RAW. Linux: CLOCK_MONOTONIC (since-boot, excludes suspend).
#[cfg(unix)]
fn boot_ts_10khz() -> u32 {
    // CLOCK_UPTIME_RAW is Darwin-only; CLOCK_MONOTONIC is the POSIX equivalent. Both
    // named explicitly so a new platform picks one deliberately.
    #[cfg(target_os = "macos")]
    let clock_id = libc::CLOCK_UPTIME_RAW;
    #[cfg(target_os = "linux")]
    let clock_id = libc::CLOCK_MONOTONIC;
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // Safety: clock_gettime writes a valid timespec given a valid clock id.
    unsafe { libc::clock_gettime(clock_id, &mut ts); }
    let total_ns = (ts.tv_sec as i128) * 1_000_000_000 + ts.tv_nsec as i128;
    (total_ns / 100_000) as u32
}

/// Windows: `QueryPerformanceCounter`, scaled to the same 10 kHz units.
///
/// **Resolution is the whole requirement here, not just monotonicity.** This value is the
/// POKE round-trip clock, and a round trip on a LAN is single-digit milliseconds — 20 to
/// 100 units. `QueryUnbiasedInterruptTime` and `GetTickCount64` both advance only on the
/// system timer tick, which is 15.6 ms by default and ~1 ms once a multimedia application
/// has raised the timer resolution. Either one quantises a 4 ms round trip to zero or to a
/// single tick, so latency reads as nothing at all with an occasional 1 ms — which is
/// exactly what it does. QPC is sub-microsecond.
///
/// It keeps the properties the unix arm has: it counts from boot, so the magnitude stays in
/// the same band as an uptime clock, and it does not advance while the machine is
/// suspended. The anchor is interop-irrelevant regardless — the peer only echoes this value
/// back, it never interprets it.
#[cfg(windows)]
fn boot_ts_10khz() -> u32 {
    use std::sync::atomic::{AtomicI64, Ordering};
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    // Fixed at boot, so read once. 0 means "not yet read".
    static FREQ: AtomicI64 = AtomicI64::new(0);
    let mut freq = FREQ.load(Ordering::Relaxed);
    if freq == 0 {
        let mut f: i64 = 0;
        // Safety: writes an i64 through a valid non-null pointer.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        // A zero frequency would divide by zero below. It cannot happen on any system that
        // supports QPC — which is every Windows version this builds for — but the fallback
        // costs nothing and keeps the divide total.
        freq = if f > 0 { f } else { 10_000_000 };
        FREQ.store(freq, Ordering::Relaxed);
    }
    let mut ticks: i64 = 0;
    // Safety: writes an i64 through a valid non-null pointer.
    let _ = unsafe { QueryPerformanceCounter(&mut ticks) };
    // ticks / freq = seconds; × 10_000 = 100 µs units. Multiply first, in i128, so the
    // division does not truncate the sub-second part that is the entire signal here.
    ((ticks as i128 * 10_000 / freq as i128) as u64) as u32
}

impl PeerTask {
    pub fn new(config: PeerConfig) -> Self {
        let addr         = config.remote_addr;
        let our_token    = config.our_token;
        let remote_token = derive_token(&config.remote_name, &config.password);
        let rx_bytes_atomic = Arc::clone(&config.rx_bytes_atomic);
        let tx_bytes_atomic = Arc::clone(&config.tx_bytes_atomic);
        let sr_44k_atomic = Arc::clone(&config.sr_44k_atomic);
        let label_change_indicator = Arc::clone(&config.label_change_indicator);
        let stat_acc = config.stat_acc.clone();
        debug!("Peer '{}': token={} (MD5 of '{}')",
              config.name, remote_token, config.remote_name.to_uppercase());
        Self {
            config, state: PeerState::Idle,
            dns_failing: false,
            last_named_count: None,
            our_token, remote_token,
            remote_addr: addr, last_rx: None, pong_host_match: false, last_poke_rsp: None,
            label_change_indicator, remote_label_change_indicator: None,
            label_request_at: None, label_request_version: 0,
            local_labels: vec![],
            stats: PeerStats {
                instance: NEXT_INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                ..PeerStats::default()
            },
            label_buf: Vec::new(), label_expected_seg: 1,
            last_resolve_attempt: None,
            resolved_host: None,
            dns_error: None,
            last_dns_error_emit: None,
            sr_mismatch: false,
            last_sr_mismatch_emit: None,
            rx_bytes_window: 0, tx_bytes_window: 0,
            pkts_rx_window: 0, pkts_lost_window: 0, jitter_max_window: 0.0,
            rx_bytes_atomic,
            tx_bytes_atomic,
            sr_44k_atomic,
            stat_acc,
        }
    }

    /// POKE/latency timestamp: boot-relative 10 kHz clock (0.1 ms / 100 µs units).
    /// The peer only echoes this for round-trip latency, so the anchor is
    /// interop-irrelevant; boot-anchoring keeps the magnitude in the usual band.
    fn now_ts(&self) -> u32 {
        boot_ts_10khz()
    }

    fn try_send(tx: &mpsc::Sender<SendRequest>, to: SocketAddr, data: Vec<u8>) -> usize {
        // Byte counters measure IP datagrams: UDP payload + IP header (20) + UDP
        // header (8) = +28.
        let n = data.len() + 28;
        let _ = tx.try_send(SendRequest { to, data });
        n
    }

    fn send_poke(&mut self, tx: &mpsc::Sender<SendRequest>) {
        if let Some(addr) = self.remote_addr {
            let indicator = self.label_change_indicator
                .load(std::sync::atomic::Ordering::Relaxed);
            // Bytes 8-11 of a POKE are 00 00 II 00 — zero word, indicator at 0x0A
            // (written by build_poke, byte 0x0A — spec §2.1).
            // When encryption is enabled for this remote, every poke carries the
            // key-exchange extension (our X25519 public key) — CASCADE_ENCRYPTION_
            // SPEC §3.2. The ordinary 53-byte poke otherwise.
            let our_pub = self.config.crypto.our_public();
            let key_ext = if self.config.crypto.is_enabled() { Some(&our_pub) } else { None };
            let pkt = build_poke(self.now_ts(),
                                  &self.our_token, &self.remote_token,
                                  indicator, key_ext);
            // Routine pokes are NOT counted in the tx figure
            // (CASCADE_SESSION_STATS_SPEC §2.2: pings excluded).
            let _ = Self::try_send(tx, addr, pkt);
        }
    }

    fn handle_poke(&mut self, from: SocketAddr, ts: u32, _sender: Token,
                   poke_label_revision: u8, ping_raw: &[u8], remote_indicator: u8,
                   _payload: &[u8], tx: &mpsc::Sender<SendRequest>) {
        // Token already validated at the dispatch gate — the single sender token at the
        // payload boundary must equal remote_token. The handler is reached only
        // for authenticated packets, so state may be updated directly. The gate also means
        // a wrong-name/password peer routed here by the peer_by_addr fallback can never
        // refresh last_rx or promote us to Connected.

        let now = Instant::now();
        // POKE inter-arrival is NOT used for jitter. Jitter is measured on AUDIO packets at
        // arrival, on the receive thread: the change between consecutive inter-arrival gaps
        // per channel, its maximum over the stats window (ArrivalStats in net/udp.rs).
        self.last_rx = Some(now);
        self.remote_addr = Some(from);
        // Update shared address map so audio can be forwarded to this peer. Signal a
        // send-cache rebuild only when the address actually CHANGED (not every packet).
        {
            let mut m = self.config.learned_addrs.write().unwrap_or_else(|e| e.into_inner());
            if m.insert(self.config.name.clone(), from) != Some(from) {
                if let Some(ref tx) = self.config.rebuild_tx { let _ = tx.try_send(()); }
            }
        }

        // KEY EXCHANGE (CASCADE_ENCRYPTION_SPEC §3.2): an 87-byte poke carries the
        // peer's X25519 public key in the extension. A poke whose length alone disagrees
        // with this remote's encryption setting (53 bytes with it on, 87 with it off) was
        // dropped whole at dispatch and never reaches here. What does reach here can still
        // fail on a malformed or degenerate key, or on any other length with encryption
        // on; ingest the key, and note whether the exchange may proceed — the pong at the
        // end of this handler is withheld when it may not.
        let key_exchange_ok = self.key_exchange_ok(_payload, false);

        // LABEL EXCHANGE PROTOCOL. The POKE's byte 0x0A is the sender's label VERSION.
        // When a
        // node receives a POKE whose indicator differs from the last value it recorded
        // for that peer (including first contact), it sends an ACK echoing that
        // indicator — the ACK IS the label request ("send me your labels for vN") —
        // in addition to the normal POKE_RSP. On an UNCHANGED indicator it sends
        // POKE_RSP only (no ACK, no labels). The peer, on receiving the ACK, sends its
        // LABEL packets. This is why receive was zero: we never asked.
        let indicator_changed = self.remote_label_change_indicator
            .map(|prev| prev != remote_indicator)
            .unwrap_or(true); // first contact counts as changed
        self.remote_label_change_indicator = Some(remote_indicator);

        if indicator_changed {
            debug!("Peer '{}': remote label version → {} — requesting labels (ACK)",
                   self.config.name, remote_indicator);
            // ACK carries the requested version at byte 0x0A.
            let mut ack = build_ack(ts, poke_label_revision, &self.our_token, remote_indicator);
            self.config.link.stamp(&mut ack);
            self.tx_bytes_window += Self::try_send(tx, from, ack.to_vec()) as u64;
            // Mark the request outstanding. The poke tick re-ACKs after
            // LABEL_REQUEST_RETRY if the batch never arrives (loss recovery);
            // handle_label clears it on completion.
            self.label_request_at = Some(Instant::now());
            self.label_request_version = remote_indicator;
        }
        // Reply with POKE_RSP unless the key exchange ruled the peer out, and change NO
        // state here: connection status is never written from the POKE receive handler.
        // Bytes 8-11 ECHO the incoming POKE's word, so the poker's version rides back at
        // byte 0x0A. Pong sends are NOT counted in the tx figure (spec §2.2: pongs belong
        // to a separate counter). The pong carries our key-exchange extension too when
        // encryption is enabled (§3).
        //
        // Withholding the pong is the whole of the refusal: a peer that never receives
        // one never promotes us to Connected, which is what "the two ends do not agree
        // about encryption" has to mean. Silence, not a plaintext fallback.
        if !key_exchange_ok {
            debug!("Peer '{}': poke from {} does not match this remote's encryption \
                   setting — no pong sent", self.config.name, from);
            return;
        }
        let our_pub = self.config.crypto.our_public();
        let key_ext = if self.config.crypto.is_enabled() { Some(&our_pub) } else { None };
        let _ = Self::try_send(tx, from,
            build_poke_rsp_from_ping(ping_raw, &self.our_token, key_ext));
    }

    /// §3.2 handshake gate: a key extension must be present exactly when this remote
    /// has encryption enabled. Encryption on with no extension, or an extension
    /// arriving at a remote with encryption off, means the two ends cannot agree, and
    /// the exchange is abandoned rather than carried on in a form neither can use. A
    /// well-formed extension on an encrypting remote is ingested here; a key rejected
    /// as degenerate (§4) abandons the exchange the same way a missing one does.
    ///
    /// `from_response` distinguishes a pong-borne key from a poke-borne one — only the
    /// former arms this side's encrypted send.
    fn key_exchange_ok(&self, payload: &[u8], from_response: bool) -> bool {
        let ext = parse_key_ext(payload);
        match (self.config.crypto.is_enabled(), ext) {
            (true,  Some(pk)) => self.config.crypto.on_peer_public(pk, from_response),
            (true,  None)     => false,
            (false, Some(_))  => false,
            (false, None)     => true,
        }
    }

    fn handle_poke_rsp(&mut self, from: SocketAddr, _sender: Token, ts: u32,
                       raw: &[u8], payload: &[u8]) {
        // KEY EXCHANGE (§3.2): a pong also carries the peer's public key when
        // encryption is enabled, and it is the pong — a peer ANSWERING one of our
        // pokes — that arms encrypted send. A pong that fails the gate abandons the
        // rest of this handler, so last_poke_rsp is not refreshed and the remote never
        // reaches Connected on a peer we cannot agree with.
        if !self.key_exchange_ok(payload, true) {
            debug!("Peer '{}': pong from {} does not match this remote's encryption \
                   setting — ignored", self.config.name, from);
            return;
        }
        // An accepted pong sets this remote's link indices (net::LinkIndices) from its own
        // bytes 0x11 and 0x0B.
        self.config.link.learn_from_pong(raw);
        // Token already validated at the dispatch gate (sender == remote_token). The
        // responder's POKE_RSP carries its own name-hash at the payload boundary (offset 21
        // on every sender seen so far) = remote_token, so it authenticates the same way a
        // POKE does. Reached only for authenticated RSPs.
        let now = Instant::now();
        self.last_rx = Some(now);
        // POKE_RSP is the ONLY connection-status signal: it proves the peer authenticated
        // our POKE and replied. derive_state/check_timeout
        // key off this, never off incoming POKEs (which authentication is one-directional
        // and would let a wrong-name peer that rejects us still appear connected).
        self.last_poke_rsp = Some(now);
        // Learn the peer's actual source address — updated from every received packet
        // type, so it corrects itself if NAT remapped.
        self.remote_addr = Some(from);
        {
            let mut m = self.config.learned_addrs.write().unwrap_or_else(|e| e.into_inner());
            if m.insert(self.config.name.clone(), from) != Some(from) {
                if let Some(ref tx) = self.config.rebuild_tx { let _ = tx.try_send(()); }
            }
        }

        // owd_ms = delta_0.1ms_units / 20.0 = RTT/2.
        // NO bound is applied — the raw delta is displayed. Clamping would make two ends
        // of the same link report different latency, and a genuinely bad satellite/3G path
        // can legitimately produce a multi-second RTT we have no basis to reject. The ONE
        // thing to guard is wraparound: now_ts() is a
        // u32 10kHz clock, and a stale or bogus echoed ts that lands *ahead* of it makes
        // wrapping_sub yield a ~2^32 delta — that's a negative real interval, not high
        // latency. Read the delta as signed: discard a negative (impossible) value, accept
        // any non-negative one unbounded.
        let delta_units = self.now_ts().wrapping_sub(ts) as i32; // signed: <0 ⇒ echo ahead
        let owd_ms = delta_units as f64 / 20.0;
        if delta_units >= 0 {
            self.stats.latency_ms = owd_ms;
        }

        // Address-match check on EVERY received pong (CASCADE_SESSION_STATS_SPEC
        // §1.1): compare THIS pong's source against the resolved configured host,
        // at receipt time. derive_state consumes the stored result rather than recomputing
        // the match on the tick from a possibly-newer learned address.
        self.pong_host_match = self.source_matches_host();

        // STATE IS NOT WRITTEN HERE. The poke tick is the sole writer of connection
        // status, computing it each tick from POKE_RSP freshness plus host-match.
        // handle_poke_rsp records only the FACTS (last_rx above, learned address,
        // latency) and derive_state() turns them into state. That split is what makes
        // recovery emergent, and it is where the amber host-match lives.
        //
        // We still kick the label send on the *edge* into Connected, but the edge is now
        // detected in derive_state() (called from the tick), not here — so a label batch
        // is sent exactly once per genuine Idle→Connected transition regardless of which
        // POKE_RSP first satisfied freshness. Nothing to do here beyond the fact-recording
        // already done above.
    }

    /// Compare a learned source address against the resolved configured-host address.
    /// IP-only — the port is deliberately excluded, because a peer moving Ethernet→WiFi
    /// keeps its port but changes IP, and only the IP change should flip the dot to amber.
    /// Returns true (match) when there is no configured host to compare against (a passive
    /// remote), so a null host reads as green.
    fn source_matches_host(&self) -> bool {
        match (self.remote_addr, self.resolved_host) {
            (Some(src), Some(cfg)) => src.ip() == cfg.ip(), // IP-only; port excluded
            (_, None)              => true,                 // no configured host ⇒ green
            (None, Some(_))        => false,                // configured but nothing learned
        }
    }

    /// The sole connection-status writer. Derives the
    /// new state from current facts — last_rx freshness vs TIMEOUT, and host-match —
    /// and returns true on the rising edge into Connected so the caller can fire the
    /// one-shot label send. Does NOT itself send packets; the tick orchestrates that.
    ///
    /// Status mapping:
    ///   stale (no fresh POKE_RSP within TIMEOUT)  → Idle
    ///   fresh + host matches (or no configured host) → Connected, host_match=true (green)
    ///   fresh + host mismatch                       → Connected, host_match=false (amber)
    /// Note amber is a CONNECTED sub-state, not Connecting: data is flowing, just from an
    /// unexpected IP. Connecting remains the pre-first-RSP phase (never been fresh).
    fn derive_state(&mut self) -> bool {
        let fresh = self.last_poke_rsp.map(|t| t.elapsed() <= TIMEOUT).unwrap_or(false);
        let was_connected = self.state == PeerState::Connected;

        if fresh {
            // Fresh POKE_RSP ⇒ Connected (green or amber by host-match). A POKE_RSP is the
            // only proof the peer authenticated us, so status is gated on POKE_RSP
            // freshness and never on incoming POKEs. The match itself was
            // evaluated when the pong ARRIVED (handle_poke_rsp, spec §1.1) — the tick
            // only publishes that stored per-pong result.
            self.state = PeerState::Connected;
            self.stats.host_match = self.pong_host_match;
        } else if self.last_poke_rsp.is_some() {
            // Was fresh once, now stale ⇒ timed out. Down-transition handled by
            // check_timeout (which also resets reconnect bookkeeping); leave state as
            // whatever check_timeout set. host_match irrelevant when not Connected.
            self.stats.host_match = false;
        } else {
            // Never received anything yet ⇒ still coming up.
            if self.state != PeerState::Idle {
                self.state = PeerState::Connecting;
            }
            self.stats.host_match = false;
        }

        // Rising edge into Connected (from anything that was not Connected).
        self.state == PeerState::Connected && !was_connected
    }

    /// Reassemble and parse multi-segment label packets — STRICT sequence
    /// (CASCADE_WIRE_PROTOCOL_SPEC §3.1): segments must arrive as 1, 2, …, N; any out-of-sequence
    /// segment is silently discarded with no reordering or recovery. A lost
    /// segment therefore drops the remainder of that transfer; the periodic
    /// re-request (5s re-ACK) triggers a fresh transfer from segment 1.
    ///
    /// b15 = segment index (1-based), b16 = total segment count.
    fn handle_label(&mut self, payload: &[u8], seg_idx: u8, total_segs: u8,
                    _tx: &mpsc::Sender<SendRequest>) {
        let total = total_segs.max(1);

        // Segment 1 is the start of a fresh transfer — reset the accumulator and
        // the expected counter (this is also how a receiver stuck mid-transfer
        // recovers when the sender restarts from 1).
        if seg_idx == 1 {
            self.label_buf.clear();
            self.label_expected_seg = 1;
        }
        if seg_idx != self.label_expected_seg {
            debug!("Peer '{}': out-of-sequence label segment {} (expected {}) — \
                    discarded", self.config.name, seg_idx, self.label_expected_seg);
            return;
        }
        self.label_buf.extend_from_slice(payload);

        if seg_idx == total {
            // Transfer complete — finalize and reset the counter for the next
            // transfer. Clears the outstanding request (stops the timed re-ACK).
            self.label_request_at = None;
            self.label_expected_seg = 1;
            let json_bytes: Vec<u8> = std::mem::take(&mut self.label_buf);

            let json_str = match std::str::from_utf8(&json_bytes) {
                Ok(s) => s.to_string(),
                Err(e) => {
                    warn!("Peer '{}': label UTF-8 error: {}", self.config.name, e);
                    return;
                }
            };

            // The label payload is a JSON array of {"c":<channel>,"l":"<label>"}
            // objects with 1-based "c" (CASCADE_WIRE_PROTOCOL_SPEC §3) — the only
            // format on the wire. Convert to 0-based internally.
            if let Ok(mut labels) = serde_json::from_str::<Vec<ChannelLabel>>(&json_str) {
                for l in labels.iter_mut() {
                    l.channel = l.channel.saturating_sub(1);
                    l.label = cap_label(&l.label);
                }
                let named = labels.iter().filter(|l| !l.label.is_empty()).count();
                // Only on a CHANGE: this refresh runs on a tick, and the slot count is a
                // fixed 128 either way — logging both every time said nothing new.
                if self.last_named_count != Some(named) {
                    info!("Peer '{}': {} named channel{}", self.config.name, named,
                          if named == 1 { "" } else { "s" });
                    self.last_named_count = Some(named);
                }
                self.stats.remote_labels = labels;
            } else {
                // Not the array format — log a prefix of what arrived so a field
                // report tells us the actual wire format / corruption.
                let prefix: String = json_str.chars().take(120).collect();
                warn!("Peer '{}': label JSON unparseable ({} bytes): {}",
                      self.config.name, json_str.len(), prefix);
            }
        } else {
            // Mid-transfer: await the next segment in sequence.
            self.label_expected_seg = self.label_expected_seg.saturating_add(1);
        }
    }

    fn send_labels(&mut self, to: SocketAddr, tx: &mpsc::Sender<SendRequest>) {
        // NOTE: an empty local set is still SENT — the full slot range goes out with all
        // labels empty. Labels are derived from send routing, so "no routes" must actively
        // CLEAR any stale names at the receiver rather than leave the last advertised set
        // showing.
        //
        // The LABEL JSON uses 1-BASED channel indices in the "c" field: the first channel
        // is {"c":1,...}, with an empty "l" for unused slots. The AUDIO wire header's
        // channel byte 0x12 is separate and stays 0-based. Labels are stored 0-based
        // internally, so emit c = index + 1 — sending c:0.. makes receivers display every
        // channel off by one.
        //
        // How many entries: LABEL_ENTRIES, fixed at the last label reload (see there).
        // Every entry in range is sent — named slots filled, the rest empty.
        let slot_count = LABEL_ENTRIES.load(std::sync::atomic::Ordering::Relaxed);
        let mut wire: Vec<ChannelLabel> = Vec::with_capacity(slot_count as usize);
        for slot0 in 0..slot_count {
            let label = self.local_labels.iter()
                .find(|l| l.channel == slot0)
                .map(|l| l.label.clone())
                .unwrap_or_default();
            wire.push(ChannelLabel { channel: slot0 + 1, label });  // 1-based on wire
        }
        let json = serde_json::to_vec(&wire).unwrap_or_default();
        let chunks: Vec<&[u8]> = json.chunks(LABEL_FRAG_SIZE).collect();
        let total_segs = chunks.len().max(1) as u8;
        debug!("send_labels → {}: {} slots ({} named), {} bytes, {} seg(s)",
              to, wire.len(),
              self.local_labels.iter().filter(|l| !l.label.is_empty()).count(),
              json.len(), total_segs);
        for (i, chunk) in chunks.iter().enumerate() {
            let seg_idx = (i + 1) as u8;
            let mut pkt = build_label(self.now_ts(), &self.our_token,
                                       chunk, seg_idx, total_segs);
            self.config.link.stamp(&mut pkt);
            // Config-push (label) segments ARE counted in the tx figure
            // (spec §2.2: included, unlike pokes/pongs).
            self.tx_bytes_window += Self::try_send(tx, to, pkt) as u64;
        }
    }

    fn check_timeout(&mut self) {
        if self.state == PeerState::Idle { return; }
        // The timeout applies in both Connecting and Connected, and both reset to Idle. The
        // connection clock is POKE_RSP only (last_poke_rsp) — an incoming POKE is NOT a
        // keepalive for connection purposes (authentication is directional). If we stop
        // receiving POKE_RSPs for 20s the peer no longer accepts us / is gone → reset.
        if self.last_poke_rsp.map(|t| t.elapsed() > TIMEOUT).unwrap_or(false) {
            warn!("Peer '{}' timed out (state={:?})", self.config.name, self.state);
            self.state = PeerState::Idle;
            self.stats.host_match = false;
            // Re-resolution resumes automatically: the poke tick re-resolves every
            // RESOLVE_MIN_INTERVAL while not Connected, so no arming is needed here.
            // We deliberately do NOT clear remote_label_change_indicator: if the peer
            // kept its state (blip), its unchanged label indicator means no
            // re-handshake; if it restarted, its indicator resets and the existing ACK
            // path (handle_poke) re-requests labels — recovery type is driven by the
            // peer's signal, not by us assuming the worst.
        }
    }

    fn update_stats(&mut self) {
        self.stats.state = match self.state {
            PeerState::Idle         => "idle",
            PeerState::Connecting   => "connecting",
            PeerState::Connected    => "connected",
        }.to_string();
        // Numeric status (spec §1): 2 = connected clean, 1 = connected with
        // address mismatch, 0 = down (idle and connecting both map to 0).
        self.stats.status = match self.state {
            PeerState::Connected if self.stats.host_match => 2,
            PeerState::Connected                          => 1,
            _                                             => 0,
        };
        // Encryption state (CASCADE_ENCRYPTION_SPEC): enabled = the user setting;
        // active = enabled AND the handshake has completed (audio is actually sealed).
        self.stats.encryption = self.config.crypto.is_enabled();
        self.stats.enc_active = self.stats.encryption && self.config.crypto.is_complete();
        self.stats.remote_token = self.remote_token.to_string();
        // Mirror the current DNS error into the published stats so it rides the 1Hz
        // stats broadcast AND the on-connect state snapshot — a newly-opened web UI
        // then shows the banner immediately instead of waiting for the next
        // DNS_ERROR_REFIRE. Set before the not-connected early return below: a peer
        // failing DNS is in Connecting/Idle, never Connected.
        self.stats.dns_error = self.dns_error.clone();

        // NOTE: stats are computed EVERY pass regardless of connection state. The byte
        // accumulators are drained unconditionally for every ENABLED remote — connection
        // status is a separate reading that only drives the status pill. So a peer that
        // never connects still shows its raw Tx/Rx rate, e.g. the ~0.001 Mbps of outgoing
        // POKEs to a failing peer. Force-zeroing rate stats when not Connected hides real
        // traffic and is wrong.
        //
        // BANDWIDTH DIVISOR — a DELIBERATE deviation from CASCADE_SESSION_STATS_SPEC
        // §2.2/§2.5, which would divide by the actual 2.0s interval.
        //
        // Interoperating peers display a bandwidth figure ~6.3% above the true rate: their
        // stats timer re-arms from completion, so its real period is 2.0s plus per-tick
        // work (measured at ~2.1259s on a busy instance), while the window's bytes are
        // divided by the NOMINAL 2.0s. Our own tick body is near-instant, so our period is
        // a flat ~2.0s and dividing by 2.0 yields the TRUE rate — which reads ~6% BELOW
        // what the other end shows for the same link. Matching the displayed number
        // therefore needs a divisor carrying the same inflation:
        //
        //   d = 2.0 * 2.0 / 2.1259 = 1.8816s     (bytes_2.0 / d == bytes_2.1259 / 2.0)
        //
        // The divisor carries the inflation, NOT the cadence: the tick stays a flat 2.0s.
        // Re-arming the sleep from completion to reproduce the longer period does not work,
        // because our tick work is too fast for it to matter.
        const BW_DIVISOR_SECS: f64 = 1.8816;  // = 2.0² / 2.1259
        let interval_secs = BW_DIVISOR_SECS;


        // Audio RX: read directly from shared atomic — no channel delivery race.
        // Written per-packet by the receive path; swapped to zero here atomically.
        // This is the single source of truth for audio bytes, replacing the
        // stats_tick → AudioRxBytes channel path which had a two-timer race.
        let audio_rx = self.rx_bytes_atomic.swap(0, std::sync::atomic::Ordering::Relaxed);
        let total_rx = self.rx_bytes_window + audio_rx;
        self.stats.rx_bps = (total_rx as f64 * 8.0 / interval_secs) as u64;

        // TX: read directly from per-peer atomic — same pattern as RX.
        // Written by apply_encode_work per packet sent, reset here.
        // No stats_tick → AudioTxBytes channel needed. One clock.
        let audio_tx = self.tx_bytes_atomic.swap(0, std::sync::atomic::Ordering::Relaxed);
        let total_tx = self.tx_bytes_window + audio_tx;
        self.stats.tx_bps = (total_tx as f64 * 8.0 / interval_secs) as u64;

        // Jitter + packet loss: drain directly from stat accumulator.
        // stat_acc is Send+Sync (no raw pointers). Same single-clock guarantee.
        let (rx_pkts, lost_pkts, jitter_max) = self.stat_acc
            .as_ref()
            .and_then(|sa| {
                sa.lock().ok().and_then(|map| {
                    map.get(&self.config.name).map(|a| a.drain())
                })
            })
            .unwrap_or((0, 0, 0.0));
        // Accumulate with any pkts already counted via window (control-path pkts)
        let total_rx_pkts  = self.pkts_rx_window  + rx_pkts;
        let total_lost_pkts = self.pkts_lost_window + lost_pkts;
        self.stats.jitter_ms = jitter_max.max(self.jitter_max_window);
        self.stats.loss_count = total_lost_pkts;
        // loss% = lost / expected (spec §2.3): expected = received + lost — the
        // packets that should have arrived this window, not just those that did.
        let expected = total_rx_pkts + total_lost_pkts;
        self.stats.pct_lost = if expected > 0 {
            total_lost_pkts as f64 / expected as f64 * 100.0
        } else { 0.0 };
        self.jitter_max_window = 0.0;
        self.tx_bytes_window   = 0;
        self.rx_bytes_window   = 0;
        self.pkts_rx_window    = 0;
        self.pkts_lost_window  = 0;
    }

    pub async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<PeerCommand>,
        send_tx:    mpsc::Sender<SendRequest>,
        stats_tx:   mpsc::Sender<(String, PeerStats)>,
    ) {
        // Connection monitor cadence — FIXED 2.0s tick (tokio interval). This is the
        // keepalive peers expect, and it sits 10x inside the 20s poke-response timeout,
        // which is a coarse liveness check rather than an inter-poke spacing measurement.
        // So the exact cadence does not need to track any particular peer's period, and
        // the displayed-bandwidth inflation is carried by BW_DIVISOR_SECS instead (see
        // update_stats) — re-arming this sleep from completion cannot reproduce it,
        // because the per-tick work here is near-instant.
        let mut poke_tick = interval(POKE_INTERVAL);
        loop {
            tokio::select! {
                _ = poke_tick.tick() => {
                    self.check_timeout();

                    // Re-resolve the hostname every RESOLVE_MIN_INTERVAL (10s) while NOT
                    // Connected (CASCADE_WIRE_PROTOCOL_SPEC §1/§7.2: periodic re-resolution
                    // while disconnected, so a remote whose IP changed — even one that has
                    // never connected — is found without waiting for any other trigger).
                    // While Connected, no re-resolution happens. lookup_host is
                    // non-blocking (thread-pool backed). IPv4 only: the protocol is
                    // IPv4-only (§1); a hostname with no A record counts as a failure and
                    // no IPv6 address is ever stored or used. On failure, the last known
                    // addr is kept so the poke still fires — the peer may still respond.
                    let resolve_due = self.state != PeerState::Connected
                        && !self.config.host.is_empty()
                        && self.last_resolve_attempt
                               .map(|t| t.elapsed() >= RESOLVE_MIN_INTERVAL)
                               .unwrap_or(true);
                    if resolve_due {
                        self.last_resolve_attempt = Some(Instant::now());
                        let lookup = tokio::net::lookup_host(
                            format!("{}:{}", self.config.host, self.config.port)
                        ).await;
                        // IPv4-only: an Ok resolution with no A record is a failure.
                        let outcome = match lookup {
                            Ok(addrs) => {
                                match addrs.into_iter().find(|a| a.is_ipv4()) {
                                    Some(a) => Ok(a),
                                    None => Err("no IPv4 address".to_string()),
                                }
                            }
                            Err(e) => Err(e.to_string()),
                        };
                        match outcome {
                            Ok(new) => {
                                // Resolution is working again; re-arm the failure log.
                                self.dns_failing = false;
                                // DNS resolved — clear any previous error.
                                self.dns_error = None;
                                self.last_dns_error_emit = None;
                                // resolved_host is the amber/green match REFERENCE —
                                // the configured host's address, written ONLY here,
                                // never from a learned packet source.
                                self.resolved_host = Some(new);
                                match self.remote_addr {
                                    Some(old) if new != old => {
                                        info!("Peer '{}': DNS '{}' → {} (was {})",
                                              self.config.name, self.config.host,
                                              new, old);
                                        self.remote_addr = Some(new);
                                    }
                                    None => {
                                        info!("Peer '{}': DNS '{}' → {}",
                                              self.config.name, self.config.host, new);
                                        self.remote_addr = Some(new);
                                    }
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                // TRANSITION, not attempt: §7.2 re-resolves every 10 s for
                                // as long as a peer is down, so logging each failure means
                                // a line every 10 s indefinitely for one absent machine.
                                if !self.dns_failing {
                                    self.dns_failing = true;
                                    warn!("Peer '{}': DNS resolve '{}' failed: {} \
                                          (keeping last addr: {:?})",
                                          self.config.name, self.config.host, e,
                                          self.remote_addr);
                                }
                                // Store the error so poke_tick re-fires it each cycle.
                                // A browser connecting after the initial failure will
                                // then see the banner on the next tick rather than
                                // missing the one-shot event.
                                self.dns_error = Some(format!("Cannot resolve hostname '{}'",
                                                               self.config.host));
                                let send_result = self.config.event_tx.send(
                                    serde_json::json!({"type":"peer_error",
                                        "peer": self.config.name,
                                        "msg": self.dns_error.as_deref().unwrap_or("")
                                    }).to_string());
                                self.last_dns_error_emit = Some(Instant::now());
                                debug!("Peer '{}': DNS error event send result: {:?}", self.config.name, send_result);
                            }
                        }
                    }
                    // Derive the connection state from the current facts — POKE_RSP freshness
                    // and host-match — now that DNS above may have refreshed resolved_host.
                    // This is the only place that promotes to Connected. No unsolicited label
                    // push on the rising edge (CASCADE_WIRE_PROTOCOL_SPEC §3.2): initial label
                    // sync is request-driven — each side notices the other's revision on first
                    // contact and ACK-requests, and the Ack arm below replies.
                    if self.derive_state() {
                        // host_match is reported only when FALSE — the §3.1
                        // address-mismatch sub-state, which still sends but is worth
                        // seeing. A normal match is the expectation, so it stays quiet.
                        if self.stats.host_match {
                            info!("Peer '{}' connected", self.config.name);
                        } else {
                            info!("Peer '{}' connected (address mismatch)", self.config.name);
                        }
                    }
                    // A CONNECTED PEER HAS NO DNS PROBLEM WORTH SHOWING. Resolution only runs
                    // while disconnected (§7.2), so an error recorded during an outage is
                    // never overwritten by a success once the link returns on the last known
                    // address — which is exactly what happens when the network comes back
                    // before the next resolve attempt. Left alone it re-fired below every
                    // DNS_ERROR_REFIRE for the life of the connection. The failure latch is
                    // cleared with it, so the next outage that also fails to resolve logs
                    // again.
                    if self.state == PeerState::Connected && self.dns_error.is_some() {
                        self.dns_error = None;
                        self.last_dns_error_emit = None;
                        self.dns_failing = false;
                    }
                    // Re-fire any unresolved DNS error periodically so a browser that
                    // connects after the initial failure sees the banner, without
                    // flooding the event channel on every poke tick.
                    if let Some(ref err) = self.dns_error {
                        let due = self.last_dns_error_emit
                            .map(|t| t.elapsed() >= DNS_ERROR_REFIRE)
                            .unwrap_or(true);
                        if due {
                            let _ = self.config.event_tx.send(
                                serde_json::json!({"type":"peer_error",
                                    "peer": self.config.name,
                                    "msg": err
                                }).to_string());
                            self.last_dns_error_emit = Some(Instant::now());
                        }
                    }
                    // ── Incoming sample-rate mismatch (44.1k) ──
                    // Runs AFTER derive_state so self.state is this tick's value. main.rs
                    // writes sr_44k_atomic on every incoming audio packet: true if the
                    // sender advertised 44.1k (byte 9 != 1), false for 48k — so the atomic
                    // is the live "most recent packet's rate". Only flag while Connected;
                    // any non-connected state force-clears (the "or disconnect" clear).
                    // A clean 48k packet flips the atomic false → clears (the "clean 48k"
                    // clear). Surfaced exactly like dns_error: (1) published in stats so a
                    // fresh/refreshed browser re-derives the banner + 44.1k badge from
                    // current state; (2) a throttled peer_error WS event so a late browser
                    // sees it promptly without flooding the channel every tick.
                    let new_sr_mismatch =
                        self.state == PeerState::Connected
                        && self.sr_44k_atomic.load(std::sync::atomic::Ordering::Relaxed);
                    if new_sr_mismatch {
                        let due = self.last_sr_mismatch_emit
                            .map(|t| t.elapsed() >= SR_MISMATCH_REFIRE)
                            .unwrap_or(true);
                        if due {
                            let _ = self.config.event_tx.send(
                                serde_json::json!({"type":"sr_error",
                                    "peer": self.config.name
                                }).to_string());
                            self.last_sr_mismatch_emit = Some(Instant::now());
                        }
                    } else {
                        self.last_sr_mismatch_emit = None;
                    }
                    self.sr_mismatch = new_sr_mismatch;
                    self.stats.sr_mismatch = new_sr_mismatch;
                    self.send_poke(&send_tx);
                    // Label re-request (loss recovery): while a
                    // label request is outstanding and >5s have passed since we last
                    // asked, re-send the ACK for the same version. The request time is
                    // stamped on the change edge, compared against a 5.0s interval,
                    // re-stamped here, and cleared on batch completion in handle_label,
                    // so a dropped LABEL batch is always re-requested.
                    if let (Some(at), Some(addr)) =
                        (self.label_request_at, self.remote_addr)
                    {
                        if at.elapsed() > LABEL_REQUEST_RETRY {
                            debug!("Peer '{}': label batch v{} not received in {:?} — \
                                    re-requesting (ACK)", self.config.name,
                                   self.label_request_version, at.elapsed());
                            let ts = self.now_ts();
                            let mut ack = build_ack(ts, 0u8, &self.our_token,
                                                    self.label_request_version);
                            self.config.link.stamp(&mut ack);
                            self.tx_bytes_window += Self::try_send(&send_tx, addr,
                                                                   ack.to_vec()) as u64;
                            self.label_request_at = Some(Instant::now());
                        }
                    }
                    self.update_stats();
                    let _ = stats_tx.try_send((self.config.name.clone(), self.stats.clone()));
                }
                Some(cmd) = cmd_rx.recv() => {
                    match cmd {
                        PeerCommand::Packet { from, ptype, ts, label_revision, raw, sender_token, label_meta, payload } => {
                            // TOKEN GATE: the token must be validated BEFORE touching this
                            // peer's state, freshness, or byte counters, so a packet matching
                            // no peer's token changes nothing. main.rs
                            // routes control packets to a peer task by token OR by source
                            // ADDRESS (peer_by_addr fallback), so a wrong-name/password peer
                            // whose address we already know reaches this task — we must
                            // therefore re-validate the token here and drop it before it can
                            // advance last_rx (which the tick reads to promote to Connected)
                            // or accrue rx bytes. AUDIO is exempt: it carries no token in the
                            // same way and is handled/accounted entirely in main.rs by the
                            // validated token→peer map; if an Audio packet ever reaches here
                            // it is a no-op arm below anyway.
                            //
                            // expected-token (packet target) must be us; sender-token must be
                            // the configured remote. Mirrors handle_poke's two checks.
                            // TOKEN GATE. Validation compares ONLY the single token at the
                            // payload boundary (0x0C + pkt[0x0C], which is 21 for every
                            // sender seen so far, since pkt[0x0C] = 9),
                            // i.e. the SENDER's own name-hash — exactly one slot, for every
                            // packet type. A POKE additionally carries a second token (the
                            // target's hash) one token further in, but NOTHING validates
                            // that slot; it exists only so the far end sees its own expected
                            // hash in slot 1 of the corresponding direction. POKE_RSP/ACK/LABEL
                            // carry one token at the boundary followed by type-specific data,
                            // so a LABEL's "slot 2" bytes are JSON, not a token.
                            //
                            // So validate the SENDER token ONLY, and never inspect slot 2.
                            // A slot-2 == our_token check rejects every POKE_RSP and LABEL,
                            // zeroing latency and Rx.
                            let authed = if ptype == PacketType::Audio {
                                true // no-op arm; never advances state
                            } else {
                                sender_token == self.remote_token
                            };
                            // Config requests and config pushes must also carry this
                            // remote's link indices (net::LinkIndices); one that does not is
                            // not this remote's traffic and is dropped the same way.
                            let authed = authed && match ptype {
                                PacketType::Ack | PacketType::Label =>
                                    self.config.link.accepts(&raw),
                                _ => true,
                            };
                            if !authed {
                                debug!("Peer '{}': {:?} from {} failed token/link-index check — \
                                       ignoring (wrong remote name/password?)",
                                       self.config.name, ptype, from);
                                // Do NOT count rx bytes, do NOT touch state: this datagram is
                                // not authenticated as this peer's traffic.
                            } else {
                            // Count Rx wire bytes for all (authenticated) control packets.
                            // `raw` is the datagram exactly as it arrived, so its length
                            // needs no reconstruction from header and payload sizes; +28
                            // for the IP and UDP headers. Every packet type is counted,
                            // not just audio.
                            self.rx_bytes_window += raw.len() as u64 + 28;
                            match ptype {
                                PacketType::Poke    => self.handle_poke(from, ts, sender_token, label_revision, &raw, label_meta as u8, &payload, &send_tx),
                                PacketType::PokeRsp => self.handle_poke_rsp(from, sender_token, ts, &raw, &payload),
                                PacketType::Ack     => {
                                    // The peer ACK'd our POKE → it saw our label version
                                    // advance and is REQUESTING our labels. Always reply —
                                    // an empty local set still sends the pre-allocated
                                    // empty-slot list (send_labels' own always-send
                                    // policy), actively clearing stale names at the
                                    // receiver rather than leaving the last set showing.
                                    self.last_rx = Some(Instant::now());
                                    self.send_labels(from, &send_tx);
                                }
                                PacketType::Label   => {
                                    // Segment framing: seg index at byte 0x0F (1-based)
                                    // and total at 0x10. main.rs packs these (label_meta)
                                    // into the
                                    // label-revision field for LABEL packets; we read label_meta
                                    // directly here for clarity. Do NOT ACK a LABEL — the
                                    // ACK is the *request*, sent from handle_poke on an
                                    // indicator change, never in response to a LABEL.
                                    let seg_idx   = (label_meta >> 8) as u8;
                                    let total_seg = label_meta as u8;
                                    self.handle_label(&payload, seg_idx, total_seg, &send_tx);
                                }
                                PacketType::Audio => {}
                            }
                            } // end if authed
                        }
                        PeerCommand::LabelsChanged(labels) => {
                            // Update our label set; the global indicator was
                            // already bumped in main.rs so our
                            // next POKE advertises the new version. We do NOT push
                            // labels here — the peer detects the version change and
                            // sends an ACK (request), and the Ack arm above replies
                            // with the labels. Sending proactively here was the race
                            // that made some routing changes not "follow" on the peer.
                            self.local_labels = labels;
                        }
                        PeerCommand::Shutdown => {
                            // Withdraw this task from the stats pump before going. Without
                            // it the pump kept this peer's last state — "connected", for a
                            // peer disabled while up — and held the send path's coarse
                            // any-connected gate open for good. Sent through the same
                            // channel as every earlier report, so it is the last one the
                            // pump sees from this task; awaited, so a full channel delays
                            // it rather than dropping it.
                            self.stats.ended = true;
                            let _ = stats_tx.send((self.config.name.clone(),
                                                   self.stats.clone())).await;
                            break;
                        }
                    }
                }
            }
        }
    }
}
