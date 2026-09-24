/// Cascade wire protocol — packet constants and constructors.
///
/// Field offsets, per-opcode meanings and endianness are defined by
/// CASCADE_WIRE_PROTOCOL_SPEC; this module implements them.

// ── Fixed constants ───────────────────────────────────────────────────────

pub const RTP_BYTE0: u8 = 0x80;   // V=2, P=0, X=0, CC=0
pub const RTP_BYTE1: u8 = 0x69;   // M=0, PT=105
pub const PROTO_ID:  u8 = 0x09;   // byte 0x0C: declared header length, always 9

/// Header field values, named per CASCADE_WIRE_PROTOCOL_SPEC §2.1. Bytes 0x08-0x0B are
/// four INDEPENDENT single-byte fields — flags, sample rate, label revision, source
/// index — not one 32-bit word; this protocol has no SSRC. Audio streams are identified by
/// source address + channel index.
///
/// 0x09 sample rate: 1 = 48000 Hz, 0 = 44100 Hz. Audio packets only.
pub const SAMPLE_RATE_48K: u8 = 1;
/// 0x0B source index / 0x11 destination index: the link indices. Every header is built
/// with both zero; audio, config-request and config-push packets then have the
/// destination remote's learned pair stamped over them (`net::LinkIndices`), pokes stay
/// zero, and a pong echoes the poke it answers. Routing is decided by the channel number
/// at 0x12, never by these.
pub const SOURCE_INDEX_INITIAL: u8 = 0;   // 0x0B
pub const DEST_INDEX_INITIAL:   u8 = 0;   // 0x11

/// Token = MD5(UPPERCASE(TRIM(name)) + UPPERCASE(TRIM(password)))
/// Trim-then-uppercase is applied to each field independently, no separator
/// (CASCADE_WIRE_PROTOCOL_SPEC §4).
pub fn derive_token(name: &str, password: &str) -> Token {
    let input = format!("{}{}", name.trim().to_uppercase(), password.trim().to_uppercase());
    let digest = md5_hex(input.as_bytes());
    let mut bytes = [0u8; TOKEN_LEN];
    for i in 0..TOKEN_LEN {
        bytes[i] = u8::from_str_radix(&digest[i*2..i*2+2], 16).unwrap();
    }
    Token(bytes)
}

fn md5_hex(data: &[u8]) -> String {
    format!("{:x}", ::md5::compute(data))
}

// ── Packet type ───────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    Audio   = 0x06,
    Poke    = 0x07,
    PokeRsp = 0x08,
    Ack     = 0x09,
    Label   = 0x0A,
}

impl TryFrom<u8> for PacketType {
    type Error = u8;
    fn try_from(v: u8) -> Result<Self, u8> {
        match v {
            0x06 => Ok(Self::Audio),
            0x07 => Ok(Self::Poke),
            0x08 => Ok(Self::PokeRsp),
            0x09 => Ok(Self::Ack),
            0x0A => Ok(Self::Label),
            // Opcode 11 is a genuine alias of opcode 6 (CASCADE_WIRE_PROTOCOL_SPEC §3):
            // same code path, codec selected by the flags byte, not the opcode.
            0x0B => Ok(Self::Audio),
            other => Err(other),
        }
    }
}

// ── Sizes ─────────────────────────────────────────────────────────────────

pub const HEADER_LEN: usize = 21;
pub const TOKEN_LEN:  usize = 16;
pub const ACK_LEN:    usize = HEADER_LEN + TOKEN_LEN;
pub const POKE_LEN:   usize = HEADER_LEN + TOKEN_LEN * 2;

// ── Token ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Token(pub [u8; TOKEN_LEN]);

impl Token {
    pub fn zero()    -> Self { Self([0u8; TOKEN_LEN]) }
}
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Token({})", hex::encode(self.0))
    }
}
impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

// ── Parsers ───────────────────────────────────────────────────────────────

pub fn parse_type(buf: &[u8]) -> Option<PacketType> {
    if buf.len() < HEADER_LEN  { return None; }
    if buf[0] != RTP_BYTE0     { return None; }
    if buf[1] != RTP_BYTE1     { return None; }
    // Declared header length (0x0C). Spec §2.1 says receivers compute payload boundaries
    // from this rather than assuming a fixed offset, so accept any value that still leaves
    // the known header fields (up to 0x14) intact — a larger value is a later protocol
    // revision with a longer header, which we can still parse.
    if (buf[12] as usize) < PROTO_ID as usize { return None; }
    PacketType::try_from(buf[13]).ok()
}

pub fn parse_sequence(buf: &[u8])  -> u16 { u16::from_be_bytes(buf[2..4].try_into().unwrap()) }

/// Flags byte at offset 0x08 (CASCADE_WIRE_PROTOCOL_SPEC §2.1). Bit 7 = payload
/// encrypted; bits 0-6 = audio codec (0 = Opus, 1 = raw 16-bit PCM, 2 = raw 24-bit PCM).
pub fn parse_flags(buf: &[u8]) -> u8 { buf[8] }

pub const FLAG_ENCRYPTED: u8 = 0x80;
pub const CODEC_OPUS:  u8 = 0;
pub const CODEC_RAW16: u8 = 1;
pub const CODEC_RAW24: u8 = 2;
pub fn parse_timestamp(buf: &[u8]) -> u32 { u32::from_be_bytes(buf[4..8].try_into().unwrap()) }
/// 0x09 sample rate (audio packets): 1 = 48kHz, anything else = 44.1kHz.
pub fn parse_sample_rate(buf: &[u8]) -> u8 { buf[9] }
/// 0x0A label revision (opcodes 7/8 only; reserved zero elsewhere).
pub fn parse_label_revision(buf: &[u8]) -> u8 { buf[10] }
/// 0x0C declared header length. Spec §2.1: "Used by receivers to compute payload
/// boundaries; do not assume payload always starts at a fixed offset without reading
/// this field." The count runs from 0x0C itself, so the payload begins at 0x0C + this.
pub fn parse_header_len(buf: &[u8]) -> usize { buf[12] as usize }
/// Absolute offset at which this packet's payload begins, DERIVED from the declared
/// header length rather than assumed — the spec's stated requirement.
pub fn payload_start(buf: &[u8]) -> usize { 0x0C + parse_header_len(buf) }
pub fn parse_channel(buf: &[u8])   -> u8  { buf[18] }
/// 0x10 read as a SIGNED byte on the audio path: a negative value is the sender saying
/// "I have restarted — flush your decoder". Not the config-push segment count, which
/// occupies the same offset on opcode 10 only; the two never coexist on one packet.
pub fn parse_sender_restart(buf: &[u8]) -> bool { (buf[16] as i8) < 0 }

/// Sender identity hash, which sits at the START of the payload on control packets.
/// Its offset is derived from the declared header length (§2.1), never assumed: a
/// sender that lengthens its header moves the token with it.
pub fn parse_sender_token(buf: &[u8]) -> Option<Token> {
    if buf.len() < HEADER_LEN { return None; }
    let start = payload_start(buf);
    if buf.len() < start + TOKEN_LEN { return None; }
    let mut t = [0u8; TOKEN_LEN];
    t.copy_from_slice(&buf[start..start + TOKEN_LEN]);
    Some(Token(t))
}

/// Opus frame length from audio packet — u16 little-endian at bytes 19-20.
pub fn parse_opus_len(buf: &[u8]) -> u16 {
    u16::from_le_bytes([buf[19], buf[20]])
}

// ── Header writer ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn write_header(buf: &mut [u8], seq: u16, ts: u32,
                flags: u8, sample_rate: u8, label_revision: u8,
                ptype: PacketType, seg_num: u8, seg_total: u8, len_lo: u8, len_hi: u8) {
    buf[0]  = RTP_BYTE0;
    buf[1]  = RTP_BYTE1;
    buf[2..4].copy_from_slice(&seq.to_be_bytes());
    buf[4..8].copy_from_slice(&ts.to_be_bytes());
    buf[8]  = flags;                     // 0x08 bit7 = encrypted, bits0-6 opcode-dependent
    buf[9]  = sample_rate;               // 0x09 audio only
    buf[10] = label_revision;            // 0x0A opcodes 7/8 only
    buf[11] = SOURCE_INDEX_INITIAL;      // 0x0B sourceIndex — link index, stamped per remote
    buf[12] = PROTO_ID;                  // 0x0C header length (9)
    buf[13] = ptype as u8;               // 0x0D opcode
    buf[14] = 0x00;
    buf[15] = seg_num;                   // 0x0F config-push, 1-based
    buf[16] = seg_total;                 // 0x10 config-push
    buf[17] = DEST_INDEX_INITIAL;        // 0x11 destinationIndex — link index, stamped per remote
    buf[18] = 0x00;                      // 0x12 channel (audio sets it after)
    buf[19] = len_lo;                    // 0x13 payload length, LITTLE-endian
    buf[20] = len_hi;
}

// ── Packet constructors ───────────────────────────────────────────────────

/// Key-exchange extension length appended to a poke/pong when encryption is
/// enabled (CASCADE_ENCRYPTION_SPEC §3.2): byte 0x35 = 0, 0x36 = 32, then the
/// 32-byte X25519 public key. 53 → 87 bytes.
pub const POKE_EXT_LEN: usize = 2 + TOKEN_LEN * 2;   // 34
pub const POKE_LEN_EXT: usize = POKE_LEN + POKE_EXT_LEN;   // 87

/// Append the key-exchange extension to a 53-byte poke/pong buffer, in place at
/// offsets 0x35-0x56 (§3.2). The buffer must already be `POKE_LEN_EXT` long.
fn write_key_ext(buf: &mut [u8], our_public: &[u8; 32]) {
    buf[POKE_LEN]     = 0;    // 0x35 — must be 0
    buf[POKE_LEN + 1] = 32;   // 0x36 — key length
    buf[POKE_LEN + 2..POKE_LEN_EXT].copy_from_slice(our_public);   // 0x37-0x56
}

/// Parse the peer's X25519 public key from an 87-byte key-exchange poke/pong
/// payload. `payload` starts one token-length past the payload boundary (§3.2),
/// i.e. past this side's echoed identity slot. Returns None unless the extension
/// is present and well-formed (0x35 == 0, 0x36 == 32).
pub fn parse_key_ext(payload: &[u8]) -> Option<[u8; 32]> {
    // payload = remote_token(16) + [0x35][0x36][pubkey(32)] = 16 + 34 = 50 bytes.
    if payload.len() < TOKEN_LEN + POKE_EXT_LEN { return None; }
    let ext = &payload[TOKEN_LEN..];
    if ext[0] != 0 || ext[1] != 32 { return None; }
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&ext[2..2 + 32]);
    Some(pk)
}

/// Build a poke. When `key_ext` is Some, the 34-byte key-exchange extension is
/// appended (§3.2) and the result is 87 bytes; otherwise the ordinary 53 bytes.
pub fn build_poke(ts: u32, our_token: &Token, remote_token: &Token,
                  label_change_indicator: u8, key_ext: Option<&[u8; 32]>) -> Vec<u8> {
    let mut buf = vec![0u8;
        if key_ext.is_some() { POKE_LEN_EXT } else { POKE_LEN }];
    // Label revision at 0x0A (§2.1) — the sender's own current value. Bytes 0x08, 0x09
    // and 0x0B are zero for this opcode, as is 0x11 — a poke never carries link indices.
    write_header(&mut buf, 0, ts, 0, 0, label_change_indicator,
                 PacketType::Poke, 0, 0, 0, 0);
    buf[HEADER_LEN..HEADER_LEN+TOKEN_LEN].copy_from_slice(&our_token.0);
    buf[HEADER_LEN+TOKEN_LEN..POKE_LEN].copy_from_slice(&remote_token.0);
    if let Some(pk) = key_ext { write_key_ext(&mut buf, pk); }
    buf
}

/// Build a poke-response. As `build_poke`, the key-exchange extension is appended
/// when `key_ext` is Some (§3: pong carries the extension too).
/// Build a pong (opcode 8) BY MODIFYING THE RECEIVED PING'S BUFFER — not by building a
/// fresh packet (CASCADE_WIRE_PROTOCOL_SPEC §2.1).
///
/// The pong is the one opcode whose "reserved (zero)" fields are not actively zeroed: it is
/// built in place from the incoming ping, so segment number (0x0F), total
/// segments (0x10) and 0x13 carry through unmodified from whatever the ping held — as do
/// the sequence, timestamp and the 0x08-0x0B field group (including the label revision,
/// which §2.1 notes a pong carries "through unchanged, by construction, rather than via a
/// separate write"). Every OTHER opcode (6/7/9/10) goes through the shared header builder,
/// which does actively write those zeros.
///
/// Building fresh and zeroing would look identical in ordinary traffic but diverge the
/// moment a ping arrives with something non-zero in those positions, which must be echoed
/// straight back.
///
/// The two documented differences from the ping are applied on top: the opcode byte, and
/// the payload, which for a pong is the responder's own identity repeated twice (§3).
pub fn build_poke_rsp_from_ping(ping: &[u8], our_token: &Token,
                                key_ext: Option<&[u8; 32]>) -> Vec<u8> {
    let out_len = if key_ext.is_some() { POKE_LEN_EXT } else { POKE_LEN };
    let mut buf = vec![0u8; out_len];
    // Carry the ping's own bytes through, bounded by what we can copy.
    let carry = ping.len().min(POKE_LEN);
    buf[..carry].copy_from_slice(&ping[..carry]);
    buf[13] = PacketType::PokeRsp as u8;                       // the one header byte written
    buf[HEADER_LEN..HEADER_LEN+TOKEN_LEN].copy_from_slice(&our_token.0);
    buf[HEADER_LEN+TOKEN_LEN..POKE_LEN].copy_from_slice(&our_token.0);
    if let Some(pk) = key_ext { write_key_ext(&mut buf, pk); }
    buf
}

/// Build an ACK packet.
///
/// Label-revision rules for the ACK (byte 0x0A):
/// - Responding to a POKE, sent immediately: carry the incoming POKE's own label-revision
///   byte through unchanged, same timestamp.
/// - After receiving a POKE_RSP, sent at the next poke tick: revision 0, own timestamp.
///
/// Built with 0x0B/0x11 zero; the caller stamps the remote's link indices
/// (`net::LinkIndices`).
pub fn build_ack(ts: u32, label_revision: u8, our_token: &Token, label_indicator: u8) -> [u8; ACK_LEN] {
    let mut buf = [0u8; ACK_LEN];
    // The ACK is the LABEL REQUEST: byte 0x0A carries the label-change indicator the
    // requester wants labels for, the same byte position as the POKE indicator.
    write_header(&mut buf, 0, ts, 0, 0, label_revision,
                 PacketType::Ack, 0, 0, 0, 0);
    buf[0x0A] = label_indicator;
    buf[HEADER_LEN..ACK_LEN].copy_from_slice(&our_token.0);
    buf
}

/// Build a label packet fragment.
///
/// Bytes 0x08-0x0A are zero for this opcode; the caller stamps the remote's link indices
/// at 0x0B/0x11 (`net::LinkIndices`).
/// 0x0F = seg_idx (1-based), 0x10 = total_segs.
/// 0x13 = LE16 body length after the header = TOKEN_LEN + this segment's JSON chunk
/// (§2.1: for config-push this field is the segment's own payload length).
pub fn build_label(ts: u32, our_token: &Token,
                   json: &[u8], seg_idx: u8, total_segs: u8) -> Vec<u8> {
    let body_len = (TOKEN_LEN + json.len()) as u16;
    let mut buf = vec![0u8; HEADER_LEN + TOKEN_LEN + json.len()];
    write_header(&mut buf, 0, ts, 0, 0, 0, PacketType::Label,
                 seg_idx, total_segs,
                 (body_len & 0xFF) as u8, (body_len >> 8) as u8);
    buf[HEADER_LEN..HEADER_LEN+TOKEN_LEN].copy_from_slice(&our_token.0);
    buf[HEADER_LEN+TOKEN_LEN..].copy_from_slice(json);
    buf
}

/// Write an audio packet header in-place into a pre-allocated buffer.
/// The Opus payload was already written at buf[HEADER_LEN..HEADER_LEN+opus_len]
/// by opus_encode_float. This function only fills the header prefix.
///
/// The header is written into a caller-supplied buffer that the Opus encoder has already
/// filled from HEADER_LEN onward — zero allocations per packet.
pub fn build_audio_into(buf: &mut [u8], seq: u16, ts: u32, channel: u8, opus_len: usize) {
    let [len_lo, len_hi] = (opus_len as u16).to_le_bytes();
    write_header(buf, seq, ts, CODEC_OPUS, SAMPLE_RATE_48K, 0,
                 PacketType::Audio, 0, 0, len_lo, len_hi);
    buf[18] = channel;
    // Opus payload already at buf[HEADER_LEN..HEADER_LEN+opus_len] — no copy needed.
}

/// The sender-restart marker at 0x10 — read as SIGNED on the audio path.
#[cfg(test)]
mod restart_marker_tests {
    use super::parse_sender_restart;

    fn hdr(byte_0x10: u8) -> [u8; 21] {
        let mut b = [0u8; 21];
        b[16] = byte_0x10;
        b
    }

    /// Only the sign bit means restart. Zero is the ordinary audio case.
    #[test]
    fn zero_is_not_a_restart() {
        assert!(!parse_sender_restart(&hdr(0)));
    }

    /// Every value with the high bit set signals a restart, not just 0xFF.
    #[test]
    fn any_negative_value_is_a_restart() {
        for v in [0x80u8, 0x81, 0xC0, 0xFE, 0xFF] {
            assert!(parse_sender_restart(&hdr(v)), "0x{v:02X} should signal restart");
        }
    }

    /// Positive values are not restarts — this is the same offset config-push uses for
    /// its segment total, and those counts are small positives.
    #[test]
    fn positive_values_are_not_restarts() {
        for v in [1u8, 2, 7, 0x7E, 0x7F] {
            assert!(!parse_sender_restart(&hdr(v)), "0x{v:02X} should not signal restart");
        }
    }
}
