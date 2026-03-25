// cmd_db.rs — Database namespacing, SCAN, TYPE, SELECT, DBSIZE, FLUSHDB on MnemeConn.

use anyhow::{bail, Result};
use bytes::Bytes;
use mneme_common::{
    CmdId, DbCreateRequest, DbDropRequest, DbInfo, DbSizeRequest, FlushDbRequest,
    GetRequest, ScanRequest, SelectRequest,
};

use crate::conn::{check_ok, MnemeConn, Consistency};
use crate::response::ScanPage;

impl MnemeConn {
    // ── SELECT / DBSIZE / FLUSHDB ─────────────────────────────────────────────

    /// SELECT — switch the active database by numeric ID.
    pub async fn select_id(&self, db_id: u16) -> Result<()> {
        let req = SelectRequest { db_id, name: String::new() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::Select, payload, Consistency::Eventual).await?;
        check_ok(&resp)
    }

    /// SELECT — switch the active database by name.
    /// The server resolves the name to a numeric ID.
    pub async fn select_name(&self, name: impl Into<String>) -> Result<()> {
        let req = SelectRequest { db_id: 0, name: name.into() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::Select, payload, Consistency::Eventual).await?;
        check_ok(&resp)
    }

    /// DBSIZE — count live keys in a specific database (by ID).
    /// Pass `None` to use the connection's currently-active database.
    pub async fn dbsize_id(&self, db_id: Option<u16>) -> Result<u64> {
        let req = DbSizeRequest { db_id, name: String::new() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::DbSize, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    /// DBSIZE — count live keys in a database by name.
    pub async fn dbsize_name(&self, name: impl Into<String>) -> Result<u64> {
        let req = DbSizeRequest { db_id: None, name: name.into() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::DbSize, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    /// FLUSHDB — delete all keys in a database by ID.
    /// `sync = true` propagates deletes to Keepers (default).
    pub async fn flushdb_id(
        &self,
        db_id:       Option<u16>,
        sync:        bool,
        consistency: Consistency,
    ) -> Result<u64> {
        let req = FlushDbRequest { db_id, name: String::new(), sync };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::FlushDb, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    /// FLUSHDB — delete all keys in a database by name.
    pub async fn flushdb_name(
        &self,
        name:        impl Into<String>,
        sync:        bool,
        consistency: Consistency,
    ) -> Result<u64> {
        let req = FlushDbRequest { db_id: None, name: name.into(), sync };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::FlushDb, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    // ── Named DB registry ─────────────────────────────────────────────────────

    /// DB-CREATE — register a new named database.
    /// `db_id = None` lets the server assign the next available ID.
    pub async fn db_create(
        &self,
        name:  impl Into<String>,
        db_id: Option<u16>,
    ) -> Result<u16> {
        let req = DbCreateRequest { name: name.into(), db_id };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::DbCreate, payload, Consistency::Quorum).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let id: u16 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(id)
    }

    /// DB-LIST — return all registered named databases.
    pub async fn db_list(&self) -> Result<Vec<DbInfo>> {
        let payload = Bytes::from(rmp_serde::to_vec(&())?);
        let resp = self.send(CmdId::DbList, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let list: Vec<DbInfo> = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
        Ok(list)
    }

    /// DB-DROP — unregister a named database.
    /// Data is NOT deleted — keys remain accessible by numeric ID.
    pub async fn db_drop(&self, name: impl Into<String>) -> Result<()> {
        let req = DbDropRequest { name: name.into() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::DbDrop, payload, Consistency::Quorum).await?;
        check_ok(&resp)
    }

    // ── SCAN / TYPE ───────────────────────────────────────────────────────────

    /// SCAN — one page of cursor-based key iteration.
    ///
    /// Pass `cursor = 0` to start a new scan. Keep passing the returned
    /// `next_cursor` until it equals 0, which signals the scan is complete.
    pub async fn scan(
        &self,
        cursor:  u64,
        pattern: Option<&str>,
        count:   u64,
    ) -> Result<ScanPage> {
        let req = ScanRequest {
            cursor,
            pattern: pattern.map(str::to_owned),
            count,
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::Scan, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let (next_cursor, keys): (u64, Vec<Vec<u8>>) =
            rmp_serde::from_slice(&resp.payload)?;
        Ok(ScanPage { next_cursor, keys })
    }

    /// SCAN ALL — convenience wrapper that auto-iterates until cursor = 0.
    /// Returns all matching keys. Caution: may be slow on large keyspaces.
    pub async fn scan_all(&self, pattern: Option<&str>) -> Result<Vec<Vec<u8>>> {
        let mut cursor = 0u64;
        let mut all = Vec::new();
        loop {
            let page = self.scan(cursor, pattern, 100).await?;
            all.extend(page.keys);
            cursor = page.next_cursor;
            if cursor == 0 { break; }
        }
        Ok(all)
    }

    /// TYPE — return the type name of a key's value.
    /// Returns `"string"`, `"hash"`, `"list"`, `"zset"`, `"counter"`, `"json"`,
    /// or `"none"` if the key does not exist.
    pub async fn type_of(&self, key: impl AsRef<[u8]>) -> Result<String> {
        let req = GetRequest { key: key.as_ref().to_vec() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::Type, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error { return Ok("none".into()); }
        let t: String = rmp_serde::from_slice(&resp.payload).unwrap_or_else(|_| "none".into());
        Ok(t)
    }
}
