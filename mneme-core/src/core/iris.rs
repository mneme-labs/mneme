// Iris — Slot router.
// CRC16 CCITT → slot (% 16384), per-slot node assignment, MOVED redirect.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

pub const NUM_SLOTS: u16 = 16384;

/// Describes the node that owns a slot.
#[derive(Debug, Clone)]
pub struct SlotOwner {
    pub node_id: u64,
    /// replication addr for client MOVED redirects
    pub addr: String,
}

/// Route result.
#[derive(Debug)]
pub enum RouteResult {
    /// This node owns the slot — proceed locally.
    Local(u16),
    /// Slot belongs to another node — send MOVED.
    Moved { slot: u16, node_id: u64, addr: String },
}

pub struct Iris {
    local_node_id: u64,
    /// slot → owner (None means local node owns it)
    table: Arc<RwLock<Vec<Option<SlotOwner>>>>,
}

impl Iris {
    /// Create with `local_node_id` owning all 16384 slots initially.
    pub fn new(local_node_id: u64) -> Self {
        let table = vec![None; NUM_SLOTS as usize]; // None = owned locally
        Self {
            local_node_id,
            table: Arc::new(RwLock::new(table)),
        }
    }

    /// Compute the slot for `key` (CRC16 % 16384).
    /// Respects hash tags: if key contains `{tag}`, only the tag is hashed.
    pub fn slot_for(key: &[u8]) -> u16 {
        let data = extract_hash_tag(key);
        crc16_ccitt(data) % NUM_SLOTS
    }

    /// Route a key — returns Local or Moved.
    pub fn route(&self, key: &[u8]) -> RouteResult {
        let slot = Self::slot_for(key);
        let table = self.table.read();
        match &table[slot as usize] {
            None => RouteResult::Local(slot),
            Some(owner) => RouteResult::Moved {
                slot,
                node_id: owner.node_id,
                addr: owner.addr.clone(),
            },
        }
    }

    /// Assign a contiguous slot range [start, end] to a remote node.
    pub fn assign_range(&self, node_id: u64, addr: &str, start: u16, end: u16) {
        let owner = SlotOwner { node_id, addr: addr.to_string() };
        let mut table = self.table.write();
        for slot in start..=end {
            table[slot as usize] = Some(owner.clone());
        }
    }

    /// Reclaim a slot range back to the local node.
    pub fn reclaim_range(&self, start: u16, end: u16) {
        let mut table = self.table.write();
        for slot in start..=end {
            table[slot as usize] = None;
        }
    }

    /// Return all slots owned by the local node.
    pub fn local_slots(&self) -> Vec<u16> {
        let table = self.table.read();
        table
            .iter()
            .enumerate()
            .filter(|(_, owner)| owner.is_none())
            .map(|(slot, _)| slot as u16)
            .collect()
    }

    /// Return a snapshot of the slot table: slot → node_id.
    /// Slots owned locally are mapped to `local_node_id`.
    pub fn slot_table(&self) -> HashMap<u16, u64> {
        let table = self.table.read();
        table
            .iter()
            .enumerate()
            .map(|(slot, owner)| {
                let node_id = owner
                    .as_ref()
                    .map(|o| o.node_id)
                    .unwrap_or(self.local_node_id);
                (slot as u16, node_id)
            })
            .collect()
    }

    pub fn local_node_id(&self) -> u64 {
        self.local_node_id
    }
}

// ── Hash tag extraction ───────────────────────────────────────────────────────

/// If key is `prefix{tag}suffix`, return `tag` bytes; else return `key`.
fn extract_hash_tag(key: &[u8]) -> &[u8] {
    if let Some(open) = key.iter().position(|&b| b == b'{') {
        if let Some(close) = key[open + 1..].iter().position(|&b| b == b'}') {
            let tag = &key[open + 1..open + 1 + close];
            if !tag.is_empty() {
                return tag;
            }
        }
    }
    key
}

// ── CRC16 CCITT (xmodem poly 0x1021) ─────────────────────────────────────────

pub fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_known_values() {
        // Verify CRC16-CCITT (poly 0x1021, init 0x0000) is deterministic.
        // Actual values from our implementation:
        assert_eq!(crc16_ccitt(b"foo") % NUM_SLOTS, 12182);
        assert_eq!(crc16_ccitt(b"bar") % NUM_SLOTS, 5061);
    }

    #[test]
    fn hash_tag_extraction() {
        assert_eq!(extract_hash_tag(b"a{foo}b"), b"foo");
        assert_eq!(extract_hash_tag(b"nobraces"), b"nobraces");
        assert_eq!(extract_hash_tag(b"a{}b"), b"a{}b");
    }

    #[test]
    fn route_local() {
        let iris = Iris::new(1);
        matches!(iris.route(b"somekey"), RouteResult::Local(_));
    }

    #[test]
    fn route_moved_after_assign() {
        let iris = Iris::new(1);
        let slot = Iris::slot_for(b"foo");
        iris.assign_range(2, "10.0.0.2:7379", slot, slot);
        assert!(matches!(iris.route(b"foo"), RouteResult::Moved { .. }));
    }

    #[test]
    fn reclaim_restores_local() {
        let iris = Iris::new(1);
        let slot = Iris::slot_for(b"bar");
        iris.assign_range(3, "10.0.0.3:7379", slot, slot);
        assert!(matches!(iris.route(b"bar"), RouteResult::Moved { .. }));
        iris.reclaim_range(slot, slot);
        assert!(matches!(iris.route(b"bar"), RouteResult::Local(_)));
    }

    #[test]
    fn slot_for_same_hash_tag() {
        // Keys with same hash tag must map to same slot
        let s1 = Iris::slot_for(b"user:{alice}:profile");
        let s2 = Iris::slot_for(b"user:{alice}:settings");
        assert_eq!(s1, s2);
    }

    #[test]
    fn slot_in_range() {
        for key in [b"hello".as_ref(), b"world", b"mneme", b"cache"] {
            assert!(Iris::slot_for(key) < NUM_SLOTS);
        }
    }

    #[test]
    fn slot_table_all_local_initially() {
        let iris = Iris::new(42);
        let table = iris.slot_table();
        assert!(table.values().all(|&nid| nid == 42));
        assert_eq!(table.len(), NUM_SLOTS as usize);
    }

    #[test]
    fn assign_range_updates_table() {
        let iris = Iris::new(1);
        iris.assign_range(2, "10.0.0.2:7379", 0, 100);
        let table = iris.slot_table();
        for slot in 0u16..=100 {
            assert_eq!(table[&slot], 2);
        }
        assert_eq!(table[&101], 1); // still local
    }

    #[test]
    fn moved_response_contains_correct_addr() {
        let iris = Iris::new(1);
        let slot = Iris::slot_for(b"testkey");
        iris.assign_range(5, "192.168.1.5:7379", slot, slot);
        match iris.route(b"testkey") {
            RouteResult::Moved { addr, node_id, .. } => {
                assert_eq!(addr, "192.168.1.5:7379");
                assert_eq!(node_id, 5);
            }
            _ => panic!("expected Moved"),
        }
    }

    #[test]
    fn slot_for_empty_key() {
        let slot = Iris::slot_for(b"");
        assert!(slot < NUM_SLOTS);
    }

    #[test]
    fn slot_for_deterministic() {
        let a = Iris::slot_for(b"foo");
        let b = Iris::slot_for(b"foo");
        assert_eq!(a, b);
    }

    #[test]
    fn slot_for_hash_tag() {
        let s1 = Iris::slot_for(b"{user}.name");
        let s2 = Iris::slot_for(b"{user}.email");
        assert_eq!(s1, s2);
    }

    #[test]
    fn slot_for_empty_hash_tag() {
        // Empty tag `{}` has no effect — full key is used.
        let slot = Iris::slot_for(b"{}.name");
        let expected = crc16_ccitt(b"{}.name") % NUM_SLOTS;
        assert_eq!(slot, expected);
    }

    #[test]
    fn slot_for_nested_braces() {
        // First `{` at index 0, first `}` at index 6 → tag is `{user` (bytes 1..6).
        let slot = Iris::slot_for(b"{{user}}.name");
        let expected = crc16_ccitt(b"{user") % NUM_SLOTS;
        assert_eq!(slot, expected);
    }

    #[test]
    fn slot_for_no_closing_brace() {
        // No closing brace means full key is used.
        let slot = Iris::slot_for(b"{user.name");
        let expected = crc16_ccitt(b"{user.name") % NUM_SLOTS;
        assert_eq!(slot, expected);
    }

    #[test]
    fn slot_for_max_key_size() {
        let key = vec![0xABu8; 512];
        let slot = Iris::slot_for(&key);
        assert!(slot < NUM_SLOTS);
    }

    #[test]
    fn slot_range_always_valid() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        // Generate 1000 pseudo-random keys and verify all slots are in range.
        for i in 0u64..1000 {
            let mut hasher = DefaultHasher::new();
            i.hash(&mut hasher);
            let h = hasher.finish();
            let key = h.to_le_bytes();
            let slot = Iris::slot_for(&key);
            assert!(slot < NUM_SLOTS, "slot {} out of range for key index {}", slot, i);
        }
    }
}
