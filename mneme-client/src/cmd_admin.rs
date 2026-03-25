// cmd_admin.rs — User management, observability, and cluster-admin commands on MnemeConn.

use anyhow::{bail, Result};
use bytes::Bytes;
use mneme_common::{
    CmdId, ConfigSetRequest, UserCreateRequest, UserDeleteRequest,
    UserGrantRequest, UserInfoRequest, UserRevokeRequest, UserSetRoleRequest,
};

use crate::conn::{check_ok, MnemeConn, Consistency};
use crate::response::{KeeperEntry, PoolStats, SlowLogEntry, UserInfo};

impl MnemeConn {
    // ── User management ───────────────────────────────────────────────────────

    /// USER-CREATE — create a new user. Caller must have admin role.
    ///
    /// `role` must be one of `"admin"`, `"readwrite"`, or `"readonly"`.
    /// An empty allowed_dbs on the server means "all databases".
    pub async fn user_create(
        &self,
        username: impl Into<String>,
        password: impl Into<String>,
        role:     impl Into<String>,
    ) -> Result<()> {
        let req = UserCreateRequest {
            username: username.into(),
            password: password.into(),
            role:     role.into(),
        };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::UserCreate, payload, Consistency::Quorum).await?;
        check_ok(&resp)
    }

    /// USER-DELETE — remove a user. Caller must have admin role.
    pub async fn user_delete(&self, username: impl Into<String>) -> Result<()> {
        let req = UserDeleteRequest { username: username.into() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::UserDelete, payload, Consistency::Quorum).await?;
        check_ok(&resp)
    }

    /// USER-LIST — return the names of all registered users.
    /// Caller must have admin role.
    pub async fn user_list(&self) -> Result<Vec<String>> {
        let payload = Bytes::from(rmp_serde::to_vec(&())?);
        let resp = self.send(CmdId::UserList, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let list: Vec<String> = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
        Ok(list)
    }

    /// USER-GRANT — grant a user access to a specific database by numeric ID.
    /// Caller must have admin role. An empty allowlist means access to all dbs.
    pub async fn user_grant(
        &self,
        username: impl Into<String>,
        db_id:    u16,
    ) -> Result<()> {
        let req = UserGrantRequest { username: username.into(), db_id };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::UserGrant, payload, Consistency::Quorum).await?;
        check_ok(&resp)
    }

    /// USER-REVOKE — revoke a user's access to a specific database.
    /// Caller must have admin role.
    pub async fn user_revoke(
        &self,
        username: impl Into<String>,
        db_id:    u16,
    ) -> Result<()> {
        let req = UserRevokeRequest { username: username.into(), db_id };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::UserRevoke, payload, Consistency::Quorum).await?;
        check_ok(&resp)
    }

    /// USER-INFO — return info for a specific user, or the calling user if
    /// `username` is `None`.
    pub async fn user_info(&self, username: Option<&str>) -> Result<UserInfo> {
        let req = UserInfoRequest { username: username.map(str::to_owned) };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::UserInfo, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let (username, role, allowed_dbs): (String, String, Vec<u16>) =
            rmp_serde::from_slice(&resp.payload)?;
        Ok(UserInfo { username, role, allowed_dbs })
    }

    /// USER-SET-ROLE — change a user's role. Caller must have admin role.
    ///
    /// `role` must be one of `"admin"`, `"readwrite"`, or `"readonly"`.
    pub async fn user_set_role(
        &self,
        username: impl Into<String>,
        role:     impl Into<String>,
    ) -> Result<()> {
        let req = UserSetRoleRequest { username: username.into(), role: role.into() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::UserSetRole, payload, Consistency::Quorum).await?;
        check_ok(&resp)
    }

    // ── Observability ─────────────────────────────────────────────────────────

    /// CLUSTER-INFO — return a key-value summary of cluster state.
    ///
    /// Includes: `raft_term`, `is_leader`, `leader_id`, `warmup_state`,
    /// `supported_modes`, `memory_pressure`, keeper count, and uptime.
    pub async fn cluster_info(&self) -> Result<Vec<(String, String)>> {
        let payload = Bytes::from(rmp_serde::to_vec(&())?);
        let resp = self.send(CmdId::ClusterInfo, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let pairs: Vec<(String, String)> =
            rmp_serde::from_slice(&resp.payload).unwrap_or_default();
        Ok(pairs)
    }

    /// KEEPER-LIST — return one entry per connected Keeper node.
    pub async fn keeper_list(&self) -> Result<Vec<KeeperEntry>> {
        let payload = Bytes::from(rmp_serde::to_vec(&())?);
        let resp = self.send(CmdId::KeeperList, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let raw: Vec<(u64, String, String, u64, u64)> =
            rmp_serde::from_slice(&resp.payload).unwrap_or_default();
        let entries = raw
            .into_iter()
            .map(|(node_id, name, addr, pool_bytes, used_bytes)| KeeperEntry {
                node_id,
                name,
                addr,
                pool_bytes,
                used_bytes,
            })
            .collect();
        Ok(entries)
    }

    /// POOL-STATS — return aggregate memory pool statistics.
    pub async fn pool_stats(&self) -> Result<PoolStats> {
        let payload = Bytes::from(rmp_serde::to_vec(&())?);
        let resp = self.send(CmdId::PoolStats, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let (used_bytes, total_bytes, keeper_count): (u64, u64, usize) =
            rmp_serde::from_slice(&resp.payload)?;
        Ok(PoolStats { used_bytes, total_bytes, keeper_count })
    }

    /// STATS — return a human-readable server statistics string (INFO-style).
    pub async fn stats(&self) -> Result<String> {
        let payload = Bytes::from(rmp_serde::to_vec(&())?);
        let resp = self.send(CmdId::Stats, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let s: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
        Ok(s)
    }

    /// METRICS — return the current Prometheus scrape epoch and total request
    /// count as `(epoch_ms, total_requests)`.
    pub async fn metrics(&self) -> Result<(u64, u64)> {
        let payload = Bytes::from(rmp_serde::to_vec(&())?);
        let resp = self.send(CmdId::Metrics, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let pair: (u64, u64) = rmp_serde::from_slice(&resp.payload)?;
        Ok(pair)
    }

    /// SLOW-LOG — return recent slow commands sorted by descending duration.
    ///
    /// The server retains the last N entries (default 128).
    pub async fn slowlog(&self) -> Result<Vec<SlowLogEntry>> {
        let payload = Bytes::from(rmp_serde::to_vec(&())?);
        let resp = self.send(CmdId::SlowLog, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let raw: Vec<(String, Vec<u8>, u64)> =
            rmp_serde::from_slice(&resp.payload).unwrap_or_default();
        let entries = raw
            .into_iter()
            .map(|(command, key, duration_us)| SlowLogEntry { command, key, duration_us })
            .collect();
        Ok(entries)
    }

    /// MEMORY-USAGE — return the approximate memory footprint of a key in bytes.
    ///
    /// Returns `0` if the key does not exist.
    pub async fn memory_usage(&self, key: impl AsRef<[u8]>) -> Result<u64> {
        // Server expects a bare key byte-vector for MemoryUsage.
        let payload = Bytes::from(rmp_serde::to_vec(&key.as_ref())?);
        let resp = self.send(CmdId::MemoryUsage, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Error {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("{msg}");
        }
        let n: u64 = rmp_serde::from_slice(&resp.payload).unwrap_or(0);
        Ok(n)
    }

    // ── Cluster admin ─────────────────────────────────────────────────────────

    /// CONFIG-SET — change a live server configuration parameter.
    ///
    /// Example parameters: `"memory.pool_bytes"`, `"io_threads"`.
    /// Not all parameters are hot-reloadable; the server returns an error for
    /// parameters that require a restart.
    pub async fn config_set(
        &self,
        param: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<()> {
        let req = ConfigSetRequest { param: param.into(), value: value.into() };
        let payload = Bytes::from(rmp_serde::to_vec(&req)?);
        let resp = self.send(CmdId::Config, payload, Consistency::Quorum).await?;
        check_ok(&resp)
    }
}
