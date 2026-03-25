// Aoide — Write-Ahead Log
//
// Platform behaviour:
//   Linux   — O_DIRECT (kernel-bypass buffering) + fallocate (pre-allocation) +
//             pwrite (positioned write) + fdatasync (data-only flush).
//             O_DIRECT requires 4 KB-aligned buffers and sector-aligned offsets.
//   macOS   — F_NOCACHE via fcntl (equivalent DIO hint) + ftruncate (pre-alloc) +
//             pwrite (POSIX) + F_FULLFSYNC for true persistence.
//   Windows — Standard file I/O: seek + write + sync_data (FlushFileBuffers).
//             No O_DIRECT equivalent — relies on write-through or FILE_FLAG_NO_BUFFERING
//             at a future optimisation pass.
//
// Format v2: [8B seq][8B expires_at_ms][4B key_len][4B val_len][4B crc32][key][value]
//   All fields little-endian. expires_at_ms = 0 means no expiry (permanent key).
//   Every record is padded to BLOCK_SIZE (4096) for DIO alignment on Linux/macOS.
//
// NOTE: v1 format was [8B seq][4B key_len][4B val_len][4B crc32][key][value].
//   v2 adds 8 bytes for expires_at_ms after seq.  Old WAL files will fail CRC
//   and stop replay cleanly (the CRC check catches the misaligned parse).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use bytes::BufMut;
use crc::{Crc, CRC_32_ISCSI};
use mneme_common::Value;
use tracing::{debug, info};

const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

/// Entry record layout v2 (all little-endian):
/// [8B seq][8B expires_at_ms][4B key_len][4B val_len][4B crc32][key bytes][value bytes]
const ENTRY_HEADER_SIZE: usize = 28;
const BLOCK_SIZE: usize = 4096; // O_DIRECT / F_NOCACHE alignment

pub struct Aoide {
    path:      PathBuf,
    file:      File,
    offset:    u64,
    max_bytes: u64,
}

impl Aoide {
    /// Open or create the WAL at `path`. Pre-allocates `max_bytes` on disk
    /// where the platform supports it (fallocate / ftruncate).
    pub fn open(path: impl AsRef<Path>, max_bytes: u64) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = platform::open_wal_file(&path)
            .with_context(|| format!("WAL open: {}", path.display()))?;

        let current_size = file.metadata()?.len();
        if current_size < max_bytes {
            platform::preallocate(&file, max_bytes);
        }

        let offset = Self::find_end_offset(&path)?;
        info!(path = %path.display(), offset, max_bytes, "Aoide WAL opened");
        Ok(Self { path, file, offset, max_bytes })
    }

    /// Append a key/value pair with sequence number and absolute expiry timestamp.
    /// `expires_at_ms` = 0 means no expiry (permanent key).
    /// Returns the file offset of the record.
    pub fn append(&mut self, seq: u64, expires_at_ms: u64, key: &[u8], value: &Value) -> Result<u64> {
        let val_bytes = rmp_serde::to_vec(value)
            .with_context(|| "WAL serialize value")?;

        let record_size  = ENTRY_HEADER_SIZE + key.len() + val_bytes.len();
        let aligned_size = align_up(record_size, BLOCK_SIZE);

        if self.offset + aligned_size as u64 > self.max_bytes {
            bail!("WAL full: offset={} max={}", self.offset, self.max_bytes);
        }

        // Build a page-aligned, zero-padded buffer.
        // O_DIRECT on Linux requires the buffer pointer to be sector/page-aligned.
        let record = aligned_buf(aligned_size);
        {
            let mut w = &mut record.as_slice_mut()[..];
            w.put_u64_le(seq);
            w.put_u64_le(expires_at_ms);
            w.put_u32_le(key.len() as u32);
            w.put_u32_le(val_bytes.len() as u32);

            let mut digest = CRC32.digest();
            digest.update(key);
            digest.update(&val_bytes);
            w.put_u32_le(digest.finalize());
        }
        record.as_slice_mut()[ENTRY_HEADER_SIZE..ENTRY_HEADER_SIZE + key.len()]
            .copy_from_slice(key);
        record.as_slice_mut()[ENTRY_HEADER_SIZE + key.len()
            ..ENTRY_HEADER_SIZE + key.len() + val_bytes.len()]
            .copy_from_slice(&val_bytes);

        platform::pwrite_all(&mut self.file, record.as_slice(), self.offset)
            .with_context(|| format!("WAL pwrite at offset {}", self.offset))?;

        let record_offset = self.offset;
        self.offset += aligned_size as u64;
        debug!(seq, expires_at_ms, offset = record_offset, "WAL append");
        Ok(record_offset)
    }

    /// Flush and rotate — rename current file to `.wal.old`, open a fresh WAL.
    pub fn rotate(&mut self) -> Result<()> {
        self.flush()?;
        let old_path = self.path.with_extension("wal.old");
        std::fs::rename(&self.path, &old_path)
            .with_context(|| "WAL rotate rename")?;
        *self = Self::open(&self.path, self.max_bytes)?;
        info!(old = %old_path.display(), "WAL rotated");
        Ok(())
    }

    /// Flush pending data to durable storage.
    pub fn flush(&mut self) -> Result<()> {
        platform::fsync(&mut self.file)
            .with_context(|| "WAL fsync")?;
        Ok(())
    }

    /// Returns current write offset (bytes written).
    #[allow(dead_code)]
    pub fn offset(&self) -> u64 { self.offset }

    /// Returns true when the WAL is at least `threshold` fraction full.
    pub fn needs_rotation(&self, threshold: f64) -> bool {
        self.offset as f64 / self.max_bytes as f64 >= threshold
    }

    /// Replay all valid records from a WAL file.
    /// Calls `cb(seq, expires_at_ms, key, value)` for each valid record.
    pub fn replay(
        path: impl AsRef<Path>,
        mut cb: impl FnMut(u64, u64, Vec<u8>, Value) -> Result<()>,
    ) -> Result<u64> {
        let path = path.as_ref();
        let mut file = File::open(path)
            .with_context(|| format!("WAL replay open: {}", path.display()))?;

        let mut replayed: u64 = 0;
        let mut offset: usize = 0;

        loop {
            let aligned = align_up(offset, BLOCK_SIZE);
            file.seek(SeekFrom::Start(aligned as u64))?;

            let mut buf = vec![0u8; BLOCK_SIZE];
            let n = file.read(&mut buf)?;
            if n < ENTRY_HEADER_SIZE { break; }

            let seq            = u64::from_le_bytes(buf[0..8].try_into().unwrap());
            let expires_at_ms  = u64::from_le_bytes(buf[8..16].try_into().unwrap());
            let key_len        = u32::from_le_bytes(buf[16..20].try_into().unwrap()) as usize;
            let val_len        = u32::from_le_bytes(buf[20..24].try_into().unwrap()) as usize;
            let stored_crc     = u32::from_le_bytes(buf[24..28].try_into().unwrap());

            if seq == 0 && key_len == 0 { break; }

            // Guard against corrupted lengths that could cause OOM allocation
            const MAX_KEY_BYTES: usize = 512;
            const MAX_VAL_BYTES: usize = 10 * 1024 * 1024; // 10 MB
            if key_len > MAX_KEY_BYTES {
                tracing::warn!(offset, key_len, "WAL replay: key_len exceeds max — stopping");
                break;
            }
            if val_len > MAX_VAL_BYTES {
                tracing::warn!(offset, val_len, "WAL replay: val_len exceeds max — stopping");
                break;
            }

            let record_size = ENTRY_HEADER_SIZE + key_len + val_len;
            let aligned_rec = align_up(record_size, BLOCK_SIZE);

            let mut full = vec![0u8; aligned_rec];
            file.seek(SeekFrom::Start(aligned as u64))?;
            file.read_exact(&mut full)
                .with_context(|| format!("WAL replay read at offset {offset}"))?;

            let key       = full[ENTRY_HEADER_SIZE..ENTRY_HEADER_SIZE + key_len].to_vec();
            let val_bytes = &full[ENTRY_HEADER_SIZE + key_len
                ..ENTRY_HEADER_SIZE + key_len + val_len];

            let mut digest = CRC32.digest();
            digest.update(&key);
            digest.update(val_bytes);
            if digest.finalize() != stored_crc {
                tracing::warn!(offset, "WAL CRC mismatch — stopping replay");
                break;
            }

            let value: Value = rmp_serde::from_slice(val_bytes)
                .with_context(|| format!("WAL deserialize at offset {offset}"))?;
            cb(seq, expires_at_ms, key, value)?;
            replayed += 1;
            offset += aligned_rec;
        }

        info!(replayed, "WAL replay complete");
        Ok(replayed)
    }

    // ── private ──────────────────────────────────────────────────────────────

    fn find_end_offset(path: &Path) -> Result<u64> {
        let mut file = match File::open(path) {
            Ok(f)  => f,
            Err(_) => return Ok(0),
        };
        let file_len = file.metadata()?.len() as usize;
        let mut offset: usize = 0;

        loop {
            let aligned = align_up(offset, BLOCK_SIZE);
            if aligned + ENTRY_HEADER_SIZE > file_len { break; }
            let mut hdr = vec![0u8; BLOCK_SIZE];
            file.seek(SeekFrom::Start(aligned as u64))?;
            let n = file.read(&mut hdr)?;
            if n < ENTRY_HEADER_SIZE { break; }
            let seq     = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
            let key_len = u32::from_le_bytes(hdr[16..20].try_into().unwrap()) as usize;
            let val_len = u32::from_le_bytes(hdr[20..24].try_into().unwrap()) as usize;
            if seq == 0 && key_len == 0 { break; }
            let record_size = ENTRY_HEADER_SIZE + key_len + val_len;
            offset = aligned + align_up(record_size, BLOCK_SIZE);
        }

        Ok(offset as u64)
    }
}

// ── Platform I/O layer (see aoide_platform.rs) ────────────────────────────────
#[path = "aoide_platform.rs"]
mod platform;

// ── helpers ───────────────────────────────────────────────────────────────────

#[inline]
fn align_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

/// A heap buffer whose pointer is guaranteed to be `BLOCK_SIZE`-aligned.
/// Required for O_DIRECT on Linux (buffer address must be sector-aligned).
struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

impl AlignedBuf {
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
    fn as_slice_mut(&self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        if self.len > 0 {
            let layout = std::alloc::Layout::from_size_align(self.len, BLOCK_SIZE).unwrap();
            unsafe { std::alloc::dealloc(self.ptr, layout) };
        }
    }
}

/// Allocate a zero-filled buffer with `BLOCK_SIZE` alignment.
fn aligned_buf(size: usize) -> AlignedBuf {
    assert!(size > 0 && size % BLOCK_SIZE == 0);
    let layout = std::alloc::Layout::from_size_align(size, BLOCK_SIZE).unwrap();
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!ptr.is_null(), "aligned allocation failed");
    AlignedBuf { ptr, len: size }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mneme_common::Value;

    /// O_DIRECT / F_NOCACHE may fail on tmpfs or network filesystems — skip gracefully.
    fn try_open_wal(path: &Path, max: u64) -> Option<Aoide> {
        match Aoide::open(path, max) {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!("WAL open skipped: {e}");
                None
            }
        }
    }

    fn string_value(s: &str) -> Value { Value::String(s.as_bytes().to_vec()) }

    fn temp_path(suffix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mneme_aoide_{suffix}_{}.wal",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ))
    }

    #[test]
    fn append_and_replay() {
        let path = temp_path("replay");
        let Some(mut wal) = try_open_wal(&path, 16 * 1024 * 1024) else { return; };
        wal.append(1, 0, b"k1", &string_value("hello")).unwrap();
        wal.append(2, 9_999_999_999_999, b"k2", &string_value("world")).unwrap();
        wal.flush().unwrap();
        drop(wal);

        let mut replayed = Vec::new();
        Aoide::replay(&path, |seq, exp, key, _val| {
            replayed.push((seq, exp, key));
            Ok(())
        }).unwrap();
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0], (1, 0, b"k1".to_vec()));
        assert_eq!(replayed[1], (2, 9_999_999_999_999, b"k2".to_vec()));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replay_preserves_expires_at_ms() {
        let path = temp_path("ttl");
        let Some(mut wal) = try_open_wal(&path, 8 * 1024 * 1024) else { return; };
        let ttl: u64 = 1_700_000_000_000; // some future epoch ms
        wal.append(1, ttl, b"ttl_key", &string_value("v")).unwrap();
        wal.flush().unwrap();
        drop(wal);

        let mut found_exp = 0u64;
        Aoide::replay(&path, |_, exp, _, _| { found_exp = exp; Ok(()) }).unwrap();
        assert_eq!(found_exp, ttl);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replay_empty_file_succeeds() {
        let path = temp_path("empty");
        let Some(mut wal) = try_open_wal(&path, 4 * 1024 * 1024) else { return; };
        wal.flush().unwrap();
        drop(wal);
        let count = Aoide::replay(&path, |_, _, _, _| Ok(())).unwrap();
        assert_eq!(count, 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn offset_advances_after_append() {
        let path = temp_path("offset");
        let Some(mut wal) = try_open_wal(&path, 8 * 1024 * 1024) else { return; };
        assert_eq!(wal.offset(), 0);
        wal.append(1, 0, b"key", &string_value("val")).unwrap();
        assert!(wal.offset() > 0);
        std::fs::remove_file(&path).ok();
    }

    /// Compute a correct CRC for key+value bytes.
    fn compute_crc(key: &[u8], val_bytes: &[u8]) -> u32 {
        let mut digest = CRC32.digest();
        digest.update(key);
        digest.update(val_bytes);
        digest.finalize()
    }

    #[test]
    fn replay_corrupted_crc_stops() {
        let path = temp_path("corrupt_crc");
        // Write a valid record via Aoide, then corrupt its CRC
        let Some(mut wal) = try_open_wal(&path, 8 * 1024 * 1024) else { return; };
        wal.append(1, 0, b"k1", &string_value("hello")).unwrap();
        wal.flush().unwrap();
        drop(wal);

        // Corrupt the CRC bytes (offset 24..28 in the first block)
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start(24)).unwrap();
            f.write_all(&[0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
            f.sync_all().unwrap();
        }

        let mut replayed = Vec::new();
        let count = Aoide::replay(&path, |seq, _, key, _| {
            replayed.push((seq, key));
            Ok(())
        }).unwrap();
        assert_eq!(count, 0, "corrupted CRC should stop replay before yielding any record");
        assert!(replayed.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replay_corrupted_key_len_no_oom() {
        // Write raw bytes with key_len = u32::MAX — should stop, not OOM
        let path = temp_path("corrupt_keylen");
        {
            let mut f = File::create(&path).unwrap();
            let val_bytes = rmp_serde::to_vec(&string_value("v")).unwrap();
            let crc = compute_crc(b"k", &val_bytes);
            // Write with key_len = u32::MAX
            let record_size = ENTRY_HEADER_SIZE + 1 + val_bytes.len();
            let aligned_size = align_up(record_size, BLOCK_SIZE);
            let mut buf = vec![0u8; aligned_size];
            buf[0..8].copy_from_slice(&1u64.to_le_bytes()); // seq
            buf[8..16].copy_from_slice(&0u64.to_le_bytes()); // expires_at_ms
            buf[16..20].copy_from_slice(&u32::MAX.to_le_bytes()); // key_len = MAX
            buf[20..24].copy_from_slice(&(val_bytes.len() as u32).to_le_bytes());
            buf[24..28].copy_from_slice(&crc.to_le_bytes());
            use std::io::Write;
            f.write_all(&buf).unwrap();
            f.sync_all().unwrap();
        }

        let count = Aoide::replay(&path, |_, _, _, _| Ok(())).unwrap();
        assert_eq!(count, 0, "corrupted key_len should stop replay");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replay_corrupted_val_len_no_oom() {
        // Write raw bytes with val_len = u32::MAX — should stop, not OOM
        let path = temp_path("corrupt_vallen");
        {
            let mut f = File::create(&path).unwrap();
            let mut buf = vec![0u8; BLOCK_SIZE];
            buf[0..8].copy_from_slice(&1u64.to_le_bytes()); // seq
            buf[8..16].copy_from_slice(&0u64.to_le_bytes()); // expires_at_ms
            buf[16..20].copy_from_slice(&3u32.to_le_bytes()); // key_len = 3 (valid)
            buf[20..24].copy_from_slice(&u32::MAX.to_le_bytes()); // val_len = MAX
            buf[24..28].copy_from_slice(&0u32.to_le_bytes()); // dummy crc
            buf[ENTRY_HEADER_SIZE..ENTRY_HEADER_SIZE + 3].copy_from_slice(b"key");
            use std::io::Write;
            f.write_all(&buf).unwrap();
            f.sync_all().unwrap();
        }

        let count = Aoide::replay(&path, |_, _, _, _| Ok(())).unwrap();
        assert_eq!(count, 0, "corrupted val_len should stop replay");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replay_truncated_record() {
        // Write only a header (no key/value data following), then pad to < full record
        let path = temp_path("truncated");
        {
            let mut f = File::create(&path).unwrap();
            // Header claims key_len=10, val_len=10 but file is only one block
            // with no actual key/value data (just zeros). The read_exact will fail.
            let mut buf = vec![0u8; BLOCK_SIZE];
            buf[0..8].copy_from_slice(&1u64.to_le_bytes()); // seq
            buf[8..16].copy_from_slice(&0u64.to_le_bytes()); // expires_at_ms
            buf[16..20].copy_from_slice(&10u32.to_le_bytes()); // key_len
            buf[20..24].copy_from_slice(&10u32.to_le_bytes()); // val_len
            buf[24..28].copy_from_slice(&0u32.to_le_bytes()); // dummy crc
            use std::io::Write;
            // Write only half the needed data (record needs ENTRY_HEADER_SIZE + 20 = 48 bytes,
            // aligned to 4096, but we only write the first block with mostly zeros).
            // The key+value are zeros so CRC won't match → replay stops cleanly.
            f.write_all(&buf).unwrap();
            f.sync_all().unwrap();
        }

        // Should not panic — stops at CRC mismatch or read error
        let count = Aoide::replay(&path, |_, _, _, _| Ok(())).unwrap();
        assert_eq!(count, 0, "truncated record should stop replay");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn append_replay_round_trip() {
        let path = temp_path("roundtrip");
        let Some(mut wal) = try_open_wal(&path, 16 * 1024 * 1024) else { return; };

        let entries = vec![
            (1u64, 0u64,                b"alpha".to_vec(), string_value("one")),
            (2u64, 1_700_000_000_000u64, b"beta".to_vec(),  string_value("two")),
            (3u64, 0u64,                b"gamma".to_vec(), string_value("three")),
        ];

        for (seq, exp, ref key, ref val) in &entries {
            wal.append(*seq, *exp, key, val).unwrap();
        }
        wal.flush().unwrap();
        drop(wal);

        // Reopen and replay
        let mut recovered = Vec::new();
        let count = Aoide::replay(&path, |seq, exp, key, val| {
            recovered.push((seq, exp, key, val));
            Ok(())
        }).unwrap();

        assert_eq!(count, 3);
        assert_eq!(recovered.len(), 3);
        for (i, (seq, exp, key, _val)) in recovered.iter().enumerate() {
            assert_eq!(*seq, entries[i].0);
            assert_eq!(*exp, entries[i].1);
            assert_eq!(*key, entries[i].2);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn find_end_offset_empty_file() {
        let path = temp_path("end_offset_empty");
        {
            // Create a file full of zeros (simulates pre-allocated empty WAL)
            use std::io::Write;
            let mut f = File::create(&path).unwrap();
            f.write_all(&vec![0u8; BLOCK_SIZE * 4]).unwrap();
            f.sync_all().unwrap();
        }
        let offset = Aoide::find_end_offset(&path).unwrap();
        assert_eq!(offset, 0, "empty WAL file should have end offset 0");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn all_value_types_persist() {
        let path = temp_path("types");
        let Some(mut wal) = try_open_wal(&path, 16 * 1024 * 1024) else { return; };
        let hash = Value::Hash(vec![(b"f".to_vec(), b"v".to_vec())]);
        let list = Value::List(std::collections::VecDeque::from(vec![b"i".to_vec()]));
        wal.append(1, 0, b"string", &string_value("s")).unwrap();
        wal.append(2, 0, b"hash",   &hash).unwrap();
        wal.append(3, 0, b"list",   &list).unwrap();
        wal.flush().unwrap();
        drop(wal);

        let mut count = 0u64;
        Aoide::replay(&path, |_, _, _, _| { count += 1; Ok(()) }).unwrap();
        assert_eq!(count, 3);
        std::fs::remove_file(&path).ok();
    }
}
