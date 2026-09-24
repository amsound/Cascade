pub mod protocol;
pub mod udp;
pub mod peer;
pub mod iface;
pub mod crypto;

/// Shared per-remote crypto state, keyed by remote name. Created in main and
/// shared with the peer task (key exchange), the audio send path (encrypt), and
/// the audio receive path (decrypt).
pub type CryptoMap = std::sync::Arc<std::sync::RwLock<
    std::collections::HashMap<String, std::sync::Arc<crypto::PeerCrypto>>>>;

/// A remote's link indices: the header bytes 0x0B (`source`) and 0x11 (`destination`)
/// this side writes on the audio, config-request and config-push packets it sends to that
/// remote (CASCADE_WIRE_PROTOCOL_SPEC §2.1).
///
/// Both start at zero and are relearned from every accepted pong: `source` takes the
/// pong's byte 0x11 and `destination` its byte 0x0B. Inbound audio, config requests and
/// config pushes from the remote must carry the same pair back — `source` at 0x11,
/// `destination` at 0x0B — or they are not accepted as that remote's traffic.
///
/// Pokes are always sent with both bytes zero and a pong echoes the poke it answers, so
/// the pair stays zero in practice; the learning and the check are what hold if a peer
/// ever sends otherwise.
#[derive(Default)]
pub struct LinkIndices(std::sync::atomic::AtomicU16);

impl LinkIndices {
    /// (source, destination).
    pub fn get(&self) -> (u8, u8) {
        let v = self.0.load(std::sync::atomic::Ordering::Relaxed);
        ((v >> 8) as u8, v as u8)
    }
    /// Learn from an accepted pong's raw bytes.
    pub fn learn_from_pong(&self, pong: &[u8]) {
        if pong.len() <= 0x11 { return; }
        let v = ((pong[0x11] as u16) << 8) | pong[0x0B] as u16;
        self.0.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    /// Whether an inbound audio / config-request / config-push header carries this pair.
    pub fn accepts(&self, buf: &[u8]) -> bool {
        if buf.len() <= 0x11 { return false; }
        let (source, destination) = self.get();
        buf[0x11] == source && buf[0x0B] == destination
    }
    /// Write this pair into an outbound header.
    pub fn stamp(&self, buf: &mut [u8]) {
        let (source, destination) = self.get();
        buf[0x0B] = source;
        buf[0x11] = destination;
    }
}

/// Per-remote link indices, keyed by remote name — shared by the peer task (learns them),
/// the send path (stamps them) and the receive path (checks them).
pub type LinkMap = std::sync::Arc<std::sync::RwLock<
    std::collections::HashMap<String, std::sync::Arc<LinkIndices>>>>;

/// Whether `buf` carries the link indices of remote `name`. A remote with no entry yet
/// holds the starting pair, zero and zero. Read-only: safe on the receive thread.
pub fn link_accepts(map: &LinkMap, name: &str, buf: &[u8]) -> bool {
    match map.read().unwrap_or_else(|e| e.into_inner()).get(name) {
        Some(l) => l.accepts(buf),
        None    => buf.len() > 0x11 && buf[0x0B] == 0 && buf[0x11] == 0,
    }
}

/// Get-or-create a remote's link indices.
pub fn link_for(map: &LinkMap, name: &str) -> std::sync::Arc<LinkIndices> {
    if let Some(l) = map.read().unwrap_or_else(|e| e.into_inner()).get(name) {
        return std::sync::Arc::clone(l);
    }
    std::sync::Arc::clone(map.write().unwrap_or_else(|e| e.into_inner())
        .entry(name.to_string()).or_default())
}

/// Resolve a hostname + port to an IPv4 SocketAddr asynchronously via tokio.
/// Uses tokio's non-blocking DNS resolver (spawns getaddrinfo on the blocking
/// thread pool).
///
/// IPv4 ONLY, like the peer task's re-resolution: the protocol is IPv4 (§1) and the audio
/// socket is AF_INET, so an IPv6 address is one nothing could send to. A host with no A
/// record — or an IPv6 literal — is an error, not a fallback.
pub async fn resolve_addr_async(host: &str, port: u16) -> anyhow::Result<std::net::SocketAddr> {
    if host.is_empty() { return Err(anyhow::anyhow!("empty host")); }
    // Fast path: an IPv4 literal skips the DNS round-trip.
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        return Ok(std::net::SocketAddr::from((ip, port)));
    }
    if host.trim_matches(|c| c == '[' || c == ']').parse::<std::net::Ipv6Addr>().is_ok() {
        return Err(anyhow::anyhow!("'{}' is an IPv6 address — remotes must be IPv4", host));
    }
    let addrs = tokio::net::lookup_host(format!("{}:{}", host, port))
        .await
        .map_err(|e| anyhow::anyhow!("DNS lookup '{}': {}", host, e))?;
    addrs.into_iter().find(|a| a.is_ipv4())
        .ok_or_else(|| anyhow::anyhow!("no IPv4 address for '{}'", host))
}

#[cfg(test)]
mod link_index_tests {
    use super::LinkIndices;

    fn hdr(b0b: u8, b11: u8) -> [u8; 21] {
        let mut b = [0u8; 21];
        b[0x0B] = b0b;
        b[0x11] = b11;
        b
    }

    /// A fresh remote accepts only the all-zero pair.
    #[test]
    fn starts_at_zero() {
        let l = LinkIndices::default();
        assert_eq!(l.get(), (0, 0));
        assert!(l.accepts(&hdr(0, 0)));
        assert!(!l.accepts(&hdr(1, 0)));
        assert!(!l.accepts(&hdr(0, 1)));
    }

    /// A pong's 0x11 becomes `source`, its 0x0B `destination`; inbound traffic must then
    /// carry `source` at 0x11 and `destination` at 0x0B, and outbound headers are stamped
    /// with `source` at 0x0B and `destination` at 0x11.
    #[test]
    fn learns_checks_and_stamps_crossed() {
        let l = LinkIndices::default();
        l.learn_from_pong(&hdr(7, 3));           // pong: 0x0B = 7, 0x11 = 3
        assert_eq!(l.get(), (3, 7));             // source = 3, destination = 7
        assert!(l.accepts(&hdr(7, 3)));          // inbound: 0x11 == 3, 0x0B == 7
        assert!(!l.accepts(&hdr(3, 7)));
        let mut out = [0u8; 21];
        l.stamp(&mut out);
        assert_eq!((out[0x0B], out[0x11]), (3, 7));
    }

    /// A truncated buffer is never accepted and never learned from.
    #[test]
    fn short_buffers_are_ignored() {
        let l = LinkIndices::default();
        l.learn_from_pong(&[0xFF; 0x11]);
        assert_eq!(l.get(), (0, 0));
        assert!(!l.accepts(&[0u8; 0x11]));
    }
}
