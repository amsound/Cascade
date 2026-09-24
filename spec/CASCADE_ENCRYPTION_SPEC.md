# Cascade — Encryption Specification (AES-256-GCM audio encryption over X25519 key exchange)

Specification for the optional end-to-end encryption of audio: keypair generation, the key
exchange carried on the ordinary poke, key derivation, the exact on-wire byte layouts, and
the gating on both send and receive. Kept separate from `CASCADE_WIRE_PROTOCOL_SPEC.md`
because of its size.

---

## 1. Scope — what gets encrypted, and what never does

**Only audio packets are ever encrypted.** Pokes, pongs, config requests and config pushes
are always sent in the clear: pokes have to work before a key exists, and they carry the key
exchange itself.

Encryption is a per-remote setting, `encryption_on`. Two conditions are checked fresh on
every audio packet — not once per session, not cached:

- `encryption_on` — this remote's setting
- `key_exchange_complete` — per remote, set once the handshake below succeeds

What happens when either is false is **not uniform** — see §5.

---

## 2. Local keypair generation — random, per remote

```
private_scalar = random_bytes(32)              # CSPRNG
public_key     = X25519_base_point_multiply(private_scalar)
```

Generated exactly once per configured remote, when that remote is created, and held for its
lifetime — not per session, not per connection attempt, not regenerated on reconnect.

---

## 3. Wire structure — the key exchange rides on the ordinary poke

There is no separate key-exchange packet. The poke is extended when encryption is on:

```
build the standard poke packet (53 bytes, wire offsets 0x00-0x34)
    — always includes both identity hashes below, whether or not
      encryption is on

if encryption_on:
    append 34 more bytes (wire offsets 0x35-0x56)
    total length = 87
else:
    total length = 53
```

A pong is built from the poke it answers and carries the same extension when the answering
side has encryption on.

### 3.1 Identity hashes — present in every poke

| Wire offset | Size | Field |
|---|---|---|
| `0x15` | 16 B | the sender's own identity hash |
| `0x25` | 16 B | the identity hash the sender expects of the peer |

Both are written on every poke, before the extension is appended. The receiver has no
advance knowledge of who is connecting: it matches the identity hash at `0x15` against the
expected identity of each configured remote until one matches, as a full 128-bit equality —
never a partial or truncated match. Neither side needs the other's address in advance
(`CASCADE_WIRE_PROTOCOL_SPEC.md` §4).

### 3.2 Key exchange extension — present exactly when encryption is on

| Wire offset | Size | Field |
|---|---|---|
| `0x35` | 1 B | Must be `0` |
| `0x36` | 1 B | Must be `32` (`0x20`) — key material length |
| `0x37`-`0x56` | 32 B | The sender's X25519 public key |

The extension must be present exactly when this remote has encryption on. Presence that
disagrees is not tolerated in either direction: the exchange is ABANDONED and the packet's
reply is never sent. Two ends that disagree about encryption therefore fall silent at the
poke stage rather than continuing into a form neither can use.

```
key_exchange(packet, length, from_response):

    if length != 87:                     # no extension present
        if encryption_on:
            ABANDON                      # we require one, peer sent none
        else:
            proceed                      # neither side wants encryption

    else:                                # extension present
        if not encryption_on:  ABANDON   # peer offers, we do not
        if packet[0x35] != 0:  ABANDON
        if packet[0x36] != 32: ABANDON

        if the packet's 32-byte key exactly matches the one already stored:
            no re-derivation — the existing keys stand and the nonce
            counter is NOT disturbed
        else:
            store the new 32-byte peer public key
            derive both session keys (§4)
            if the shared secret is the all-zero point: ABANDON

    # reached whether the key was new, unchanged, or absent-by-agreement
    if from_response:
        mark key exchange complete for this remote
    overwrite the packet's key field with our own public key
      # the reply is built from the received buffer in place
```

`from_response` is true only when the key arrived on a pong, and it alone sets the
completion flag that §5 gates encrypted send on. A key carried by an inbound poke derives
both session keys but does NOT mark the exchange complete: a side begins encrypting once a
peer has ANSWERED one of its own pokes, not merely because a peer has spoken to it.
Decryption is not gated this way (§7), so keys derived from an inbound poke open packets
immediately.

**ABANDON means the reply is withheld entirely** — no pong is sent, and a pong that is
itself abandoned updates no connection state, so the remote never reaches a connected
state. It is silence, not an error reply and not a plaintext fallback.

**A poke whose length alone disagrees is dropped before any of this.** At packet dispatch,
once a poke's sender identity has matched a remote and before anything else is done with it:

```
if poke.length == 53 and remote.encryption_on:     # no key offered
    drop                                           # UNENCRYPTED
if poke.length == 87 and not remote.encryption_on: # key offered
    drop                                           # ENCRYPTED
```

Dropped whole: no address learning, no config request, no pong, no receive-byte count.
Other lengths pass on to `key_exchange` above. The disagreement is logged — "Remote 'X' is
sending UNENCRYPTED packets. Encryption is required for remote 'X' and must be enabled on
the remote side", or the ENCRYPTED counterpart — once when it starts, and again only after
a poke of the agreeing length has been seen and the disagreement returns. Nothing is shown
in the UI.

---

## 4. Key derivation

```
shared = X25519_scalarmult(our_private_scalar, peer_public_key)
if shared == all_zero_32_bytes: FAIL      # degenerate peer key

recv_key = BLAKE2b-512(shared || our_public_key  || peer_public_key)[0:32]
send_key = BLAKE2b-512(shared || peer_public_key || our_public_key )[0:32]
```

Two independent 32-byte keys, one per direction — never one key used both ways. The digest
is BLAKE2b-512, unkeyed, 64 bytes of output, and the FIRST 32 bytes are taken, not the last.
The two public keys are hashed in opposite order between the two derivations — the crossed
construction, where one side's send key equals the peer's receive key.

This is the `crypto_kx` construction as libsodium defines it: its client role takes the
first half of `BLAKE2b-512(q ‖ client_pk ‖ server_pk)` as the receive key, and its server
role the first half of the same digest with the two keys the other way round. Computing the
receive key in the client role and the send key in the server role lets both ends run the
same code with no notion of which is "client" — which is exactly the crossed pair above.
Any X25519 and BLAKE2b implementation produces the same keys.

The audio cipher is standard AES-256-GCM (§5.1); any correct implementation interoperates.

---

## 5. Send-side behaviour — the two conditions do not fail the same way

```
if not encryption_on:
    send unencrypted                     # encryption not requested

elif not key_exchange_complete:
    DROP the packet entirely             # requested but not ready —
                                          # never falls back to plaintext
else:
    encrypt and send (below)
```

A remote with encryption on is never sent audio in the clear: until its key exchange
completes, its packets are dropped.

### 5.1 On-wire encrypted payload layout

```
nonce  = the per-remote counter (§6), then advanced
sealed = AES_256_GCM_seal(plaintext, key: send_key, nonce: nonce,
                          additionalData: AAD)          # see below
on_wire_payload = nonce || ciphertext || tag            # in that order
```

`additionalData` is a FIXED 16-BYTE CONSTANT, the same for every remote and every packet,
on both the seal and the open side:

```
a6 e7 14 47 2f 5c e2 4d ac 8f 90 8c 14 85 53 32
```

(base64 `pucURy9c4k2sj5CMFIVTMg==`). It is not secret, is never transmitted, and carries no
per-packet information — but GCM's tag covers it, so a packet sealed with a different value,
or with none, fails to open. An implementation that omits it authenticates a different
message and every packet is rejected, silently, at the receiver.

**Complete on-wire layout for an encrypted audio packet's payload**:

```
nonce (12 bytes) || ciphertext (same length as plaintext) || tag (16 bytes)
```

Total overhead: exactly 28 bytes over the unencrypted payload length. The payload-length
field is updated to the sealed length before transmission, and bit 7 of the flags byte
(`0x08`) is set; the receiver does the inverse on decrypt (§7).

---

## 6. Nonce

A 96-bit (12-byte) counter, stored per remote, initialized to zero when the remote is
created, and incremented as a LITTLE-ENDIAN integer: the lowest-addressed of the twelve
bytes is the least significant and carries upward. The twelve bytes go on the wire in that
same memory order, so the first byte of the payload is the one that changes on every
packet. Reading and incrementing it is one atomic step under a lock.

**Never reset except at creation.** Every packet under a given key gets a unique nonce.
Nonce reuse under a fixed AES-GCM key is a catastrophic failure — an observer of two
ciphertexts under the same key and nonce recovers the XOR of the two plaintexts and, in GCM,
can forge the authentication tag.

---

## 7. Receive-side behaviour

```
encrypted = the packet's encrypted-flag bit (offset 0x08, bit 7)

if remote.encryption_on:
    if not encrypted: reject the packet                 # plaintext
    combined  = the packet's payload, used as-is — the open call takes
                the nonce from its first 12 bytes
    plaintext = AES_256_GCM_open(combined, key: recv_key,
                                 additionalData: AAD)   # §5.1's constant
    if plaintext is nil: reject the packet              # see §7.1
    replace payload with plaintext; update length field
else:
    if encrypted: reject the packet
```

The flag must agree with the remote's setting in both directions: a remote with encryption
on never has plaintext audio played, and one with it off never has an encrypted packet fed
to its decoder. A rejected packet goes no further — no decode, no meter — though it has
already been counted, and its channel still shows as active
(`CASCADE_AUDIO_RECEIVE_SPEC.md` §1.1).

### 7.1 Decrypt failure — silent and unambiguous

A failed open — an authentication failure, corruption, or a wrong key — rejects the packet
outright. There is no logging and no distinction between causes at this layer; the packet
is neither partially processed nor decoded.

### 7.2 No replay protection

Nothing tracks which nonces have already been processed. The nonce is used only as part of
the sealed payload handed to the open call; it is never compared against earlier values.

**This does not affect decryption correctness — it is a freshness gap, not a
confidentiality or authenticity gap.** A replayed packet genuinely is authentic: it was
sealed by a holder of the key and has not been altered. Nothing fake, corrupted or crafted
can be injected through it; what gets through is real audio the peer genuinely sent, at
some point. A captured packet replayed later by someone with network-path access would
decrypt and play again.

**Scope**: a single replayed packet is one audio frame (2.5-20ms), a minor and likely
inaudible glitch. It matters more only if an attacker can capture and replay a *sequence*
of packets, which needs enough network-path access to intercept and inject UDP traffic in
the first place — more relevant across the open internet than on an isolated venue LAN.

An implementation adding protection should use a **sliding window per remote**, not a
"reject anything at or below the last-seen nonce" check, which would drop legitimately
reordered UDP traffic: track the highest nonce successfully opened, accept anything within
a window below it not already seen (a small per-remote bitmap over the last N nonces), and
reject exact repeats and anything older than the window. Replay protection is not part of
this specification.

---

## 8. Summary — implementation checklist

- [ ] A random X25519 keypair per remote, created with the remote (§2)
- [ ] Both identity hashes on every poke, at wire offsets `0x15`/`0x25` (§3.1)
- [ ] Key exchange extension exactly when encryption is on, appended to the ordinary poke, not a separate packet (§3.2)
- [ ] A 53-byte poke to an encrypting remote, or an 87-byte one to a non-encrypting remote, is dropped whole at dispatch — no address learning, config request, pong or byte count — and logged once per disagreement (§3.2)
- [ ] Extension presence must agree with the remote's setting; either disagreement ABANDONS the exchange and withholds the reply entirely (§3.2)
- [ ] Completion is set only by a key arriving on a pong; a poke-borne key derives without arming encrypted send (§3.2)
- [ ] Derivation: scalarmult → all-zero check → `BLAKE2b-512(shared‖pk‖pk)` first 32 bytes, crossed (§4)
- [ ] Send: encryption off sends plaintext; encryption on without a completed exchange DROPS (§5)
- [ ] On-wire encrypted payload: `nonce(12) ‖ ciphertext ‖ tag(16)`, `additionalData` the fixed 16-byte constant (§5.1)
- [ ] Nonce: 96-bit little-endian counter per remote, advanced atomically, never reused, never reset (§6)
- [ ] Receive: the encrypted flag must agree with the remote's setting both ways — plaintext at an encrypting remote and ciphertext at a non-encrypting one are both rejected (§7)
- [ ] Decrypt failure: silent, unambiguous rejection (§7.1)
- [ ] Replay protection: none (§7.2)
