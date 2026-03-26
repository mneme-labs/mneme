// cmd_pipeline.rs — Pipeline: batch multiple commands in a single write.
//
// A Pipeline queues command frames locally and flushes them all in one
// `write_all()` call, eliminating per-command TCP round-trips.  Responses
// are collected in submission order.
//
// # Example
//
// ```no_run
// use mneme_client::{Pipeline, Consistency};
// use mneme_common::types::Value;
//
// let mut p = Pipeline::new();
// p.set(b"counter", Value::Counter(0), 0, Consistency::Quorum);
// p.incr(b"counter");
// p.get(b"counter");
//
// let results = conn.execute_pipeline(p).await?;
// // results[2] contains the final counter value
// ```

use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use bytes::Bytes;
use mneme_common::{
    CmdId, Frame,
    GetRequest, SetRequest, DelRequest, ExpireRequest,
    HGetRequest, HSetRequest, HDelRequest,
    ListPushRequest, LRangeRequest,
    ZAddRequest, ZRangeRequest, ZRangeByScoreRequest, ZRemRequest, ZRankRequest,
    IncrByRequest, IncrByFloatRequest,
    types::Value,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

use crate::conn::{consistency_flags, MnemeConn, Consistency};

// ── Pipeline ───────────────────────────────────────────────────────────────────

/// A batch of commands accumulated for a single flushed write.
///
/// Build by calling the provided command methods, then execute with
/// [`MnemeConn::execute_pipeline`].
pub struct Pipeline {
    commands: Vec<(CmdId, Bytes, Consistency)>,
}

impl Pipeline {
    /// Create an empty pipeline.
    pub fn new() -> Self {
        Self { commands: Vec::new() }
    }

    /// Append a raw command frame. Use this when no typed builder method exists.
    pub fn raw(&mut self, cmd: CmdId, payload: Bytes, consistency: Consistency) -> &mut Self {
        self.commands.push((cmd, payload, consistency));
        self
    }

    /// Number of commands queued so far.
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// True if no commands have been queued.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    // ── String / KV ──────────────────────────────────────────────────────────

    /// Queue a GET command. Response payload is a msgpack-encoded `Value` or
    /// a `KeyNotFound` error.
    pub fn get(&mut self, key: impl AsRef<[u8]>) -> &mut Self {
        let req = GetRequest { key: key.as_ref().to_vec() };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::Get, payload, Consistency::Quorum));
        self
    }

    /// Queue a SET command with optional TTL (0 = no expiry).
    pub fn set(
        &mut self,
        key:     impl AsRef<[u8]>,
        value:   Value,
        ttl_ms:  u64,
        consistency: Consistency,
    ) -> &mut Self {
        let req = SetRequest { key: key.as_ref().to_vec(), value, ttl_ms };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::Set, payload, consistency));
        self
    }

    /// Queue a DEL command for one or more keys.
    pub fn del(&mut self, keys: &[impl AsRef<[u8]>]) -> &mut Self {
        let req = DelRequest {
            keys: keys.iter().map(|k| k.as_ref().to_vec()).collect(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::Del, payload, Consistency::Quorum));
        self
    }

    /// Queue an EXISTS check.
    pub fn exists(&mut self, key: impl AsRef<[u8]>) -> &mut Self {
        let payload = Bytes::from(rmp_serde::to_vec(&key.as_ref()).unwrap_or_default());
        self.commands.push((CmdId::Exists, payload, Consistency::Eventual));
        self
    }

    /// Queue a TTL query. Response is `i64` ms remaining (-1 = no expiry, -2 = missing).
    pub fn ttl(&mut self, key: impl AsRef<[u8]>) -> &mut Self {
        let payload = Bytes::from(rmp_serde::to_vec(&key.as_ref()).unwrap_or_default());
        self.commands.push((CmdId::Ttl, payload, Consistency::Eventual));
        self
    }

    /// Queue an EXPIRE command.
    pub fn expire(&mut self, key: impl AsRef<[u8]>, seconds: u64) -> &mut Self {
        let req = ExpireRequest { key: key.as_ref().to_vec(), seconds };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::Expire, payload, Consistency::Quorum));
        self
    }

    // ── Counters ──────────────────────────────────────────────────────────────

    /// Queue INCR. Response is `i64` (new value).
    pub fn incr(&mut self, key: impl AsRef<[u8]>) -> &mut Self {
        let payload = Bytes::from(rmp_serde::to_vec(&key.as_ref()).unwrap_or_default());
        self.commands.push((CmdId::Incr, payload, Consistency::Quorum));
        self
    }

    /// Queue DECR. Response is `i64`.
    pub fn decr(&mut self, key: impl AsRef<[u8]>) -> &mut Self {
        let payload = Bytes::from(rmp_serde::to_vec(&key.as_ref()).unwrap_or_default());
        self.commands.push((CmdId::Decr, payload, Consistency::Quorum));
        self
    }

    /// Queue INCRBY with a signed delta. Response is `i64`.
    pub fn incrby(&mut self, key: impl AsRef<[u8]>, delta: i64) -> &mut Self {
        let req = IncrByRequest { key: key.as_ref().to_vec(), delta };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::IncrBy, payload, Consistency::Quorum));
        self
    }

    /// Queue DECRBY. Response is `i64`.
    pub fn decrby(&mut self, key: impl AsRef<[u8]>, delta: i64) -> &mut Self {
        let req = IncrByRequest { key: key.as_ref().to_vec(), delta };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::DecrBy, payload, Consistency::Quorum));
        self
    }

    /// Queue INCRBYFLOAT. Response is `f64`.
    pub fn incrbyfloat(&mut self, key: impl AsRef<[u8]>, delta: f64) -> &mut Self {
        let req = IncrByFloatRequest { key: key.as_ref().to_vec(), delta };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::IncrByFloat, payload, Consistency::Quorum));
        self
    }

    // ── Hash ──────────────────────────────────────────────────────────────────

    /// Queue HSET for multiple field-value pairs (raw bytes).
    pub fn hset(
        &mut self,
        key:   impl AsRef<[u8]>,
        pairs: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> &mut Self {
        let req = HSetRequest { key: key.as_ref().to_vec(), pairs };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::HSet, payload, Consistency::Quorum));
        self
    }

    /// Queue HGET. Response payload is a msgpack-encoded `Value` or `KeyNotFound`.
    pub fn hget(&mut self, key: impl AsRef<[u8]>, field: impl AsRef<[u8]>) -> &mut Self {
        let req = HGetRequest {
            key:   key.as_ref().to_vec(),
            field: field.as_ref().to_vec(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::HGet, payload, Consistency::Quorum));
        self
    }

    /// Queue HDEL for one or more fields.
    pub fn hdel(&mut self, key: impl AsRef<[u8]>, fields: &[impl AsRef<[u8]>]) -> &mut Self {
        let req = HDelRequest {
            key:    key.as_ref().to_vec(),
            fields: fields.iter().map(|f| f.as_ref().to_vec()).collect(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::HDel, payload, Consistency::Quorum));
        self
    }

    /// Queue HGETALL. Response is `Vec<(field_bytes, value_bytes)>`.
    pub fn hgetall(&mut self, key: impl AsRef<[u8]>) -> &mut Self {
        let payload = Bytes::from(rmp_serde::to_vec(&key.as_ref()).unwrap_or_default());
        self.commands.push((CmdId::HGetAll, payload, Consistency::Quorum));
        self
    }

    // ── List ──────────────────────────────────────────────────────────────────

    /// Queue LPUSH with one or more raw-byte values.
    pub fn lpush(&mut self, key: impl AsRef<[u8]>, values: Vec<Vec<u8>>) -> &mut Self {
        let req = ListPushRequest { key: key.as_ref().to_vec(), values };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::LPush, payload, Consistency::Quorum));
        self
    }

    /// Queue RPUSH with one or more raw-byte values.
    pub fn rpush(&mut self, key: impl AsRef<[u8]>, values: Vec<Vec<u8>>) -> &mut Self {
        let req = ListPushRequest { key: key.as_ref().to_vec(), values };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::RPush, payload, Consistency::Quorum));
        self
    }

    /// Queue LPOP. Response is a msgpack-encoded value.
    pub fn lpop(&mut self, key: impl AsRef<[u8]>) -> &mut Self {
        let payload = Bytes::from(rmp_serde::to_vec(&key.as_ref()).unwrap_or_default());
        self.commands.push((CmdId::LPop, payload, Consistency::Quorum));
        self
    }

    /// Queue RPOP.
    pub fn rpop(&mut self, key: impl AsRef<[u8]>) -> &mut Self {
        let payload = Bytes::from(rmp_serde::to_vec(&key.as_ref()).unwrap_or_default());
        self.commands.push((CmdId::RPop, payload, Consistency::Quorum));
        self
    }

    /// Queue LRANGE. Response is `Vec<Value>`.
    pub fn lrange(&mut self, key: impl AsRef<[u8]>, start: i64, stop: i64) -> &mut Self {
        let req = LRangeRequest { key: key.as_ref().to_vec(), start, stop };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::LRange, payload, Consistency::Quorum));
        self
    }

    // ── Sorted set ────────────────────────────────────────────────────────────

    /// Queue ZADD. Response is `u64` (count of elements actually added).
    pub fn zadd(
        &mut self,
        key:     impl AsRef<[u8]>,
        members: Vec<mneme_common::types::ZSetMember>,
    ) -> &mut Self {
        let req = ZAddRequest { key: key.as_ref().to_vec(), members };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::ZAdd, payload, Consistency::Quorum));
        self
    }

    /// Queue ZRANGE. Response is `Vec<ZSetMember>`.
    pub fn zrange(
        &mut self,
        key:         impl AsRef<[u8]>,
        start:       i64,
        stop:        i64,
        with_scores: bool,
    ) -> &mut Self {
        let req = ZRangeRequest { key: key.as_ref().to_vec(), start, stop, with_scores };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::ZRange, payload, Consistency::Eventual));
        self
    }

    /// Queue ZRANGEBYSCORE. Response is `Vec<ZSetMember>`.
    pub fn zrangebyscore(
        &mut self,
        key: impl AsRef<[u8]>,
        min: f64,
        max: f64,
    ) -> &mut Self {
        let req = ZRangeByScoreRequest { key: key.as_ref().to_vec(), min, max };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::ZRangeByScore, payload, Consistency::Eventual));
        self
    }

    /// Queue ZRANK. Response is `u64` rank or `KeyNotFound`.
    pub fn zrank(&mut self, key: impl AsRef<[u8]>, member: impl AsRef<[u8]>) -> &mut Self {
        let req = ZRankRequest {
            key:    key.as_ref().to_vec(),
            member: member.as_ref().to_vec(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::ZRank, payload, Consistency::Eventual));
        self
    }

    /// Queue ZREM. Response is `u64` (number removed).
    pub fn zrem(&mut self, key: impl AsRef<[u8]>, members: &[impl AsRef<[u8]>]) -> &mut Self {
        let req = ZRemRequest {
            key:     key.as_ref().to_vec(),
            members: members.iter().map(|m| m.as_ref().to_vec()).collect(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req).unwrap_or_default());
        self.commands.push((CmdId::ZRem, payload, Consistency::Quorum));
        self
    }
}

impl Default for Pipeline {
    fn default() -> Self { Self::new() }
}

// ── MnemeConn::execute_pipeline ───────────────────────────────────────────────

impl MnemeConn {
    /// Execute all queued pipeline commands in a single write.
    ///
    /// All frames are serialised together and flushed in one `write_all()` call.
    /// Responses are returned in **submission order** — one `Frame` per queued
    /// command.
    ///
    /// Check each frame's `cmd_id`:
    /// - `CmdId::Ok`    → success; decode `payload` per the command's response spec.
    /// - `CmdId::Error` → failure; payload is a msgpack-encoded error string.
    ///
    /// Returns an empty vec if the pipeline is empty.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use mneme_client::{Pipeline, Consistency};
    /// use mneme_common::CmdId;
    ///
    /// let mut p = Pipeline::new();
    /// p.get(b"a").get(b"b").get(b"c");
    ///
    /// let results = conn.execute_pipeline(p).await?;
    /// for frame in &results {
    ///     if frame.cmd_id == CmdId::Ok {
    ///         let val: mneme_common::types::Value =
    ///             rmp_serde::from_slice(&frame.payload)?;
    ///         println!("{val:?}");
    ///     }
    /// }
    /// ```
    pub async fn execute_pipeline(&self, mut pipeline: Pipeline) -> Result<Vec<Frame>> {
        if pipeline.commands.is_empty() {
            return Ok(vec![]);
        }

        let mut receivers = Vec::with_capacity(pipeline.commands.len());
        let mut combined  = Vec::new();

        for (cmd, payload, consistency) in pipeline.commands.drain(..) {
            // Allocate unique req_id (same logic as send())
            let id = loop {
                let candidate = self.req_id.fetch_add(1, Ordering::Relaxed);
                if candidate != 0 && !self.pending.contains_key(&candidate) {
                    break candidate;
                }
            };
            let flags = consistency_flags(consistency);
            let frame = Frame { cmd_id: cmd, flags, req_id: id, payload };
            let (tx, rx) = oneshot::channel();
            self.pending.insert(id, tx);
            receivers.push(rx);
            combined.extend_from_slice(&frame.encode());
        }

        {
            let mut w = self.writer.lock().await;
            w.write_all(&combined).await.context("pipeline write")?;
        }

        let mut results = Vec::with_capacity(receivers.len());
        for rx in receivers {
            results.push(rx.await.context("pipeline response channel closed")?);
        }
        Ok(results)
    }
}
