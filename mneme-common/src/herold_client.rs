// herold_client.rs — Client-side Herold registration, shared between mneme-keeper
// and any future node type that needs to register with the Core.
//
// A-03 sub-problem 1 fix: extracted from mneme-core/src/cluster/herold.rs so
// that mneme-keeper (a separate binary crate) can call it without depending on
// mneme-core. Uses plain TLS (CA-cert-only verification, no client cert). Auth
// is carried inside the RegisterPayload via the join_token field.
//
// Usage:
//   let ack = mneme_common::herold_client::register_with_core(
//       &config.node.core_addr,
//       RegisterPayload { node_id: config.node.node_id.clone(), role: "keeper".into(),
//                         grant_bytes: 0, replication_addr: config.replication_addr(),
//                         join_token: config.node.join_token.clone() },
//       &config.tls,
//   ).await?;

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use bytes::{Bytes, BytesMut};
use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::info;

use crate::config::TlsConfig;
use crate::frame::{CmdId, Frame, RegisterAck, RegisterPayload, HEADER_LEN, REGISTER_FLAGS};

/// Connect to the Core's replication port and execute the one-shot Herold REGISTER
/// handshake.  Returns the assigned node ID on success.
///
/// The connection is used only for this handshake and is closed afterward.
/// The caller should then establish a separate connection to drive the SyncStart
/// warm-up and ongoing replication (see `run_replication_client` in hypnos.rs).
pub async fn register_with_core(
    core_addr: &str,
    payload: RegisterPayload,
    tls_config: &TlsConfig,
) -> Result<RegisterAck> {
    // Clone all borrowed data into owned Strings upfront so that no closures below
    // (including those passed to with_context / map_err) need to capture references
    // that might not satisfy the 'static bound required by tokio::spawn.
    let core_addr  = core_addr.to_owned();
    let ca_cert    = tls_config.ca_cert.clone();
    let server_name_str = tls_config.server_name.clone();

    // Build plain TLS client config — CA cert only, no client certificate.
    // Auth is via join_token inside the RegisterPayload.
    let ca_pem = std::fs::read(&ca_cert)
        .with_context(|| format!("herold_client: read CA cert '{ca_cert}'"))?;
    let ca_certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut ca_pem.as_slice())
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("herold_client: parse CA cert")?;

    let mut root_store = RootCertStore::empty();
    for ca in ca_certs {
        root_store.add(ca).context("herold_client: add CA cert to root store")?;
    }

    let client_cfg = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let connector = TlsConnector::from(Arc::new(client_cfg));

    let tcp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        TcpStream::connect(&core_addr),
    )
        .await
        .with_context(|| format!("herold_client: connect timeout to {core_addr}"))?
        .with_context(|| format!("herold_client: TCP connect to {core_addr}"))?;
    tcp.set_nodelay(true)?;

    // TLS server name comes from config (A-03 sub-problem 2 fix: never hardcoded).
    let server_name = ServerName::try_from(server_name_str.as_str())
        .map_err(|e| anyhow::anyhow!("herold_client: invalid server_name '{server_name_str}': {e}"))?
        .to_owned();

    let mut stream = connector
        .connect(server_name, tcp)
        .await
        .with_context(|| format!("herold_client: TLS handshake with {core_addr}"))?;

    // Encode and send REGISTER frame: SyncStart with flags=REGISTER_FLAGS.
    let frame_payload = rmp_serde::to_vec(&payload)
        .context("herold_client: serialize RegisterPayload")?;
    let frame = Frame {
        cmd_id: CmdId::SyncStart,
        flags:  REGISTER_FLAGS,
        req_id: 0,
        payload: Bytes::from(frame_payload),
    };
    stream.write_all(&frame.encode()).await?;
    stream.flush().await?;

    // Read the RegisterAck response frame.
    let mut buf = BytesMut::with_capacity(512);
    loop {
        let n = stream.read_buf(&mut buf).await?;
        if n == 0 {
            bail!("herold_client: Core closed connection before sending RegisterAck");
        }
        if buf.len() >= HEADER_LEN {
            let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
            if buf.len() >= HEADER_LEN + plen {
                break;
            }
        }
    }

    let (resp, _) = Frame::decode(&buf).map_err(|e| anyhow::anyhow!("{e}"))?;
    let ack: RegisterAck = rmp_serde::from_slice(&resp.payload)
        .context("herold_client: decode RegisterAck")?;

    if !ack.accepted {
        bail!("herold_client: Core rejected registration — {}", ack.message);
    }

    info!(
        assigned_id = ack.assigned_id,
        core = %core_addr,
        "Herold client: registered with Core"
    );
    Ok(ack)
}
