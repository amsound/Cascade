# Cascade — Audio Receive Specification

This document specifies the receive-side audio pipeline: from packet
arrival through to samples being mixed into the output. For the wire
format, see `CASCADE_WIRE_PROTOCOL_SPEC.md`. For clock-skew correction
and the resampler, see `CASCADE_SYNC_MECHANISM_SPEC.md` — this
document covers the ring buffer and the decode path that feeds it, and
stops at the handoff point.

---

## 1. Pipeline overview

```
Packet arrival (network thread)
    → arrival gates (§1.1)
    → handoff to the channel's own serial decode queue (§2)
    → decode (Opus with loss concealment, or Raw16/Raw24)
    → post-decode buffer management (§5)
    → write into the channel's ring buffer, with its parallel timestamp array
    → [handoff to the sync mechanism, CASCADE_SYNC_MECHANISM_SPEC.md]
    → routing to outputs, mix, output stage (§7) → output device
```

### 1.1 Arrival gates

Every audio packet passes these on the network thread, in this
order, before any decode work is handed off:

```
remote = the remote whose address the packet came from
if remote is none
   or packet[0x11] != remote.source_index
   or packet[0x0B] != remote.destination_index:     # WIRE §2.1.2
    drop; report per WIRE §2.3a

count the packet: receive bytes (datagram length + 28),
                  loss and jitter (CASCADE_SESSION_STATS_SPEC §2.3/§2.4),
                  channel marked active

if datagram_length - header_length != payload_length   # exact
   or payload_length > 8192:
    stop here                        # no decrypt, no decode, no meter

if the encrypted flag disagrees with the remote's setting,
   or decryption fails:              # CASCADE_ENCRYPTION_SPEC §7
    stop here

decode and meter as below
```

`header_length` is `0x0C + packet[0x0C]` and `payload_length` the
little-endian field at `0x13`: the datagram must hold exactly the
header plus the declared payload, no more and no less. A packet
stopped after counting is still that remote's traffic — it shows in
the statistics and the channel list — but no sample of it is played
or metered.

Audio at a sample rate other than 48 kHz is not played. Cascade
never resamples to change rate and never silently changes the device
topology it was asked for.

---

## 2. Threading model

Packet arrival and decode are decoupled, the same way the send path
decouples capture from encode:

```
on packet arrival (network thread):
    copy the payload into a job
    channel.decode_queue.dispatch(job)     # never waits
        → process_audio_packet(job)         # on a worker, §3–§5
```

**Each incoming channel has its own serial decode queue** — channels
of one remote do not share a queue. The queue is created with the
channel, alongside its ring buffer and timestamp array, and the
dispatch goes straight to it: the network thread does no per-packet
lookup once a channel is known.

- **Serial per channel.** Jobs for one channel never run concurrently
  with each other and run in arrival order. Decode state (the Opus
  decoder, the last sequence number, the write side of the ring) is
  owned by that queue and needs no further synchronisation.
- **Parallel across channels.** Queues draw on a shared worker pool,
  so N active channels of one busy remote decode concurrently rather
  than through one per-remote choke point. The thread count does not
  scale with the number of queues.
- **Audio-adjacent priority.** The network receive thread and the
  decode workers run in the same elevated class (user-initiated QoS
  on macOS, `nice -10` where permitted on Linux), so neither preempts
  the other.

The network receive thread is a dedicated thread performing blocking
receives. It must not borrow a thread from the pool the decode and
encode queues use: a handler waiting for a pool thread leaves packets
unread in the socket buffer while it waits, which corrupts the arrival
timing that loss, jitter and the sync mechanism all measure.

### 2.1 Real-time constraints

Mixing into output (§7) runs on the device's real-time output
callback and must observe the same constraints as the send path's
capture callback (`CASCADE_AUDIO_SEND_SPEC.md` §2): no blocking on
non-real-time work, no unbounded allocation, no lock held across
decode.

- **The ring between decode and render is single-producer,
  single-consumer and lock-free.** The decode worker is the only
  writer; the render callback is the only reader.
- **No decode ever runs under a lock the render callback takes.**
  Each decode holds only its own channel's decode state, which no
  other context touches.
- **Hand-offs to the render callback are non-blocking on the render
  side.** New channels and replacement rings (a frame-size resize,
  §3.2) are posted to a small mailbox that the render callback drains
  with a try-lock; if it is momentarily busy the callback picks the
  change up on its next cycle.
- **Routing and configuration are published whole.** State the render
  callback reads but rarely changes — which channels exist, where each
  is routed — is prepared entirely off the real-time thread and then
  exchanged in one short step (§7.2). Nothing is computed or allocated
  inside that step.
- **Status reads never block the render callback.** Depth reports and
  other monitoring use try-locks or atomics and skip a cycle rather
  than wait.

---

## 3. Decode

Two input formats, selected by the codec in the packet (see
`CASCADE_WIRE_PROTOCOL_SPEC.md` for the wire-level field):

```
match packet.codec:
    Opus  => decode_opus(packet)                            # §3.1
    Raw16 => for each sample: output[i] = s16_le[i] as f32 / 32767.0
    Raw24 => for each sample: output[i] = s24_le[i] as f32 / 2^23
```

Raw samples are little-endian. A Raw24 sample is the top three bytes
of a 32-bit sample scaled by `INT32_MAX`, which is algebraically a
plain 24-bit little-endian sample over 2^23.

Raw audio carries no in-band redundancy: there is no FEC/PLC pass and
no concealment on a gap. A lost raw packet is a hole. Everything
downstream — §5's tree, the boxcar, the write, the splice — is shared
with the Opus path unchanged, with `fec_count = 0` and `lost = 0` by
construction.

### 3.1 Opus decode with loss concealment

**One persistent decoder per channel**, created with the channel, not
per packet. It is reset on the channel's first packet and whenever the
sender announces a restart (`CASCADE_WIRE_PROTOCOL_SPEC.md` §2.1.1):
both mean the stream on the far side is discontinuous with anything in
the decoder's state.

**Sequence handling.** The gap comes from the 16-bit sequence number,
not from timestamps:

```
if first packet, or sender restart:
    gap = 0                          # anchor; a restart's jump is not loss
else:
    d = (seq - last_seq) mod 65536
    if d == 0:          drop         # duplicate, no decode
    if d >= 64002:                   # behind or reordered by 1..1534
        last_seq = seq               # adopt it as the new baseline
        drop
    gap = d - 1                      # missing-frame count; 0 = in order
last_seq = seq
```

`gap` is the number of **missing frames**, not the raw sequence
delta: one missing packet is `gap = 1`. In-order packets have
`gap = 0` and never take the concealment branch. The threshold admits
any forward distance up to 64001 — a session restart can jump tens of
thousands of sequence numbers forward — and adopting the received
sequence number as the baseline before dropping a "behind" packet lets
a sender whose numbering restarted near zero be followed from its next
packet on.

**A pre-decode check.** Before decoding, the packet's samples-per-frame
is read from its TOC byte (arithmetic only, no decoder state, no
audio). A packet claiming more than 960 samples (20 ms at 48 kHz) is
dropped. The same value sizes the concealment request below.

**Concealment for a gap of 1–4 frames is two decode calls, into one
buffer, back to back, and their counts are summed:**

```
fn decode_opus(channel, packet, gap):
    single_frame_samples = opus_packet_get_samples_per_frame(packet)
    if single_frame_samples > 960: drop
    lost      = 0
    fec_count = 0
    if 1 <= gap <= 4:
        fec_count = opus_decode(channel.decoder, packet.payload,
                                buf[0..], frame_size: single_frame_samples * gap,
                                decode_fec: true)
        if fec_count < 0:                       # concealment failed
            fec_count = 0
            lost = single_frame_samples * gap   # §5.1 pads exactly this
    normal_count = opus_decode(channel.decoder, packet.payload,
                               buf[fec_count..], frame_size: 4800 - fec_count,
                               decode_fec: false)
    if normal_count < 0: drop
    frame_length = fec_count + normal_count
```

- The FEC call is asked for the whole gap in one request. Opus
  conceals the earlier missing frames with PLC and rebuilds the last
  one from this packet's in-band FEC (all PLC if the packet carries
  none).
- **Order is load-bearing**: the FEC call precedes the ordinary
  decode, which appends the packet's own audio directly after it. The
  ordinary decode requests the remaining space up to 4800 samples
  (100 ms); Opus returns however many samples the packet holds.
- **Never implement this as one call with a scaled frame size and no
  second decode** — that drops the arriving packet's own audio on
  every gap.
- **A failed FEC call does not drop the packet.** `fec_count` becomes
  0, the ordinary decode writes at offset 0, and only the concealed
  audio is lost — which is exactly what `lost` records for §5.1.
- **A gap of 5 frames or more is neither concealed nor padded.**
  Concealing a long outage would invent audio. The packet decodes
  normally and the buffer is left short (§5.1).

### 3.2 Frame-size detection and buffer auto-adaptation

No frame-size negotiation happens between peers. The receiver learns a
sender's frame size from the **ordinary decode's own returned sample
count** (`normal_count`, never the concealment-inflated total), checked
immediately after every successful decode. Detection is therefore the
decode result itself and can never disagree with what was decoded.

```
decoded = normal_count
if decoded not in {120, 240, 480, 960}: drop

if decoded > setpoint / 2:
    # GROW — the buffer cannot hold this frame comfortably
    resize(setpoint = max(configured_setpoint, 2 × decoded))
elif decoded < setpoint / 2 and setpoint > configured_setpoint:
    # SHRINK — only a previous GROW can leave the setpoint above the
    # configured one, so this undoes one and nothing else
    resize(setpoint = max(configured_setpoint, 2 × decoded))
else:
    record the frame size; no resize
```

`configured_setpoint` is the setpoint the buffer setting gives
(`CASCADE_SYNC_MECHANISM_SPEC.md` §2), with the platform period floor
applied (§13.1b).

- **The trigger is not "any change of frame size"** — the grow arm
  fires only when the frame exceeds half the current setpoint.
- **The grown setpoint is at least twice the frame size** — 2.5 ms →
  5 ms, 5 ms → 10 ms, 10 ms → 20 ms, 20 ms → 40 ms.
- **Adaptation runs in both directions.** Without the shrink arm, a
  link that briefly carries a large frame keeps the enlarged buffer
  for the rest of the session. Restoring
  `max(configured_setpoint, 2 × frame)` in one resize reaches the
  settled setpoint directly and costs one prebuffer re-arm. At buffer
  settings of 40 ms and above the shrink arm can never fire: the
  configured setpoint is already at least 1920 samples, which is twice
  the largest frame.

A sender switching frame size mid-stream, or a buffer setting smaller
than the sender's frame, is corrected on the first packet that needs
it. No signalling of frame size is required.

**The first ring.** A channel's first ring is sized for a 20 ms frame,
the largest carried, before any packet has been decoded. This decides
start-up memory only: the real frame size comes from the first decoded
packet, and the ring is sized to it then, before anything is written.

**What a resize does.** It is per channel: it reallocates that
channel's ring and timestamp array, sets that channel's setpoint,
overrun ceiling, capacity and boxcar window length, and clears the
averaging window and both direction flags. It touches no sibling
channel and no source-level state, so two channels of one source
receiving different frame sizes carry different setpoints. The new
ring reaches the render callback through the non-blocking mailbox
(§2.1).

**The resampler is never reset**, by this or by any other event.
It is created once per channel:

```
constructor, once:   setup(ratio = 1.0, nchan = 1, hlen = 32)
                     inp_count = 960 ; out_count = 2048
                     process                      # priming call

per resample cycle:  set_rratio, inp_count, out_count,
                     inp_data, out_data, process
```

None of the events that reset ring state — this resize, a buffer
setting change (§4.2), the Gate 1 release, the drain-to-empty re-arm,
or the averaging-window reset (`CASCADE_SYNC_MECHANISM_SPEC.md` §6.5)
— touches the resampler's phase accumulator or filter history. Those
describe the audio *stream*, which is continuous across every one of
these events; only the buffer holding it changed. Resetting the
resampler zeroes a valid filter history and restarts its output from
silence, turning a depth change into an audible one. The resampler
reads from whichever ring currently exists and holds no reference that
a reallocation could invalidate.

---

## 4. The ring buffer

One ring buffer per channel, with two parallel arrays and a set of
control fields:

| Field | Role |
|---|---|
| `capacity` | Ring size in samples: `2 × setpoint + 1920` (`CASCADE_SYNC_MECHANISM_SPEC.md` §2) |
| `write_index`, `read_index` | Ring cursors |
| `sample_data[capacity]` | The audio samples |
| `timestamp_data[capacity]` | **A parallel array, indexed identically to `sample_data`** — §4.1 |
| `depth_high_water_mark` | Running maximum of observed depth |
| `setpoint` | Target fill level |
| `overrun_ceiling` | Exactly `2 × setpoint` |
| `discontinuity_flag` | Set by an overrun; drives §5's recovery branch |
| `gate1_hold_flag` | The prebuffer hold (§4.2, Gate 1) |
| `last_decoded_frame_size` | The detected frame size (§3.2) |

**With Sync on, the buffer setting has a 20 ms minimum.** A smaller
value is raised to 20 ms when the setting is applied and saved. With
Sync off the value is used exactly as given.

### 4.1 Per-sample timestamps

Every sample gets its own timestamp, not just each packet:

```
fn write_to_ring_buffer(channel, samples, base_timestamp, original):
    for i, sample in samples.enumerate():
        idx = (channel.write_index + i) % channel.capacity
        channel.sample_data[idx]    = sample
        channel.timestamp_data[idx] = base_timestamp + i
    channel.write_index += samples.len()
    bound the tail timestamps by `original` (§12.2)
```

`base_timestamp` is the header timestamp minus the decoded count. The
sync mechanism depends on per-sample resolution directly
(`CASCADE_SYNC_MECHANISM_SPEC.md`): keep both arrays in lockstep from
the start rather than deriving timestamps from packet metadata later.

### 4.2 Read-side gating: two independent gates

There are two gates, not one shared flag. Which applies depends on the
per-source Sync setting, read once per render cycle
(`CASCADE_SYNC_MECHANISM_SPEC.md` §1). They gate two mutually exclusive
read paths, so a channel is only ever subject to one of them.

**Gate 1 — the prebuffer hold (Sync off).** Set when the channel is
created, on a full flush (a buffer-setting change), and by the
drain-to-empty re-arm (§5). While set, the non-Sync read path refuses
reads entirely, however much data is present.

It clears from the write side, evaluated after each packet is written:

```
if depth >= setpoint
   and (no source-level release condition, or it holds)
   and the channel's own release condition holds:
    gate1_hold_flag = false
    boxcar_running_sum = 0
    boxcar_write_index = 0
    boxcar_window_full_flag = false
# a condition not yet met defers the release to a later packet;
# it is never abandoned
```

Clearing the averaging window at release means a channel's
post-release averaging starts clean, carrying no pre-release history
(`CASCADE_SYNC_MECHANISM_SPEC.md` §2.2).

**Gate 2 — per-cycle readiness (Sync on).** Not a latched hold. It is
evaluated fresh every render cycle: the channel is ready if its
resample result for this cycle is available, and not ready otherwise.
A not-ready channel contributes nothing and its read cursor does not
advance (`CASCADE_SYNC_MECHANISM_SPEC.md` §7.2) — not silence written
into the mix, not a partial block, but an absent contribution. It
recovers on the next cycle a result is available, with no re-arm.

The Gate 1 hold still governs the Sync-on path indirectly: while it is
set, the resample job's recovery copy takes nothing from the ring, so
that job's result is never marked ready.

**The buffer setting is per source.** Changing it fans one new value
out to every channel of that source, as two calls per channel, in this
order:

```
source.buffer_ms = new_ms
for channel in source.channels:
    channel.set_latency(new_ms)   # reallocate both ring arrays; recompute
                                  # setpoint, overrun ceiling, capacity and
                                  # boxcar window length; clear the averaging
                                  # window and BOTH direction flags
    channel.flush_prebuffer()     # write_index = read_index = 0,
                                  # discontinuity_flag = false,
                                  # gate1_hold_flag = true
```

The pairing is required: `set_latency` allocates new arrays but does
not move the cursors, which would otherwise index a buffer of a
different size.

**The channel objects survive.** Neither call destroys or re-creates a
channel: the decoder keeps decoding across the change and the
resampler keeps its filter history and phase. The sender's timeline is
continuous across a buffer change; only the depth at which it is held
has moved. Tearing the channel down and rebuilding it restarts a
decoder mid-stream on a link that lost nothing and zeroes a valid
filter history, turning a depth change into an audible one.

### 4.3 Channel lifetime

A channel — its decoder, resampler, ring and timestamp arrays — exists
while its slot is **routed to at least one output the current output
device can play**. It is created when a packet arrives for such a slot
and destroyed when that stops being true: a route removed, the output
device changed to one with fewer channels, or the remote removed or
disabled.

**Connection state plays no part.** A channel whose routing has not
changed is kept, fully allocated and untouched, however long its
remote is disconnected. On reconnect it resumes with no rebuffering
beyond what §5's drain-to-empty re-arm does for any channel that ran
dry.

**Routing edits apply live.** A change to a remote's receive routing
updates each existing channel's output set in place (§7.2). Channels
that remain routed keep their decoder and ring — no re-warm, no
resampler restart. Channels that lost every playable output are
removed; a later re-route recreates them cleanly.

An unrouted slot decodes nothing. Its packets are still counted
(§1.1) and may still be metered (§9.2).

---

## 5. Post-decode buffer management

Immediately after each successful decode, before the frame is
committed to the ring, run this decision tree. It keeps the buffer at
its target depth under both clean and lossy conditions.

**Only two branches are Gate 1-specific** — the `gate1_hold_flag`
check and the `depth == 0` re-arm. Everything else — overrun handling,
gap padding, the concealment decode — is write-side management that
runs on both read paths. Disabling it under Sync would switch off
packet-loss handling on that path and leave only the ±0.2% resampler
correction to recover from a depth error.

```
fn post_decode_buffer_management(channel, frame, lost):
    depth = write_index - read_index          # wrap-corrected
    channel.depth_high_water_mark = max(channel.depth_high_water_mark, depth)

    if channel.gate1_hold_flag:
        # PATH A — still filling, or held since the last flush
        pad(channel, lost, depth, frame)      # §5.1
        write_prebuffer(channel, frame)       # §5.0: clamped, keep the tail
        check the Gate 1 release (§4.2)
        return

    if depth == 0:
        # drained to empty: an active recovery, treated as a fresh join
        channel.depth_high_water_mark = 0
        channel.discontinuity_flag    = false
        channel.gate1_hold_flag       = true   # RE-ARMED
        # this packet then takes PATH A exactly as above
        pad(...) ; write_prebuffer(...) ; check release
        return

    if channel.discontinuity_flag:
        # PATH B — overrun recovery
        if depth >= channel.setpoint:
            return            # discard this packet; the flag stays set,
                              # so the next packet checks again
        pad(channel, lost, depth, frame)
        write_whole(channel, frame)
        # The averaging history goes with the overrun that made it:
        # every depth in the window was recorded over the ceiling.
        channel.discontinuity_flag      = false
        channel.boxcar_running_sum      = 0
        channel.boxcar_write_index      = 0
        channel.boxcar_window_full_flag = false
        channel.adjusting_flag          = 0
        channel.skew_adjusting_flag     = 0
        return                # no boxcar update, no servo, no splice

    # PATH C — normal operation
    if depth >= channel.overrun_ceiling:
        channel.discontinuity_flag = true
        return                # discard this packet now; no write of any kind
    if a pending averaging-window reset (SYNC §6.5):
        reset the window; this packet is not measured
    else:
        update the boxcar with depth (SYNC §2.2)
    pad(channel, lost, depth, frame)          # only if this packet had a gap
    splice decision (SYNC §2.1), then write_whole(channel, frame)
```

**The overrun check discards in the same cycle.** A packet arriving
with depth at or above `2 × setpoint` is not written at all. The flag
it sets is consumed by the *next* packet's lower-threshold check,
which keeps discarding while depth is at or above the setpoint — not
just the ceiling — and so walks depth back down. Only then does
recovery pad (by §5.1's rule), write the frame whole, and clear the
flag and the averaging state. Carrying that state forward would leave
the average reading an inflated buffer for a full window after the
buffer recovered, and the sync mechanism would re-latch its reference
(`CASCADE_SYNC_MECHANISM_SPEC.md` §2.3) against that stale history.

Recovery pads only what a gap on the recovering packet itself lost.
Its headroom term counts the frame, so padding can never push depth
past the setpoint, and since this branch is only reached below the
setpoint, the overrun ceiling can never bind here.

### 5.0 The three write paths

| Path | Condition | Silence | Audio write |
|---|---|---|---|
| A | prebuffer hold set, **or** depth == 0 | §5.1 | **clamped to the setpoint, keep the TAIL** |
| B | discontinuity flag set (overrun recovery) | §5.1 | whole frame; then the boxcar and both direction flags are cleared |
| C | otherwise | §5.1 | whole frame, after the boxcar measurement and through the splice decision |

Path A does not feed the boxcar — nothing is being read yet, so there
is no depth history worth keeping. Path B discards while
`depth >= setpoint`, and the packet that finally recovers is not
measured either. Only path C feeds the boxcar
(`CASCADE_SYNC_MECHANISM_SPEC.md` §2.2), only path C can splice, and
only path C takes a pending §6.5 reset request — resetting the window
and leaving that packet unmeasured.

**Path A's prebuffer write:**

```
available = max(0, setpoint - depth)
keep      = min(frame.len(), available)
overflow  = frame.len() - keep
write_to_ring_buffer(channel, frame[overflow..], frame.timestamp + overflow,
                     original = keep)
# the FIRST `overflow` samples are dropped and the LAST `keep` kept
```

**Why A keeps the opposite end from B and C.** During a prebuffer fill
nothing reads the ring, so anything beyond the target is latency the
channel keeps for the rest of its life — the hold releases the moment
depth reaches the setpoint, and any overshoot is permanent. A
prebuffer should also begin playing the most recent audio it holds, so
the tail survives. On the other two paths the ring is live, its own
allocation is the only bound (§12.2), and the head survives.

**Path A's clamp does not bind on a clean fill.** Every setpoint is an
exact multiple of every frame size, so depth walks onto the target
rather than over it. It binds when concealment makes one arrival worth
several frames — most likely during the drain-to-empty refill that
follows a burst of loss.

### 5.1 Silence replaces what was lost

**Silence replaces what was LOST — it does not refill the buffer to
its setpoint.** The setpoint appears in the expression only as a
ceiling:

```
fn pad(channel, lost, depth, frame):
    silence = min(lost, max(0, setpoint - depth - frame.len()))
    insert_silence(channel, silence)
    depth += silence
```

`setpoint − depth − frame.len()` is the clamp, not the quantity.
Reading it as the whole formula produces an implementation that pads
on every gap, which is a different mechanism with different behaviour
under loss. `lost` is defined in §5.4.

Topping up to the setpoint on every gap instead injects invented audio
and adds latency the link never asked for, exactly when the network is
already struggling.

**Recovery from a fully drained buffer is a silent refill, not a
jump.** A channel reaching `depth == 0` is put back into exactly the
state of a fresh join: held silently, refilled from real frames in
whole-frame steps, released once depth reaches the setpoint through
ordinary accumulation. That takes time roughly proportional to the
buffer setting, and the channel is silent throughout, so recovery from
a long outage sounds the same as recovery from a short one. There is
no intermediate "catching up" state to display: the channel is either
re-buffering or recovered.

### 5.2 The render period tracks the buffer setting

The render callback's period follows the buffer setting (§13), and
that is what makes small frame sizes artifact-free. Because the period
shrinks with the buffer, one network packet arrives per render drain at
every frame size — 2.5, 5, 10 and 20 ms alike. The failure mode in
which several packets land before one drain and all but the first are
truncated needs a render period that stays wide while the frame size
shrinks, and that state never arises.

**Cascade must request a render period that tracks the buffer
setting.** Requesting a fixed platform-default callback size
reproduces the truncation failure even with §5.1 implemented exactly.

With one packet per drain, the settled depth tracks the buffer setting
across its whole range, at every frame size.

#### The reconfiguration sequence

When the shared callback period changes (§13.4), or when either audio
device changes, both directions are torn down and rebuilt. Instances
are never reused and properties are never re-applied to a running
stream: each configure creates a fresh instance.

```
stop_audio():
    remove the input device's listeners;  stop the input stream
    remove the output device's listeners
    release exclusive output access if held (§11)   # re-claimed by configure
    stop the output stream
    sleep(20 ms)                            # both stopped; settle
    dispose of both streams

configure_output():                         # configure_input() has the same shape
    device = the configured output device
    if device is none: return
    create a fresh output stream on it
    claim exclusive access if enabled (§11)
    set the device's nominal sample rate (§5.3)
    apply the stream properties and period (§13.1)
    initialise it — prepared, not started

start_audio():
    set the system power hint to "none"     # latency over power; re-asserted
                                            # on every rebuild
    configure_output();  add the output device's listeners
    configure_input();   add the input device's listeners
    reload labels (WIRE §3.3)
    start the input stream                  # input first
    start the output stream                 # then output
```

Both triggers run the same pair:

```
on_settings_loaded():                       # buffer or frame-size change
    shared = the shared period (§13)
    if shared != applied_period and audio_running:
        stop_audio()
        start_audio()

on_audio_device_changed(direction):
    write the new device to that direction's setting; save
    asynchronously:
        stop_audio()
        reload configuration
        set the new device's nominal sample rate (§5.3)
        sleep(500 ms)
        start_audio()
```

The 500 ms delay applies to the device path only. The settings path
has no delay beyond the 20 ms inside `stop_audio`.

**Requirements:**

- **Both directions are rebuilt whenever either changes.** There is no
  per-direction path. An input device change rebuilds the output as
  well, and vice versa.
- **Every reconfigure trigger goes through this one implementation.**
  Separate single-direction paths that stop, reconfigure and restart
  only their own stream leave the other running throughout — the
  drain-without-fill window this sequence exists to prevent.
- **Both directions are fully configured and initialised while neither
  is running, then started input first, output second.** Input
  delivering fresh samples before output begins draining biases any
  timing imperfection during the swap toward a temporary surplus.
- **The direction of that bias is the point.** A surplus recovers
  passively: ordinary drain returns depth to the setpoint, and the
  overrun path discards while depth is at or above it. A deficit does
  not recover: nothing adds samples beyond the arrival rate, and
  padding (§5.1) needs an actual sequence gap, which a clean link may
  not produce for a long time.
- **A stream must not render during the configure phase.** Where a
  platform API starts a stream as part of creating it, stop it the
  moment it is created, before configuring the other direction. Across
  a correct reconfigure, receive depth stays flat at the setpoint with
  no dip to recover from.
- **Do not compensate for a depth drop after the fact.** Marking
  channels discontinuous to force a corrective fill masks the ordering
  defect and costs an audible silence equal to the lost depth.

### 5.3 The sample-rate setting reconfigures the device

Cascade sets the hardware device's own nominal sample rate, not only
its own stream's requested format. When the device is not already at
48 kHz, configure sets it there before starting. Where the platform
cannot provide the requested rate and channel count on a device (ALSA
`hw:` devices, WASAPI exclusive mode), configuration fails and says so
rather than converting.

**The setting is not isolated from other applications sharing the
device**: changing the device's rate changes it for everything else
using that device.

### 5.4 Padding runs only on a detected gap

Padding (§5.1) is gated on a sequence gap detected on *this* packet.
`lost` is set once, by the decode (§3.1), and is non-zero in exactly
one case:

| Case | `lost` |
|---|---|
| in order (no gap) | 0 |
| gap of 5 frames or more | 0 |
| raw codec (no in-band redundancy) | 0 |
| Opus, concealment **succeeded** | **0** |
| Opus, concealment **failed** | `single_frame_samples × gap` |

So padding runs only in direct response to a gap detected on this
packet — never on ordinary, gap-free jitter, however low depth is. A
gap FEC/PLC concealed inserts nothing: it lost nothing needing
replacement. A gap too large to conceal inserts nothing either: the
buffer is left short and the depth servo
(`CASCADE_SYNC_MECHANISM_SPEC.md` §2.3) walks it back, a correction
spread over seconds rather than a step inside one packet. Only a
failed concealment is padded, and only by what it cost.


---

## 6. Loss and jitter

Loss and jitter are measured at packet **arrival**, in the network
dispatcher, before any routing check (§1.1) — see
`CASCADE_SESSION_STATS_SPEC.md` §2.3 and §2.4 for the formulas and
accumulators. Measuring inside a decode worker would count scheduling
latency as network jitter, and would measure nothing for an unrouted
channel, which never reaches decode.

The decode side keeps its own sequence tracking (§3.1) for concealment
and duplicate/reorder handling. Both are computed whether or not the
statistics are ever displayed.

---

## 7. Receive routing and mixing

Each remote has a receive routing table mapping each incoming channel
slot (0–127, the packet's channel field) to **any number** of local
output channels. A route is on or off; **there is no gain on a
crosspoint.** Nothing is routed by default: a slot with no route is
neither decoded nor played (§4.3).

Every remote has its own 128-slot space: several simultaneous remotes
each get their full 128 channels, not a share of one pool. No limit is
placed on the number of simultaneous remotes.

The routing a channel uses is resolved to a bit mask of the outputs it
feeds, **clamped to the live output device's channel count** — a route
to an output the device does not have contributes nothing, and a slot
routed only to such outputs has no channel at all. The unclamped
routing is kept, so it takes effect again on a device with more
outputs.

```
fn render(output, frames):                     # real-time callback
    output.fill(0)
    active[0..nch] = 0
    for source in sources:
        for channel in source.channels:        # routed channels only
            samples = channel.read(frames)     # direct ring read (Sync off) or
                                               # resampled staging (Sync on)
            if samples is none: continue       # not ready (Gate 1 / Gate 2)
            for out in channel.output_mask:
                output[out] += samples         # summed, no gain
            if channel.level > 0.001:
                for out in channel.output_mask: active[out] += 1
    output_stage(output, active)               # §7.3
```

Several channels can sum into one output. Routing applies identically
whether or not Sync is active; only the source of the samples differs
(a direct ring read, or the resampler's staging buffer).

`channel.level` is a smoothed peak of the channel's own output, used
only to count active sources:
`level = peak if peak > level else level × 0.98 + peak × 0.02`.

### 7.1 Nothing read when nothing is routed

A channel is only read while it has at least one playable output. A
slot with no playable route has no channel (§4.3), so no read occurs,
no cursor advances and no silence is written.

### 7.2 Routing updates are applied whole

A routing change must reach the real-time mixing thread as one
complete update, never cell by cell interleaved with live mixing,
which would risk torn reads and audible routing glitches:

```
fn update_receive_routing(remote, routes):
    store the full, unclamped routing for the remote
    # everything below the line is computed OFF the real-time path
    masks[slot] = clamp(resolve(routes, slot)) for slot in 0..128
    ---
    with the render state briefly held:
        for channel in remote.channels:
            channel.output_mask = masks[channel.slot]   # none → removed
    drop decode state for channels that were removed
```

Resolution and allocation happen before the render state is taken, so
the exchange itself is a handful of stores. A routing edit never
re-creates a channel that remains routed (§4.3).

### 7.3 Output stage

After mixing, each output channel passes through two stages, in this
order:

**Gain share.** An output fed by N active sources (`active[out]`, from
§7) is scaled by a gain that glides toward `1 / max(N, 1)`:

```
target = 1.0 / max(active[out], 1)
gain  += (target - gain) × 0.25          # per callback
if |gain - target| < 1e-4: gain = target
if gain == 1.0: leave the samples untouched
else: samples × gain
```

The glide avoids an audible step when a source starts or stops; at a
single source it settles at exactly 1.0 and the output is bit-exact.

**Catch limiter.** Per output channel, at −1 dBFS (threshold 0.891):

```
for each sample s:
    peak = |s|
    if peak > threshold: gain = min(gain, threshold / peak)
    if gain < 1.0:
        s    *= gain
        gain  = min(gain / 0.9998, 1.0)   # release
    # gain 1.0 and within threshold: untouched
```

Below the threshold with the gain recovered, samples pass bit-exact.
There is no hard clip.

Output metering (§9.4) samples after the limiter.

---

## 8. Sample-rate mismatch

Audio declared at any rate other than 48 kHz (the rate field,
`CASCADE_WIRE_PROTOCOL_SPEC.md` §2.1) is not decoded. It is still
counted and its channel still registers (§1.1), and a per-remote flag
records the mismatch so the UI can show it. Cascade does not resample
incoming audio to a different rate.

---

## 9. Peak-level metering (for UI backing data)

There are two receive-side meter sources, and each channel's
UI-facing value comes from exactly one of them.

**The post-buffer meter** covers routed channels. It is updated where
decoded audio is **written** into the ring (§4.1), immediately after
decode on packet arrival:

```
channel.peak_level = max(channel.peak_level, |sample|)   # per written sample
```

"Post-buffer" names where in the code it fires, not when relative to
playback: it does not wait for the audio to be read out, which would
delay it by the buffer depth. Updating it at the read/output side
instead makes routed channels' meters lag unrouted ones by roughly the
setpoint.

It exists only for channels that have a channel object — that is,
routed ones (§4.3). A channel being received but not routed has no
post-buffer meter; §9.2 covers it.

### 9.1 Presentation belongs to the UI

The meter data is a linear peak per channel, `0.0`–`1.0` and above.
How a viewer draws it — scale, ballistics, hold, colour — is the UI's
choice and not part of this specification. Cascade's own web UI draws
it on a dBFS scale with a peak hold.

### 9.2 The pre-decode meter

This covers the channels §9's meter cannot: those received but not
routed. It is fed from arrival, independently of the decode pipeline,
and only when both conditions hold:

```
on an incoming audio packet (after the §1.1 gates):
    if the channel is routed:            nothing — §9 meters it
    elif nobody is viewing this remote:  nothing
    else:                                hand the payload to the meter worker

meter worker (its own thread, never the network thread):
    match codec:
        Raw16 or Raw24:
            peak = max(|sample| for sample in the raw payload) / full_scale
        Opus:
            # a disposable decode into a scratch buffer, never the real
            # ring; one throwaway decoder per (remote, channel), dropped
            # after a few seconds unused
            peak = max(|sample| for sample in opus_decode(scratch, packet))
    pre_decode_meter[remote][channel] = max(itself, peak)
```

**Viewing.** Every meter poll names what it shows (the receive page
names its remote) and pushes that remote's "viewed until" deadline
about 1.5 s into the future. Any number of viewers keep the same
deadline alive; when the last one stops polling it lapses, and the
throwaway decoding stops with no action from anyone. The network
thread's whole cost for a packet that is not metered is one atomic
read; for one that is, one copy into a bounded queue it never waits on
— a full queue drops the job, because a meter missing one packet's
peak is invisible and a stalled receive thread is not.

### 9.3 Combining the two into one value

A single read selects, per channel, by the same routing state that
decides whether the channel decodes:

```
for each channel slot 0..127 of a remote:
    if the channel is routed:
        value = channel.peak_level           # §9
        channel.peak_level = 0.0
    else:
        value = pre_decode_meter[slot]       # §9.2
    output[slot] = value
    pre_decode_meter[slot] = 0.0             # unconditional
```

An inactive slot reads `0.0`: nothing caches a last-known value.

**One reader resets.** A single snapshot task performs this read on a
fixed cadence (40 ms) while any meter is being viewed, and publishes
the result; it is the only reader that resets the accumulators. Meter
polls return the latest snapshot and reset nothing, so every viewer
sees the same values however many there are. While nothing is viewed
the task does nothing, and its first cycle after an idle period drains
without publishing, so maxima from before the idle period never reach
a viewer.

There is no window where both sources are live for one channel and no
gap where neither is. **Never read a fixed source per view** ("this
page always reads the pre-decode meter"): select per channel at read
time.

### 9.4 Output meters

Output-channel peaks are sampled from the output buffer after the
output stage (§7.3) — what actually reaches the device — and published
by the same snapshot task.

---

## 10. Buffer status (for UI backing data)

The status of each channel is read from three values that already
exist: `depth` (§4), `setpoint` (§4) and `gate1_hold_flag` (§4.2).

These are gauges, not counters: reading them consumes and resets
nothing, unlike the accumulate-then-snapshot statistics of
`CASCADE_SESSION_STATS_SPEC.md`. They must be read without blocking
the real-time thread (§2.1) — a read that cannot get the value at once
skips a cycle.

---

## 11. Exclusive output-device access

**macOS (hog mode).** When the `exclusive_output` setting is on,
Cascade claims exclusive access to its output device through the
device's hog-mode property. This takes the device out of the shared
mix engine and gives a direct hardware path, lowering output latency.

**Output only.** The input device is always left shared, so other
applications can go on capturing from it.

**Property semantics**: the value is a process ID. Reading it returns
the owning process's ID, or `-1` if unowned. Writing the process's own
ID claims it; writing `-1` releases it. Ownership is process-wide and
enforced by the operating system: while held, no other application can
open the device.

- **The claim is best-effort, never fatal.** The device may already be
  held elsewhere or may not support hog mode. On failure, continue in
  shared mode and log it — refusing to play because an optimisation
  could not be applied is the wrong failure.
- **Verify the claim.** Read the property back after writing and
  confirm it holds this process's ID; a successful write alone does
  not prove the claim.
- **Release on every path that ends use of the device**: the setting
  turned off, an output-device change (release the old device *before*
  claiming the new one), stream teardown (§5.2), and process shutdown.
  A missed release leaves the device unusable by every other
  application until Cascade exits.
- **Report the held state, not the preference.** If the claim fails,
  the saved setting is corrected to match reality rather than retried
  forever against a state the user cannot see.
- **The preference persists across restarts**, but the claim is
  re-established and re-verified at every start, never assumed from a
  previous session.

**Windows** opens every device in WASAPI exclusive mode, so the output
is always exclusive and the setting has no further effect. **Linux**
opens ALSA `hw:` devices directly, which are exclusive by nature.

---

## 12. The ring write

### 12.1 Which bound applies where

Path A (§5.0) clamps its write at the setpoint and keeps the tail.
Paths B and C — essentially all traffic once a channel is playing —
write the whole frame, bounded only by the ring's allocation (§12.2).
At frame sizes where the splice cannot fire
(`CASCADE_SYNC_MECHANISM_SPEC.md` §2.1, §4), the overrun discard
(§5) is what bounds depth.

### 12.2 The bound: physical capacity, not setpoint

```
free = capacity - 1 - depth
n    = min(frame.len(), free)
write the FIRST n samples of the frame; discard the rest
```

- **The bound is the physical capacity, not the setpoint.** The
  setpoint is not a write-time ceiling for traffic on these paths; its
  roles are the Gate 1 release, the overrun ceiling, the recovery
  threshold and the padding clamp.
- **This bound keeps the head and drops the tail** — the reverse of
  path A's prebuffer clamp. The two drop different ends deliberately
  (§5.0).
- **One slot is reserved** in a wrapped-index ring, so the maximum
  depth is `capacity − 1`; without it full and empty are
  indistinguishable. A ring with monotonic (unwrapped) counters may
  use the full capacity; the one-sample difference is below every
  threshold in this document.

**The tail timestamps are bounded by the original count.** The write
takes a frame count and an original count, which differ only when a
splice (`CASCADE_SYNC_MECHANISM_SPEC.md` §2.1) has changed the frame's
length. When more samples are written than the sender sent, the final
`written − original` slots are rewritten walking backward from the
last one, with values descending from `base_timestamp + original − 1`:

```
if written > original:
    stamp = base_timestamp + original - 1
    slot  = last written slot
    while written > original:
        timestamp_data[slot] = stamp
        slot -= 1 ; stamp -= 1 ; written -= 1     # slot wraps to capacity-1
```

The highest timestamp in the ring is therefore exactly the last one the
sender transmitted, whatever a fill splice did to the frame's length.
That array is what the read cursor reports as `merge_time_stamp`
(`CASCADE_SYNC_MECHANISM_SPEC.md` §6.4); without the bound a fill would
advance the reported timeline past the sender's own.

### 12.3 Depth at write lands on whole frames

With one packet per drain and whole-frame writes, depth observed at a write takes values that are
multiples of the frame size (for example 0, 120, 240 and 360 at a
2.5 ms frame and a 240-sample setpoint). It never appears at or above
the overrun ceiling at a write, because a packet arriving there is
discarded before writing; it reaches the ceiling only momentarily,
just after a write that started below it.

---

## 13. The callback period

### 13.1 One shared value for both directions

Capture and playback are two separate streams on independently
selected devices. They share **one computed callback period**, applied
to both:

```
receive_half = §13.2
send_half    = CASCADE_AUDIO_SEND_SPEC.md §3a
period       = min(receive_half, send_half)
```

The smallest relevant setting anywhere — receive or send, on any
enabled remote — sets the period for all audio I/O, not just the
direction it was configured for.

### 13.1a macOS — the stream properties

Each stream is configured with these properties, in this
order, before it is initialised:

```
# input stream
EnableIO               scope Input,  element 1  = 1   # enabled
EnableIO               scope Output, element 0  = 0   # disabled
CurrentDevice          scope Global, element 0  = input device
StreamFormat           scope Output, element 1  = 48 kHz float
MaximumFramesPerSlice  scope Global, element 0  = period
BufferFrameSize        scope Input,  element 1  = period
SetInputCallback       scope Global, element 0
initialise

# output stream
EnableIO               scope Input,  element 1  = 0   # disabled
EnableIO               scope Output, element 0  = 1   # enabled
CurrentDevice          scope Global, element 0  = output device
StreamFormat           scope Input,  element 0  = 48 kHz float
MaximumFramesPerSlice  scope Global, element 0  = period
BufferFrameSize        scope Input,  element 1  = period
SetRenderCallback      scope Global, element 0
initialise
```

**Both period properties are required.** `MaximumFramesPerSlice`
bounds the stream's internal working buffer; `BufferFrameSize` is what
requests the device's callback period. Setting only the first does not
change the period. Both carry the same scope and element on both
streams, so the one shared period applies identically to each.
`BufferFrameSize` is set on the audio unit, at scope Input, element 1,
on both the input and the output stream.

### 13.1b Windows and Linux — request, accept, report

The driver chooses the period, so Cascade
follows one rule on every platform: **request, accept, report.**

- Request the shared period once.
- Use whatever period is granted, and size everything that depends on
  it from the grant.
- Log a difference once; never re-plan the period around a grant, and
  never retry to chase the request. A device proven to ignore requests
  is not rebuilt again for it.
- The one permitted retry is where the API grants nothing at all
  (WASAPI exclusive refuses a period it cannot use rather than
  rounding). Its order is: requested → byte-aligned → device default →
  device minimum.

On these platforms each assigned device is asked, when assigned, for
its shortest callback period. The longer of the input's and output's
answers is the **agreed period**, the shortest both devices can run:

- The request is never shorter than the agreed period.
- The outgoing frame size is raised to at least the smallest frame at
  least as long as the agreed period (20 ms is always offered), and
  the web UI offers no shorter one.
- Every channel's setpoint is floored at the **period floor**: twice
  the longer of the granted and agreed periods, rounded up to the next
  level a buffer setting produces (120, 240 or 480, then whole 960s).
  Each render callback takes one period from the ring, so a setpoint
  no deeper than one period would be emptied by every callback. A
  period of exactly half a buffer level returns that level, so the
  floor never raises a setpoint whose requested period was granted as
  asked. On macOS the floor is zero.

### 13.2 The receive half — from the buffer setting, never from frame size

```
receive_half = 480                          # default
for remote in remotes:
    if not remote.enabled: continue
    if remote.buffer_ms == 5:  receive_half = min(receive_half, 120)
    if remote.buffer_ms == 10: receive_half = min(receive_half, 240)
    # every other value — 7, 15, 20 and above — leaves 480
```

This is an **exact-match table, not a formula**. Deriving it from the
setpoint instead agrees at 5, 10 and 20-and-above but turns 7 into 120
and 15 into 240, holding the callback far shorter than specified for a
value the API and a hand-edited settings file both accept.

- **`enabled` is the only gate.** There is no test of whether a remote
  is connected or decoding. The value is a pure function of settings,
  fully determined before any audio flows, and does not move when a
  peer connects or drops. An activity gate would make the period read
  its default while nothing decodes and change when a peer starts,
  forcing a rebuild at connect time; the render callback stops for
  around 100 ms during that rebuild while packets keep arriving, and
  if the resulting overshoot lands in a channel's first three seconds
  it becomes the depth the sync mechanism calibrates against
  (`CASCADE_SYNC_MECHANISM_SPEC.md` §2.3).
- **The detected incoming frame size is never an input.** The ring
  grows to accommodate whatever frame arrives (§3.2), entirely within
  whatever period is already active. A receive buffer set to 5 ms
  holds the callback at 120 samples for the life of the connection,
  even after the ring has grown to hold a larger frame. The two
  mechanisms are independent; rebuilding the audio streams because a
  detected frame size changed is wrong.

### 13.3 The send half

```
send_half = 480                             # default
current   = the smallest send frame size in use (CASCADE_AUDIO_SEND_SPEC.md §3a)
if current == 5 ms:   send_half = 240
if current == 2.5 ms: send_half = 120
# 10 ms and 20 ms both leave the 480 default
```

### 13.4 Re-application only on a genuine change

Both halves are recomputed on every relevant settings change — adding
or removing a remote, changing a buffer or frame size, enabling or
disabling a remote — and once at audio start. Nothing recomputes them
on a timer or on a connection event.

The streams are rebuilt only when the combined period actually differs
from the one applied. Changing a setting that does not move the shared
minimum restarts nothing. When the rebuild does happen, it follows
§5.2's sequence exactly.
