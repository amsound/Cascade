# Cascade — Wire Protocol Specification

This document specifies the network protocol implemented by Cascade
for peer discovery, session control, and audio transport. It
describes *what the bytes mean and how peers behave*, independent of
how any particular implementation is built. For send-side capture,
encoding, and transmission, see `CASCADE_AUDIO_SEND_SPEC.md`. For
receive-side decoding and buffering, see
`CASCADE_AUDIO_RECEIVE_SPEC.md`. For clock-skew correction and the
resampler, see `CASCADE_SYNC_MECHANISM_SPEC.md`.

---

## 1. Transport

- **Audio and control plane**: UDP. Default port `20102`, user-configurable
  to any value in the full `1–65535` range. A single UDP socket handles
  both audio and control messages.
- **HTTP**: an optional monitoring plane and Cascade's own web interface, both over TCP
  and independent of the UDP protocol. See §6.
- Each peer only needs to know the *other* peer's address — a
  connection is viable if only one side has a resolvable host/port for
  the other. This makes asymmetric deployments (e.g. one peer behind
  NAT/DHCP, the other at a fixed address) work without special
  handling.
- Host fields accept either a literal IP address or a DNS hostname. If
  a configured remote is unreachable, its hostname is re-resolved
  automatically on a periodic basis (every 10 seconds while
  disconnected, §7) so that reconnection succeeds after the remote's
  IP address changes.
- **This protocol is IPv4-only.** No IPv6 address is ever stored or used for sending. A
  resolved IPv4 address is a prerequisite for sending *any* packet to a remote (ping,
  pong, config, or audio); an implementation need not support IPv6 at all.

## 2. UDP Packet Structure

Every UDP packet on this protocol begins with a 2-byte magic value.
Packets without this magic are handled by a small set of legacy
plaintext control messages (§2.3) or silently discarded.

### 2.1 Common header

| Offset | Size | Field | Notes |
|---|---|---|---|
| `0x00` | 2 | Magic | Fixed bytes `0x80 0x69` |
| `0x02` | 2 | Sequence number | Big-endian. Per stream — one counter per encoder, i.e. per (channel, frame size, mode) sent — incrementing by 1 per packet, starting at 0 when the stream starts. Wraps at 16 bits; comparisons must handle wraparound (see §2.4). |
| `0x04` | 4 | Timestamp | Big-endian 32-bit. Meaning is opcode-dependent — see §2.2 and §3. |
| `0x08` | 1 | Flags | Bit 7: this packet's payload is encrypted (§5). Bits 0–6: opcode-dependent — for audio packets (opcode 6), these bits select the payload codec (§3, Audio Pipeline spec). |
| `0x09` | 1 | Sample rate | Audio packets only: `1` = 48000 Hz, `0` = 44100 Hz. |
| `0x0A` | 1 | Label revision (ping and pong) | Opcodes 7 and 8 only: a revision counter for the sender's current label set. Written by the ping sender's own current value; a pong carries the same byte through unchanged, by construction, rather than via a separate write. A receiver comparing this against its cached copy of the peer's last-known value uses a mismatch to trigger an immediate config-request (§3.2). Reserved (zero) for other opcodes. |
| `0x0B` | 1 | Source index | A link index, per remote. Written on audio (6/11), config-request (9) and config-push (10) packets from the destination remote's learned `source_index`; zero on pokes; a pong echoes the poke's byte. Learned from every accepted pong: `source_index ← pong[0x11]`. Inbound audio, config requests and config pushes from that remote must carry its `destination_index` here, or they are not that remote's traffic (§2.1.2). Starts at zero, and stays zero between peers that never send otherwise. Routing is decided entirely by the channel number (`0x12`), never by this. |
| `0x0C` | 1 | Header length | Fixed value `9` for the current protocol version. Used by receivers to compute payload boundaries; do not assume payload always starts at a fixed offset without reading this field. |
| `0x0D` | 1 | Opcode | See §3. |
| `0x0F` | 1 | Segment number | Config-push (opcode 10) only: the **1-based** index of this segment within a multi-part label transfer (single-packet pushes carry `1`; a multi-segment transfer runs `1..N`). Reserved (zero) for other opcodes. |
| `0x10` | 1 | Total segments **/ sender-restart marker** | Two unrelated meanings, disambiguated by opcode. **Config-push (opcode 10):** the total number of segments in this transfer. Because numbering is 1-based, this value is also the index of the final segment — the receiver treats the transfer as complete when a segment whose segment-number (`0x0F`) equals this value arrives. A single-packet push carries `1` here. **Audio (opcodes 6 and 11):** read as a SIGNED byte, and a negative value means the sender has restarted — see §2.1.1. Zero on audio packets that are not announcing a restart. The two readings never collide: segment counts are small positives, and a restart marker only appears on audio. |
| `0x11` | 1 | Destination index | The other link index — same packets, same rules as `0x0B` (§2.1.2): written from the remote's learned `destination_index`, zero on pokes, echoed by a pong, learned as `destination_index ← pong[0x0B]`, and inbound traffic must carry `source_index` here. |
| `0x12` | 1 | Channel number | Audio packets only. 0-indexed. |
| `0x13` | 2 | Opcode-dependent | For audio packets: declared payload length, validated against actual received length; reject on mismatch. For config-push (opcode 10): this segment's own payload length (16-byte identity + JSON chunk), little-endian, equal to `total_packet_length − 21` — used by the receiver to bound how many bytes of this segment's content to parse before appending to the reassembly buffer (§3.1). For all other opcodes, this field is reserved (send as zero). |
| `0x15` | variable | Payload | Opcode-dependent, see §3. |

**Header field meaning is opcode-dependent, not fixed protocol-wide.**
Do not assume a byte offset means the same thing across every opcode —
several offsets (including `0x08`, `0x0A`, and `0x13`) carry different
meanings, or are unused/reserved, depending on the opcode. `0x13`
specifically has **two** distinct meanings depending on opcode (audio
payload-length validation; config-push segment length), not one
meaning with the field reserved elsewhere. The table above states the
meaning per opcode explicitly; treat anything not listed for a given
opcode as reserved (send zero, ignore on receive).

**A pong's "reserved (zero)" fields work differently from the other opcodes'.** A pong
(opcode 8) is built by modifying the received ping's own buffer in place: the opcode byte
is rewritten and the payload replaced (§3), and nothing else in the header is touched.
Segment number (`0x0F`), total segments (`0x10`), `0x13`, the sequence, the timestamp and
the `0x08`-`0x0B` group all carry through unmodified from the ping — zero in ordinary
traffic because a ping has zero there, not because the pong writes zero. Every other opcode
is built fresh, with its reserved fields written as zero. Build the pong from the received
ping's buffer: a pong built fresh looks identical in ordinary traffic but differs the
moment a ping arrives with something non-zero in those positions, which must be echoed
straight back.

#### 2.1.1 The sender-restart marker (`0x10`, audio only)

A sender that has restarted its encoder — reconnecting, resuming after
transmit was toggled off, or otherwise beginning a stream that is
discontinuous with what it sent before — sets the sign bit of `0x10`
on the audio packets announcing it. The receiver reads the byte as
signed and, when it is negative:

```
opus_decoder_ctl(decoder, OPUS_RESET_STATE)   # 4028
gap        = 0          # the sequence jump is NOT loss
last_seq   = this packet's sequence number
first_packet_flag = cleared
```

All four together, and the gap suppression is the part most easily
missed. A sender restart produces a sequence discontinuity that looks
exactly like loss, and treating it as loss does two wrong things:
FEC/PLC concealment runs against a packet from a different encoder
generation, and §5.4 pads a hole that nothing actually dropped. The
marker exists to distinguish the two, so the receiver flushes rather
than conceals.

This is the second of the two decoder-reset triggers; the other is a
channel's own first packet. They are the same event as far as the
decoder is concerned — the far side is discontinuous with anything
already in its state — which is why they share a code path.

#### 2.1.2 Link indices (`0x0B`, `0x11`)

Each side keeps, per remote, a pair of link indices, both starting at
zero:

```
on accepted pong from remote:            # the key exchange (encryption
    remote.source_index      = pong[0x11]  # spec §3.2) did not refuse it
    remote.destination_index = pong[0x0B]

on sending audio / config request / config push to remote:
    packet[0x0B] = remote.source_index
    packet[0x11] = remote.destination_index

on receiving audio / config request / config push claiming remote:
    accept only if packet[0x11] == remote.source_index
               and packet[0x0B] == remote.destination_index
```

Pokes are sent with both bytes zero, and a pong is the poke echoed
back (§2.1), so between peers that follow this, every learned pair is
zero and the check always passes. A packet that fails the check is
treated exactly like one from no configured remote: an audio packet
is not attributed to that remote (§2.3a), and a config request or
push is ignored.

### 2.1.1 Byte order — mixed, not a uniform convention

There is **no single endianness convention across this protocol.**
Every multi-byte binary numeric field in the core header:

| Field | Offset | Size | Byte order |
|---|---|---|---|
| Sequence number | `0x02` | 2 bytes | **Big-endian** |
| Timestamp | `0x04` | 4 bytes | **Big-endian** |
| Payload length | `0x13` | 2 bytes | **Little-endian** |

That's the complete set — every other field in the header is either a single byte (no
endianness applies), a fixed magic constant, or an opaque byte array. A 2-vs-1 split, not
uniform either way: **do not assume** one field's byte order from another's — check this
table per field.

### 2.2 Timestamp field, dual meaning

The 32-bit timestamp at offset `0x04` serves two distinct purposes depending on opcode.
Each has a specific clock *source*, not just a numeric scale — the wrong kind of clock
produces plausible-looking values that behave wrongly under real conditions (sleep/wake,
NTP adjustment, multi-connection state).

**Ping/pong (opcodes 7–8)**: `monotonic_clock_seconds() × 10000.0`,
truncated/wrapped into the 32-bit field. The value in a ping is
echoed back in the corresponding pong.

- **A monotonic, boot-relative clock** (in Rust, `Instant`, not `SystemTime`). **Never
  wall-clock/calendar time.** Wall-clock
  time can jump backward (NTP correction, manual clock changes) or
  forward in large steps, which would corrupt round-trip latency
  measurements and could cause spurious values after any clock
  adjustment. `Instant`-based monotonic time has no such failure mode.
- Scale factor is exactly `10000.0` — 100µs
  resolution, matching the field's role as a round-trip-timing value,
  not a wall-clock timestamp meant to be human-readable.

**Audio (opcode 6)**: a sample-count clock, nominally 48000 Hz — exactly this shape, not a
per-channel or per-connection counter:

- **One global counter per frame-size bucket** (2.5/5/10/20ms — four
  counters total), shared across every channel currently using that
  bucket. Not per-channel, not per-connection, not per-remote.
- **Initialize each to `0` exactly once, at application startup.** Do not reset on
  connect, reconnect, or stream start; a channel joining an
  already-running bucket continues from that bucket's current value,
  it does not get its own zero-based sequence.
- **Increment by exactly `1` per encode call** on that bucket — the
  counter itself is a plain call count, not a sample count.
- **Multiply by the frame size in samples only at the point the value
  is written onto the wire** (120/240/480/960 for the four buckets
  respectively) — not at the increment site. These are two separate steps; collapsing
  them changes the wraparound arithmetic below.
- **Wrap the raw (pre-multiplication) counter at `⌊2³²/960⌋ − 1 =
  4,473,923`** — this specific threshold, not a per-bucket-scaled one.
  It's sized for the worst case (the largest bucket, 960 samples)
  and shared across all four buckets so the same guard covers every case:
  `counter × 960` must fit in 32 bits. A per-bucket threshold would also avoid overflow,
  but wraps at a different point.

### 2.3 Non-magic control messages

Two short, non-magic-prefixed UDP messages can arrive on the audio
port outside the header structure above. Both are answered to their
sender, never originated by an instance, and neither appears in
traffic between peers:

- **`hello`** — exactly 5 bytes, `"hello"`. Answered with the
  instance's product name and version as ASCII text.
- **`loss`** — 4 to 160 bytes beginning `"loss"`. Echoed back
  unchanged.

**Cascade must answer neither**: both are discarded like any other
non-magic packet. Any non-magic, non-matching packet is silently
discarded — there is no error response for malformed or unrecognized
input.

### 2.3a Traffic from no configured remote

A poke whose sender identity matches no enabled remote, and audio
whose source address (and link indices, §2.1.2) match no remote, are
dropped. Each is reported in the log, at most once per sending host
for the life of the process — a misconfigured sender repeats
continuously, and the first line says everything the rest would:

```
poke, identity matches no enabled remote:
    if it matches a DISABLED remote: drop silently
    else: drop; log once per host
          "Unhandled probe packets received from address <ip>.
           Check stream name/password"

audio, no remote claims it:
    drop; log once per host
          "Unhandled audio packets received from address <ip>,
           port <port>. Check stream name/password"
```

Nothing is shown in the UI for either.

### 2.4 Sequence and timestamp wraparound

Both the 16-bit sequence number and the 32-bit sample-timestamp are
subject to wraparound (the sequence number every 65536 packets; the
sample timestamp roughly every 25 hours at 48 kHz). **All continuity
and gap comparisons must use signed subtraction between the expected
and received values, not direct greater-than comparisons.** This is
the standard "serial number arithmetic" technique: subtracting two
wrapped counters yields the correct signed delta regardless of
wraparound, provided the true gap is small relative to the field's
full range (which it always is for consecutive packets). Bound the
computed gap to a sane maximum (on the order of a few hundred to a
thousand) before treating it as a genuine loss count, to guard against
a stream restart or corrupt packet producing a nonsensical delta.

---

## 3. Opcode Reference

| Opcode | Name | Payload |
|---|---|---|
| 6 | Audio data | Encoded/raw audio samples for one channel, one frame (see Audio Pipeline spec) |
| 7 | Ping ("poke request") | 32 bytes: sender's own 16-byte identity (§4), followed by peer's 16-byte identity |
| 8 | Pong ("poke response") | 32 bytes: sender's own identity, repeated twice |
| 9 | Config request | 16 bytes: sender's identity |
| 10 | Config push | 16-byte sender identity, followed by a JSON array of `{"c": <channel>, "l": "<label>"}` objects. Segmented — see §3.1. |
| 11 | Audio data (alias of 6) | Falls through to the identical code path as opcode 6. A genuine alias, not a distinct packet type — implementations should treat it identically to opcode 6. |
| other | — | Silently discarded |

Ping and pong packets are both exactly 53 bytes total (21-byte header
+ 32-byte payload). Config-request packets are 37 bytes (21 + 16).

### 3.1 Config-push (opcode 10) — a real segmented transfer protocol, not a single-shot message

Labels are not guaranteed to arrive in a single packet. The JSON label
array is split into **500-byte JSON chunks** (the final chunk carries
the remainder). A sending implementation should compute
`segmentCount = ceil(totalJsonLength / 500)` and split accordingly.

**Each segment repeats the 16-byte sender identity (§4) at the start
of its payload**, immediately after the 21-byte header, followed by up
to 500 bytes of the JSON label array. So on the wire a non-final
segment's payload is `16 + 500 = 516` bytes, and every segment — not
just the first — carries the identity. The 500-byte figure is the
*JSON chunk* size, not the on-wire payload size; do not confuse the
two. Reassembly strips the leading 16 identity bytes from each segment
and concatenates only the JSON chunks; the identity is not part of the
JSON stream — raw-concatenating full payloads with identity bytes
included fails to parse; concatenating only the post-identity chunks
parses cleanly.

A config-push transfer uses the segment fields at offsets `0x0F`/`0x10`
(§2.1) as follows. **Segment numbering is 1-based** — the first
segment carries segment-number `1`, and the total-segments field
carries the count `N` (so a single-packet push is segment `1` of `1`,
not `0` of `0`):

- The receiver tracks an expected-segment counter per remote,
  initialized to `1` at the start of a transfer.
- When a config-push packet arrives, its segment number
  (`packet[0x0F]`) is compared against the receiver's expected value.
  **A mismatch causes the entire packet to be silently discarded** —
  there is no reordering buffer and no partial-data recovery for an
  out-of-sequence segment.
- On a match, the segment's JSON chunk (payload after the leading
  16-byte identity) is appended to an accumulating buffer for that
  remote's label data.
- If the segment number just processed equals the transfer's total
  segment count (`packet[0x10]`), the transfer is complete: the
  accumulated JSON is finalized as the remote's current label set, and
  the expected-segment counter resets (to `1` for the next transfer).
- Otherwise, the expected-segment counter increments by one, awaiting
  the next segment.

**Implications for implementers**: a sender must number segments
sequentially starting at `1`, prefix every segment's payload with the
16-byte identity, chunk the JSON at exactly 500 bytes per segment, and
set the total-segment field to the true segment count on every segment
of a given transfer (not just the last one). A receiver that misses a
segment will silently drop every subsequent segment of that transfer
until a fresh transfer restarts the sequence from `1` — there is no
gap-filling or retransmission request built into this exchange
itself. Recovery instead relies on the periodic and pong-triggered
mechanisms below.

### 3.2 Label propagation and recovery

A locally-edited label list is never pushed to peers as a distinct operation. Pokes are
sent only by the regular, periodic poke loop; there is no dedicated push.

Three mechanisms exist, not one — but the first is a local revision
bump, not a network push:

- **A local edit updates a revision indicator immediately, with no
  debounce on this side at all.** No edit-time debounce timer exists.
  The 5-second figure belongs entirely to the *other* direction
  (below): how often a
  receiver may re-request config after noticing a mismatch, not how
  long an edit takes to be reflected locally. This update is local
  bookkeeping only regardless — it does not itself transmit anything.
  The updated value simply rides along on whatever poke the
  already-running periodic loop sends next (§7.2's normal ~1-2s
  cadence) — there is no separate "push" packet or dedicated
  transmission triggered by the edit itself.
- A label-change-revision byte at offset `0x0A` (§2.1) is present on
  both ping and pong packets — written once, by the ping sender's own
  current value, and carried through unchanged on the corresponding
  pong. A receiver compares this against its cached copy of the
  peer's last-known revision; a mismatch triggers an immediate
  config-request (opcode 9), without waiting for any timer. This is
  the primary, fast-path trigger for detecting label changes in
  practice, and it's what actually propagates the change — not the
  edit-time bump itself.
- **On first connection** (transition from status `0` to `2`): each
  side requests the other's config once, establishing the initial
  label state.

**So**: a label reload must bump the local revision immediately, with no debounce, and let
it ride the existing periodic poke cadence rather than triggering any transmission of its
own; the revision-mismatch check on the receiving side drives the actual transfer. A
proactive push must not be added: the poke cadence already carries the new revision within
its normal interval, and a push races the receiver's own request.

### 3.3 Label set size and content

A config push always carries a contiguous run of entries from channel
`1` (channels are 1-based in the JSON, distinct from the 0-based
audio channel number at header offset `0x12`), every entry present,
named or empty. How many entries is fixed at each **label reload** —
the same points that advance the label revision (§3.2): twice at
launch, on label and routing edits, and on every audio restart:

```
fn label_entries_at_reload():
    if an input device is running:
        return min(input_device_channels, 128)
    return 128
```

The count holds until the next reload — an input that stops later
does not change what is sent until something reloads the labels. So a
peer that asks before audio input has started receives 128 entries,
and one that asks after receives one per input channel.

**Content.** Each entry's `"l"` names what this side sends on that wire channel to the
requesting remote, derived per remote from that remote's send routing: a routed slot
carries its source channel's label (a `Ch N` placeholder until one is set), and an
unrouted slot is empty. Send routing is bounded by the same input channel count, so no
routed slot can fall outside the entries sent.

Two consequences for implementers:

- A **receiver** must tolerate any entry count from 1 to 128, and the
  count from one peer can change between pushes.
- Segment count tracks **total JSON length, not channel count**: a
  large set of short labels can pack into fewer segments than a small
  set of long ones. Compute segment count from byte length per §3.1,
  never from channel count.

---

## 4. Identity and Authentication

Each peer's identity, as exchanged in the ping/pong/config-request
payloads, is computed as follows:

```
trimmedName     = TRIM_WHITESPACE_AND_NEWLINES(stream_name)
trimmedPassword = TRIM_WHITESPACE_AND_NEWLINES(password)
identity = MD5( UPPERCASE(trimmedName) + UPPERCASE(trimmedPassword) )
```

- **Trim, then uppercase, applied to each field independently** —
  not applied to the combined string as a single operation.
- Concatenation has **no separator** between the two uppercased
  fields.
- When no password is configured, the trimmed/uppercased password
  contributes an empty string, reducing this to `MD5(UPPERCASE(TRIM(stream_name)))`.
- The resulting 16-byte MD5 digest is transmitted directly (not
  hex-encoded) in the relevant payload fields.
- This value is deterministic and does not change across
  reconnects — it depends only on configuration, not session state.

A peer is considered "matched" for a given ping/pong/audio exchange if
this identity matches what the local configuration expects for a
configured remote. **There is no distinct rejection message for an
identity mismatch or an unreachable peer** — both conditions produce
the same observable behavior: the receiving
side simply never responds, and the sending side's pings go
permanently unanswered. Implementations should not expect to
distinguish these cases from the wire alone; a connection-timeout
policy (§7) is the only defense.

---

## 5. Encryption

Specified in its own document, `CASCADE_ENCRYPTION_SPEC.md`. In outline:

- Only audio packets are ever encrypted; pokes, pongs, config requests and config pushes
  never are.
- The key exchange is not a separate packet type: it rides on the ordinary 53-byte poke as
  a 34-byte extension (87 bytes in all), present exactly when the remote has encryption
  on. The poke's two identity hashes (`0x15`, `0x25`) are present either way.
- Send: encryption off sends plaintext; encryption on with the key exchange not yet
  complete DROPS the packet — never plaintext.
- The encrypted payload is `nonce(12 bytes) ‖ ciphertext ‖ tag(16 bytes)`, with bit 7 of
  the flags byte set; the nonce travels with the packet.
- Receive: the encrypted flag must agree with the remote's setting both ways.
- There is no replay protection.

## 6. HTTP REST API (Monitoring Plane)

This section describes an optional monitoring plane a peer may offer. **It is not part of
Cascade**; Cascade's own HTTP interface is specified in §6.1. Nothing here affects the UDP
protocol — a peer need not implement either.

A minimal embedded HTTP/1.1 server, disabled by default, user-enabled
per instance with its own configurable port (defaulting to the same
value as the UDP port, `20102`, but independently changeable).

- **`GET /verify`** → `200 OK`, body `{"status":0}`. Liveness probe.
- **`GET /remotestatus`** → `200 OK`, JSON body:
  ```json
  {"remotes": [
    {
      "remote": "<name>",
      "port": <int>,
      "enabled": "yes"|"no",
      "status": <int>,
      "tx": <bytes>,
      "rx": <bytes>,
      "lost": <int>,
      "jitter": <float>,
      "host": "<address>",
      "latency": <float>
    }
  ]}
  ```
- Any other path or method: connection is silently dropped, no 404 or
  error response.
- Requests are parsed via a simple string-prefix match on the raw
  request line, not a full HTTP parser — implementations replicating
  this endpoint do not need to handle arbitrary HTTP method/header
  combinations, only recognize the two paths above.
- `tx`/`rx` byte counts include realistic UDP/IP packet overhead (28
  bytes per packet), not just payload size. **These counters are not
  gated on connection state** — they reflect raw socket I/O and begin
  accumulating (and are reported) before any ping/pong exchange
  establishes a connection. A non-zero `tx`/`rx` therefore does not
  imply a live peer.
- `latency` **is gated on an established connection**: it is reported
  as `"Unknown"` until at least one pong round-trip has been measured
  (§2.2, §7). Unlike `tx`/`rx`, a meaningful `latency` value requires
  the ping/pong plane to be up.
- `jitter`: for each received packet, compute the absolute difference
  between this packet's arrival-time gap and the previous packet's
  gap; track the running **maximum** such value per remote, reset
  periodically. This is not RFC 3550's jitter estimate — it uses only
  the receiver's local clock (no sender-timestamp comparison) and
  tracks a maximum rather than an exponentially-smoothed average. See
  `CASCADE_SESSION_STATS_SPEC.md` §2.4 for the full detail.
- `lost` is computed from sequence-number gaps (§2.4): whenever a
  received sequence number is not exactly one more than expected, the
  gap size (bounded to a sane maximum) is added to both a total-packets
  counter and a lost-packets counter.
- `status`: its range of values is not defined by this document.

### 6.1 Cascade's web interface

Cascade serves its web UI and API on `[api] bind`:`[api] port` (default `0.0.0.0:8080`),
with no authentication. The UI itself is the page at `/`; it talks to the daemon over a
WebSocket at `/ws` (settings changes, routing, and pushed events) and polls:

| Path | |
|---|---|
| `GET /api/status` | instance and per-remote status, statistics and buffer figures |
| `GET /api/peaks[?peer=NAME]` | meter snapshot; naming a remote keeps its receive-side metering running (`CASCADE_AUDIO_RECEIVE_SPEC.md` §9.2) |
| `GET /api/config` | the current settings |
| `GET /api/channels` | incoming and outgoing channel lists and labels |
| `GET /api/devices`, `GET /api/interfaces` | selectable audio devices and network interfaces |
| `POST /api/bitrate` | set the Opus bitrate |
| `POST /api/routing/{peer}/send`, `/receive` | set a remote's routing matrix |
| `POST /api/phase/{peer}` | turn Sync on or off for a remote |

---

## 7. Connection Lifecycle

### 7.1 Connection status — three states

A connection's status is one of three values:

| Value | Meaning | Transition condition |
|---|---|---|
| `0` | Down / not connected | Default; set on the 20-second timeout (§7.2), or immediately if the remote is disabled |
| `1` | Connected, address mismatch | A pong was received from this remote, but its source address does not match the configured/expected address for that remote |
| `2` | Connected, clean | A pong was received from exactly the expected address |

The address-match check happens on every received pong: if the
source address matches, status is `2`; if it doesn't, status is `1`
(this is a real, live state — not a warning overlay on top of `2`).
An implementation should compare the actual source address of each
pong against whichever address it currently believes is correct for
that remote, and set status accordingly on every pong received, not
just on the first one.

### 7.2 Timers and transitions

- On startup, or when a remote is added or enabled, pings (opcode 7) are sent to it every
  2 seconds, whether or not it is currently reachable.
- The **very next** ping is answered as soon as the remote peer's
  process becomes reachable and its identity matches — there is no
  separate "connect" handshake beyond the ordinary periodic ping
  landing on a now-live listener.
- A periodic monitor cycle checks each configured remote against
  **three distinct timers**, each serving a different purpose:
  - **20 seconds** — the primary disconnect threshold, checked against
    time since the last pong received. Once this elapses with no
    activity, status transitions to `0`.
  - **10 seconds** — checked against time since the last DNS resolution attempt (the clock
    restarts on each attempt, not on the last pong received). This re-resolves the
    remote's configured hostname, so a remote on a dynamic IP (e.g. DHCP) is found again
    after its address changes, without waiting for a full disconnect/reconnect cycle.
  - **5 seconds** — governs label/config re-request behavior; see
    §3.2.

  The 10-second DNS retry runs for as long as the remote's status is `0` (down), however
  long that is — there is no "down too long, stop retrying" state. A remote whose address
  changed while it was down is found whenever it returns.

  A remote whose hostname cannot be resolved stays configured, and its pings wait for an
  address. The failure is shown in the UI, and resolution keeps being retried every
  10 seconds until it succeeds or the remote is changed or disabled.

  All three timers operate independently; the shorter two are what
  make reconnection and label-sync self-healing in practice, not
  merely the disconnect threshold.
- **Disconnect has no distinct wire signature.** A graceful quit, a
  network interface being removed, and an authentication failure all
  produce identical behavior from the wire's perspective: the
  still-running side continues pinging on its normal schedule,
  indefinitely, into silence. Implementations must supply their own
  timeout policy; there is no rejection or teardown packet to key off
  of. This detection is entirely poll-based, worth being explicit
  about: there is no push notification or socket-level disconnect
  event that resets connection status directly — the 20-second timer
  above is checked periodically, not triggered by an event.
- Identity is stable across a full process restart on either side, as
  it is derived entirely from configuration (§4), not session state.

---

## 8. Summary Table — Constants

| Constant | Value |
|---|---|
| Default UDP port | 20102 |
| Magic bytes | `0x80 0x69` |
| Fixed header length | 9 (transmitted at offset `0x0C`) |
| Ping/pong packet size | 53 bytes (87 with the encryption extension) |
| Config-request packet size | 37 bytes |
| Ping/pong clock resolution | ~10 kHz (100 µs) |
| Secondary resync threshold | 10 seconds |
| Primary disconnect threshold | 20 seconds |
| UDP+IP overhead (for stats accounting) | 28 bytes/packet |
| Config-push JSON chunk size | 500 bytes per segment (on-wire payload is 516 = 16-byte identity + 500-byte JSON chunk) |
| Config-push segment numbering | 1-based (first segment = `1`, single-packet push = `1` of `1`) |
| Label set size | 128 entries until audio input starts; then one per input channel, up to 128 (§3.3) |
