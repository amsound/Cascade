# Cascade — Web UI Specification

The web UI is Cascade's only control surface: Cascade runs headless and
is configured and monitored entirely from a browser, at
`http://<host>:<api port>` (8080 by default). This document states how
the UI must behave and the principles behind it. The HTTP and WebSocket
routes it uses are listed in `CASCADE_WIRE_PROTOCOL_SPEC.md` §6.1.

---

## 1. Audience

Broadcast audio professionals and technically competent operators, not
consumers. Users understand port numbers, network interfaces and
buffers. The UI does not over-explain, does not ask for confirmation of
obvious outcomes, and does not pad the screen with guidance that assumes
ignorance. Someone who changes the web UI port knows to browse to the
new address.

---

## 2. Principles

**Offer only what applies.** A control offers only the values that are
valid right now. Invalid options are removed — never shown greyed out,
relabelled or accompanied by a warning — so an operator cannot pick
something the engine would override:

- Selecting **Voice** leaves 20 ms as the only frame size offered, and
  selects it. Returning to **Audio** offers every size again, keeping
  20 ms selected.
- The **Receive buffer** list starts at the highest floor that
  applies: twice the incoming frame size (40 ms until a frame has been
  measured), 20 ms while **Phase lock** is on, and the audio devices'
  callback floor.
- The **Frame size** list omits sizes shorter than the audio devices
  can run.
- The routing grids show only channels that exist: the input device's
  channels, the incoming streams actually received, the output
  device's channels.

**State over messages.** A persistent condition is shown by the state
of the interface itself — a topbar indicator, a pill, a badge, a
missing control — not by a lasting banner or toast. Banners are for
transient feedback on an action (saved, failed), and for a condition
the operator needs to hear about once, which clears itself when the
condition clears (§8).

**The interface reflects reality.** What is shown is what is running:

- The device dropdowns show the device actually running, not just the
  one configured.
- The receive-buffer dropdown shows the buffer the engine is actually
  running, which a large incoming frame or the device floor can hold
  above the saved value.
- The exclusive-output control shows whether exclusive access is
  actually held; a claim that fails is corrected in the settings.
- Stale data is worse than none: if the daemon is unreachable the
  whole UI is covered (§9).

**Absence communicates.** With no input or output device, the routing
grid for that direction shows nothing but a short note pointing to
Settings, and never a grid filled with fallback data. A device is
never chosen automatically: it is present because the operator chose
it, or it is `- none -`.

**Three severity colours.** Good (green), warn (amber), bad (red). Red
means something needs attention and is never decoration: a port
conflict, ON AIR, a buffer close to underrun. The buffer gauge adds one
more state, blue, for a buffer running *over* its target (extra
latency).

**Controls are where the fix is.** A port conflict shows in the topbar
as `PORT 20102 IN USE`. The operator goes to Settings because that is
where the port field is, not because a banner said so.

**Nothing restarts.** Every setting applies in place: the audio socket
rebinds for a port or interface change, a rename re-derives each
remote's identity, devices rebuild live, and the web server rebinds
itself. There is no "apply and restart" step.

---

## 3. Applying changes

**Settings are saved explicitly.** A change on the Settings page marks
the page dirty and shows the save bar (`Unsaved changes` — `Discard`,
`Save`). Nothing is sent until **Save**. Settings that take effect
live on the backend — bitrate, exclusive output, and each remote's
buffer, frame size, mode, phase lock and encryption — are applied as
part of the save, not instead of it. Choosing a receive buffer counts
as a change even if it equals the saved value, so re-selecting a
buffer the engine had raised applies it.

**Routing and ON AIR act at once.** Clicking a crosspoint, 1:1, Clear,
a tone slot or ON AIR applies immediately, with no save step; a channel
label applies when its edit is committed. Routing and labels are saved
by the daemon. Tone routes and ON AIR are live state, not settings, and
are not saved.

Feedback is a short banner: `Saving...`, `Saved.`, `Save failed.`, or
`Not connected - change not saved.`

---

## 4. Layout

**Topbar**, left to right:

- the Cascade mark and name, followed by the instance name when it is
  not "Cascade";
- the tabs **Monitor**, **TX Routing**, **RX Routing**, **Settings**;
- the port-conflict pill, shown only during a conflict;
- **ON AIR**, hidden during a port conflict;
- the theme button, cycling **System**, **Light** and **Dark**;
- the connection dot.

Below the topbar, one banner line, then the page.

### 4.1 Monitor

Read-only.

- **Instance**: name, listen port, audio in and audio out (device and
  rate, or `none`, or the configured device marked `(unavailable)` in
  amber), stream counts (`TX n · RX n`), bitrate.
- **Remotes**, in the order they were added, one row each:

| Column | Shows |
|---|---|
| Name | the remote's name, its routed channel counts (`TX n · RX n`), a `44.1k` badge while it sends a rate Cascade does not play, a lock while encryption is active |
| State | a pill: green when connected, amber when connected from an address other than the configured host, plain when idle, amber in any other state |
| TX / RX | Mbps |
| Loss | percent |
| Jitter | ms |
| Latency | ms while connected, otherwise `-` |
| Buffer | the receive-buffer gauge (§6) |

With no remotes: `No remotes configured.`

### 4.2 TX Routing

A grid for one remote at a time, chosen by the remote buttons: local
sources down the left, the remote's channel slots across the top.

- Rows are the input device's channels, then **Tone L** (EBU
  interrupted) and **Tone R** (steady) line-up tone.
- **One source per slot.** Routing a source into a slot replaces
  whatever fed it, including tone; a source may feed several slots.
- Column headers read `N · <label>` for what is routed to the slot, or
  `N · -`.
- Row labels are edited by clicking them; Enter moves to the next
  label. **Clear labels** resets them to `Ch N`.
- **1:1** routes the diagonal; **Clear** removes every route.
- **ON AIR** locks the tone rows: tone is cleared from every remote
  and cannot be routed until ON AIR is turned off.

With no input device: `No input device selected - choose one in
Settings.`

### 4.3 RX Routing

A grid for one remote at a time: the incoming streams actually
received from that remote down the left, labelled with the names the
remote sends, and the output device's channels across the top.

- One incoming stream may feed several outputs, and several may sum
  into one output.
- Column headers read `N · <label>`, `N · <label> (+k)` when several
  streams sum there, or `N · -`.
- **1:1** and **Clear** as on TX.

With nothing received: `No active incoming streams from this remote
yet.` With no output device: `No output device selected - choose one
in Settings.`

### 4.4 Settings

- **Identity & Network**: Name, Port, Web UI port, Network interface.
- **Audio**: Input device, Output device, Bitrate (Auto or 8–320
  kbps), Output access (**Exclusive**). On Windows and Linux exclusive
  access is how the device is opened, so the control shows as on and
  does not respond.
- **Remotes**, each on two aligned rows:
  - Name, Host, Port, Password, **Enabled**, **Remove**;
  - Mode (**Voice** / **Audio**), Frame size, Receive buffer,
    **Phase lock**, **Encryption**.

  A stored password is never sent back to the browser: the field shows
  a placeholder, and a password is sent only when typed. **Add
  remote** adds one. With none: `No remotes. Add one to start sending
  or receiving audio.`

---

## 5. Metering

Meters sit in the row labels of both routing grids: input channels and
tone on TX, incoming streams on RX.

- **Scale**: dBFS, from −60 to 0, hinged so that −18 dBFS sits at
  mid-scale — −60 to −18 fills the lower half, −18 to 0 the upper.
  −12 reads 66.7% and −6 83.3%.
- **Peak hold**: a marker rises instantly with the level, holds for
  about 700 ms, then falls steadily; it is hidden near the bottom of
  the scale.
- **Updates**: about every 40 ms, and only while a routing page is
  visible and the browser tab is in view. Each poll names the remote
  being viewed; that is what keeps the daemon metering that remote's
  unrouted streams, and metering stops by itself when nobody is
  looking (`CASCADE_AUDIO_RECEIVE_SPEC.md` §9.2).
- **No blinking**: routing changes repaint crosspoints and headers in
  place; the meters are never rebuilt, so bars and peak markers keep
  their state.

A meter shows every incoming stream, routed or not, and every input
channel, whether or not it is sent anywhere.

---

## 6. The receive-buffer gauge

One gauge per connected remote, in the Monitor's Buffer column. The
text beside it is the buffer's **set size**, which changes only with
the setting or the incoming frame size. The bar is the live fill: the
average across the remote's channels and across every render since
the last update, so the ordinary one-frame sawtooth does not make it
swing.

| Bar | Meaning |
|---|---|
| striped, full | prebuffering: filling to the set size before playback resumes |
| green | within tolerance of the target |
| amber | drained below tolerance |
| red | below half the target — close to underrun |
| blue, full | over the target by more than the tolerance — extra latency |

Tolerance is `max(15% of target, one frame + 5 ms)`, so the bands stay
meaningful at every buffer and frame size. The tooltip gives the
average and the range it moved through. A remote that is not connected
shows `-`.

---

## 7. Audio device states

Input and output are independent. Each dropdown reads the device
actually running.

| State | Dropdown | Monitor | Routing page |
|---|---|---|---|
| Running | the device, selected | `Name · 48 kHz` | grid sized to its channels |
| Lost (unplugged; still configured) | `Name (unavailable)` selected | `Name (unavailable)` in amber | empty, with the no-device note |
| None (deliberate) | `- none -` | `none` | empty, with the no-device note |

- A lost device recovers by itself: when it returns, the engine
  rebuilds, routing is restored, and the dropdown returns to normal —
  no action and no restart.
- Choosing `- none -` is a deliberate stop and is not an error. A
  device that disappears while running raises one banner:
  `Input device unavailable` or `Output device unavailable`.
- Routing is kept across loss, `none` and a device with fewer
  channels, so it returns intact with the device.

---

## 8. Banners

One banner line, below the topbar.

- **Transient**: action feedback (`Saved.`) and backend notices,
  dismissed after a couple of seconds.
- **Until cleared**: errors that need reading once — a failed save, a
  device or network error.
- **Condition-bound**: shown while a condition holds and removed
  automatically when it ends:
  - `Remote 'X': incoming 44.1kHz - audio dropped (48kHz only)`;
  - a remote's host name failing to resolve.

Some conditions are **logged only** and never shown in the UI: an
encryption-setting mismatch with a remote, and traffic from an unknown
or unconfigured sender.

---

## 9. Offline overlay

When the daemon is unreachable, a full-page overlay covers the whole
UI: the page background colour, no card, no border, no animation — the
Cascade mark and wordmark centred, with `Reconnecting` in muted text
beneath. Nothing behind it can be reached. It stays still: like a
hardware unit that has lost its peer, it goes dark and waits rather
than flashing. It clears as soon as the daemon answers.

---

## 10. Wording

- **Labels**: short, no trailing punctuation — `Port`, `Web UI port`,
  `Network interface`, `Receive buffer`.
- **Status**: factual, never alarmed — `PORT 20102 IN USE`, not
  `ERROR: Port conflict detected`.
- **Feedback**: minimal — `Saved.`, `Save failed.`, `Saving...`.
- **Devices**: state the fact only — `Input device unavailable`, not
  "lost", and no promise of recovery; recovery is automatic and needs
  no narration.
- **Empty states**: present tense, stating what is true now — `No
  active incoming streams from this remote yet.`
- **Punctuation (UK)**: in user-visible text use the hyphen `-`, not
  the em dash; the none option is `- none -`; the ellipsis is `...`,
  not `…`. The middle dot `·` separates values (`TX 2 · RX 4`).
  Em dashes are fine in source comments and specifications.
