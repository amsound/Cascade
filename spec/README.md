# Cascade — Specification Index

Specification of the Cascade audio-over-IP protocol and the mechanisms behind it, in
pseudocode. Seven documents, listed below in a reasonable reading order.

---

## `CASCADE_WIRE_PROTOCOL_SPEC.md`

The UDP wire format itself: packet header structure, byte order, the
opcode table (ping/pong, audio, config, label exchange), identity and
authentication, link indices, connection state machine and timers, and
the HTTP interfaces.

## `CASCADE_AUDIO_SEND_SPEC.md`

The outgoing audio pipeline: capture, per-remote encoders shared by
reference count, Opus encoding, frame-size buckets, the Voice/Audio
settings rule, and peak-level metering on the send side.

## `CASCADE_AUDIO_RECEIVE_SPEC.md`

The incoming audio pipeline: arrival checks, decode (Opus, RAW16 and
RAW24), the jitter/ring buffer and its management, routing to outputs
and the output stage, peak-level metering on the receive side, and the
shared device callback period.

## `CASCADE_SYNC_MECHANISM_SPEC.md`

Clock-skew correction and buffer synchronization: the Sync on/off
gate, the resampler-based correction path, the discrete splice
mechanism, and the deadband/hysteresis logic governing when
correction engages.

## `CASCADE_SESSION_STATS_SPEC.md`

Per-connection telemetry: transmit/receive byte counts, packet loss,
jitter, and latency — the accumulate-and-snapshot pattern used to
back both the UI and the REST API.

## `CASCADE_ENCRYPTION_SPEC.md`

Optional end-to-end audio encryption: X25519 keypair generation and
key exchange, BLAKE2b key derivation, AES-256-GCM, the on-wire packet
layout, and the encryption gating logic on both send and receive.

## `CASCADE_UI_SPEC.md`

The web UI: its principles (offer only what applies, state over
messages, the interface reflects reality), layout and pages, how
changes are applied, metering, the receive-buffer gauge, device
states, banners and wording.
