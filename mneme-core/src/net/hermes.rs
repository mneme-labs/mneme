// Hermes — Replication fabric.
// ACK fix: accepts both Frame::ok_response(Bytes::new()) and AckPayload.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::BytesMut;
use mneme_common::{AckPayload, CmdId, Frame};
use parking_lot::RwLock;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{self, timeout};
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tracing::{info, warn};

use crate::core::moirai::KeeperHandle;
use crate::net::aegis::Aegis;

const FRAME_HEADER: usize = mneme_common::HEADER_LEN; // 16B: magic+ver+cmd+flags+plen+req_id
const CHANNEL_CAP: usize = 256;
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const ACK_TIMEOUT: Duration = Duration::from_millis(2000);

pub struct Hermes {
    aegis: Arc<Aegis>,
    /// TLS SNI name sent to keeper during the mTLS handshake.
    /// Taken from config.tls.server_name — never hardcoded.
    tls_server_name: String,
    handles: Arc<RwLock<HashMap<u64, KeeperHandle>>>,
}

impl Hermes {
    pub fn new(aegis: Arc<Aegis>, tls_server_name: String) -> Self {
        Self {
            aegis,
            tls_server_name,
            handles: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn connect_to_keeper(&self, node_id: u64, addr: String) -> KeeperHandle {
        let (tx, rx) = mpsc::channel::<(Frame, mpsc::Sender<Result<()>>)>(CHANNEL_CAP);
        let aegis           = self.aegis.clone();
        let server_name_str = self.tls_server_name.clone();
        tokio::spawn(keeper_conn_loop(node_id, addr.clone(), rx, aegis, server_name_str));
        let handle = KeeperHandle { node_id, tx };
        self.handles.write().insert(node_id, handle.clone());
        info!(node_id, %addr, "Hermes: keeper scheduled");
        handle
    }

    pub fn handles(&self) -> Vec<KeeperHandle> {
        self.handles.read().values().cloned().collect()
    }

    pub fn remove_keeper(&self, node_id: u64) {
        self.handles.write().remove(&node_id);
    }

    /// Graceful shutdown: drop all keeper handles.
    /// This closes the mpsc senders, causing each keeper_conn_loop to exit.
    pub fn shutdown(&self) {
        let mut handles = self.handles.write();
        let count = handles.len();
        handles.clear();
        info!(count, "Hermes: shutdown — all keeper handles dropped");
    }
}

async fn keeper_conn_loop(
    node_id: u64,
    addr: String,
    mut rx: mpsc::Receiver<(Frame, mpsc::Sender<Result<()>>)>,
    aegis: Arc<Aegis>,
    server_name_str: String,
) {
    loop {
        match dial_keeper(&addr, &aegis, &server_name_str).await {
            Ok(mut stream) => {
                info!(node_id, %addr, "Hermes: connected");
                loop {
                    match rx.recv().await {
                        None => return,
                        Some((frame, ack_tx)) => {
                            let result = send_and_ack(&mut stream, &frame).await;
                            if result.is_err() {
                                // Connection likely broken; surface the error and
                                // break inner loop so we reconnect.
                                let _ = ack_tx.try_send(result);
                                break;
                            }
                            let _ = ack_tx.try_send(result);
                        }
                    }
                }
                warn!(node_id, %addr, "Hermes: lost connection — reconnecting");
            }
            Err(e) => {
                warn!(node_id, %addr, "Hermes: {e:#} — retry in {RECONNECT_DELAY:?}");
                time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

/// Establish an mTLS connection to a keeper's replication port.
/// The SNI server name comes from `server_name_str` (config.tls.server_name),
/// never hardcoded, so it matches whatever SAN is in the keeper's certificate.
async fn dial_keeper(
    addr: &str,
    aegis: &Aegis,
    server_name_str: &str,
) -> Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    tcp.set_nodelay(true)?;
    let connector = TlsConnector::from(aegis.client_config());
    // Use the configured server_name — never hardcode "mneme.local".
    let server_name = ServerName::try_from(server_name_str.to_string())
        .map_err(|_| anyhow::anyhow!("invalid tls.server_name '{server_name_str}'"))?
        .to_owned();
    let tls = connector.connect(server_name, tcp)
        .await
        .with_context(|| format!("mTLS to {addr}"))?;
    Ok(tls)
}

// ── ACK — tolerates Ok frame OR AckPayload ────────────────────────────────────

async fn send_and_ack(stream: &mut TlsStream<TcpStream>, frame: &Frame) -> Result<()> {
    stream.write_all(&frame.encode()).await?;
    stream.flush().await?;

    let mut buf = BytesMut::with_capacity(256);
    let resp = timeout(ACK_TIMEOUT, async {
        loop {
            if buf.len() >= FRAME_HEADER {
                let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
                if buf.len() >= FRAME_HEADER + plen {
                    return Frame::decode(&buf)
                        .map(|(f, _)| f)
                        .map_err(|e| anyhow::anyhow!("{e}"));
                }
            }
            if stream.read_buf(&mut buf).await? == 0 {
                anyhow::bail!("keeper closed connection");
            }
        }
    })
        .await
        .context("ACK timeout")??;

    match resp.cmd_id {
        CmdId::Ok => Ok(()),
        CmdId::AckWrite => {
            if resp.payload.is_empty() { return Ok(()); }
            let ack: AckPayload = rmp_serde::from_slice(&resp.payload)
                .with_context(|| "decode AckPayload")?;
            if ack.ok { Ok(()) } else { anyhow::bail!("keeper NAK seq={}", ack.seq) }
        }
        CmdId::Error => {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            anyhow::bail!("keeper error: {msg}")
        }
        other => {
            warn!(?other, "Hermes: unknown ACK type — assuming OK");
            Ok(())
        }
    }
}
