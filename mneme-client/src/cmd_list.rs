// cmd_list.rs — List commands on MnemeConn.

use anyhow::{bail, Result};
use bytes::Bytes;
use mneme_common::{CmdId, ListPushRequest, LRangeRequest, GetRequest, Value};

use crate::conn::{MnemeConn, Consistency};

impl MnemeConn {
    /// LPUSH — prepend one or more values to the list (head). Creates the key if absent.
    pub async fn lpush(
        &self,
        key:    impl AsRef<[u8]>,
        values: impl IntoIterator<Item = impl AsRef<[u8]>>,
        consistency: Consistency,
    ) -> Result<u64> {
        let req = ListPushRequest {
            key:    key.as_ref().to_vec(),
            values: values.into_iter().map(|v| v.as_ref().to_vec()).collect(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::LPush, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    /// RPUSH — append one or more values to the list (tail). Creates the key if absent.
    pub async fn rpush(
        &self,
        key:    impl AsRef<[u8]>,
        values: impl IntoIterator<Item = impl AsRef<[u8]>>,
        consistency: Consistency,
    ) -> Result<u64> {
        let req = ListPushRequest {
            key:    key.as_ref().to_vec(),
            values: values.into_iter().map(|v| v.as_ref().to_vec()).collect(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::RPush, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    /// LPOP — remove and return the leftmost element. Returns `None` if the list is empty.
    pub async fn lpop(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        self.list_pop(CmdId::LPop, key).await
    }

    /// RPOP — remove and return the rightmost element. Returns `None` if the list is empty.
    pub async fn rpop(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        self.list_pop(CmdId::RPop, key).await
    }

    async fn list_pop(&self, cmd: CmdId, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let req = GetRequest { key: key.as_ref().to_vec() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(cmd, payload, Consistency::Quorum).await?;
        if resp.cmd_id == CmdId::Error { return Ok(None); }
        let val: Value = rmp_serde::from_slice(&resp.payload)?;
        match val {
            Value::String(b) => Ok(Some(b)),
            _ => Ok(None),
        }
    }

    /// LRANGE — return elements from `start` to `stop` (0-based, -1 = last).
    pub async fn lrange(
        &self,
        key:         impl AsRef<[u8]>,
        start:       i64,
        stop:        i64,
        consistency: Consistency,
    ) -> Result<Vec<Vec<u8>>> {
        let req = LRangeRequest { key: key.as_ref().to_vec(), start, stop };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::LRange, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error { return Ok(vec![]); }
        let val: Value = rmp_serde::from_slice(&resp.payload)?;
        match val {
            Value::List(items) => Ok(items.into_iter().collect()),
            _ => Ok(vec![]),
        }
    }
}
