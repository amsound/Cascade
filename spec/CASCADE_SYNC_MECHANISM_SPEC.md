# Cascade — Sync Mechanism & Resampler Specification

This document specifies clock-skew correction: how receive-side ring
buffers stay aligned, both across channels of one source and against
real-time playback, under clean and adverse network conditions. Every
constant is stated exactly — where a formula produces a value, the
formula is given; where a constant is a fixed literal, its exact value
is given. For the ring buffer's read/write mechanics and the decode
path, see `CASCADE_AUDIO_RECEIVE_SPEC.md`; this document picks up from
"a channel has decoded audio in its ring buffer" and covers everything
from there to "samples are ready to mix, correctly timed".

Cascade runs at 48 kHz only (`CASCADE_AUDIO_RECEIVE_SPEC.md` §8), so
every sample count below is at 48 kHz.

---

## 1. The gate: two paths, selected once per cycle

A single per-source flag — the remote's **Sync** setting
(`phase_lock` in the settings file, **Phase lock** in the UI) — read
once per render cycle, selects the correction
strategy for every channel of that source at once:

```
if sync_enabled == false:
    → Path B (§4) — ring buffer + rate-limited discrete splice,
                      no continuous correction
else:
    → Path A (§5–§7) — group-consensus resampling, every channel, every cycle
```

**Sync is per source, not global.** Each remote has its own setting,
and changing it fans the new value out to every channel of that
source (§5). Several remotes can run with different Sync settings at
the same time.

This is an either/or, not a blend: while Sync is off, none of the
continuous correction in §6–§7 runs, and no resampler is touched.
Both paths share the same ring buffer foundation (§2), including the
splice's dispatch and timer check — but the splice's effect is
Sync-off only (§2.1).

---

## 2. Shared foundation — the ring buffer

Applies identically whichever path is active.

**Setpoint**: the target depth, in samples, from the buffer setting:

```
setpoint_for(buffer_ms):
    if buffer_ms < 5:  120          # 2 × 2.5 ms frames
    if buffer_ms < 10: 240          # 2 × 5 ms frames
    if buffer_ms < 20: 480          # 2 × 10 ms frames
    else:              (buffer_ms / 20) × 960     # integer division: whole 20 ms steps

setpoint = max(setpoint_for(buffer_ms),
               2 × detected_frame_samples,       # RECEIVE §3.2
               period_floor)                     # RECEIVE §13.1b; 0 on macOS
```

**Capacity**: `2 × setpoint + 1920` samples. The `1920` is a fixed
headroom margin — exactly two 20 ms frames — and does not scale with
the current frame size.

**Depth**: `write_index − read_index`, wrapped by capacity — an
ordinary circular-buffer distance.

**Overrun policy**: audio that would overflow the free space is not
written — a passive drop, never an overwrite
(`CASCADE_AUDIO_RECEIVE_SPEC.md` §12.2). Nothing already resident in
the buffer is touched by a write.

**Reconfiguration**: changing the buffer setting on a live channel is
a full flush — both indices zeroed, the prebuffer hold re-armed
(`CASCADE_AUDIO_RECEIVE_SPEC.md` §4.2). It produces an audible pause:
the channel drops to empty and refills to the new setpoint before
resuming.

### 2.1 The discrete zero-crossing splice — dispatched always, effective only while Sync is off

Two layers, which must not be collapsed into one claim either way.

**The dispatch and timer check run on every normal-path packet,
whatever the Sync state.** A normal-path packet is one that reaches
the averaging step of `CASCADE_AUDIO_RECEIVE_SPEC.md` §5 (path C): not
a prebuffer packet, not an overrun-recovery packet, not a discarded
overrun. It runs after that packet's own measurement and relay update
(§2.2, §2.3), so the splice acts on the direction this packet
produced:

```
elapsed = now - last_successful_splice_time

if elapsed < 0.5 s:
    plain copy into the ring, unspliced
else:
    count = attempt_splice(frame)
    if count != 0 and averaging_window_full:
        last_successful_splice_time = now
        fold the splice out of the averaging window (below)
```

**`attempt_splice` checks Sync state at its own entry:**

```
fn attempt_splice(frame):
    if sync_enabled:        return 0     # no effect while Sync is on
    if adjusting_flag == 0: return 0     # nothing to correct
    search the frame for the quietest segment between two consecutive
    positive-to-negative zero crossings (below)
    if found:
        drop that segment (adjusting_flag = DRAIN) or
        repeat it          (adjusting_flag = FILL)
        return the number of samples dropped or inserted
    return 0                              # plain copy
```

`adjusting_flag` is §2.3's relay output, computed on every measured
packet with the deadband matching the Sync state at that moment —
here always the Sync-off ±4 ms band, since this is the only state in
which the splice acts.

**The last-splice time advances only on a real splice** — when the
splice changed the frame's length *and* the averaging window is full.
It never advances on a throttle pass, on entry to the attempt, or on
an attempt that found no crossing. Gating on Sync state before the
throttle, rather than inside `attempt_splice`, is equivalent only
under this exact rule.

**The search:**

```
# Phase 1: locate a starting point
scan forward from the start of the frame for the first
positive-to-negative zero crossing (sample[i] >= 0, sample[i+1] < 0).
This only establishes where phase 2 begins.

# Phase 2: bounded search for the best consecutive-crossing pair
window_start = int(48000 × 0.0025) = 120
window_end   = int(48000 × 0.005)  = 240
# all scoring arithmetic is single precision

energy = sample[first_negative]^2   # the sample just after the phase-1
                                     # crossing counts, as every later
                                     # segment's first sample does
absolute_position = 0               # samples scanned in phase 2; never resets
segment_start     = 0               # absolute_position at the last candidate
best_score = 2^63
best_pair  = none
for each subsequent pair (sample[i], sample[i+1]) after the phase-1 crossing:
    absolute_position += 1
    if NOT (sample[i] >= 0 AND sample[i+1] < 0):
        energy += sample[i+1]^2      # running loudness of this segment
        continue
    # a positive-to-negative crossing candidate
    segment_length = absolute_position - segment_start   # since the
                                                           # PREVIOUS candidate
    if segment_length < window_start or segment_length >= window_end:
        energy = sample[i+1]^2
        segment_start = absolute_position
        continue
    score = energy / segment_length  # mean squared amplitude of the segment
    if score < best_score:           # strictly lower: a tie keeps the earlier
        best_score = score
        best_pair  = (segment_start, absolute_position)
    energy = sample[i+1]^2           # every candidate restarts the segment,
    segment_start = absolute_position    # win or lose
```

- The window bounds the **segment length between two consecutive
  candidates**, not either candidate's position in the frame. That
  length becomes the splice amount: **120–239 samples**, weighted
  toward the low end, varying with the waveform. A fixed one- or
  few-sample adjustment is not this mechanism.
- **Measure each segment from the previous candidate, not from the
  start of phase 2.** Measuring from phase 2's start gives a larger
  divisor once more than one candidate has passed and changes which
  pair wins on material with several crossings.
- The quietest segment wins — lowest mean-squared amplitude between
  two consecutive crossings — not the first crossing found or the one
  nearest a window edge.
- The energy sum rounds the way the architecture's multiply-add does:
  on aarch64 the multiply and add are one fused operation with a
  single rounding; elsewhere the product is rounded before the sum.
  The two can differ in the last bit, which only matters to a
  near-tie between candidates.

**Drain and fill:**

```
# drain (remove gap = later - earlier samples):
copy source[0 .. earlier] to output
copy source[later .. frame_end] to output
# the segment between the crossings is never copied
# net length = original - gap

# fill (insert gap samples):
copy source[0 .. later] to output
copy source[earlier .. frame_end] to output
# the segment is copied a second time
# net length = original + gap
```

Both use the same pair from the same search; direction changes only
what happens to the segment. The write carries both the new frame
count and the original count, so the ring's timestamps stay bounded
by what the sender sent (`CASCADE_AUDIO_RECEIVE_SPEC.md` §12.2).

**The splice operates on the triggering packet's decoded length** —
`fec_count + normal_count` from `CASCADE_AUDIO_RECEIVE_SPEC.md` §3.1
— not the configured frame size:

- On a gap-free packet this is the frame size: 120 samples at 2.5 ms,
  240 at 5 ms, 480 at 10 ms, 960 at 20 ms.
- On a packet that recovered missing frames through concealment it is
  larger, by up to four frames.

So **below a 20 ms frame size the splice is unreachable on clean
traffic** — a 120- or 240-sample frame cannot contain a 120–239-sample
segment plus its bounding crossings — and becomes reachable only on
gap-recovery packets. At 20 ms and above the frame contains the
search window, and the splice corrects ordinary Sync-off drift with no
loss required.

**A completed splice is folded out of the boxcar at once.** Removing
audio makes every depth already recorded in the averaging window
overstate the buffer by the amount removed, so each entry has that
amount subtracted and the running sum is decremented to match; entries
that would go negative are left unchanged. A fill is folded the other
way: every entry has the inserted amount added. The triggering packet's
own entry is included — it was recorded before the splice. This lets
the relay release on the very next packet instead of waiting for the
removal to wash through a full window; without it the average keeps
reading the pre-splice depth, DRAIN stays armed, and the 0.5 s throttle
lets several more splices fire against a correction already applied.
The fold runs only on a splice that changed the frame's length, and
only with the window full.

**Net effect**: the 0.5 s check and the call into `attempt_splice`
happen on every normal-path packet whatever the Sync state, but no
sample is inserted or removed while Sync is on — Path A's ratio
correction (§6–§7) is the only timing mechanism in that state. At
most one splice per 0.5 s; every other packet is a plain copy. Both
directions are reachable.

### 2.2 The packet-arrival boxcar average

Computed on packet arrival, on the normal path
(`CASCADE_AUDIO_RECEIVE_SPEC.md` §5, path C) only. These packets are
never measured:

- a packet discarded by the overrun check;
- every packet written while the prebuffer hold is set, including the
  one that releases it;
- the packet that recovers from an overrun;
- the packet that takes a pending §6.5 reset request — it resets the
  window and is not added to the fresh one.

An unmeasured packet contributes nothing — not a zero; the window
simply does not advance.

```
window_samples = keyed to the SETPOINT, set only where the setpoint is set:
    150  at a 960-sample setpoint and above   (20 ms and up)
    300  at 480                               (10 ms)
    600  at 240                               (5 ms)
    1200 at 120                               (2.5 ms)

on each measured packet:
    value = depth_after_padding + frame_len    # before the splice
    if window_full:
        running_sum -= history[rolling_index]  # the oldest entry
    history[rolling_index] = value
    running_sum += value
    rolling_index += 1
    if rolling_index == window_samples:
        rolling_index = 0
        window_full = true                     # the transition §2.3 latches on

boxcar_average = running_sum / (window_samples if window_full else rolling_index)
    # integers throughout; the division truncates
```

- **The window length is keyed to the setpoint, not to the incoming
  frame size**, and is written only where the setpoint is written —
  when the buffer setting changes, or when a frame-size change is
  large enough to resize the ring (`CASCADE_AUDIO_RECEIVE_SPEC.md`
  §3.2). A frame-size change that keeps the ring leaves it alone. The
  two keys coincide only while the sender's frame size matches the
  setpoint's tier, and the difference matters: re-keying resets the
  window, a reset re-arms §2.3's latch, and a frame-size switch
  disturbs depth by up to one frame — so a window keyed to frame size
  would re-latch the reference onto that disturbance and the servo
  would then defend it. Keyed to the setpoint, the window survives the
  switch, the reference holds, and correction walks the buffer back
  where the active path can.
- **`frame_len` is the frame as decoded** — concealed frames plus the
  current one — before §2.1's splice. A splice on the same packet is
  folded out afterwards (§2.1).
- **The pushed value is this computed sum, not a depth read back from
  the ring after writing.** The two differ whenever the write is
  bounded: a value read back after a bounded write can never exceed
  what the ring holds, which would leave §2.3's `hi` threshold
  unreachable in exactly the cases it exists for — for example 2.5 ms
  frames over a 5 ms buffer, where `setpoint + frame_len` lands right
  at `hi`.

The result, compared with hysteresis (§2.3), produces
`adjusting_flag`, which drives both the splice (§2.1, Sync off) and
the common-mode backstop (§6.3, Sync on) — one signal, computed once.

### 2.3 The deadband and hysteresis state machine

```
base_deadband = 576 samples (±12 ms)   if sync_enabled
              = 192 samples (±4 ms)    otherwise

deadband = min(base_deadband, setpoint / 2)

hi = calibrated_reference + deadband
lo = calibrated_reference − deadband
```

**Both deadbands are operative.** The comparison runs on every
measured packet once the window is full, whatever the Sync state;
Sync only selects which deadband applies to *this* packet. With Sync
on the result feeds the common-mode backstop (§6.3); with Sync off it
gates the splice (§2.1), at the tighter ±4 ms. A Sync-off correction
gated on ±12 ms would respond far less to real drift.

**`calibrated_reference` is per channel and is not the setpoint.**
Conflating them gives a state machine that behaves correctly most of
the time and diverges in exactly the window that matters:

```
calibrated_reference = 0                   # at channel construction

on the packet where the window FIRST becomes full (the transition from
partially filled to full, not any later wraparound):
    calibrated_reference = boxcar_average
```

- For a channel's first `window_samples` measured packets — about
  three seconds — the reference is 0, so `hi` is the deadband alone
  and DRAIN arms far more readily than a setpoint-relative formula
  would.
- After the first fill, the reference holds what the channel's
  average was during those first seconds — a start-up-dependent value.
  Two identically configured channels can have different thresholds
  because of what the network was doing when each joined.
- **It is bounded.** It is the same field §6.6 calls `skew_reference`,
  and with Sync on §6.6 holds it within one band of the setpoint.

**The latch is armed by the window's own `filled` state**, tested
immediately before the update that sets it; there is no separate
"calibrated" flag. **Every site that clears `filled` re-arms the
latch**, and each also clears both direction flags:

- the buffer-setting change (`CASCADE_AUDIO_RECEIVE_SPEC.md` §4.2);
- the Gate 1 release (§3.1);
- the overrun recovery (`CASCADE_AUDIO_RECEIVE_SPEC.md` §5);
- the §6.5 reset request.

A stale reference cannot survive a window reset. This matters most
when the setpoint moves under a live channel: a frame-size change
large enough to resize the ring re-arms the prebuffer and resets the
window, and the channel then settles at a new depth. Latching once
per channel lifetime would strand the reference at the old operating
point — the average permanently outside the band, DRAIN never
released, and the Sync-off splice firing continuously on a link with
no drift at all.

**The reference is also adjusted at run time** by §6.6, which adds or
subtracts one render period's worth of samples.

Every threshold here — `hi`, `lo`, and the release conditions — is
relative to `calibrated_reference`. The setpoint's only role is sizing
the deadband through `min(base, setpoint / 2)`, read from the real
setpoint, not the calibrated reference.

**Hysteresis**: the relay trips at the deadband edge but releases only
when the average returns all the way to the reference, not merely
back inside the band. The average is §2.2's truncated integer:

```
IDLE  → DRAIN  when boxcar_average >= hi
IDLE  → FILL   when boxcar_average <= lo
DRAIN stays DRAIN while calibrated_reference <= average < hi
DRAIN → IDLE   once average < calibrated_reference
FILL  stays FILL while lo < average <= calibrated_reference
FILL  → IDLE   once average > calibrated_reference

adjusting_flag = 0 (IDLE), 1 (FILL), 2 (DRAIN)
```

All three values are produced; both directions are reachable.

---

## 3. How a channel enters sync

Both paths share one join: the prebuffer hold of
`CASCADE_AUDIO_RECEIVE_SPEC.md` §4.2.

### 3.1 The hold, released at the setpoint

```
On join, the channel is held (emits nothing) while its buffer fills in
whole network-frame steps: 0 → 960 → 1920 → ... → setpoint.
No partial frames are released early.

When depth reaches the setpoint, the channel is released and begins
emitting from wherever that landed. No flush, no anchor timestamp, no
resampling — release is a pure scheduling event.
```

The release condition and its mechanics are
`CASCADE_AUDIO_RECEIVE_SPEC.md` §4.2's Gate 1 in full. Release also
zeroes the boxcar's running sum and index (§2.2) — the same state §6.5
resets — so post-release averaging starts clean.

**Why this alone aligns channels**: every channel of one source
carries the same sender timestamps, so releasing each at the same
depth against that shared timeline aligns them as a structural
consequence. A joining channel's small residual offset then converges
passively with Sync off — scheduling only, no sample altered.

**The steady-state sawtooth is bookkeeping, not a rate change.** With
a 480-sample render period and 960-sample (20 ms) frames, every other
pull drains one frame, so depth sawtooths by ±480 around the setpoint,
and the per-channel timestamp advances by 479 or 481 on alternate
pulls — exactly 480 per pull on average.

### 3.2 Sync on uses the same hold

A Sync-on join goes through the same held-until-setpoint phase:

1. **The hold is set at construction whatever the Sync state.** Every
   channel starts held.
2. **The Sync-on path respects it.** The recovery copy that feeds the
   resampler (§7.2) takes nothing while the hold is set, so the
   resample job never marks a result ready and the channel contributes
   nothing.

The release lives in the write-side packet handler and never inspects
Sync state. **Every channel holds until its own depth reaches the
setpoint and releases at exactly that depth.** There is no separate,
threshold-free Sync-on join.

**Consequence**: every channel of a source releases at the same depth
relative to its own write position, and every write position tracks
the same sender clock, so every channel's read position — and so its
`merge_time_stamp` (§6.4) — lands at the same point on that clock the
moment it releases. A channel that prebuffered with Sync off and one
that joins after Sync is on arrive aligned. The ladder (§6.2) then
handles only residual jitter, in either direction, with no
special-casing for a channel's age; it is not there to close a large
join gap, because a join does not produce one.

**Gate 2** (`CASCADE_AUDIO_RECEIVE_SPEC.md` §4.2) governs only
ongoing per-cycle readiness after release, not the join.

---

## 4. Path B — Sync off

```
decoded audio → ring buffer (§2)
    → direct, synchronous, 1:1 read-and-add into the output
    → advance the read cursor by what was consumed
```

No ratio and no resampler. The only correction on this path is the
splice (§2.1), driven by the ±4 ms relay: at most one small insertion
or removal every 0.5 s, whenever drift pushes the average outside the
band. On an ordinary network this runs indefinitely without trouble.

What Path B lacks is continuous correction. With a hard rate limit of
one splice per 0.5 s, sustained severe loss can drain the buffer
faster than the splice can act (§8).

**Below a 20 ms frame size, clean traffic gets no correction on Path
B.** The splice cannot fire (§2.1), the resampler is Path A only, and
§6.6's bound is Sync-on only. Depth stays wherever it was last
displaced to; displacement reaching the overrun ceiling (`2 ×
setpoint`, `CASCADE_AUDIO_RECEIVE_SPEC.md` §5) is held there by the
discard. Correction returns when the frame size rises to 20 ms, when
loss produces gap-recovery packets, or when Sync is turned on.

**A render-period change can cause such a displacement**: rebuilding
the audio streams stops the read side while arrivals continue
(`CASCADE_AUDIO_RECEIVE_SPEC.md` §5.2). The period is
`min(receive_half, send_half)` (`CASCADE_AUDIO_RECEIVE_SPEC.md` §13),
so changing the *outgoing* frame size can move it even when the
incoming frame size is unchanged. Receive-side frame-size adaptation
never changes the period, so it causes no such displacement.

**Cross-channel alignment without cross-channel comparison.** Each
channel is corrected independently toward its own setpoint; the
splice never reads another channel's state. Channels stay aligned
because they share one target: every channel of a source has the same
setpoint and deadband **while they share one incoming frame size**.
A resize is per channel (`CASCADE_AUDIO_RECEIVE_SPEC.md` §3.2), so
channels of one source receiving different frame sizes carry
different setpoints, and correcting each toward a different target
does not pull them together.

---

## 5. Path A trigger: the Sync fan-out

```
fn set_sync_enabled(source, new_value):
    source.sync_enabled = new_value
    for channel in source.channels:      # one synchronous pass
        channel.sync = new_value         # a plain store, no side effects
```

**The shape matters, not the content.** Every channel of the source
flips in one loop, microseconds apart, whatever its join time — a
channel that joined a moment ago and one running for minutes enter the
active state at the same instant.

**This is why enabling Sync produces a synchronised correction burst,
not a gradual glide.** While Sync is off no resampler runs, and depth
can float well outside the Sync-on band. On the first cycle after the
fan-out, several channels may already be outside it and engage
correction together.

**This is the purpose of Sync**: channels of one source, mixed
together, stay sample-accurate *relative to each other*. If each
channel corrected on its own schedule, the drift between them during
correction would itself be an audible phase problem. Describe Sync as
inter-channel alignment, not as drift tolerance that happens to align.

Per-channel correction is otherwise independent: in one correction
episode some channels may move and others not, and those that move
may move in opposite directions.

---

## 6. The continuous correction system (Path A, per render cycle, per source)

Two passes, per source, every render cycle while Sync is on.

### 6.1 Pass 1 — compute a group average

```
for channel in source.channels:
    channel.participates_this_cycle = false

sum = 0; count = 0
for channel in source.channels:                  # ONE loop
    if channel.get_synced_audio():               # mixed a ready result (§7.2)
        if channel.last_merge_time_stamp < channel.merge_time_stamp:   # unsigned
            sum += channel.merge_time_stamp
            count += 1
            channel.participates_this_cycle = true
    # otherwise nothing below runs for this channel this cycle

if count == 0:
    dispatch every channel at ratio 1.0, reset_averaging = false
    # no ladder, no group decision this cycle
else:
    group_average = (sum + count / 2) / count    # rounded: floor(mean + 0.5)
    # sum is the raw unsigned 32-bit stamps added as a 64-bit integer,
    # divided unsigned; stamps on both sides of the counter's wrap
    # average to a value near neither, for the one cycle in ~24.9 hours
    # where that can happen
```

`participates_this_cycle` is the single gate on everything downstream.
A channel that does not set it takes no part in the average, cannot
arm the ladder (§6.2) or receive a ladder ratio, casts no vote in
§6.3/§6.6's tallies, and receives no skew adjustment. It is cleared
before Pass 1, so a channel that stops qualifying stops voting.

**Two conditions, in sequence:**

1. **Retrieval succeeded** — `get_synced_audio` found a ready result
   and mixed it. On failure the channel contributes **nothing** to the
   mix this cycle — silence by omission, not a repeat of the last
   result and not a partial block — and no timestamp is read or
   compared.
2. **Its position moved forward** — `merge_time_stamp` is greater,
   compared unsigned, than `last_merge_time_stamp`. A channel whose
   stamp has not advanced is excluded even though its retrieval
   succeeded.

**`merge_time_stamp` is read only after the retrieval has succeeded.**
The resample job that produced the mixed result wrote it (§6.4, §7.2);
a successful retrieval is what makes it the stamp describing the audio
mixed this cycle. Reading it before the check, or from a cached value,
pairs a stamp with audio it does not describe.

**`last_merge_time_stamp` is latched at the end of the cycle**, by the
per-channel dispatch that clears readiness and queues the next job
(§7.2), before that job can move anything. The dispatch runs whether
or not retrieval succeeded, so the baseline advances even on a failed
cycle — which stops a stalled channel re-qualifying later on a stamp
that never moved.

**A ready result means "this resample had new input".** Readiness is
set only by a job whose recovery copy took samples from the ring
(§7.2). A job that ran on carried-over input or on silence padding
still advances the resampler's state, but its output is discarded.

This is a **live group consensus**: the average is recomputed every
cycle from whichever channels are delivering and advancing.

### 6.2 Pass 2 — compare each channel against the group

```
# for each participating channel — every other channel's ratio is 1.0
# on a ladder cycle
diff = channel.merge_time_stamp - group_average

diff == 0                      → ratio = 1.0
diff < 0, |diff| >= 11         → ratio = 0.998
diff < 0, |diff| in [2, 11)    → ratio = 0.999
diff < 0, |diff| < 2           → ratio = 0.9995
diff > 0, diff >= 11           → ratio = 1.002
diff > 0, diff in [2, 11)      → ratio = 1.001
diff > 0, diff < 2             → ratio = 1.0005

# The sign is an UNSIGNED 32-bit comparison — at or above the average
# is diff > 0 — and |diff| is the unsigned difference on that side. A
# stamp and an average on opposite sides of the counter's wrap read as
# a large deviation.

ladder_active = any participating channel has diff != 0
```

Whether these ratios are used is §6.3's decision: they are dispatched
only while the common mode is disengaged and `ladder_active` is true.

The ratios are double precision, written exactly as the decimal values
above.

**This is a group-consensus servo, not a fixed-target one.** Each
channel is measured against the current average of its delivering
siblings, which is what produces sample-accurate alignment *between*
channels. The two-pass structure — average first, then compare, with
direct tier selection and no hysteresis — is required. Driving the
ratio from pairwise deviation between channels produces continuous,
audible resampling artifacts.

### 6.3 The common-mode backstop

The ladder cannot see **common-mode drift**: if every channel drifts
together, each stays near the group average and Pass 2 finds nothing
to correct. A backstop on the same per-cycle loop covers exactly this:

```
# per-source state, zero at construction:
#   common_engaged — the latch
#   common_dir     — the direction last applied: +1 fill, -1 drain, 0

if not common_engaged and ladder_active:
    dispatch the ladder's per-channel ratios (§6.2) —
    the group decision below does not run this cycle

else:
    # THE GROUP DECISION — every cycle while the common mode is engaged,
    # and every cycle where no participating channel is off the average
    adjusting_tally = 0
    skew_tally      = 0
    for channel in the participating channels (§6.1):
        if channel.adjusting_flag == FILL:       adjusting_tally += 1
        if channel.adjusting_flag == DRAIN:      adjusting_tally -= 1
        if channel.skew_adjusting_flag == FILL:  skew_tally += 1
        if channel.skew_adjusting_flag == DRAIN: skew_tally -= 1

    if skew_tally != 0:
        adjust every participating channel's skew reference (§6.6)
        ratio = 1.0
        # the adjusting tally decides nothing this cycle: common_engaged
        # and common_dir are left exactly as they were
    else:
        # only the SIGN of the tally is used
        if adjusting_tally  < 0: ratio = 0.998; dir = -1
        if adjusting_tally == 0: ratio = 1.0;   dir =  0
        if adjusting_tally  > 0: ratio = 1.002; dir = +1
        if dir != common_dir:
            common_dir     = dir
            common_engaged = (dir != 0)

    broadcast ratio to every channel of the source
```

- **The correction latches.** A non-zero adjusting tally engages it;
  from then on every cycle takes the group decision and the ladder
  does not run, however far channels drift from the average, until a
  zero tally releases it. The ladder corrects relative skew only while
  the common mode is disengaged.
- **The backstop is symmetric**: `0.998` when the group runs
  consistently ahead, `1.002` when consistently behind, selected by
  the tally's sign alone. Dropping the fill direction leaves a source
  whose packets run persistently slow with no common-mode correction.
- **The two checks watch different things.** The ladder watches
  *relative* position against live siblings; the boxcar/deadband
  relay watches an *absolute* reference from packet cadence. The
  backstop engages the absolute check exactly where the relative one
  is blind.

### 6.4 `merge_time_stamp` — exact derivation

With Sync on, computed and stored by the resample job at the staging
copy (§7.2):

```
merge_time_stamp = timestamp_data[read_cursor] - leftover

# read_cursor: the ring position BEFORE the top-up copy advances it, so
# the lookup gives the timestamp of the first sample that copy takes
# (CASCADE_AUDIO_RECEIVE_SPEC.md §4.1)
#
# leftover: the resampler's carried-over input count, read BEFORE it is
# replaced by this window's length. Those samples sit at the front of
# the staging buffer, ahead of the first newly copied one, so the
# window starts that many samples earlier on the timeline
```

- It is written on cycles that recover input and left standing on the
  others, which is what makes §6.1's advancement test meaningful: a
  cycle with no new input leaves the stamp where it was.
- **The subtrahend is the resampler's live leftover, not a constant.**
  The window is `frames + 6`, and at a ratio of exactly 1.0 the
  leftover settles near 6, so the two are easily confused. Away from
  unity they differ — and those are exactly the cycles on which the
  ladder is correcting. A constant would measure the ring's arrival
  cadence instead, leaving the ladder blind to its own correction.
- It is a per-sample timing position — the timestamp of the front of
  the resampler's input window — which is why §6.2's thresholds (`2`,
  `11`) are small, sample-accurate numbers. They are not comparable
  with §2.3's deadbands, which measure packet-arrival depth.

With Sync off the same field is maintained by the plain retrieval,
with no leftover term:

```
merge_time_stamp = timestamp_data[read_cursor]      # before the read advances
```

`last_merge_time_stamp` is not written while Sync is off, so it holds
its value from when Sync was last on while `merge_time_stamp` keeps
advancing. When Sync is turned back on the channel therefore takes
part in the group consensus on its first cycle.

### 6.5 `reset_averaging` — the boxcar reset

```
reset_averaging = (common_engaged == false) AND ladder_active

# evaluated AFTER this cycle's §6.3 group decision, which may just have
# moved the latch: on the cycle that releases it, with channels off the
# average, the reset fires alongside the broadcast 1.0. On a cycle where
# no channel participated it is false.
#
# ONE value for the whole source, STORED to every channel dispatched
# this cycle — including channels whose own §6.2 ratio is unity.
#
# The producer takes the request on its next normal-path packet
# (CASCADE_AUDIO_RECEIVE_SPEC.md §5); that packet is not measured and
# the refilled window starts with the one after it.

if the request is taken:
    boxcar_running_sum      = 0
    boxcar_write_index      = 0
    boxcar_window_full_flag = false
    adjusting_flag          = 0
    skew_adjusting_flag     = 0      # both, together
```

- **Reset the windows while the ladder is correcting and the common
  mode is disengaged.** While it is engaged, windows are left alone:
  resetting them would discard the evidence behind the broadcast.
- **The condition belongs to the cycle, not the channel.** A channel
  at unity on a ladder cycle sits at the group mean, and its window is
  invalidated just as much by the group moving around it.
- **The value is stored every cycle, not latched.** A cycle that does
  not want a reset clears a request an earlier cycle left, so a stale
  request cannot fire a reset — and with it a §2.3 re-latch — cycles
  after its condition has passed.
- **Every window reset clears both direction flags**, at every site
  that resets the window: this one, the drain-to-empty re-arm, the
  overrun recovery, the Gate 1 release, the buffer-setting change, and
  construction. Both flags are derived from the history being
  discarded.

### 6.6 The bound on the skew reference

**`skew_reference` is §2.3's calibrated reference — one field.** §2.3
latches and reads it; this section bounds it. An implementation that
latches a reference and never bounds it leaves a badly calibrated
channel stuck for good.

**The check** compares `skew_reference` itself against the setpoint —
not depth, not arrival timing — with its own band, writing its own
flag:

```
band = min(1728, setpoint / 2)           # 1728 samples = ±36 ms

if skew_reference <= setpoint - band:  skew_adjusting_flag = FILL
elif skew_reference >= setpoint + band: skew_adjusting_flag = DRAIN
else:                                   skew_adjusting_flag = IDLE
```

**The clamp usually decides the band.** `setpoint / 2` exceeds 1728
only from a 72 ms setpoint up; at a 40 ms setpoint the band is 960
samples (20 ms). Hard-coding ±36 ms bounds far too loosely at ordinary
settings.

**Where it runs**: inside the resample job (§7.2), and only on a job
whose recovery copy took at least one sample from the ring, and only
once the reference is calibrated:

```
reference = skew_reference            # read before the copy
... recovery copy takes `took` samples ...
if took > 0 and reference >= 1:
    re-evaluate skew_adjusting_flag as above
# otherwise the flag keeps the value it last had
```

`skew_reference` is 0 until §2.3's first-fill latch writes it, so
`>= 1` is simply "is this channel calibrated yet". Because the leftover
a job starts from is the resampler's small per-cycle remainder, not a
ring depth, a top-up that takes samples is the normal case — it runs
on essentially every cycle. It must not be gated on a *shortfall*: a
buffer parked above its setpoint always satisfies its top-up in full,
which is exactly when the bound is needed.

**The tally and the adjustment**: §6.3's group decision counts
`skew_adjusting_flag` over participating channels, alongside the
adjusting tally. A non-zero skew tally takes the cycle — ratio 1.0,
§6.3's latch untouched — and every participating channel is adjusted:

```
adjust_skew_reference(direction):
    if direction > 0: skew_reference += frames_this_cycle
    if direction < 0: skew_reference -= frames_this_cycle
    skew_adjusting_flag = 0              # unconditionally
```

The step is one render period's worth of samples. A zero tally
applies nothing and clears nothing. Because the flag is recomputed
only on a job that took input, a channel otherwise votes with its last
value — which is why the adjustment's unconditional clear is required;
without it a stale flag would keep voting.

**Effect**: a channel's reference is set once per window fill by §2.3,
and thereafter cannot wander further than one band from the setpoint.
Once it strays, the next non-zero tally walks it back one render period
per cycle.

---

## 7. The resampler

`zita-resampler`'s `VResampler` (GPL-3.0, Fons Adriaensen), a
windowed-sinc interpolator for converting between two nominally equal
rates whose exact ratio is unknown and may drift slowly — precisely
this problem: a local playback clock and a network stream with no
shared word clock. Library documentation:
`kokkinizita.linuxaudio.org/linuxaudio/zita-resampler/resampler.html`.

### 7.1 Configuration — one instance per channel, set once

```
ratio = 1.0      # nominal; the runtime ratio comes from §6.2/§6.3
nchan = 1        # mono
hlen  = 32       # half-length of the interpolation filter
frel  = 1.0 - 2.6 / hlen = 0.91875
NP    = 256      # filter phases
                 # (the stock library fixes 120; Cascade uses 256, and
                 # the coefficient table is hlen × (NP + 1))

window(x) = 0.384 + 0.500 × cos(x) + 0.116 × cos(2x)
     # a three-term modified Blackman window, not a raised cosine

group_delay ≈ hlen = 32 samples (~0.66 ms)
```

The filter coefficients are computed from the window when the
resampler is constructed; there is no precomputed table. The resampler
is primed once at construction (`inp_count = 960`, `out_count = 2048`,
one `process`) and is never reset afterwards
(`CASCADE_AUDIO_RECEIVE_SPEC.md` §3.2).

**The ratio applies immediately.** The phase step snaps to its new
value the moment the ratio changes; the resampler's internal ratio
smoothing is not used. Any gradual-looking progression across cycles
(`0.998 → 0.999 → 0.9995`) is the ladder re-evaluating each cycle.
Adding internal smoothing produces a different convergence curve.

### 7.2 Per-cycle operation

While Sync is on, every channel, every render cycle: the ratio is
computed (§6.2/§6.3), and the resample itself is handed to that
channel's own **serial job queue**, off the real-time thread:

```
# every render cycle, in the render callback:
get_synced_audio(channel):                    # Pass 1 (§6.1)
    if not channel.ready:
        return false          # contribute nothing — silence by omission
    mix the result the most recent completed job stored
    return true

dispatch_resample(channel, ratio, reset):     # after Pass 2, EVERY channel
    channel.reset_averaging_request = reset   # a store (§6.5)
    channel.last_merge_time_stamp   = channel.merge_time_stamp   # §6.1 latch
    channel.ready                   = false
    channel.resample_queue.dispatch(job(frames_this_cycle, ratio))

# the job — later, off the real-time thread, one at a time per channel,
# in order:
job(frames, ratio):
    need = frames + 6
    recovered = false
    if need > carried:                        # carried: last job's leftover
        reference = skew_reference            # read before the copy
        took = recovery copy of up to (need - carried) samples from the
               ring into staging[carried ..]  # takes nothing while the
                                              # prebuffer hold is set or
                                              # the ring is empty
        pad staging[carried + took .. need] with silence
        if took > 0:
            merge_time_stamp = timestamp at read cursor before the copy - carried
            advance the ring's read cursor by took     # clamped, below
            if reference >= 1: re-evaluate skew_adjusting_flag (§6.6)
            recovered = true
    set_rratio(ratio)
    inp_count = need ; out_count = frames     # the whole window, padding included
    inp_data = staging ; out_data = result
    process()
    carried = inp_count                       # unconsumed input, padding included
    move staging[need - carried .. need] to the front
    channel.ready = recovered
```

**1. A result is mixed the cycle after it is computed.** Pass 1 reads
what an earlier job completed, before this cycle's dispatch queues the
next one. Output is one cycle behind computation.

**2. The delay depends on real job completion.** This is a genuine
asynchronous dispatch, not a fixed buffer swap: under load a job can
finish late, and that channel alone reads not-ready for a cycle and
recovers on the next. Use a real asynchronous queue; a synchronous
stand-in hides this load-dependent timing.

**3. Not ready means silence.** On a not-ready read the channel adds
nothing to the mix — on its first cycle before any job has completed,
and on a late job mid-session alike. Never re-emit the last result.

**Readiness is set only by a job whose recovery copy took at least one
sample.** A job whose carried input already covered the window, or
whose copy took nothing (an empty ring, or the prebuffer hold set),
still runs the resampler over the full window, but leaves readiness
clear and its output is never mixed. Readiness is distinct from
`skew_adjusting_flag`. Every dispatch clears readiness before queuing
the next job.

**A late job does not lose position.** The queue is serial and the
dispatch unconditional, so a cycle whose previous job has not finished
queues its own behind it. When the late job finishes it marks its
result ready, and the queued one then runs straight after it, taking
its own quantum from the ring and replacing that result — so the late
result is mixed only if a render reads in between, and otherwise that
quantum of output is lost. Position is never lost: every job consumes
its own cycle's input, so the channel stays on its siblings' timeline
and the ladder has no slip-induced deviation to correct.

**4. Queues are per channel.** Each channel creates its own serial
queue at construction; no channel shares one. A late job on one
channel cannot delay its siblings.

**What the read position advances by.** After `process()`, the
leftover carried into the next cycle is whatever the resampler reports
as unconsumed from the complete window — real and padded content
alike. This keeps the resampler's input accounting and the staging
buffer in step whatever share of a window was real audio. A cursor
that advanced by real samples only, while the resampler consumed a
differently sized window, would insert a silent sample on every
shortfall cycle.

**The ring's own read cursor advances only on a copy that took
samples**, and by exactly what was taken — never by the padding. The
advance is a hard clamp:

```
if pending >= available:
    read_index = write_index       # stop; no deficit is carried
else:
    read_index += pending
```

No deficit is tracked: the next real samples are written and read
normally. The read index never passes the write index. An
implementation that carries a deficit forward and applies it against
later, real samples skips different audio and does not match this.

On a starved ring (nothing available) the copy takes nothing and the
cursor does not move, whatever readiness says. Gate the cursor advance
on the copy's own result (`took > 0`), not on a readiness or
completion flag.

### 7.3 Correction rate

With a runtime ratio `r`, the resampler consumes `1/r` input samples
per output sample. At a 480-sample render period (10 ms):

```
correction_rate (samples/cycle) = 480 × (1/r - 1)

r = 0.998  (coarse):  0.962 samples/cycle
r = 0.999  (mid):     0.480 samples/cycle
r = 0.9995 (fine):    0.240 samples/cycle
```

The rate scales with the render period (§13 of
`CASCADE_AUDIO_RECEIVE_SPEC.md`).

**Expected correction duration depends on the number of participating
channels**, for two reasons:

1. **The correction does not stop at a tier boundary.** Below
   `|diff| = 2` the fine tier takes over at half the rate and runs to
   zero, so a trajectory covers the whole distance.
2. **The group mean is not a fixed target.** The channel being
   corrected contributes to the mean, so correcting it by `c` moves the
   mean by `c/n` and closes the deviation by only `c × (1 − 1/n)`.

Entering the mid tier at `diff = +11`:

```
naive (stops at the boundary, mean fixed)     18.7 cycles
 2 participants                                46
 3 participants                                35
 4 participants                                31
 8 participants                                27
16 participants                                24
```

A fine-tier correction from `|diff| < 2` completes in a handful of
cycles.

### 7.4 Mixing the resampled result

A ready result is added into each output its channel is routed to,
exactly as on Path B (`CASCADE_AUDIO_RECEIVE_SPEC.md` §7): summed,
with no crosspoint gain. The output stage (§7.3 of that document)
follows.

---

## 8. Adverse network conditions

Both paths absorb a **constant** delay or offset cleanly: a one-time
step in arrival time is taken up by the ring settling at a new
position, Sync on or off.

**Sustained loss or heavy jitter is where the paths diverge:**

- **Path A (Sync on)**: continuous correction by ratio, every render
  cycle, with no ceiling on how often it can act.
- **Path B (Sync off)**: the splice corrects ordinary drift
  indefinitely, but at most once per 0.5 s. Loss severe and sustained
  enough to drain the buffer faster than that can drive it to empty —
  an audible dropout — after which the channel re-buffers
  (`CASCADE_AUDIO_RECEIVE_SPEC.md` §5.1).

So neither "Sync off has no defence" nor "Sync off always runs fine"
is accurate: Path B defends against ordinary drift well and
indefinitely, and has no defence against loss that overwhelms its rate
limit, which is the condition Path A's continuous correction handles.

**Audibility.** A join disturbs settled channels only briefly — the
finest tier for a few samples over a fraction of a second — and is not
noticeable. Drift corrections that use the coarser tiers for several
seconds can be heard.

---

## 9. No relaxed tolerance mode

Cascade has no mode that widens the deadbands for externally clocked
sources: the bands of §2.3 and §6.6 always apply. A legacy
`atomic_clock` settings key is ignored and removed on the next save.

---

## 10. Summary — every constant, one table

| Constant | Value | Context |
|---|---|---|
| Setpoint | `<5 ms` 120, `<10 ms` 240, `<20 ms` 480, else `(ms / 20) × 960`; floored at `2 × frame` and the period floor | §2 |
| Ring capacity | `2 × setpoint + 1920` samples | §2 |
| Deadband (Sync on) | 576 samples, ±12 ms, clamped to `setpoint / 2` | §2.3 |
| Deadband (Sync off) | 192 samples, ±4 ms, clamped to `setpoint / 2` | §2.3 |
| Skew-reference band | 1728 samples, ±36 ms, clamped to `setpoint / 2` | §6.6 |
| Splice rate limit | 0.5 s between splices | §2.1 |
| Splice search window | 120–239 samples between consecutive crossings | §2.1 |
| Boxcar window | 150 / 300 / 600 / 1200 packets at a 960+ / 480 / 240 / 120-sample setpoint | §2.2 |
| Ladder ratios | `0.998`, `0.999`, `0.9995`, `1.0`, `1.0005`, `1.001`, `1.002` | §6.2 |
| Ladder thresholds | `2`, `11` (`merge_time_stamp` samples) | §6.2 |
| Backstop ratios | `0.998`, `1.0`, `1.002`, by the sign of the tally | §6.3 |
| Skew adjustment step | one render period of samples | §6.6 |
| Resample window | `frames + 6` | §7.2 |
| Resampler `ratio` / `nchan` / `hlen` | `1.0` / `1` / `32` | §7.1 |
| Resampler `frel` | `0.91875` (`1.0 − 2.6 / 32`) | §7.1 |
| Resampler `NP` | `256` | §7.1 |
| Window function | `0.384 + 0.5·cos(x) + 0.116·cos(2x)` | §7.1 |
| Group delay | ≈32 samples (~0.66 ms) | §7.1 |
| Minimum buffer with Sync on | 20 ms | §11 |

---

## 11. Total latency added by enabling Sync

**Always, while Sync is on — about one render period plus 0.66 ms:**

- **One render period** (10 ms at a 480-sample period): the pipeline
  lag of §7.2. The result mixed each cycle is the one computed on the
  previous cycle, because retrieval comes before dispatch within the
  cycle. This is structural, not a race.
- **~0.66 ms**: the resampler's group delay (32 samples, §7.1).

**Depending on the buffer setting — 0 to 15 ms**: with Sync on the
buffer setting has a 20 ms minimum (`CASCADE_AUDIO_RECEIVE_SPEC.md`
§4). A buffer already at 20 ms or more adds nothing; a 5 ms buffer
becomes 20 ms, adding 15 ms.

**Total**: about 10.7 ms with a buffer already at 20 ms or more, up to
about 25.7 ms from a 5 ms buffer. None of this applies with Sync off:
Path B has no resampler, no job queue and no buffer minimum.
