// Integration test — Solo mode end-to-end.
// Spins up Mnemosyne (solo), connects a TLS client, exercises all basic commands.

use std::time::Duration;

use bytes::Bytes;
use mneme_common::{
    CmdId, DelRequest, Frame, GetRequest, MnemeConfig, SetRequest, Value,
};
use mneme_core::net::aegis::Aegis;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsConnector;

const FRAME_HEADER: usize = mneme_common::HEADER_LEN; // 16B

// ── helpers ───────────────────────────────────────────────────────────────────

fn solo_config() -> MnemeConfig {
    let mut cfg = MnemeConfig::default();
    // Pick a random-ish high port to avoid conflicts
    cfg.node.bind = "127.0.0.1".to_string();
    cfg.node.port = 16381;
    cfg.node.rep_port = 17381;
    cfg.node.metrics_port = 19092;
    cfg.tls.auto_generate = true;
    cfg.tls.cert = "/tmp/mneme-test-solo/node.crt".into();
    cfg.tls.key = "/tmp/mneme-test-solo/node.key".into();
    cfg.tls.ca_cert = "/tmp/mneme-test-solo/ca.crt".into();
    cfg.auth.cluster_secret = "test-secret".to_string();
    cfg
}

async fn connect_client(
    aegis: &Aegis,
    addr: &str,
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let connector = TlsConnector::from(aegis.client_config());
    let server_name = rustls::pki_types::ServerName::try_from("mneme.local")
        .unwrap()
        .to_owned();

    // Retry up to 20 times (server may not be ready)
    for _ in 0..20 {
        if let Ok(tcp) = TcpStream::connect(addr).await {
            tcp.set_nodelay(true).ok();
            if let Ok(tls) = connector.connect(server_name.clone(), tcp).await {
                return tls;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("Could not connect to {addr} after retries");
}

async fn send_recv(
    stream: &mut (impl AsyncReadExt + AsyncWriteExt + Unpin),
    frame: Frame,
) -> Frame {
    stream.write_all(&frame.encode()).await.unwrap();
    stream.flush().await.unwrap();

    let mut buf = bytes::BytesMut::with_capacity(512);
    timeout(Duration::from_secs(5), async {
        loop {
            if buf.len() >= FRAME_HEADER {
                let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
                if buf.len() >= FRAME_HEADER + plen {
                    let (f, _) = Frame::decode(&buf).unwrap();
                    return f;
                }
            }
            stream.read_buf(&mut buf).await.unwrap();
        }
    })
    .await
    .expect("recv timeout")
}

fn auth_frame(token: &str) -> Frame {
    let payload = rmp_serde::to_vec(token).unwrap();
    Frame { cmd_id: CmdId::Auth, flags: 0, req_id: 0, payload: Bytes::from(payload) }
}

fn set_frame(key: &[u8], value: Value, ttl_ms: u64) -> Frame {
    let req = SetRequest { key: key.to_vec(), value, ttl_ms };
    let payload = rmp_serde::to_vec(&req).unwrap();
    Frame { cmd_id: CmdId::Set, flags: 0b0100, req_id: 0, payload: Bytes::from(payload) }
    // flags bits 3-2 = 01 = Quorum, but solo mode degrades it to EVENTUAL
}

fn get_frame(key: &[u8]) -> Frame {
    let req = GetRequest { key: key.to_vec() };
    let payload = rmp_serde::to_vec(&req).unwrap();
    Frame { cmd_id: CmdId::Get, flags: 0, req_id: 0, payload: Bytes::from(payload) }
}

fn del_frame(keys: Vec<Vec<u8>>) -> Frame {
    let req = DelRequest { keys };
    let payload = rmp_serde::to_vec(&req).unwrap();
    Frame { cmd_id: CmdId::Del, flags: 0, req_id: 0, payload: Bytes::from(payload) }
}

fn ttl_frame(key: &[u8]) -> Frame {
    let payload = rmp_serde::to_vec(&key.to_vec()).unwrap();
    Frame { cmd_id: CmdId::Ttl, flags: 0, req_id: 0, payload: Bytes::from(payload) }
}

// ── test ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn solo_mode_get_set_del_ttl() {
    rustls::crypto::ring::default_provider().install_default().ok();
    let config = solo_config();
    let aegis = Aegis::new(&config.tls).expect("Aegis init");
    let argus = mneme_core::auth::argus::Argus::new(&config.auth.cluster_secret);

    // Spawn the server
    let cfg = config.clone();
    tokio::spawn(async move {
        mneme_core::core::mnemosyne::Mnemosyne::start(cfg)
            .await
            .unwrap_or_else(|e| eprintln!("server error: {e}"));
    });

    // Give server time to start
    sleep(Duration::from_millis(200)).await;

    let mut client = connect_client(&aegis, &config.client_addr()).await;

    // ── Auth ──────────────────────────────────────────────────────────────────
    let token = argus.issue(1, 3600).unwrap();
    let resp = send_recv(&mut client, auth_frame(&token)).await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "AUTH should succeed");

    // ── SET ───────────────────────────────────────────────────────────────────
    let resp = send_recv(
        &mut client,
        set_frame(b"greeting", Value::String(b"hello".to_vec()), 0),
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "SET should succeed");

    // ── GET ───────────────────────────────────────────────────────────────────
    let resp = send_recv(&mut client, get_frame(b"greeting")).await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "GET should succeed");
    let got: Value = rmp_serde::from_slice(&resp.payload).unwrap();
    assert!(matches!(got, Value::String(ref b) if b == b"hello"));

    // ── GET missing key → Error ───────────────────────────────────────────────
    let resp = send_recv(&mut client, get_frame(b"no_such_key")).await;
    assert_eq!(resp.cmd_id, CmdId::Error, "GET missing should return Error");

    // ── SET with TTL ──────────────────────────────────────────────────────────
    let resp = send_recv(
        &mut client,
        set_frame(b"temp_key", Value::String(b"bye".to_vec()), 5000),
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);

    // ── TTL ───────────────────────────────────────────────────────────────────
    let resp = send_recv(&mut client, ttl_frame(b"temp_key")).await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let ttl_ms: i64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert!(ttl_ms > 0 && ttl_ms <= 5000, "TTL should be positive and ≤5000ms");

    // TTL for key without expiry → -1
    let resp = send_recv(&mut client, ttl_frame(b"greeting")).await;
    let ttl_ms: i64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(ttl_ms, -1);

    // ── DEL ───────────────────────────────────────────────────────────────────
    let resp = send_recv(
        &mut client,
        del_frame(vec![b"greeting".to_vec(), b"temp_key".to_vec()]),
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let deleted: u64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(deleted, 2);

    // Verify keys are gone
    let resp = send_recv(&mut client, get_frame(b"greeting")).await;
    assert_eq!(resp.cmd_id, CmdId::Error);

    // ── SLOWLOG ───────────────────────────────────────────────────────────────
    let payload = rmp_serde::to_vec(&10usize).unwrap();
    let slowlog_frame = Frame {
        cmd_id: CmdId::SlowLog,
        flags: 0,
        req_id: 0, payload: Bytes::from(payload),
    };
    let resp = send_recv(&mut client, slowlog_frame).await;
    assert_eq!(resp.cmd_id, CmdId::Ok);

    // ── METRICS ───────────────────────────────────────────────────────────────
    let metrics_frame = Frame {
        cmd_id: CmdId::Metrics,
        flags: 0,
        req_id: 0, payload: Bytes::new(),
    };
    let resp = send_recv(&mut client, metrics_frame).await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let (used, total): (u64, u64) = rmp_serde::from_slice(&resp.payload).unwrap();
    assert!(total > 0);
    assert!(used <= total);
}

#[tokio::test]
async fn noauth_command_rejected() {
    rustls::crypto::ring::default_provider().install_default().ok();
    let config = {
        let mut c = solo_config();
        c.node.port = 16382;
        c.node.rep_port = 17382;
        c.node.metrics_port = 19093;
        c
    };
    let aegis = Aegis::new(&config.tls).expect("Aegis init");

    let cfg = config.clone();
    tokio::spawn(async move {
        let _ = mneme_core::core::mnemosyne::Mnemosyne::start(cfg).await;
    });

    sleep(Duration::from_millis(200)).await;

    let mut client = connect_client(&aegis, &config.client_addr()).await;

    // Send a GET without authenticating first
    let resp = send_recv(&mut client, get_frame(b"k")).await;
    assert_eq!(resp.cmd_id, CmdId::Error, "unauthenticated command should fail");
}
