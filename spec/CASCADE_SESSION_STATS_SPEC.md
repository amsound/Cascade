# Cascade — Session State & Statistics Specification

This document specifies the per-remote connection state machine and
the statistics Cascade must compute to track link health. It does not
cover the wire protocol itself (see `WIRE_PROTOCOL_SPEC.md`) or
codec/DSP behavior (see `CASCADE_AUDIO_SEND_SPEC.md`,
`CASCADE_AUDIO_RECEIVE_SPEC.md`, `CASCADE_SYNC_MECHANISM_SPEC.md`).
Presentation is explicitly out of scope — this document specifies the
values a UI layer would need, not how to display them.

---

## 1. Connection state machine

Three states, per remote:

| State | Meaning |
|---|---|
| `0` | Down / disconnected |
| `1` | Connected, address mismatch |
| `2` | Connected, clean |

### 1.1 Transitions into state 2 (clean connect) and state 1 (address mismatch)

Both decided exclusively on receipt of a ping response (pong), by
comparing the response's actual source address against the
configured address for that remote:

```
on pong received:
    matches = (source_address_of_this_pong == configured_address_for_remote)

    if matches:
        connectionStatus = 2
    else:
        connectionStatus = 1
```

State `1` is a real, meaningful condition, not a transient glitch: the
remote is reachable and responding, but from an address that doesn't
match configuration — most commonly a remote behind a different
public IP than expected (NAT, multi-homed host, DNS pointing
somewhere stale).

### 1.2 Transition into state 0 (disconnect)

Driven by a per-remote liveness timer, checked on every monitor
cycle:

```
elapsed = now - last_activity_timestamp[remote]   # last pong received

if elapsed >= 20.0 seconds:
    connectionStatus = 0    # primary disconnect transition
```

A second timer handles DNS re-resolution. It does **not** resend
label/config — that is a separate 5-second timer
(`WIRE_PROTOCOL_SPEC.md` §7.2, §3.2) — and it is **not** nested inside
the 20-second check above. It is an independent condition, gated on
`connectionStatus == 0` itself, and therefore checked *after* a
disconnect has occurred rather than only before one:

```
if connectionStatus == 0:                              # "down" -- not gated by elapsed time
    if (now - last_dns_resolution_attempt) >= 10.0 seconds:
        re-resolve this remote's configured hostname
        # recovers a remote on a dynamic IP (e.g. DHCP) without
        # waiting for a full disconnect/reconnect cycle
```

**This runs for as long as `connectionStatus` stays `0` — indefinitely,
with no upper bound and no separate "been down too long, stop
retrying" sub-state.** A remote disconnected for hours is retried on
exactly the same 10-second cadence as one that just went down seconds
ago. There is no elapsed-time window on this retry (e.g. "only within
some period after disconnect"); adding one changes the behaviour
specified here.

Neither the 10-second DNS timer nor the separate 5-second label timer
changes `connectionStatus` — both are recovery mechanisms, not state
transitions. Implement all three thresholds (20s disconnect, 10s DNS,
5s label) independently, with the 10s DNS retry gated on the
disconnected state itself rather than on elapsed time since the
disconnect transition — conflating any of them, or nesting the DNS
retry inside a time-bounded window, will produce incorrect recovery
behavior for a remote that stays down for an extended period.

### 1.3 Disabled remotes

A disabled remote is forced to `connectionStatus = 0` unconditionally,
overriding whatever the ping-response logic would otherwise compute.
Check "is this remote enabled" before evaluating 1.1/1.2, not after.

---

## 2. Statistics

Five values per remote: transmit rate, receive rate, packet loss,
jitter, and latency. All five follow one of two computation patterns
below.

### 2.1 The accumulate-then-snapshot pattern (tx, rx, loss)

Transmit bytes, receive bytes, received-packet count, and lost-packet
count are each tracked as a **raw, continuously-incrementing
accumulator**. On a periodic cycle (nominally every 2 seconds — see
§2.5), each accumulator is copied out to a **snapshot** value and then
reset to zero:

```
on each stats cycle, for each accumulator:
    snapshot = accumulator
    accumulator = 0
```

This produces a genuine per-interval rate/count, not a running total.
Implement accumulators and snapshots as separate fields — do not
compute rates by diffing two point-in-time reads of a single counter,
since that requires remembering the previous read and is exactly the
class of bug this two-field pattern avoids.

### 2.2 Transmit / receive rate

```
tx_mbps = (tx_bytes_snapshot × 8 / 1,000,000) × 0.5
rx_mbps = (rx_bytes_snapshot × 8 / 1,000,000) × 0.5
```

The `× 0.5` is the per-2-second-interval-to-per-second-rate
conversion (`1 / 2 = 0.5`) — it is **not** a half-duplex factor or
anything protocol-specific, and it must be adjusted if Cascade's own
stats cycle uses a different interval (see §2.5).

**What actually feeds `tx_bytes_snapshot` is narrower than "every
outgoing packet," and it's worth being precise about exactly what's
in and out**, since it's not the obvious "audio only" or "everything"
answer:

- **Included**: encoded audio packet transmission (every packet).
- **Included**: the 10-second deferred config-request resync
  specifically (a control-plane packet, but a rare one).
- **Included**: config-push (label) segment transmission.
- **Not included**: routine ping sends — the frequent, periodic
  keepalive traffic (roughly every 2 seconds) is excluded from this
  figure entirely.
- **Not included**: pong sends.

In practice, since audio dominates by volume whenever a call is
active, this rarely produces a visibly wrong-looking number — but an
idle session with only keepalive traffic running would show `tx_mbps`
at or near zero despite genuine outgoing packets on the wire.

### 2.3 Packet loss

Loss is detected by **sequence-number gap analysis** on every
received audio packet, per channel:

```
on each received audio packet:
    seq = packet.sequence_number          # wire field, big-endian
    expected = last_seen_seq[channel]

    if seq != expected + 1:
        gap = seq - (expected + 1)
        if 0 < gap <= 999:                 # sanity bound; larger gaps are
                                            # treated as a stream reset, not loss
            packets_expected_accumulator += gap
            packets_lost_accumulator += gap

    last_seen_seq[channel] = seq
```

The 999 bound matters: without it, a genuine stream reset (a remote
reconnecting with a fresh sequence counter) would register as a
massive, spurious loss spike. Two representations of loss are useful
to expose, computed from the same underlying accumulators:

```
loss_count = packets_lost_accumulator_snapshot            # raw integer

loss_percent = (packets_lost_accumulator_snapshot
                 / packets_expected_accumulator_snapshot) × 100
```

Both are legitimate; pick one consistently rather than mixing them —
a raw count and a percentage answer different questions ("how many
packets were lost" vs. "what fraction of traffic was lost") and
neither is more "correct" than the other.

**This runs regardless of routing.** The accumulation happens in the
packet-arrival dispatcher, alongside the jitter calculation below,
before any routing check (`CASCADE_AUDIO_RECEIVE_SPEC.md` §1.1).
Channel construction — and with it the decode/buffer pipeline — is
gated on routing (`CASCADE_AUDIO_RECEIVE_SPEC.md` §4.3), but loss
counting is not part of that pipeline: a channel received but never
routed to any output still accumulates loss statistics.

### 2.4 Jitter

The **maximum** observed deviation between consecutive
inter-packet-arrival times, per channel, since the last reset:

```
on each received audio packet:
    now = current_time()
    gap = |now - last_arrival_time[channel]|
    jitter_sample = |gap - last_gap[channel]|

    if jitter_sample > jitter_max:
        jitter_max = jitter_sample

    last_arrival_time[channel] = now
    last_gap[channel] = gap
```

**This is not an RFC 3550 jitter estimate, despite a surface
resemblance, and differs from it in two structural ways, not one.**
First, RFC 3550's `J` is an exponentially-smoothed running average
(gain `1/16`); this is a running **maximum**, which never decays and
is only reset by the periodic snapshot below — a single large gap
stays reported at full magnitude until the next reset, rather than
fading out over subsequent packets. Second, and more fundamentally,
RFC 3550's own `D(i,j)` compares actual arrival spacing against
*expected* spacing derived from the sender's own embedded RTP
timestamp — it measures transit-time variation. This calculation uses
**only the receiver's local clock** (`current_time()`), comparing
consecutive arrival gaps to each other with no reference to any
sender-side timestamp at all. It is a genuine, real jitter-adjacent
measurement — just not RFC 3550's, and an implementation should not
substitute the standard RTP formula expecting equivalent output. On
each stats cycle, snapshot and reset the same way as §2.1:

```
jitter_ms_snapshot = jitter_max × 1000    # seconds → milliseconds
jitter_max = 0
```

**This also runs regardless of routing**, like loss (§2.3): both are
computed in the packet-arrival dispatcher, before any routing
decision. A received but unrouted channel accumulates jitter data.

### 2.5 The stats cycle interval

A fixed-rate 2.0-second interval. Cycles do not drift with the time
each one takes to run. The `× 0.5` in §2.2 is `1 / interval_seconds`;
a different interval changes that constant to match.

### 2.6 Latency

Round-trip time, computed from a timestamp embedded in each
outgoing ping and echoed back in the corresponding pong:

```
outgoing_timestamp = round(current_time_seconds() × 10000)
# embedded directly in the ping's wire timestamp field

on pong received:
    rtt_ticks = (round(current_time_seconds() × 10000) - echoed_timestamp) / 20.0
    latency_ms = rtt_ticks    # already in ~2ms-resolution units by construction
```

The `× 10000` / `÷ 20.0` combination yields a result already
expressed in units of ~2ms (`10000 / 20 = 500` ticks per second =
2ms per tick) — no further unit conversion is needed once the
division by 20.0 is applied.

---

## 3. Summary — fields to expose

| Field | Type | Computed from |
|---|---|---|
| `connection_status` | enum (0/1/2) | §1 |
| `tx_mbps` | float | §2.2 |
| `rx_mbps` | float | §2.2 |
| `loss_count` | integer | §2.3 |
| `loss_percent` | float | §2.3 |
| `jitter_ms` | float | §2.4 |
| `latency_ms` | float | §2.6 |

Presentation, refresh cadence in a UI, and any historical/graphing
storage are UI-layer concerns, out of scope for this document.
