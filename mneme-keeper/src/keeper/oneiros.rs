// Oneiros — Cold store backed by redb B-tree with zstd compression.
// Stores all 4 value types. Keys are raw bytes; values are zstd(msgpack).

use std::path::Path;

use anyhow::{Context, Result};
use mneme_common::Value;
use redb::{Database, ReadableTable, TableDefinition};
use tracing::debug;

/// Primary KV table: key bytes → compressed(msgpack(Value + expires_at_ms + slot))
const KV_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");

/// Internal stored record (serialized alongside the value).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredRecord {
    value: Value,
    expires_at_ms: u64,
    slot: u16,
}

pub struct Oneiros {
    db: Database,
}

impl Oneiros {
    /// Open or create the cold store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let db = Database::create(path)
            .with_context(|| format!("Oneiros open: {}", path.display()))?;

        // Ensure the table exists
        let write_txn = db.begin_write()?;
        write_txn.open_table(KV_TABLE)?;
        write_txn.commit()?;

        Ok(Self { db })
    }

    /// Get a value by key. Returns None if not found or expired.
    #[allow(dead_code)]
    pub fn get(&self, key: &[u8]) -> Result<Option<Value>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(KV_TABLE)?;

        match table.get(key)? {
            None => Ok(None),
            Some(raw) => {
                let record = Self::decompress_and_decode(raw.value())?;
                if record.expires_at_ms != 0 && record.expires_at_ms <= now_ms() {
                    debug!("Oneiros: key expired, returning None");
                    return Ok(None);
                }
                Ok(Some(record.value))
            }
        }
    }

    /// Set a key/value pair with optional TTL (0 = no expiry).
    pub fn set(
        &self,
        key: &[u8],
        value: &Value,
        expires_at_ms: u64,
        slot: u16,
    ) -> Result<()> {
        let record = StoredRecord {
            value: value.clone(),
            expires_at_ms,
            slot,
        };
        let encoded = Self::encode_and_compress(&record)?;

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(KV_TABLE)?;
            table.insert(key, encoded.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Delete a key. Returns true if it existed.
    #[allow(dead_code)]
    pub fn del(&self, key: &[u8]) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let existed = {
            let mut table = write_txn.open_table(KV_TABLE)?;
            // Bind result before table drops to avoid AccessGuard lifetime error
            let found = table.remove(key)?.is_some();
            found
        };
        write_txn.commit()?;
        Ok(existed)
    }

    /// Check if a key exists and is not expired.
    #[allow(dead_code)]
    pub fn exists(&self, key: &[u8]) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// Return all key bytes in the table (no expiry filter — for snapshot purposes).
    pub fn list_keys(&self) -> Result<Vec<Vec<u8>>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(KV_TABLE)?;
        let mut keys = Vec::new();
        for entry in table.iter()? {
            let (k, _) = entry?;
            keys.push(k.value().to_vec());
        }
        Ok(keys)
    }

    /// Get value + metadata for one key. Returns None if the key does not exist.
    /// Does NOT filter expired keys — the caller decides whether to skip them.
    pub fn get_with_meta(&self, key: &[u8]) -> Result<Option<(Value, u64, u16)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(KV_TABLE)?;
        match table.get(key)? {
            None => Ok(None),
            Some(raw) => {
                let record = Self::decompress_and_decode(raw.value())?;
                Ok(Some((record.value, record.expires_at_ms, record.slot)))
            }
        }
    }

    /// Bulk insert in a single transaction — much faster than one `set()` per key.
    pub fn set_batch(&self, entries: &[(Vec<u8>, Value, u64, u16)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(KV_TABLE)?;
            for (key, value, expires_at_ms, slot) in entries {
                let record = StoredRecord {
                    value: value.clone(),
                    expires_at_ms: *expires_at_ms,
                    slot: *slot,
                };
                let encoded = Self::encode_and_compress(&record)?;
                table.insert(key.as_slice(), encoded.as_slice())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Scan all keys. Calls `cb(key, value, expires_at_ms, slot)` for each non-expired entry.
    pub fn scan(
        &self,
        mut cb: impl FnMut(Vec<u8>, Value, u64, u16) -> Result<()>,
    ) -> Result<u64> {
        let now = now_ms();
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(KV_TABLE)?;

        let mut count: u64 = 0;
        for entry in table.iter()? {
            let (k, v) = entry?;
            let record = Self::decompress_and_decode(v.value())?;
            if record.expires_at_ms != 0 && record.expires_at_ms <= now {
                continue;
            }
            cb(k.value().to_vec(), record.value, record.expires_at_ms, record.slot)?;
            count += 1;
        }
        Ok(count)
    }

    /// Remove all expired keys. Returns number of keys purged.
    #[allow(dead_code)]
    pub fn purge_expired(&self) -> Result<u64> {
        let now = now_ms();
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(KV_TABLE)?;

        let expired: Vec<Vec<u8>> = table
            .iter()?
            .filter_map(|e| {
                let (k, v) = e.ok()?;
                let record = Self::decompress_and_decode(v.value()).ok()?;
                if record.expires_at_ms != 0 && record.expires_at_ms <= now {
                    Some(k.value().to_vec())
                } else {
                    None
                }
            })
            .collect();
        drop(table);
        drop(read_txn);

        let count = expired.len() as u64;
        if count > 0 {
            let write_txn = self.db.begin_write()?;
            {
                let mut table = write_txn.open_table(KV_TABLE)?;
                for key in &expired {
                    table.remove(key.as_slice())?;
                }
            }
            write_txn.commit()?;
        }
        Ok(count)
    }

    // ── encoding helpers ────────────────────────────────────────────────────

    fn encode_and_compress(record: &StoredRecord) -> Result<Vec<u8>> {
        let msgpack = rmp_serde::to_vec(record)
            .with_context(|| "Oneiros encode")?;
        zstd::encode_all(msgpack.as_slice(), 3)
            .with_context(|| "Oneiros compress")
    }

    fn decompress_and_decode(data: &[u8]) -> Result<StoredRecord> {
        let decompressed = zstd::decode_all(data)
            .with_context(|| "Oneiros decompress")?;
        rmp_serde::from_slice(&decompressed)
            .with_context(|| "Oneiros decode")
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use mneme_common::Value;

    fn temp_db() -> (Oneiros, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "mneme_oneiros_test_{}.redb",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let db = Oneiros::open(&path).unwrap();
        (db, path)
    }

    #[test]
    fn set_and_get_string() {
        let (db, path) = temp_db();
        let value = Value::String(b"hello world".to_vec());
        db.set(b"k1", &value, 0, 0).unwrap();
        let got = db.get(b"k1").unwrap();
        assert!(matches!(got, Some(Value::String(_))));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn get_missing_key_returns_none() {
        let (db, path) = temp_db();
        assert!(db.get(b"nonexistent").unwrap().is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn del_removes_key() {
        let (db, path) = temp_db();
        db.set(b"del_key", &Value::String(b"v".to_vec()), 0, 0).unwrap();
        assert!(db.del(b"del_key").unwrap());
        assert!(db.get(b"del_key").unwrap().is_none());
        assert!(!db.del(b"del_key").unwrap()); // second del returns false
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn expired_key_not_returned() {
        let (db, path) = temp_db();
        // expires_at_ms = 1 (epoch + 1ms) → always in the past
        db.set(b"exp", &Value::String(b"v".to_vec()), 1, 0).unwrap();
        assert!(db.get(b"exp").unwrap().is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn scan_returns_non_expired() {
        let (db, path) = temp_db();
        db.set(b"live", &Value::String(b"a".to_vec()), 0, 0).unwrap();
        db.set(b"dead", &Value::String(b"b".to_vec()), 1, 0).unwrap(); // expired

        let mut found = Vec::new();
        db.scan(|k, _, _, _| {
            found.push(k);
            Ok(())
        }).unwrap();

        assert!(found.contains(&b"live".to_vec()));
        assert!(!found.contains(&b"dead".to_vec()));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn purge_expired_removes_stale_keys() {
        let (db, path) = temp_db();
        db.set(b"stale1", &Value::String(b"a".to_vec()), 1, 0).unwrap();
        db.set(b"stale2", &Value::String(b"b".to_vec()), 1, 0).unwrap();
        db.set(b"fresh", &Value::String(b"c".to_vec()), u64::MAX, 0).unwrap();

        let purged = db.purge_expired().unwrap();
        assert_eq!(purged, 2);
        assert!(db.exists(b"fresh").unwrap());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn all_value_types_round_trip() {
        let (db, path) = temp_db();

        let hash = Value::Hash(vec![(b"f".to_vec(), b"v".to_vec())]);
        let list = Value::List(std::collections::VecDeque::from(vec![b"i".to_vec()]));
        let zset = Value::ZSet(vec![mneme_common::ZSetMember::new(1.0, b"m")]);

        db.set(b"hash_k", &hash, 0, 1).unwrap();
        db.set(b"list_k", &list, 0, 2).unwrap();
        db.set(b"zset_k", &zset, 0, 3).unwrap();

        assert!(matches!(db.get(b"hash_k").unwrap(), Some(Value::Hash(_))));
        assert!(matches!(db.get(b"list_k").unwrap(), Some(Value::List(_))));
        assert!(matches!(db.get(b"zset_k").unwrap(), Some(Value::ZSet(_))));

        std::fs::remove_file(&path).ok();
    }

    /// Unique temp DB helper to avoid lock collisions with parallel tests.
    fn temp_db_unique(label: &str) -> (Oneiros, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mneme_oneiros_{}_{}_{}.redb",
            label,
            std::process::id(),
            id,
        ));
        let db = Oneiros::open(&path).unwrap();
        (db, path)
    }

    #[test]
    fn set_get_round_trip() {
        let (db, path) = temp_db_unique("set_get_rt");
        let value = Value::String(b"round-trip-value".to_vec());
        db.set(b"rt_key", &value, 0, 42).unwrap();

        let got = db.get(b"rt_key").unwrap().expect("key should exist");
        match got {
            Value::String(v) => assert_eq!(v, b"round-trip-value"),
            _ => panic!("expected Value::String"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn get_nonexistent_key() {
        let (db, path) = temp_db_unique("get_nonexist");
        let result = db.get(b"does_not_exist").unwrap();
        assert!(result.is_none(), "nonexistent key should return None");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn delete_key() {
        let (db, path) = temp_db_unique("delete_key");
        db.set(b"to_delete", &Value::String(b"bye".to_vec()), 0, 0)
            .unwrap();

        // Key exists before delete
        assert!(db.get(b"to_delete").unwrap().is_some());

        // Delete returns true (existed)
        assert!(db.del(b"to_delete").unwrap());

        // Key gone after delete
        assert!(db.get(b"to_delete").unwrap().is_none());

        // Second delete returns false (already gone)
        assert!(!db.del(b"to_delete").unwrap());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn overwrite_existing_key() {
        let (db, path) = temp_db_unique("overwrite");
        let v1 = Value::String(b"original".to_vec());
        let v2 = Value::String(b"updated".to_vec());

        db.set(b"ow_key", &v1, 0, 0).unwrap();
        db.set(b"ow_key", &v2, 0, 0).unwrap();

        let got = db.get(b"ow_key").unwrap().expect("key should exist");
        match got {
            Value::String(v) => assert_eq!(v, b"updated"),
            _ => panic!("expected Value::String"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn set_batch_all_or_nothing() {
        let (db, path) = temp_db_unique("set_batch");
        let entries = vec![
            (b"batch_a".to_vec(), Value::String(b"va".to_vec()), 0u64, 0u16),
            (b"batch_b".to_vec(), Value::String(b"vb".to_vec()), 0u64, 1u16),
            (b"batch_c".to_vec(), Value::String(b"vc".to_vec()), 0u64, 2u16),
        ];

        db.set_batch(&entries).unwrap();

        for (key, expected_val, _, _) in &entries {
            let got = db.get(key).unwrap().expect("batch key should exist");
            match (&got, expected_val) {
                (Value::String(actual), Value::String(expected)) => {
                    assert_eq!(actual, expected);
                }
                _ => panic!("unexpected value type"),
            }
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn list_keys_returns_all() {
        let (db, path) = temp_db_unique("list_keys");
        let keys: Vec<&[u8]> = vec![b"lk_alpha", b"lk_beta", b"lk_gamma"];

        for key in &keys {
            db.set(key, &Value::String(b"v".to_vec()), 0, 0).unwrap();
        }

        let listed = db.list_keys().unwrap();
        for key in &keys {
            assert!(
                listed.contains(&key.to_vec()),
                "list_keys should contain {:?}",
                std::str::from_utf8(key).unwrap()
            );
        }
        assert_eq!(listed.len(), keys.len());
        std::fs::remove_file(&path).ok();
    }
}
