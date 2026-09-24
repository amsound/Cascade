//! Per-remote end-to-end audio encryption (CASCADE_ENCRYPTION_SPEC).
//!
//! X25519 key agreement + BLAKE2b-512 key derivation (the crossed construction) +
//! AES-256-GCM AEAD over a fixed additional-data constant. One `PeerCrypto` per
//! configured remote, shared (behind Arc)
//! between three contexts: the peer task (does the key exchange on poke/pong),
//! the audio send path (encrypts), and the audio receive path (decrypts).
//!
//! Gates and their asymmetry are implemented exactly as §5 specifies: when
//! encryption is not enabled the packet goes out in the clear; when it IS enabled
//! but the handshake has not completed, the packet is DROPPED — never sent in the
//! clear while encryption is pending.
//!
//! Replay protection: NONE, as §7.2 requires. AES-GCM still authenticates every
//! packet as genuinely from the key holder; what is absent is freshness tracking, so
//! a captured ciphertext replayed by a network-path attacker would decode again.
//! This is a deliberate wire-compatibility decision, not an oversight — a per-remote
//! sliding window is the shape to add if freshness is later required.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use aes_gcm::{Aes256Gcm, Key, Nonce, KeyInit, aead::{Aead, Payload}};
use blake2::{Blake2b512, Digest};
use x25519_dalek::{StaticSecret, PublicKey};

const NONCE_LEN: usize = 12;
const TAG_LEN:   usize = 16;

/// The additional authenticated data every packet is sealed and opened with
/// (CASCADE_ENCRYPTION_SPEC §5.1). A fixed 16-byte constant, identical for every
/// remote and every packet: it is not secret and is never transmitted — both ends
/// simply hold it, and GCM's tag covers it, so a packet sealed with any other value
/// (or with none) fails to open.
const AAD: [u8; 16] = [
    0xa6, 0xe7, 0x14, 0x47, 0x2f, 0x5c, 0xe2, 0x4d,
    0xac, 0x8f, 0x90, 0x8c, 0x14, 0x85, 0x53, 0x32,
];

/// Result of the send-side gate (§5) — the three outcomes are NOT uniform.
pub enum SealResult {
    /// Encryption not enabled for this remote — send the payload unencrypted.
    Plaintext,
    /// Encryption enabled but the handshake has not completed — DROP the packet
    /// entirely (never leak plaintext while encryption is pending).
    Drop,
    /// Encrypted payload: `nonce(12) || ciphertext || tag(16)`. Set flags bit 7.
    Sealed(Vec<u8>),
}

struct Inner {
    /// Peer's X25519 public key, once learned from a key-exchange poke.
    peer_public: Option<[u8; 32]>,
    /// Direction keys (§4): our send == peer's recv, via the crossed derivation.
    send_key: Option<[u8; 32]>,
    recv_key: Option<[u8; 32]>,
    complete: bool,
    /// The 96-bit nonce itself, held as the 12 bytes that go on the wire and
    /// incremented as a little-endian integer once per sealed packet (§6). Starts at
    /// zero and is never reset except at construction: a repeat under one key would
    /// expose the keystream for both packets sealed with it.
    nonce: [u8; NONCE_LEN],
}

pub struct PeerCrypto {
    our_secret: StaticSecret,
    our_public: [u8; 32],
    /// Per-remote encryption setting. §1's separate licence gate is folded in as
    /// always-true here — Cascade has no licence concept — so the spec's two
    /// user-facing gates collapse to this one flag plus `complete`.
    enabled: AtomicBool,
    inner: Mutex<Inner>,
}

impl PeerCrypto {
    /// Fresh, random keypair (§2) — generated once, at remote construction.
    pub fn new(enabled: bool) -> Self {
        let our_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let our_public = PublicKey::from(&our_secret).to_bytes();
        PeerCrypto {
            our_secret,
            our_public,
            enabled: AtomicBool::new(enabled),
            inner: Mutex::new(Inner {
                peer_public: None,
                send_key: None,
                recv_key: None,
                complete: false,
                nonce: [0u8; NONCE_LEN],
            }),
        }
    }

    pub fn set_enabled(&self, on: bool) { self.enabled.store(on, Ordering::Relaxed); }
    pub fn is_enabled(&self) -> bool { self.enabled.load(Ordering::Relaxed) }
    pub fn our_public(&self) -> [u8; 32] { self.our_public }

    /// Whether the send-side latch is set — gates encrypted send. Distinct from
    /// "keys are derived": a key learned from an inbound poke derives both keys but
    /// leaves this false until a peer answers one of ours (see `on_peer_public`).
    pub fn is_complete(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).complete
    }

    /// Ingest a peer public key from a key-exchange poke or pong (§3.2/§4). An
    /// unchanged key takes the fast path — the existing keys stand, nothing is
    /// re-derived and the nonce keeps counting. A new key derives both session keys,
    /// rejecting a degenerate all-zero shared secret.
    ///
    /// `from_response` says the key arrived on a POKE RESPONSE rather than on an
    /// inbound poke, and it alone sets the send-side latch: this side begins
    /// encrypting once a peer has ANSWERED one of its pokes, not merely because a
    /// peer has spoken to it. Keys derived from an inbound poke are usable for
    /// decryption immediately — `open` needs no latch — so an unanswered poke still
    /// leaves this side able to read that peer's traffic.
    ///
    /// Returns whether the key was ACCEPTED. False means rejected outright, which is
    /// a reason to abandon the exchange; it is not the latch, which `is_complete`
    /// reports and which a poke-borne key deliberately leaves alone.
    pub fn on_peer_public(&self, peer_pub: [u8; 32], from_response: bool) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.peer_public != Some(peer_pub) {
            let shared = self.our_secret
                .diffie_hellman(&PublicKey::from(peer_pub));
            // All-zero shared secret rejection (§4) — a low-order peer key. Never
            // expected with two real random keypairs, but checked as the spec requires.
            if !shared.was_contributory() {
                tracing::warn!("crypto: rejecting all-zero shared secret (low-order peer key)");
                return false;
            }
            let shared = shared.to_bytes();
            // Crossed derivation (§4): the FIRST 32 bytes of each BLAKE2b-512 digest, with
            // the two public keys in opposite orders for the two directions.
            //   recv_key = BLAKE2b512(shared || our_pub  || peer_pub)[0:32]
            //   send_key = BLAKE2b512(shared || peer_pub || our_pub )[0:32]
            // Crossing them is what makes the scheme symmetric without either end having to
            // know which of the two it is: this side's send key is the other side's recv key,
            // because both are the digest of the same three inputs in the same order.
            g.recv_key = Some(blake2b_low(&[&shared, &self.our_public, &peer_pub]));
            g.send_key = Some(blake2b_low(&[&shared, &peer_pub, &self.our_public]));
            g.peer_public = Some(peer_pub);
            tracing::info!("crypto: session keys derived");
        }
        if from_response && !g.complete {
            g.complete = true;
            tracing::info!("crypto: key exchange complete");
        }
        true
    }

    // Note (not part of `seal`'s docs): there is no re-arm on disconnect/reconnect.
    // The keypair and any derived keys persist for the remote's lifetime (§2). A
    // genuine peer restart sends a fresh public key, which `on_peer_public` picks up
    // via the changed-key path.

    /// Send-side gate + seal (§5/§5.1/§6).
    pub fn seal(&self, plaintext: &[u8]) -> SealResult {
        if !self.enabled.load(Ordering::Relaxed) {
            return SealResult::Plaintext;
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !g.complete {
            return SealResult::Drop;   // enabled but not ready — never plaintext
        }
        let key = match g.send_key { Some(k) => k, None => return SealResult::Drop };
        // 96-bit nonce (§6): this packet takes the current value, and the counter moves
        // on for the next one.
        let nonce = g.nonce;
        increment_le(&mut g.nonce);
        drop(g);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        match cipher.encrypt(Nonce::from_slice(&nonce),
                             Payload { msg: plaintext, aad: &AAD }) {
            Ok(ct) => {
                // nonce || ciphertext || tag (the tag is already appended by the
                // AEAD; the nonce travels with the packet, §5.1).
                let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
                out.extend_from_slice(&nonce);
                out.extend_from_slice(&ct);
                SealResult::Sealed(out)
            }
            Err(_) => SealResult::Drop,   // seal failure — do not send in the clear
        }
    }

    /// Receive-side open (§7/§7.1). Caller has already checked that encryption is
    /// enabled and the packet's bit-7 flag is set. `combined` = nonce||ct||tag.
    /// Returns None on any AEAD failure — a clean, silent, unambiguous rejection.
    pub fn open(&self, combined: &[u8]) -> Option<Vec<u8>> {
        if combined.len() < NONCE_LEN + TAG_LEN { return None; }
        let key = {
            let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.recv_key?
        };
        let (nonce, ct) = combined.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: &AAD }).ok()
    }
}

/// First 32 bytes of `BLAKE2b-512(parts concatenated)`, unkeyed.
fn blake2b_low(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Blake2b512::new();
    for p in parts { h.update(p); }
    let digest = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest[0..32]);
    out
}

/// Add one to a little-endian integer held as bytes: the lowest-addressed byte is the
/// least significant, and a carry moves upward. Wraps to zero at 2^96, which one key
/// cannot reach — at 50 packets a second per channel it is longer than the age of the
/// universe.
fn increment_le(n: &mut [u8; NONCE_LEN]) {
    for b in n.iter_mut() {
        let (v, carry) = b.overflowing_add(1);
        *b = v;
        if !carry { break; }
    }
}

/// The three wire-visible parts of the construction: the derived keys, the nonce
/// sequence, and the additional data the tag covers. Each is a property another
/// implementation has to match exactly for a packet to open at all.
#[cfg(test)]
mod tests {
    use super::*;

    /// Two ends that have each answered the other — both latched, so both may seal.
    fn pair() -> (PeerCrypto, PeerCrypto) {
        let a = PeerCrypto::new(true);
        let b = PeerCrypto::new(true);
        assert!(a.on_peer_public(b.our_public(), true));
        assert!(b.on_peer_public(a.our_public(), true));
        (a, b)
    }

    /// A key borne by a POKE derives both session keys but does NOT arm the send side:
    /// this end encrypts only once a peer has answered a poke of its own.
    #[test]
    fn a_poke_borne_key_derives_without_arming_send() {
        let a = PeerCrypto::new(true);
        let b = PeerCrypto::new(true);
        assert!(a.on_peer_public(b.our_public(), false));
        assert!(!a.is_complete());
        assert!(matches!(a.seal(b"frame"), SealResult::Drop));

        // ...and the keys it derived are already usable for decryption, which needs no
        // latch: b seals with its send key, a opens with the recv key it just derived.
        assert!(b.on_peer_public(a.our_public(), true));
        let sealed = match b.seal(b"frame") {
            SealResult::Sealed(v) => v,
            _ => panic!("b should seal once armed"),
        };
        assert_eq!(a.open(&sealed).as_deref(), Some(&b"frame"[..]));

        // The pong that finally answers a's poke arms it, without re-deriving.
        assert!(a.on_peer_public(b.our_public(), true));
        assert!(a.is_complete());
        assert!(matches!(a.seal(b"frame"), SealResult::Sealed(_)));
    }

    /// BLAKE2b-512, first 32 bytes, against a digest computed outside this crate.
    #[test]
    fn the_derivation_hash_is_blake2b_512() {
        let q = [1u8; 32];
        let pk_a = [2u8; 32];
        let pk_b = [3u8; 32];
        assert_eq!(blake2b_low(&[&q, &pk_a, &pk_b]), [
            0x33, 0x14, 0xca, 0xf2, 0xdd, 0x15, 0x5f, 0x7e,
            0x7f, 0xde, 0xa9, 0xec, 0xc3, 0xa4, 0x2f, 0xaf,
            0x64, 0x22, 0x52, 0xe9, 0x87, 0x3e, 0xbb, 0xb1,
            0x0b, 0xbf, 0x10, 0x8d, 0xc9, 0x51, 0x12, 0xc7,
        ]);
    }

    /// Crossing the public keys is what makes one side's send key the other's receive
    /// key, with neither end needing to know which of the two it is.
    #[test]
    fn each_sides_send_key_is_the_others_receive_key() {
        let (a, b) = pair();
        let (a_send, a_recv) = {
            let g = a.inner.lock().unwrap();
            (g.send_key.unwrap(), g.recv_key.unwrap())
        };
        let (b_send, b_recv) = {
            let g = b.inner.lock().unwrap();
            (g.send_key.unwrap(), g.recv_key.unwrap())
        };
        assert_eq!(a_send, b_recv);
        assert_eq!(b_send, a_recv);
        assert_ne!(a_send, a_recv, "the two directions use different keys");
    }

    #[test]
    fn a_sealed_packet_opens_at_the_other_end() {
        let (a, b) = pair();
        let sealed = match a.seal(b"the quick brown fox") {
            SealResult::Sealed(v) => v,
            _ => panic!("expected a sealed packet"),
        };
        assert_eq!(b.open(&sealed).as_deref(), Some(&b"the quick brown fox"[..]));
    }

    /// The nonce is the 12 bytes on the wire, counting up from zero as a little-endian
    /// integer — the low-addressed byte moves first.
    #[test]
    fn nonces_count_up_from_zero_little_endian() {
        let (a, b) = pair();
        for expected in 0u8..3 {
            let sealed = match a.seal(b"x") {
                SealResult::Sealed(v) => v,
                _ => panic!("expected a sealed packet"),
            };
            let mut want = [0u8; NONCE_LEN];
            want[0] = expected;
            assert_eq!(&sealed[..NONCE_LEN], &want, "packet {expected}");
        }
        let _ = b;
    }

    /// The carry moves upward through the bytes, so 0xff rolls into the next one.
    #[test]
    fn the_nonce_carries_upward() {
        let mut n = [0u8; NONCE_LEN];
        n[0] = 0xff;
        increment_le(&mut n);
        let mut want = [0u8; NONCE_LEN];
        want[1] = 1;
        assert_eq!(n, want);

        let mut all_ones = [0xffu8; NONCE_LEN];
        increment_le(&mut all_ones);
        assert_eq!(all_ones, [0u8; NONCE_LEN], "wraps to zero at 2^96");
    }

    /// The tag covers the additional data, so the same ciphertext under the same key
    /// and nonce does not open without it. This is what a mismatched constant costs:
    /// every packet fails, silently.
    #[test]
    fn the_tag_covers_the_additional_data() {
        let (a, b) = pair();
        let sealed = match a.seal(b"payload") {
            SealResult::Sealed(v) => v,
            _ => panic!("expected a sealed packet"),
        };
        let key = { b.inner.lock().unwrap().recv_key.unwrap() };
        let (nonce, ct) = sealed.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        assert!(cipher.decrypt(Nonce::from_slice(nonce),
                               Payload { msg: ct, aad: b"" }).is_err(),
                "opened without the additional data");
        assert!(cipher.decrypt(Nonce::from_slice(nonce),
                               Payload { msg: ct, aad: &AAD }).is_ok(),
                "and opens with it");
    }
}
