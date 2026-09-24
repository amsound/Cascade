//! Routing table types: which local channels are sent to which remote slots, and which
//! incoming slots play on which local outputs.
//!
//! # Matrix string
//! A remote's send and receive matrices — as saved in config and sent by the web UI — are
//! comma-separated "row:col:value" entries.
//!   - row  = source channel index (0-based, 0-127)
//!   - col  = destination channel index (0-based, 0-127)
//!   - value = int32, non-zero = routed (typically 1), zero = absent (not emitted)
//! Only non-zero cells are serialised. Indices are 0-based with no +1/-1 offset.
//!
//! # Routing model
//! Send routing, per remote: local source channel → remote slot(s), carried in the packet
//!   header's channel field. One source per slot; a source may feed several slots.
//!
//! Receive routing, per remote: incoming packet channel field → local output channel(s),
//!   one-to-many.
//!
//! Nothing is routed by default in either direction: a channel with no route is neither
//! sent nor played.

use std::collections::HashMap;

/// One routing entry. row=source, col=destination, value=int (non-zero=enabled).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
    pub src:   u8,   // source channel  (row, 0-based)
    pub dst:   u8,   // destination channel (col, 0-based)
    pub value: i32,  // matrix cell value (typically 1 for enabled)
}

/// Namespace for the routing helpers below. Never constructed — the parsers and
/// lookup builders are associated functions, and callers hold `Vec<RouteEntry>`
/// directly rather than a table object.
pub struct RoutingTable;

impl RoutingTable {
    /// Parse a channelmatrix wire string into a list of route entries.
    /// Silently skips malformed entries and zero-value cells.
    pub fn parse_matrix(s: &str) -> Vec<RouteEntry> {
        s.split(',')
            .filter_map(|entry| {
                let mut parts = entry.trim().split(':');
                let src: u8  = parts.next()?.parse().ok()?;
                let dst: u8  = parts.next()?.parse().ok()?;
                let val: i32 = parts.next()?.parse().ok()?;
                if val == 0 { return None; }
                Some(RouteEntry { src, dst, value: val })
            })
            .collect()
    }

    /// Build a fast lookup: incoming slot → LIST of local output channels.
    /// A slot may route to multiple outputs (one-to-many fan-out). Empty/missing
    /// = unrouted (dropped).
    pub fn receive_lookup(routes: &[RouteEntry]) -> HashMap<u8, Vec<u8>> {
        let mut m: HashMap<u8, Vec<u8>> = HashMap::new();
        for r in routes {
            if r.value == 0 { continue; }
            let outs = m.entry(r.src).or_default();
            if !outs.contains(&r.dst) { outs.push(r.dst); }
        }
        m
    }
}

/// Per-peer receive routing state held inside PeerGroup.
#[derive(Debug, Clone, Default)]
pub struct PeerReceiveRouting {
    /// incoming slot → list of local output channels (one-to-many fan-out).
    pub slot_to_outs: HashMap<u8, Vec<u8>>,
}

impl PeerReceiveRouting {
    pub fn new(routes: &[RouteEntry], _num_out_ch: u8) -> Self {
        Self { slot_to_outs: RoutingTable::receive_lookup(routes) }
    }

    /// Resolve incoming slot to its list of local output channels.
    /// Empty when this slot has no route (policy: nothing routed unless specified).
    pub fn resolve(&self, slot: u8) -> Vec<u8> {
        self.slot_to_outs.get(&slot).cloned().unwrap_or_default()
    }
}

/// Per-remote send routing state held in CaptureEngine.
///
/// SEND MODEL — one source per outgoing slot, many slots per source.
///
/// The load-bearing invariant is ONE SOURCE PER SLOT: two sources into one outgoing
/// Opus slot is invalid — it would require mixing before encode, or send TWO competing
/// packets for one slot. It is enforced HERE so a malformed matrix can never do that.
///
/// A source feeding SEVERAL slots is legal and is required by the line-up tone legs,
/// which the TX matrix allows onto any number of outputs (web.html: the tone-row
/// click handler clears only the column, never the row). Each slot is an independent
/// wire channel carrying the same encoded frame with its own destination index, so
/// the encode still happens once per source per frame — the fan-out is at the header,
/// exactly as it is across remotes.
#[derive(Debug, Clone)]
pub struct PeerSendRouting {
    /// local source channel → remote slots. Slots are unique across the whole map;
    /// a source may hold more than one.
    pub channel_to_slot: HashMap<u8, Vec<u8>>,
}

impl PeerSendRouting {
    pub fn new(routes: &[RouteEntry]) -> Self {
        // Routes are applied IN ORDER and a later route claiming an in-use slot evicts
        // the previous source from that slot. Callers rely on this: the composed route
        // list puts tone AFTER signal, so a tone leg dropped onto an occupied slot
        // displaces the signal source there — which is the mutual exclusion the TX
        // matrix shows, obtained from the ordering rather than a special case.
        let mut channel_to_slot: HashMap<u8, Vec<u8>> = HashMap::new();
        let mut slot_owner: HashMap<u8, u8> = HashMap::new(); // slot → source
        for r in routes {
            if r.value == 0 { continue; }
            let (src, slot) = (r.src, r.dst);
            // Evict any previous source on this slot (one source per slot). A source
            // left with no slots at all is dropped so `channel_to_slot` never carries
            // an empty vector — callers treat "absent" and "empty" alike, and keeping
            // both spellings would mean two ways to say unrouted.
            if let Some(prev_src) = slot_owner.remove(&slot) {
                if let Some(v) = channel_to_slot.get_mut(&prev_src) {
                    v.retain(|&s| s != slot);
                    if v.is_empty() { channel_to_slot.remove(&prev_src); }
                }
            }
            let v = channel_to_slot.entry(src).or_default();
            if !v.contains(&slot) { v.push(slot); }
            slot_owner.insert(slot, src);
        }
        // Deterministic emission order per source (the send pass walks these in turn).
        for v in channel_to_slot.values_mut() { v.sort_unstable(); }
        Self { channel_to_slot }
    }

    /// The remote slots local channel `ch` is sent as. Empty when unrouted.
    pub fn resolve(&self, local_ch: u8) -> &[u8] {
        self.channel_to_slot.get(&local_ch).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Total outgoing wire channels to this remote — one per (source, slot) pair.
    pub fn stream_count(&self) -> usize {
        self.channel_to_slot.values().map(|v| v.len()).sum()
    }
}

#[cfg(test)]
mod send_routing_tests {
    use super::*;

    fn r(src: u8, dst: u8) -> RouteEntry { RouteEntry { src, dst, value: 1 } }

    #[test]
    fn source_may_feed_many_slots() {
        // The tone legs rely on this: the TX matrix puts one leg on any number of
        // outputs, and each is an independent wire channel.
        let t = PeerSendRouting::new(&[r(9, 0), r(9, 3), r(9, 1)]);
        assert_eq!(t.resolve(9), &[0, 1, 3]);          // sorted for deterministic emission
        assert_eq!(t.stream_count(), 3);
    }

    #[test]
    fn one_source_per_slot_last_write_wins() {
        // The load-bearing invariant: two sources into one outgoing Opus slot would
        // produce two competing packets for that slot.
        let t = PeerSendRouting::new(&[r(2, 5), r(7, 5)]);
        assert_eq!(t.resolve(2), &[] as &[u8]);
        assert_eq!(t.resolve(7), &[5]);
        assert_eq!(t.stream_count(), 1);
    }

    #[test]
    fn eviction_keeps_the_sources_other_slots() {
        // Losing one slot to another source must not unroute the rest.
        let t = PeerSendRouting::new(&[r(1, 0), r(1, 1), r(1, 2), r(4, 1)]);
        assert_eq!(t.resolve(1), &[0, 2]);
        assert_eq!(t.resolve(4), &[1]);
    }

    #[test]
    fn tone_applied_after_signal_displaces_it() {
        // rebuild_send_routing composes signal first, then tone — this ordering IS the
        // mutual exclusion the TX matrix shows, with no special-case pass.
        let t = PeerSendRouting::new(&[r(3, 0), r(3, 1), r(64, 1)]);   // 64 = a tone leg
        assert_eq!(t.resolve(3), &[0]);
        assert_eq!(t.resolve(64), &[1]);
    }

    #[test]
    fn a_source_stripped_of_every_slot_is_absent_not_empty() {
        // Callers treat "absent" and "empty" alike; keeping both would be two spellings
        // of unrouted.
        let t = PeerSendRouting::new(&[r(2, 5), r(7, 5)]);
        assert!(!t.channel_to_slot.contains_key(&2));
    }

    #[test]
    fn duplicate_route_does_not_double_the_slot() {
        let t = PeerSendRouting::new(&[r(1, 4), r(1, 4)]);
        assert_eq!(t.resolve(1), &[4]);
        assert_eq!(t.stream_count(), 1);
    }
}
