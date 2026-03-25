// cmd_kv.rs — String, counter, and bulk operations on MnemeConn.

use anyhow::{bail, Result};
use bytes::Bytes;
use mneme_common::{
    CmdId, DelRequest, ExpireRequest, GetRequest, GetSetRequest,
    IncrByFloatRequest, IncrByRequest, MGetRequest, MSetRequest, SetRequest, Value,
};

use crate::conn::{check_ok, MnemeConn, Consistency};

impl MnemeConn {
    // ── String ────────────────────────────────────────────────────────────────

    /// SET key value with TTL (ms) and consistency.
    pub async fn set(
        &self,
        key:   impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
        ttl_ms:      u64,
        consistency: Consistency,
    ) -> Result<()> {
        let req = SetRequest {
            key:   key.as_ref().to_vec(),
            value: Value::String(value.as_ref().to_vec()),
            ttl_ms,
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::Set, payload, consistency).await?;
        check_ok(&resp)
    }

    /// GET key — returns `None` if the key does not exist.
    pub async fn get(
        &self,
        key:         impl AsRef<[u8]>,
        consistency: Consistency,
    ) -> Result<Option<Vec<u8>>> {
        let req = GetRequest { key: key.as_ref().to_vec() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::Get, payload, consistency).await?;
        match resp.cmd_id {
            CmdId::Ok => {
                let val: Value = rmp_serde::from_slice(&resp.payload)?;
                match val {
                    Value::String(b) => Ok(Some(b)),
                    Value::Counter(n) => Ok(Some(n.to_string().into_bytes())),
                    _ => Ok(None),
                }
            }
            CmdId::Error => Ok(None),
            _ => bail!("unexpected response: {:?}", resp.cmd_id),
        }
    }

    /// DEL one or more keys. Returns the number of keys that were actually deleted.
    pub async fn del(
        &self,
        keys:        impl IntoIterator<Item = impl AsRef<[u8]>>,
        consistency: Consistency,
    ) -> Result<u64> {
        let req = DelRequest { keys: keys.into_iter().map(|k| k.as_ref().to_vec()).collect() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::Del, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    /// EXISTS — returns true if the key is present and not expired.
    pub async fn exists(&self, key: impl AsRef<[u8]>) -> Result<bool> {
        let req = GetRequest { key: key.as_ref().to_vec() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::Exists, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error { return Ok(false); }
        let v: bool = rmp_serde::from_slice(&resp.payload).unwrap_or(false);
        Ok(v)
    }

    /// EXPIRE — set TTL in seconds. Returns true if the key was found.
    pub async fn expire(
        &self,
        key:     impl AsRef<[u8]>,
        seconds: u64,
    ) -> Result<bool> {
        let req = ExpireRequest { key: key.as_ref().to_vec(), seconds };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::Expire, payload, Consistency::Quorum).await?;
        if resp.cmd_id == CmdId::Error { return Ok(false); }
        Ok(true)
    }

    /// TTL — remaining seconds. Returns -1 (no TTL), -2 (missing), or N>0.
    pub async fn ttl(&self, key: impl AsRef<[u8]>) -> Result<i64> {
        let req = GetRequest { key: key.as_ref().to_vec() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::Ttl, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error { return Ok(-2); }
        let n: i64 = rmp_serde::from_slice(&resp.payload).unwrap_or(-2);
        Ok(n)
    }

    /// GETSET — atomically return the old value and store the new one.
    pub async fn getset(
        &self,
        key:         impl AsRef<[u8]>,
        value:       impl AsRef<[u8]>,
        consistency: Consistency,
    ) -> Result<Option<Vec<u8>>> {
        let req = GetSetRequest {
            key:   key.as_ref().to_vec(),
            value: Value::String(value.as_ref().to_vec()),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::GetSet, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error { return Ok(None); }
        let val: Value = rmp_serde::from_slice(&resp.payload)?;
        match val {
            Value::String(b) => Ok(Some(b)),
            Value::Counter(n) => Ok(Some(n.to_string().into_bytes())),
            _ => Ok(None),
        }
    }

    // ── Counter ops ───────────────────────────────────────────────────────────

    /// INCR — increment integer by 1. Returns the new value.
    pub async fn incr(&self, key: impl AsRef<[u8]>, consistency: Consistency) -> Result<i64> {
        self.incrby(key, 1, consistency).await
    }

    /// DECR — decrement integer by 1. Returns the new value.
    pub async fn decr(&self, key: impl AsRef<[u8]>, consistency: Consistency) -> Result<i64> {
        self.incrby(key, -1, consistency).await
    }

    /// INCRBY — increment integer by delta. Returns the new value.
    pub async fn incrby(
        &self,
        key:         impl AsRef<[u8]>,
        delta:       i64,
        consistency: Consistency,
    ) -> Result<i64> {
        let req = IncrByRequest { key: key.as_ref().to_vec(), delta };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::IncrBy, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: i64 = rmp_serde::from_slice(&resp.payload)?;
        Ok(n)
    }

    /// DECRBY — decrement integer by delta. Returns the new value.
    pub async fn decrby(
        &self,
        key:         impl AsRef<[u8]>,
        delta:       i64,
        consistency: Consistency,
    ) -> Result<i64> {
        self.incrby(key, -delta, consistency).await
    }

    /// INCRBYFLOAT — increment float by delta. Returns the new value as bytes.
    pub async fn incrbyfloat(
        &self,
        key:         impl AsRef<[u8]>,
        delta:       f64,
        consistency: Consistency,
    ) -> Result<Vec<u8>> {
        let req = IncrByFloatRequest { key: key.as_ref().to_vec(), delta };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::IncrByFloat, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let s: String = rmp_serde::from_slice(&resp.payload)?;
        Ok(s.into_bytes())
    }

    // ── Bulk ops ──────────────────────────────────────────────────────────────

    /// MGET — fetch multiple keys. Returns `None` for missing keys.
    pub async fn mget(
        &self,
        keys:        impl IntoIterator<Item = impl AsRef<[u8]>>,
        consistency: Consistency,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let req = MGetRequest { keys: keys.into_iter().map(|k| k.as_ref().to_vec()).collect() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::MGet, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let vals: Vec<Option<Value>> = rmp_serde::from_slice(&resp.payload)?;
        Ok(vals.into_iter().map(|v| match v {
            Some(Value::String(b)) => Some(b),
            Some(Value::Counter(n)) => Some(n.to_string().into_bytes()),
            _ => None,
        }).collect())
    }

    /// MSET — write multiple key-value pairs atomically.
    /// `pairs` is an iterator of `(key, value, ttl_ms)` tuples (ttl_ms=0 = no expiry).
    pub async fn mset(
        &self,
        pairs: impl IntoIterator<Item = (impl AsRef<[u8]>, impl AsRef<[u8]>, u64)>,
        consistency: Consistency,
    ) -> Result<()> {
        let req = MSetRequest {
            pairs: pairs.into_iter().map(|(k, v, ttl)| {
                (k.as_ref().to_vec(), Value::String(v.as_ref().to_vec()), ttl)
            }).collect(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::MSet, payload, consistency).await?;
        check_ok(&resp)
    }
}
