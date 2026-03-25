// cmd_zset.rs — Sorted-set commands on MnemeConn.

use anyhow::{bail, Result};
use bytes::Bytes;
use mneme_common::{
    CmdId, GetRequest, ZAddRequest, ZRangeByScoreRequest, ZRangeRequest, ZRankRequest,
    ZRemRequest, ZSetMember, Value,
};

use crate::conn::{MnemeConn, Consistency};

impl MnemeConn {
    /// ZADD — add or update members with scores.
    pub async fn zadd(
        &self,
        key:     impl AsRef<[u8]>,
        members: impl IntoIterator<Item = (f64, impl AsRef<[u8]>)>,
        consistency: Consistency,
    ) -> Result<u64> {
        let req = ZAddRequest {
            key: key.as_ref().to_vec(),
            members: members.into_iter()
                .map(|(score, m)| ZSetMember::new(score, m.as_ref()))
                .collect(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::ZAdd, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    /// ZRANK — return the 0-based rank of a member (ascending score order).
    /// Returns `None` if the key or member does not exist.
    pub async fn zrank(
        &self,
        key:    impl AsRef<[u8]>,
        member: impl AsRef<[u8]>,
    ) -> Result<Option<i64>> {
        let req = ZRankRequest { key: key.as_ref().to_vec(), member: member.as_ref().to_vec() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::ZRank, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error { return Ok(None); }
        let rank: i64 = rmp_serde::from_slice(&resp.payload).unwrap_or(-1);
        if rank < 0 { Ok(None) } else { Ok(Some(rank)) }
    }

    /// ZSCORE — return the score of a member.
    /// Returns `None` if the key or member does not exist.
    pub async fn zscore(
        &self,
        key:    impl AsRef<[u8]>,
        member: impl AsRef<[u8]>,
    ) -> Result<Option<f64>> {
        let req = ZRankRequest { key: key.as_ref().to_vec(), member: member.as_ref().to_vec() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::ZScore, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error { return Ok(None); }
        let score: f64 = rmp_serde::from_slice(&resp.payload).unwrap_or(f64::NEG_INFINITY);
        if score == f64::NEG_INFINITY { Ok(None) } else { Ok(Some(score)) }
    }

    /// ZRANGE — return elements by rank range.
    /// If `with_scores` is true, returns `(member, score)` pairs.
    pub async fn zrange(
        &self,
        key:         impl AsRef<[u8]>,
        start:       i64,
        stop:        i64,
        with_scores: bool,
        consistency: Consistency,
    ) -> Result<Vec<(Vec<u8>, Option<f64>)>> {
        let req = ZRangeRequest { key: key.as_ref().to_vec(), start, stop, with_scores };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::ZRange, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error { return Ok(vec![]); }
        let val: Value = rmp_serde::from_slice(&resp.payload)?;
        match val {
            Value::ZSet(members) => Ok(members.into_iter()
                .map(|m| (m.member, if with_scores { Some(m.score) } else { None }))
                .collect()),
            _ => Ok(vec![]),
        }
    }

    /// ZRANGEBYSCORE — return elements with scores between `min` and `max` (inclusive).
    pub async fn zrangebyscore(
        &self,
        key:         impl AsRef<[u8]>,
        min:         f64,
        max:         f64,
        consistency: Consistency,
    ) -> Result<Vec<Vec<u8>>> {
        let req = ZRangeByScoreRequest { key: key.as_ref().to_vec(), min, max };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::ZRangeByScore, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error { return Ok(vec![]); }
        let val: Value = rmp_serde::from_slice(&resp.payload)?;
        match val {
            Value::ZSet(members) => Ok(members.into_iter().map(|m| m.member).collect()),
            _ => Ok(vec![]),
        }
    }

    /// ZREM — remove one or more members from a sorted set.
    /// Returns the number of members actually removed.
    pub async fn zrem(
        &self,
        key:     impl AsRef<[u8]>,
        members: impl IntoIterator<Item = impl AsRef<[u8]>>,
        consistency: Consistency,
    ) -> Result<u64> {
        let req = ZRemRequest {
            key:     key.as_ref().to_vec(),
            members: members.into_iter().map(|m| m.as_ref().to_vec()).collect(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::ZRem, payload, consistency).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    /// ZCARD — return the number of members in the sorted set.
    pub async fn zcard(&self, key: impl AsRef<[u8]>) -> Result<u64> {
        let req = GetRequest { key: key.as_ref().to_vec() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::ZCard, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error { return Ok(0); }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }
}

// suppress unused import warning for check_ok when not called here
#[allow(unused_imports)]
use crate::conn::check_ok as _check_ok;
