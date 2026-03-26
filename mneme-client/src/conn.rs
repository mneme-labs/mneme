// conn.rs — MnemeConn: single multiplexed TLS connection.
// req_id dispatch: parallel requests, out-of-order responses.
// Command methods live in the cmd_*.rs files.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use bytes::BytesMut;
use dashmap::DashMap;
use mneme_common::{CmdId, Frame, HEADER_LEN};
pub use mneme_common::ConsistencyLevel as Consistency;
use parking_lot::Mutex as ParkingMutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex as TokioMutex, mpsc};
use tokio_rustls::client::TlsStream;

// ── Core struct ────────────────────────────────────────────────────────────────

pub struct MnemeConn {
    /// Pending request map: req_id → response sender.
    pub(crate) pending:    Arc<DashMap<u32, oneshot::Sender<Frame>>>,
    pub(crate) req_id:     Arc<AtomicU32>,
    pub(crate) writer:     Arc<TokioMutex<tokio::io::WriteHalf<TlsStream<TcpStream>>>>,
    /// Active MONITOR subscription sender. Set by `monitor()`, cleared on drop.
    pub(crate) monitor_tx: Arc<ParkingMutex<Option<mpsc::Sender<String>>>>,
}

impl MnemeConn {
    /// Wrap an established TLS stream. Spawns a background reader task.
    pub fn new(stream: TlsStream<TcpStream>) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        let pending: Arc<DashMap<u32, oneshot::Sender<Frame>>> = Arc::new(DashMap::new());
        let pending2    = pending.clone();
        let monitor_tx  = Arc::new(ParkingMutex::new(None::<mpsc::Sender<String>>));
        let monitor_tx2 = monitor_tx.clone();
        tokio::spawn(recv_loop(reader, pending2, monitor_tx2));
        Self {
            pending,
            req_id: Arc::new(AtomicU32::new(1)),
            writer: Arc::new(TokioMutex::new(writer)),
            monitor_tx,
        }
    }

    /// Send a command and wait for its response (non-blocking to other requests).
    pub async fn send(&self, cmd: CmdId, payload: Bytes, consistency: Consistency) -> Result<Frame> {
        // Allocate a unique req_id, skipping 0 (reserved for single-plex)
        // and any id that is still in the pending map (collision after u32 wrap).
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
        {
            let mut w = self.writer.lock().await;
            w.write_all(&frame.encode()).await.context("write frame")?;
        }
        rx.await.context("response channel closed")
    }

    // ── Auth ──────────────────────────────────────────────────────────────────

    /// Authenticate with a pre-issued HMAC session token.
    pub async fn auth_token(&self, token: &str) -> Result<()> {
        let payload = Bytes::from(rmp_serde::to_vec(token)?);
        let resp = self.send(CmdId::Auth, payload, Consistency::Eventual).await?;
        if resp.cmd_id != CmdId::Ok {
            let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            bail!("AUTH failed: {msg}");
        }
        Ok(())
    }

    /// Revoke the current session token (immediate JTI blocklist insertion).
    pub async fn revoke_token(&self) -> Result<()> {
        let payload = Bytes::from(rmp_serde::to_vec(&())?);
        let resp = self.send(CmdId::RevokeToken, payload, Consistency::Eventual).await?;
        check_ok(&resp)
    }

    /// PING / health check — sends STATS with empty payload.
    pub async fn ping(&self) -> Result<()> {
        let payload = Bytes::from(rmp_serde::to_vec(&())?);
        let resp = self.send(CmdId::Stats, payload, Consistency::Eventual).await?;
        if resp.cmd_id == CmdId::Ok { Ok(()) } else { bail!("ping failed") }
    }
}

// ── helpers shared across cmd_*.rs files ──────────────────────────────────────

/// Return `()` if the frame is Ok, else bail with the error message.
pub(crate) fn check_ok(resp: &Frame) -> Result<()> {
    if resp.cmd_id == CmdId::Ok { return Ok(()); }
    let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_else(|_| "server error".into());
    bail!("{msg}")
}

/// Return the error message as an `Err`, or `Ok(None)` for KeyNotFound errors.
#[allow(dead_code)]
pub(crate) fn check_ok_or_none(resp: &Frame) -> Result<Option<()>> {
    match resp.cmd_id {
        CmdId::Ok => Ok(Some(())),
        CmdId::Error => {
            let msg: String =
                rmp_serde::from_slice(&resp.payload).unwrap_or_else(|_| "server error".into());
            if msg.contains("KeyNotFound") || msg.contains("not found") {
                Ok(None)
            } else {
                bail!("{msg}")
            }
        }
        _ => bail!("unexpected response cmd: {:?}", resp.cmd_id),
    }
}

pub(crate) fn consistency_flags(c: Consistency) -> u16 {
    let bits: u16 = match c {
        Consistency::Eventual => 0b00,
        Consistency::Quorum   => 0b01,
        Consistency::All      => 0b10,
        Consistency::One      => 0b11,
    };
    bits << 2
}

// ── background recv loop ──────────────────────────────────────────────────────

async fn recv_loop(
    mut reader: tokio::io::ReadHalf<TlsStream<TcpStream>>,
    pending:    Arc<DashMap<u32, oneshot::Sender<Frame>>>,
    monitor_tx: Arc<ParkingMutex<Option<mpsc::Sender<String>>>>,
) {
    let mut buf = BytesMut::with_capacity(4096);
    loop {
        loop {
            if buf.len() >= HEADER_LEN {
                let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
                if buf.len() >= HEADER_LEN + plen { break; }
            }
            match reader.read_buf(&mut buf).await {
                Ok(0) | Err(_) => {
                    // Connection closed — wake all pending waiters.
                    let keys: Vec<u32> = pending.iter().map(|e| *e.key()).collect();
                    for id in keys {
                        if let Some((_, tx)) = pending.remove(&id) {
                            let _ = tx.send(Frame::error_response("connection closed"));
                        }
                    }
                    return;
                }
                Ok(_) => {}
            }
        }
        let (frame, consumed) = match Frame::decode(&buf) {
            Ok(r)  => r,
            Err(_) => return,
        };
        let _ = buf.split_to(consumed);

        // req_id=0 with cmd_id=Ok/Monitor → server-push (MONITOR stream event)
        if frame.req_id == 0 && matches!(frame.cmd_id, CmdId::Ok | CmdId::Monitor) {
            let msg: String = rmp_serde::from_slice(&frame.payload).unwrap_or_default();
            if let Some(tx) = monitor_tx.lock().as_ref() {
                let _ = tx.try_send(msg);
            }
            continue;
        }

        if let Some((_, tx)) = pending.remove(&frame.req_id) {
            let _ = tx.send(frame);
        }
    }
}
