// Melete — Snapshot engine
// Serialises all 4 value types with msgpack, writes to .tmp, atomic rename.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use mneme_common::{Entry, Value};
use serde::{Deserialize, Serialize};
use tracing::info;

/// One record inside the snapshot file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapEntry {
    pub key: Vec<u8>,
    pub value: Value,
    /// Absolute expiry ms since epoch (0 = no expiry).
    pub expires_at_ms: u64,
    pub lfu_counter: u8,
    pub slot: u16,
}

/// Snapshot header.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapHeader {
    version: u8,
    created_at_ms: u64,
    entry_count: u64,
}

pub struct Melete;

impl Melete {
    /// Serialise `entries` to a snapshot at `path`.
    /// Writes to `path.tmp` first, then atomically renames to `path`.
    pub fn save(
        entries: impl IntoIterator<Item = (Vec<u8>, Entry)>,
        path: impl AsRef<Path>,
    ) -> Result<u64> {
        let path = path.as_ref();
        let tmp_path = path.with_extension("snap.tmp");

        let entries: Vec<SnapEntry> = entries
            .into_iter()
            .map(|(key, e)| SnapEntry {
                key,
                value: e.value,
                expires_at_ms: e.expires_at_ms,
                lfu_counter: e.lfu_counter,
                slot: e.slot,
            })
            .collect();

        let count = entries.len() as u64;

        let header = SnapHeader {
            version: 1,
            created_at_ms: now_ms(),
            entry_count: count,
        };

        // Write header + entries as a msgpack sequence into a temp file.
        let file = std::fs::File::create(&tmp_path)
            .with_context(|| format!("snap create tmp: {}", tmp_path.display()))?;
        let mut writer = std::io::BufWriter::new(file);

        rmp_serde::encode::write(&mut writer, &header)
            .with_context(|| "snap write header")?;

        for entry in &entries {
            rmp_serde::encode::write(&mut writer, entry)
                .with_context(|| "snap write entry")?;
        }

        // Ensure data is on disk before rename.
        use std::io::Write;
        writer.flush()?;
        let file = writer.into_inner().map_err(|e| anyhow::anyhow!("{e}"))?;
        file.sync_data()?;
        drop(file);

        // Atomic rename
        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("snap rename to {}", path.display()))?;

        info!(count, path = %path.display(), "Snapshot saved");
        Ok(count)
    }

    /// Load a snapshot from `path`. Returns (entries, created_at_ms).
    pub fn load(path: impl AsRef<Path>) -> Result<(Vec<SnapEntry>, u64)> {
        let path = path.as_ref();
        let file = std::fs::File::open(path)
            .with_context(|| format!("snap open: {}", path.display()))?;
        let mut reader = std::io::BufReader::new(file);

        let header: SnapHeader = rmp_serde::decode::from_read(&mut reader)
            .with_context(|| "snap read header")?;

        if header.version != 1 {
            anyhow::bail!(
                "unsupported snapshot version {} (expected 1) in {}",
                header.version,
                path.display()
            );
        }

        let mut entries = Vec::with_capacity(header.entry_count as usize);
        for i in 0..header.entry_count {
            let entry: SnapEntry = rmp_serde::decode::from_read(&mut reader)
                .with_context(|| format!("snap read entry {i}"))?;
            entries.push(entry);
        }

        info!(
            count = entries.len(),
            path = %path.display(),
            "Snapshot loaded"
        );
        Ok((entries, header.created_at_ms))
    }

    /// Check if a snapshot file exists and is readable.
    pub fn exists(path: impl AsRef<Path>) -> bool {
        path.as_ref().exists()
    }

    /// Convert loaded snapshot entries into a HashMap suitable for re-hydration.
    #[allow(dead_code)]
    pub fn into_map(entries: Vec<SnapEntry>) -> HashMap<Vec<u8>, Entry> {
        entries
            .into_iter()
            .map(|se| {
                let mut entry = Entry::new(se.value, se.slot);
                entry.expires_at_ms = se.expires_at_ms;
                entry.lfu_counter = se.lfu_counter;
                (se.key, entry)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mneme_common::Value;

    fn make_entries() -> Vec<(Vec<u8>, mneme_common::Entry)> {
        vec![
            (
                b"key:string".to_vec(),
                mneme_common::Entry::new(Value::String(b"hello".to_vec()), 0),
            ),
            (
                b"key:hash".to_vec(),
                mneme_common::Entry::new(
                    Value::Hash(vec![(b"field".to_vec(), b"val".to_vec())]),
                    1,
                ),
            ),
            (
                b"key:list".to_vec(),
                mneme_common::Entry::new(
                    Value::List(std::collections::VecDeque::from(vec![b"item1".to_vec()])),
                    2,
                ),
            ),
        ]
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir();
        let path = dir.join("mneme_test_snap.snap");

        let entries = make_entries();
        let count = Melete::save(entries.clone(), &path).unwrap();
        assert_eq!(count, 3);

        let (loaded, _ts) = Melete::load(&path).unwrap();
        assert_eq!(loaded.len(), 3);

        let keys: Vec<Vec<u8>> = loaded.iter().map(|e| e.key.clone()).collect();
        assert!(keys.contains(&b"key:string".to_vec()));
        assert!(keys.contains(&b"key:hash".to_vec()));
        assert!(keys.contains(&b"key:list".to_vec()));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_nonexistent_fails() {
        let result = Melete::load("/tmp/mneme_nonexistent_9999.snap");
        assert!(result.is_err());
    }

    #[test]
    fn exists_check() {
        let dir = std::env::temp_dir();
        let path = dir.join("mneme_exists_test.snap");
        assert!(!Melete::exists(&path));
        Melete::save(vec![], &path).unwrap();
        assert!(Melete::exists(&path));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn into_map_preserves_fields() {
        let dir = std::env::temp_dir();
        let path = dir.join("mneme_map_test.snap");
        let entries = make_entries();
        Melete::save(entries, &path).unwrap();
        let (loaded, _) = Melete::load(&path).unwrap();
        let map = Melete::into_map(loaded);
        assert!(map.contains_key(b"key:string".as_ref()));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_wrong_version_fails() {
        let dir = std::env::temp_dir().join("mneme_test_wrong_ver");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wrong_version.snap");

        // Write a header with version=99 using msgpack
        let bad_header = SnapHeader {
            version: 99,
            created_at_ms: 0,
            entry_count: 0,
        };
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        rmp_serde::encode::write(&mut writer, &bad_header).unwrap();
        use std::io::Write;
        writer.flush().unwrap();
        drop(writer);

        let result = Melete::load(&path);
        assert!(result.is_err());
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("unsupported snapshot version"),
            "expected 'unsupported snapshot version' in error, got: {err_msg}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_empty_entries() {
        let dir = std::env::temp_dir().join("mneme_test_empty");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.snap");

        let count = Melete::save(Vec::<(Vec<u8>, mneme_common::Entry)>::new(), &path).unwrap();
        assert_eq!(count, 0);

        let (loaded, _ts) = Melete::load(&path).unwrap();
        assert!(loaded.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_load_round_trip_extended() {
        let dir = std::env::temp_dir().join("mneme_test_rt_ext");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("round_trip.snap");

        let mut entries = make_entries();
        // Add a ZSet entry for broader coverage
        entries.push((
            b"key:zset".to_vec(),
            mneme_common::Entry::new(
                Value::ZSet(vec![mneme_common::ZSetMember::new(1.5, b"member1".to_vec())]),
                3,
            ),
        ));

        let count = Melete::save(entries.clone(), &path).unwrap();
        assert_eq!(count, 4);

        let (loaded, _ts) = Melete::load(&path).unwrap();
        assert_eq!(loaded.len(), 4);

        // Verify each key is present and values match
        for (orig_key, orig_entry) in &entries {
            let snap = loaded.iter().find(|se| se.key == *orig_key)
                .unwrap_or_else(|| panic!("missing key {:?}", orig_key));
            assert_eq!(snap.slot, orig_entry.slot);
            assert_eq!(snap.expires_at_ms, orig_entry.expires_at_ms);
            // Compare serialized values for equality
            let orig_bytes = rmp_serde::to_vec(&orig_entry.value).unwrap();
            let loaded_bytes = rmp_serde::to_vec(&snap.value).unwrap();
            assert_eq!(orig_bytes, loaded_bytes, "value mismatch for key {:?}", orig_key);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snap_preserves_ttl() {
        let dir = std::env::temp_dir().join("mneme_test_ttl");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ttl.snap");

        let expires = 1_700_000_000_000_u64; // some future timestamp
        let mut entry = mneme_common::Entry::new(Value::String(b"val".to_vec()), 5);
        entry.expires_at_ms = expires;

        Melete::save(vec![(b"ttl_key".to_vec(), entry)], &path).unwrap();

        let (loaded, _) = Melete::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].expires_at_ms, expires);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snap_preserves_lfu_counter() {
        let dir = std::env::temp_dir().join("mneme_test_lfu");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lfu.snap");

        let mut entry = mneme_common::Entry::new(Value::String(b"val".to_vec()), 7);
        entry.lfu_counter = 42;

        Melete::save(vec![(b"lfu_key".to_vec(), entry)], &path).unwrap();

        let (loaded, _) = Melete::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].lfu_counter, 42);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_corrupted_entry_fails() {
        let dir = std::env::temp_dir().join("mneme_test_corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corrupted.snap");

        // Write a valid header claiming 2 entries, but write garbage instead
        let header = SnapHeader {
            version: 1,
            created_at_ms: 0,
            entry_count: 2,
        };
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        rmp_serde::encode::write(&mut writer, &header).unwrap();
        // Write random garbage bytes that cannot be a valid SnapEntry
        use std::io::Write;
        writer.write_all(&[0xFF, 0xFE, 0xAB, 0x00, 0x01, 0x02, 0x03]).unwrap();
        writer.flush().unwrap();
        drop(writer);

        let result = Melete::load(&path);
        assert!(result.is_err(), "loading corrupted snapshot should fail");

        std::fs::remove_dir_all(&dir).ok();
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
