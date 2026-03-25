// cmd_json.rs — JSON document commands on MnemeConn.

use anyhow::{bail, Result};
use bytes::Bytes;
use mneme_common::{
    CmdId, JsonArrAppendRequest, JsonDelRequest, JsonGetRequest,
    JsonNumIncrByRequest, JsonSetRequest, GetRequest,
};

use crate::conn::{check_ok, MnemeConn, Consistency};

impl MnemeConn {
    /// JSON.SET — store or update a JSON document (or sub-path).
    pub async fn json_set(
        &self,
        key:         impl AsRef<[u8]>,
        path:        impl Into<String>,
        value:       impl Into<String>,
        consistency: Consistency,
    ) -> Result<()> {
        let req = JsonSetRequest {
            key:   key.as_ref().to_vec(),
            path:  path.into(),
            value: value.into(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::JsonSet, payload, consistency).await?;
        check_ok(&resp)
    }

    /// JSON.GET — retrieve a value at `path` as a JSON string.
    /// Use `"$"` or `""` for the root document.
    pub async fn json_get(
        &self,
        key:         impl AsRef<[u8]>,
        path:        impl Into<String>,
        consistency: Consistency,
    ) -> Result<Option<String>> {
        let req = JsonGetRequest { key: key.as_ref().to_vec(), path: path.into() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::JsonGet, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error { return Ok(None); }
        let s: String = rmp_serde::from_slice(&resp.payload)?;
        Ok(Some(s))
    }

    /// JSON.DEL — delete a path (or the whole key if path is `"$"`).
    /// Returns true if the path existed.
    pub async fn json_del(
        &self,
        key:         impl AsRef<[u8]>,
        path:        impl Into<String>,
        consistency: Consistency,
    ) -> Result<bool> {
        let req = JsonDelRequest { key: key.as_ref().to_vec(), path: path.into() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::JsonDel, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error { return Ok(false); }
        let existed: bool = rmp_serde::from_slice(&resp.payload).unwrap_or(false);
        Ok(existed)
    }

    /// JSON.EXISTS — check whether `path` exists in the document.
    pub async fn json_exists(
        &self,
        key:  impl AsRef<[u8]>,
        path: impl Into<String>,
    ) -> Result<bool> {
        let req = JsonGetRequest { key: key.as_ref().to_vec(), path: path.into() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::JsonExists, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error { return Ok(false); }
        let b: bool = rmp_serde::from_slice(&resp.payload).unwrap_or(false);
        Ok(b)
    }

    /// JSON.TYPE — return the JSON type at `path`:
    /// `"object"`, `"array"`, `"string"`, `"number"`, `"boolean"`, `"null"`.
    pub async fn json_type(
        &self,
        key:  impl AsRef<[u8]>,
        path: impl Into<String>,
    ) -> Result<Option<String>> {
        let req = JsonGetRequest { key: key.as_ref().to_vec(), path: path.into() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::JsonType, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error { return Ok(None); }
        let t: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
        Ok(Some(t))
    }

    /// JSON.ARRAPPEND — append `value` (a JSON string) to the array at `path`.
    pub async fn json_arrappend(
        &self,
        key:         impl AsRef<[u8]>,
        path:        impl Into<String>,
        value:       impl Into<String>,
        consistency: Consistency,
    ) -> Result<u64> {
        let req = JsonArrAppendRequest {
            key:   key.as_ref().to_vec(),
            path:  path.into(),
            value: value.into(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::JsonArrAppend, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    /// JSON.NUMINCRBY — increment the number at `path` by `delta`. Returns the new value.
    pub async fn json_numincrby(
        &self,
        key:         impl AsRef<[u8]>,
        path:        impl Into<String>,
        delta:       f64,
        consistency: Consistency,
    ) -> Result<f64> {
        let req = JsonNumIncrByRequest {
            key:   key.as_ref().to_vec(),
            path:  path.into(),
            delta,
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::JsonNumIncrBy, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let v: f64 = rmp_serde::from_slice(&resp.payload)?;
        Ok(v)
    }

    // suppress unused import if all callers have their own check_ok
    #[allow(dead_code)]
    fn _uses_get_req(_: GetRequest) {}
}
