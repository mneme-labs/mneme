// cmd_hash.rs — Hash commands on MnemeConn.

use anyhow::{bail, Result};
use bytes::Bytes;
use mneme_common::{CmdId, HDelRequest, HGetRequest, HSetRequest, Value};

use crate::conn::{check_ok, MnemeConn, Consistency};

impl MnemeConn {
    /// HSET — set one or more field-value pairs in a hash. Creates the key if absent.
    pub async fn hset(
        &self,
        key:   impl AsRef<[u8]>,
        pairs: impl IntoIterator<Item = (impl AsRef<[u8]>, impl AsRef<[u8]>)>,
        consistency: Consistency,
    ) -> Result<()> {
        let req = HSetRequest {
            key: key.as_ref().to_vec(),
            pairs: pairs.into_iter()
                .map(|(f, v)| (f.as_ref().to_vec(), v.as_ref().to_vec()))
                .collect(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::HSet, payload, consistency).await?;
        check_ok(&resp)
    }

    /// HGET — get the value of one field. Returns `None` if the key or field is absent.
    pub async fn hget(
        &self,
        key:         impl AsRef<[u8]>,
        field:       impl AsRef<[u8]>,
        consistency: Consistency,
    ) -> Result<Option<Vec<u8>>> {
        let req = HGetRequest {
            key:   key.as_ref().to_vec(),
            field: field.as_ref().to_vec(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::HGet, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error { return Ok(None); }
        let val: Value = rmp_serde::from_slice(&resp.payload)?;
        match val {
            Value::String(b) => Ok(Some(b)),
            _ => Ok(None),
        }
    }

    /// HDEL — delete one or more fields from a hash.
    /// Returns the number of fields actually removed.
    pub async fn hdel(
        &self,
        key:    impl AsRef<[u8]>,
        fields: impl IntoIterator<Item = impl AsRef<[u8]>>,
        consistency: Consistency,
    ) -> Result<u64> {
        let req = HDelRequest {
            key:    key.as_ref().to_vec(),
            fields: fields.into_iter().map(|f| f.as_ref().to_vec()).collect(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::HDel, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    /// HGETALL — return all field-value pairs in the hash.
    pub async fn hgetall(
        &self,
        key:         impl AsRef<[u8]>,
        consistency: Consistency,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let req = HGetRequest {
            key:   key.as_ref().to_vec(),
            field: b"*".to_vec(), // sentinel: server returns all fields
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::HGetAll, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error { return Ok(vec![]); }
        let val: Value = rmp_serde::from_slice(&resp.payload)?;
        match val {
            Value::Hash(pairs) => Ok(pairs),
            _ => Ok(vec![]),
        }
    }
}
