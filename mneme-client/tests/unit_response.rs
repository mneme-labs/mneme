// mneme-client/tests/unit_response.rs — Unit tests for response structs and
// serialisation round-trips. No network required.

use mneme_client::{KeeperEntry, PoolStats, ScanPage, SlowLogEntry, UserInfo};

// ── KeeperEntry ───────────────────────────────────────────────────────────────

#[test]
fn keeper_entry_debug() {
    let e = KeeperEntry {
        node_id:    42,
        name:       "hypnos-1".into(),
        addr:       "10.0.0.2:7379".into(),
        pool_bytes: 4 * 1024 * 1024 * 1024,
        used_bytes: 256 * 1024 * 1024,
    };
    let s = format!("{e:?}");
    assert!(s.contains("hypnos-1"));
    assert!(s.contains("10.0.0.2"));
}

#[test]
fn keeper_entry_clone() {
    let e = KeeperEntry {
        node_id: 1, name: "k".into(), addr: "a:1".into(),
        pool_bytes: 100, used_bytes: 10,
    };
    let c = e.clone();
    assert_eq!(c.node_id, e.node_id);
    assert_eq!(c.addr, e.addr);
}

// ── PoolStats ─────────────────────────────────────────────────────────────────

#[test]
fn pool_stats_pressure_ratio() {
    let s = PoolStats { used_bytes: 512 * 1024 * 1024, total_bytes: 1024 * 1024 * 1024, keeper_count: 2 };
    let ratio = s.used_bytes as f64 / s.total_bytes as f64;
    assert!((ratio - 0.5).abs() < 1e-9, "ratio should be 0.5, got {ratio}");
}

#[test]
fn pool_stats_zero_used() {
    let s = PoolStats { used_bytes: 0, total_bytes: 1_000_000, keeper_count: 0 };
    assert_eq!(s.used_bytes, 0);
    assert_eq!(s.keeper_count, 0);
}

// ── SlowLogEntry ──────────────────────────────────────────────────────────────

#[test]
fn slowlog_entry_fields() {
    let e = SlowLogEntry {
        command:     "SET".into(),
        key:         b"user:1".to_vec(),
        duration_us: 1_500,
    };
    assert_eq!(e.command, "SET");
    assert_eq!(e.key, b"user:1");
    assert_eq!(e.duration_us, 1_500);
}

#[test]
fn slowlog_entry_zero_key() {
    let e = SlowLogEntry { command: "STATS".into(), key: vec![], duration_us: 50 };
    assert!(e.key.is_empty());
}

// ── UserInfo ──────────────────────────────────────────────────────────────────

#[test]
fn user_info_all_dbs_allowed_when_empty() {
    let u = UserInfo {
        username:    "alice".into(),
        role:        "readwrite".into(),
        allowed_dbs: vec![],  // empty = all databases
    };
    assert!(u.allowed_dbs.is_empty(), "empty allowed_dbs means all databases");
}

#[test]
fn user_info_restricted_dbs() {
    let u = UserInfo {
        username:    "bob".into(),
        role:        "readonly".into(),
        allowed_dbs: vec![0, 1, 5],
    };
    assert_eq!(u.allowed_dbs.len(), 3);
    assert!(u.allowed_dbs.contains(&5));
}

#[test]
fn user_info_roles() {
    for role in &["admin", "readwrite", "readonly"] {
        let u = UserInfo { username: "x".into(), role: (*role).into(), allowed_dbs: vec![] };
        assert_eq!(&u.role, role);
    }
}

// ── ScanPage ──────────────────────────────────────────────────────────────────

#[test]
fn scan_page_done_when_cursor_zero() {
    let page = ScanPage { next_cursor: 0, keys: vec![b"k1".to_vec(), b"k2".to_vec()] };
    assert_eq!(page.next_cursor, 0, "cursor=0 means scan complete");
    assert_eq!(page.keys.len(), 2);
}

#[test]
fn scan_page_has_more_when_cursor_nonzero() {
    let page = ScanPage { next_cursor: 42, keys: vec![b"x".to_vec()] };
    assert!(page.next_cursor != 0, "nonzero cursor means more pages");
}

#[test]
fn scan_page_empty_keys() {
    let page = ScanPage { next_cursor: 0, keys: vec![] };
    assert!(page.keys.is_empty());
}
