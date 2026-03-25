// RaftTransport — real mTLS network layer for Core-to-Core Raft RPCs.
//
// Replaces the stub MnemeNetwork. Each peer Core gets one persistent mTLS
// connection reused across all Raft RPCs. Serializes openraft requests with
// rmp_serde, wraps them in the standard 16-byte frame protocol, and reads
// back frame-wrapped responses.

use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::BytesMut;
use openraft::error::{NetworkError, RPCError, RaftError, InstallSnapshotError};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse,
    InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::{BasicNode, RaftNetwork, RaftNetworkFactory};
use parking_lot::Mutex;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tracing::warn;

use mneme_common::{CmdId, Frame, HEADER_LEN};

use super::themis::{MnemeNodeId, TypeConfig};
use crate::net::aegis::Aegis;

const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// ── Transport Factory ────────────────────────────────────────────────────────

pub struct RaftTransport {
    aegis: Arc<Aegis>,
    server_name: String,
}

impl RaftTransport {
    pub fn new(aegis: Arc<Aegis>, server_name: String) -> Self {
        Self { aegis, server_name }
    }
}

impl RaftNetworkFactory<TypeConfig> for Arc<RaftTransport> {
    type Network = RaftNetworkConn;

    async fn new_client(
        &mut self,
        target: MnemeNodeId,
        node: &BasicNode,
    ) -> Self::Network {
        RaftNetworkConn {
            target,
            addr: node.addr.clone(),
            aegis: self.aegis.clone(),
            server_name: self.server_name.clone(),
            conn: Arc::new(Mutex::new(None)),
        }
    }
}

// ── Per-peer connection ──────────────────────────────────────────────────────

pub struct RaftNetworkConn {
    target: MnemeNodeId,
    addr: String,
    aegis: Arc<Aegis>,
    server_name: String,
    conn: Arc<Mutex<Option<TlsStream<TcpStream>>>>,
}

impl RaftNetworkConn {
    async fn ensure_conn(&self) -> Result<()> {
        {
            let guard = self.conn.lock();
            if guard.is_some() {
                return Ok(());
            }
        }
        let stream = dial_peer(&self.addr, &self.aegis, &self.server_name).await?;
        *self.conn.lock() = Some(stream);
        Ok(())
    }

    fn take_conn(&self) -> Option<TlsStream<TcpStream>> {
        self.conn.lock().take()
    }

    fn put_conn(&self, stream: TlsStream<TcpStream>) {
        *self.conn.lock() = Some(stream);
    }

    async fn rpc<Req, Resp>(
        &self,
        cmd_id: CmdId,
        request: &Req,
    ) -> Result<Resp>
    where
        Req: serde::Serialize,
        Resp: serde::de::DeserializeOwned,
    {
        if let Err(e) = self.ensure_conn().await {
            warn!(target = %self.target, addr = %self.addr, "Raft dial failed: {e:#}");
            return Err(e);
        }
        let mut stream = self.take_conn().unwrap();

        let payload = rmp_serde::to_vec(request)
            .context("serialize Raft RPC")?;
        let frame = Frame {
            cmd_id,
            flags: 0,
            req_id: 0,
            payload: bytes::Bytes::from(payload),
        };

        let result = tokio::time::timeout(RPC_TIMEOUT, async {
            stream.write_all(&frame.encode()).await?;
            stream.flush().await?;
            let resp_frame = read_frame(&mut stream).await?;
            if resp_frame.cmd_id == CmdId::Error {
                let msg: String = rmp_serde::from_slice(&resp_frame.payload)
                    .unwrap_or_else(|_| "unknown error".into());
                anyhow::bail!("peer error: {msg}");
            }
            let resp: Resp = rmp_serde::from_slice(&resp_frame.payload)
                .context("deserialize Raft response")?;
            Ok(resp)
        }).await;

        match result {
            Ok(Ok(resp)) => {
                self.put_conn(stream);
                Ok(resp)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("Raft RPC timeout to {}", self.addr),
        }
    }
}

fn network_err<E: std::fmt::Display>(e: E) -> RPCError<MnemeNodeId, BasicNode, RaftError<MnemeNodeId>> {
    RPCError::Network(NetworkError::new(&std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        e.to_string(),
    )))
}

fn network_err_snap<E: std::fmt::Display>(e: E) -> RPCError<MnemeNodeId, BasicNode, RaftError<MnemeNodeId, InstallSnapshotError>> {
    RPCError::Network(NetworkError::new(&std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        e.to_string(),
    )))
}

impl RaftNetwork<TypeConfig> for RaftNetworkConn {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        AppendEntriesResponse<MnemeNodeId>,
        RPCError<MnemeNodeId, BasicNode, RaftError<MnemeNodeId>>,
    > {
        self.rpc(CmdId::RaftAppendEntries, &rpc)
            .await
            .map_err(|e| network_err(e))
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        InstallSnapshotResponse<MnemeNodeId>,
        RPCError<MnemeNodeId, BasicNode, RaftError<MnemeNodeId, InstallSnapshotError>>,
    > {
        self.rpc(CmdId::RaftInstallSnapshot, &rpc)
            .await
            .map_err(|e| network_err_snap(e))
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<MnemeNodeId>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        VoteResponse<MnemeNodeId>,
        RPCError<MnemeNodeId, BasicNode, RaftError<MnemeNodeId>>,
    > {
        self.rpc(CmdId::RaftVote, &rpc)
            .await
            .map_err(|e| network_err(e))
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

async fn dial_peer(
    addr: &str,
    aegis: &Aegis,
    server_name_str: &str,
) -> Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    tcp.set_nodelay(true)?;
    let connector = TlsConnector::from(aegis.client_config());
    let server_name = ServerName::try_from(server_name_str.to_string())
        .map_err(|_| anyhow::anyhow!("invalid tls.server_name '{server_name_str}'"))?
        .to_owned();
    let tls = connector.connect(server_name, tcp)
        .await
        .with_context(|| format!("mTLS to peer {addr}"))?;
    Ok(tls)
}

async fn read_frame(stream: &mut TlsStream<TcpStream>) -> Result<Frame> {
    let mut buf = BytesMut::with_capacity(4096);
    loop {
        if buf.len() >= HEADER_LEN {
            let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
            if buf.len() >= HEADER_LEN + plen {
                let (frame, _) = Frame::decode(&buf)?;
                return Ok(frame);
            }
        }
        if stream.read_buf(&mut buf).await? == 0 {
            anyhow::bail!("peer closed connection");
        }
    }
}
