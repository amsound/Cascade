# Cascade — Audio Send Specification

This document specifies the send-side audio pipeline: from hardware
capture through to bytes leaving the machine. For wire format, see
`CASCADE_WIRE_PROTOCOL_SPEC.md`. For clock-skew correction and the resampler
(receive-side only — the send path has no equivalent), see
`CASCADE_SYNC_MECHANISM_SPEC.md`.

---

## 1. Pipeline overview

```
Hardware capture (real-time thread)
    → per-encoder buffering, frame-size-bucket readiness check
    → dispatch_async handoff off the real-time thread
    → per-encoder encode (parallel)
    → per-destination transmit
```

Four independent frame sizes are supported simultaneously —
**2.5ms, 5ms, 10ms, 20ms** at 48kHz, corresponding to 120, 240, 480,
and 960 samples per frame. Each destination is configured with
exactly one of these, and a channel sent to destinations with
different settings is encoded once per setting (§4.1); the pipeline
tracks readiness per bucket, not globally, since different encoders
may be ready at different points within a render cycle.

---

## 2. Threading model

Three distinct execution contexts. Do not collapse these into fewer
threads or a single queue — the separation is load-bearing for
real-time correctness.

| Stage | Context | Requirement |
|---|---|---|
| Capture, buffering, level metering | The real-time audio I/O thread | Must never block: no network I/O, no lock contention with non-real-time code, no unbounded allocation |
| Handoff after capture | A dedicated queue, distinct from the encode/transmit queue | One `dispatch_async` (or equivalent) hop off the real-time thread |
| Encode and transmit | A second, separate, **concurrent** pool at `USER_INITIATED` priority (or the platform equivalent) | Concurrent, not serial — every stream ready in a cycle is encoded in parallel across cores; transmission then follows in fixed channel order |

The real-time thread's only job is: read captured samples, run the
per-channel buffering/readiness check (§3), and hand off. Everything
downstream — encode, wire construction, transmission — happens off
that thread entirely.

**The capture thread takes no lock shared with any other thread.** What it reads — which
frame sizes each channel is buffered at, and each channel's streams and destinations — is
published by the configuration side as immutable snapshots and loaded atomically once per
callback; a change is a new snapshot, never an in-place edit.

---

## 3. Capture and frame-size-bucket readiness

**The invariant this section depends on, stated explicitly — buffers
are per encoder, but the timestamp a frame is sent with is per
bucket (§4).** That's only safe if every encoder assigned to a bucket
fills its own buffer on the same frame grid — because only then does
"same timestamp" genuinely mean "same sample position" for every
channel sharing it. Break this on the sender and the receiver has no
way to detect it: `merge_time_stamp` and the six-tier ladder
(`CASCADE_SYNC_MECHANISM_SPEC.md` §6.2) assume identical timestamps
mean identical sample positions by construction — a bucket that fires
with channels at different content offsets, all carrying one shared
timestamp, produces a receiver that measures perfect alignment and
correctly does nothing, forever. Every symptom of a violation appears
on the *receiving* machine, and no receive-side mechanism can detect
or correct it — the fix has to be on the sending side, at the point
this invariant is established or broken.

On each real-time callback, for every live encoder — one per
`(opus_application, latency, channel)` in use (§5):

```
frame_ready = encoder.buffer_audio(captured_samples[encoder.channel])

if frame_ready:
    match encoder.latency_ms:
        2.5  => mark_ready(bucket_2_5ms)
        5    => mark_ready(bucket_5ms)
        10   => mark_ready(bucket_10ms)
        20   => mark_ready(bucket_20ms)   # default
```

Each encoder buffers its own channel's samples, so one channel sent at
two frame sizes is buffered twice, once per size. Readiness
accumulates across all encoders within a cycle (a bucket is "ready"
if *any* encoder assigned to it has a full frame) — the send trigger
operates on the set of ready buckets, not per encoder directly. Under
the invariant above, every encoder in a bucket completes its own frame
on the same callback, so "any" and "all" coincide here and this
wording is harmless; without the invariant holding, this same wording
would silently permit exactly the violation described above.

**Keeping new encoders on the grid.** Encoders are created and freed
as the configuration changes (§5): an encoder exists only while at
least one destination uses its `(type, latency, channel)`, and a
freed one is never revived. A newly created encoder therefore starts
next to siblings that are already mid-frame, and it has to start on
their grid:

```
fn Encoder::new(..., latency, ...):
    # initial fill of the first frame buffer
    initial_buffer_size = fill of any existing encoder with the same
                          latency, or 0 if there is none
```

The fill is copied from a same-latency sibling on any channel —
every encoder in a bucket is on the same grid, so any of them gives
the right phase. With no sibling, the new encoder starts the bucket's
grid afresh.

An implementation may keep the grid by another means — for example a
capture-sample counter that every frame size's accumulator is primed
against (`fill = samples_captured mod frame_size`) whenever that
frame size comes into use on a channel. What must hold is the
invariant: every encoder in a bucket completes its frames on the same
callback.

If any bucket is ready, hand off:

```
dispatch_async(handoff_queue):
    encode_and_send(ready_buckets)
```

### 3.1 Enabled, and the connection-status gate

Two separate mechanisms stop audio reaching a remote, and they must not be merged:

**Enabled** is the user's per-remote switch. Disabling a remote removes it from the
configuration rebuild (§5.1): its references to encoders are released, and nothing is sent
to it or played from it until it is enabled again.

**The connection-status gate** is automatic. At send time, per destination, every cycle:

```
fn destination_ready(destination) -> bool:
    if destination.remote.connection_status == DOWN:   # exactly the fully-down
        return false                                    # value, 0
    return true                                          # 1 (address mismatch) and
                                                         # 2 (clean) both send
```

**This is what stops audio on disconnect without tearing anything down.** No encoder is
released and no routing changes: the destination, its encoders and their state stay intact,
and sending resumes the instant the status leaves `0`. It is a single comparison
re-evaluated every cycle, not a stateful enable/disable transition. **Only the fully-down
state (`0`) gates sending** — a remote connected from an unexpected address (`1`) is sent to
normally.

---

## 3a. The send-side half of the shared callback period

Capture and playback run through two separate device units, each bound to its own,
independently selected device — input and output hardware need not match. **What they
share is one computed value, the callback period**, which both apply as their buffer size.
`CASCADE_AUDIO_RECEIVE_SPEC.md` §5.2 and §13 specify the receive half, the reconfigure
ordering (both units prepared before either starts, input started before output) and how a
device change is handled; this section is the send half.

The output device alone may be claimed exclusively (`CASCADE_AUDIO_RECEIVE_SPEC.md` §11).
The capture device is always left shared, so other applications can go on capturing from it.

**The send half of the period**:

```
value = 480                        # default
current = the smallest send frame size in use (below)
if current == 5ms:   value = 240   # one 5ms frame
if current == 2.5ms: value = 120   # one 2.5ms frame
# 10ms and 20ms both leave the 480 default, the same value the
# receive side's two largest settings land on
```

`current` is the minimum, over every enabled remote that is sent at least one channel, of
that remote's own frame size (§4.1). Nothing routed leaves the send half at its default.

**The period applied to both units is `min(send_half, receive_half)`**, never shorter than
the shortest callback both assigned devices can run. The units are reconfigured only when
that combined value actually changes: changing a send frame size that isn't the binding
constraint (e.g. raising it while a receive-side remote is already smaller) restarts
nothing.

So the capture callback period is not "whatever this side's own send frame size implies":
it is the smallest frame size in use across send AND receive. One shared mechanism must
compute that minimum and reconfigure one shared capture+render period — not independent
capture-rate and render-rate logic that merely tend to agree.

---

## 4. Per-stream encode

A **stream** is one live encoder: `(opus_application, latency,
channel)`. For each stream whose bucket is ready, run concurrently:

```
fn encode_stream(encoder, bucket_timestamps):
    encoder.compress(
        bit_rate: global_encoder_bit_rate,
        timestamp_base: bucket_timestamps[encoder.latency_bucket],
    )
```

Then, per destination, send each of its streams' newly encoded
packets (§8).

### 4.1 Every destination is sent at its own settings

Frame size (`encoder_latency`) and mode (`opus_application`) are
per-destination settings, one value each, applying to every channel
that destination receives. **A destination is always sent every one
of its channels at exactly its own frame size and mode.** Nothing is
resolved across destinations: two destinations receiving the same
channel at different frame sizes are fed by two encoders, each
buffering and encoding that channel at its own size.

```
fn streams_for(destination):
    for channel in destination.channels_sent:
        yield get_or_create_encoder(
            type:    destination.opus_application,
            latency: destination.encoder_latency,
            channel: channel,
        )
```

Destinations that agree on both settings for a channel share one
encoder (§5).

The shared device callback period (§3a) follows from this: its send
half is the smallest `encoder_latency` among destinations that send
at least one channel.

---

## 5. Encoder lifetime

### 5.0 The key, and what scales

- **The key is `(type, latency, channel)`** — three fields. The
  destination is not part of it: destinations with the same settings
  for a channel share that channel's encoder.
- **The number of live encoders** is the number of distinct
  `(type, latency)` pairs in use per channel, summed over channels. At
  most 128 channels × 4 frame sizes × 2 modes; in practice one per
  sent channel when every destination agrees.
- Each encoder carries its own packet sequence counter and its own
  frame buffering (§3).

### 5.1 Get-or-create, with a reference count

The encoder set is rebuilt from the configuration whenever it changes
(a destination added, removed or enabled, its channels, frame size or
mode changed). Each destination holds one reference per channel it
sends:

```
fn rebuild(destinations):
    for destination in destinations:
        for channel in 0..128:
            if destination.enabled and destination.sends(channel):
                enc = get_or_create_encoder(destination.opus_application,
                                            destination.encoder_latency,
                                            channel)
                destination.set_encoder(channel, enc)   # releases the
                                                        # encoder it held
                                                        # for this channel,
                                                        # if different;
                                                        # refcount += 1
            else:
                destination.release_encoder(channel)    # refcount -= 1
    for enc in encoders:
        if enc.refcount == 0:
            free(enc)

fn get_or_create_encoder(type, latency, channel):
    for candidate in encoders:
        if candidate.latency == latency and candidate.type == type
           and candidate.channel == channel:
            return candidate
    enc = Encoder::new(type, latency, channel, bit_rate,
                       initial_buffer_size: §3)
    encoders.add(enc)
    return enc
```

### 5.2 Freed at zero, never revived

An encoder whose reference count reaches zero is freed at the end of
the rebuild: its Opus state and sequence counter are gone. If the
same `(type, latency, channel)` is needed again later, a **new**
encoder is created — fresh Opus state, sequence starting at 0, grid
fill per §3. A frame-size or mode change on a destination is exactly
this: the destination moves to a different key, the old encoder is
freed if nothing else uses it, and the new key's encoder is shared if
it already exists or created fresh if not.

A destination going fully disconnected (§3.1) changes nothing here:
the connection status is not part of the rebuild. Its encoders stay
referenced and keep running for as long as it remains configured and
enabled; only sending is gated.

### 5.3 Setting changes mid-stream

Changing frame size or mode moves a destination onto another
encoder (§5.2); changing the bitrate applies live to every existing
encoder through the encoder's own bitrate control — no rebuild:

```
fn compress(encoder, bit_rate, timestamp_base):
    if bit_rate != encoder.last_bit_rate:
        encoder.set_bitrate(if bit_rate != 0 { bit_rate * 1000 }
                             else { -1000 })   # -1000 = auto/VBR sentinel
        encoder.last_bit_rate = bit_rate
    # ... encode as normal, see §6
```

The receiver needs no advance warning for either: Opus's own bitstream
is per-packet self-describing (frame size and mode are declared in
each packet's own header byte; bitrate is not decoder-visible state at
all), and a stream moving to a different encoder shows at the receiver
as a sequence discontinuity, which its gap handling absorbs.

---

## 6. Encoder construction

```
fn Encoder::new(type, latency_ms, channel, bit_rate, initial_buffer_size):
    sample_rate = 48000    # only value used; encoder validates against
                            # {8000, 12000, 16000, 24000, 48000} but this
                            # protocol only ever constructs at 48000
    channels = 1            # mono

    application_mode = type
    if latency_ms < 20:
        application_mode = OPUS_APPLICATION_AUDIO   # forced override,
                                                       # regardless of
                                                       # the requested type
    # for latency_ms == 20 (the default), application_mode == type unmodified

    encoder = opus_encoder_create(sample_rate, channels, application_mode)

    # Bandpass pinned, unconditionally, on every encoder.
    encoder.set_bandwidth(OPUS_BANDWIDTH_FULLBAND)

    # §6.1's pair, under the gate described there.
    if latency_ms >= 20 and application_mode == OPUS_APPLICATION_VOIP:
        encoder.set_inband_fec(true)
        encoder.set_packet_loss_perc(1)

    if bit_rate != 0:
        encoder.set_bitrate(bit_rate * 1000)

    frame_size_samples = match latency_ms {
        2.5 => 120, 5 => 240, 10 => 480, 20 => 960,
    }
    return Encoder { encoder, frame_size_samples, initial_buffer_size, ... }
```

**These are the COMPLETE encoder configuration**, together with the bitrate-on-change path
in §5.3. Nothing sets complexity, DTX, signal type or LSB depth, so those keep their
libopus defaults. VBR on and constrained VBR on are the libopus defaults too; they may be
set explicitly to those same values for legibility, never to anything else.

**`set_bandwidth` is the hard setting (`OPUS_SET_BANDWIDTH`), not
`OPUS_SET_MAX_BANDWIDTH`, and it is not conditional.** Leaving the
encoder on `OPUS_AUTO` is not equivalent: the two agree only at
bitrates where auto would select fullband anyway. Below that, auto
narrows the bandpass and spends the bits on a cleaner, darker signal
while pinning keeps the full spectrum and accepts the artefacts —
audibly different output from the same settings.

**`initial_buffer_size` is a phase alignment, not a capacity.** It is the current fill of
any already-existing encoder sharing this one's latency — how far into the current frame
that latency bucket stands — and the constructor primes this encoder's first buffer to that
fill level with silence. A channel
routed at an arbitrary instant therefore completes its frames on the
same boundary as its siblings rather than offset by however far into
the frame period it was created. Zero when no such encoder exists, in
which case this encoder defines the bucket's phase.

Deriving the same quantity from a shared capture-sample counter
(`total_captured mod frame_size_samples`) is equivalent for every
observable purpose and additionally covers the no-sibling case.

**The sub-20ms override**: for the three faster settings, the encoder is forced into
`OPUS_APPLICATION_AUDIO` whatever application mode was requested — counterintuitive, since
`VOIP` is normally the one associated with *lower* algorithmic delay. Skipping it produces
audibly different encoding at low-latency settings. With §6.2 in force the override never
meets a Voice setting in practice; it stays as the encoder's own guarantee.

`application_mode` is a user/config-facing
setting distinct from anything transmitted on the wire — Opus's real
per-packet header (the TOC byte) encodes mode/bandwidth/frame-size/
channel-count and has no application-mode field at all. This setting
only affects local encoder behavior.

### 6.1 Forward error correction

Immediately after construction, two further encoder settings are
applied conditionally:

```
if latency_ms == 20 and application_mode == OPUS_APPLICATION_VOIP:
    encoder.set_inband_fec(true)
    encoder.set_packet_loss_perc(1)
# otherwise: neither is set; FEC remains off (the encoder's own default)
```

Both conditions must hold — the 20ms latency setting specifically
(not 2.5/5/10ms), and `OPUS_APPLICATION_VOIP` specifically (not
`OPUS_APPLICATION_AUDIO`). Bitrate has no bearing on this decision.
The expected-packet-loss value is the constant `1` — not configurable, and not derived from
any measured network condition.

### 6.2 Voice exists only at 20 ms — the settings rule

Because of the override above, Voice below 20 ms would be a setting
that never takes effect. The configuration never holds one: the two
settings are kept consistent at the moment either changes.

```
fn select_mode(destination, mode):
    destination.mode = mode
    if mode == VOICE and destination.frame_ms < 20:
        destination.frame_ms = 20          # Voice raises the frame size

fn select_frame(destination, frame_ms):
    destination.frame_ms = frame_ms
    if destination.mode == VOICE and frame_ms < 20:
        destination.mode = AUDIO           # a short frame leaves Voice
```

The adjusted value is saved like any other change. A stored Voice setting below 20 ms (a
hand-edited file) is read as Voice at 20 ms. The settings page offers only the 20 ms frame
size while Voice is selected, so the first rule is the one an operator meets; the second
applies to other routes in (the API).

---

## 7. The encode call

```
fn compress(encoder, samples, bit_rate, bucket_counter):
    apply_bitrate_if_changed(encoder, bit_rate)   # §5.3

    packet = build_header(
        opcode: AUDIO, flags: codec 0 (Opus),
        sequence: encoder.sequence,               # then advanced by 1
        timestamp: bucket_counter × frame_size_samples,
        sample_rate: 48000, channel,              # channel set per destination
    )
    encoded = opus_encode_float(encoder, samples, output_buffer_size: 1276)
    packet.append(encoded)

    transmit(packet)   # §8, once per destination
```

**Cascade sends Opus only.** The wire also defines two raw PCM codecs — codec byte `1` =
16-bit, `2` = 24-bit — which the receive side decodes
(`CASCADE_AUDIO_RECEIVE_SPEC.md` §3); they are never offered for sending.

**The timestamp**: `bucket_counter` is the frame-size bucket's shared counter
(`CASCADE_WIRE_PROTOCOL_SPEC.md` §2.2) — one per bucket, advanced once per cycle in which
that bucket has a frame ready, so every stream in the bucket stamps the identical value. It
wraps at `⌊2³²/960⌋ − 1 = 4,473,923`, so `counter × frame_size_samples` can never overflow
the 32-bit field at any frame size. **The sequence number** belongs to the encoder: one
counter per stream, starting at 0.

---

## 8. Transmission

```
fn transmit(packet, destination):
    if channel_number > 127:
        return   # reject — 128 is the structural channel ceiling

    # the destination remote's link indices (WIRE_PROTOCOL_SPEC §2.1)
    packet[0x0B] = destination.remote.source_index
    packet[0x11] = destination.remote.destination_index

    if destination.remote.encryption_on:
        if not key_exchange_complete:
            return   # DROP — encryption requested but not ready;
                     # never sent in the clear
        packet.flags |= 0x80   # bit 7 of the flags byte = encrypted
        packet.payload = encrypt(packet.payload)
    # encryption off: sent unencrypted, bit 7 clear

    udp_send(packet, destination_address)
    destination.transmit_bytes_accumulator += packet.len()
```

A destination with encryption on is never sent plaintext: until its
key exchange completes, its packets are dropped, not sent unencrypted.
Only a destination with encryption off is sent in the clear.
`CASCADE_ENCRYPTION_SPEC.md` §5 has the full gate and §5.1 the
encrypted payload layout.

---

## 9. Peak-level metering (for UI backing data)

Independent of the encode path, each channel's peak sample magnitude
is tracked for level-meter display purposes:

```
on each captured frame, per channel:
    for each sample:
        if |sample| > channel.peak_level:
            channel.peak_level = |sample|
```

On a periodic cycle, snapshot and reset each channel's peak (the same copy-then-zero pattern
as the statistics in `CASCADE_SESSION_STATS_SPEC.md` §2.1):

```
snapshot = channel.peak_level
channel.peak_level = 0.0
```

The accumulators are drained only by the one snapshot task that serves every meter viewer
(`CASCADE_AUDIO_RECEIVE_SPEC.md` §9.3); reading a meter never resets it. How a UI draws the
value — scale, ballistics, peak hold — is the UI's own choice.

Presentation is out of scope for this document; this section only
specifies the value a UI layer would need.
