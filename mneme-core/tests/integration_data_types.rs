// Integration tests — data type commands: Hash, List, ZSet, Counter, JSON.
// Each test spins up a solo Mnemosyne on its own port.

use std::time::Duration;

use bytes::Bytes;
use mneme_common::{
    CmdId, DelRequest, Frame, GetRequest, HDelRequest, HGetRequest, HSetRequest,
    IncrByFloatRequest, IncrByRequest, JsonArrAppendRequest, JsonDelRequest,
    JsonGetRequest, JsonNumIncrByRequest, JsonSetRequest, LRangeRequest, ListPushRequest,
    MnemeConfig, SetRequest, Value, ZAddRequest, ZRangeRequest, ZRankRequest, ZRemRequest,
    ZSetMember,
};
use mneme_core::net::aegis::Aegis;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsConnector;

const FRAME_HEADER: usize = mneme_common::HEADER_LEN; // 16B

// ── helpers ───────────────────────────────────────────────────────────────────

fn test_config(port: u16) -> MnemeConfig {
    let mut cfg = MnemeConfig::default();
    cfg.node.bind = "127.0.0.1".to_string();
    cfg.node.port = port;
    cfg.node.rep_port = port + 1000;
    cfg.node.metrics_port = port + 2000;
    cfg.tls.auto_generate = true;
    cfg.tls.cert = format!("/tmp/mneme-test-{port}/node.crt");
    cfg.tls.key = format!("/tmp/mneme-test-{port}/node.key");
    cfg.tls.ca_cert = format!("/tmp/mneme-test-{port}/ca.crt");
    cfg.auth.cluster_secret = "test-secret-dt".to_string();
    cfg.persistence.wal_dir = format!("/tmp/mneme-test-{port}");
    cfg
}

async fn start_server(port: u16) -> MnemeConfig {
    rustls::crypto::ring::default_provider().install_default().ok();
    let config = test_config(port);
    let cfg = config.clone();
    tokio::spawn(async move {
        mneme_core::core::mnemosyne::Mnemosyne::start(cfg)
            .await
            .unwrap_or_else(|e| eprintln!("server error: {e}"));
    });
    sleep(Duration::from_millis(250)).await;
    config
}

async fn connect(aegis: &Aegis, addr: &str) -> tokio_rustls::client::TlsStream<TcpStream> {
    let connector = TlsConnector::from(aegis.client_config());
    let server_name = rustls::pki_types::ServerName::try_from("mneme.local")
        .unwrap()
        .to_owned();
    for _ in 0..20 {
        if let Ok(tcp) = TcpStream::connect(addr).await {
            tcp.set_nodelay(true).ok();
            if let Ok(tls) = connector.connect(server_name.clone(), tcp).await {
                return tls;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("Could not connect to {addr}");
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

async fn auth(stream: &mut (impl AsyncReadExt + AsyncWriteExt + Unpin), secret: &str) {
    let argus = mneme_core::auth::argus::Argus::new(secret);
    let token = argus.issue(1, 3600).unwrap();
    let payload = rmp_serde::to_vec(&token).unwrap();
    let frame = Frame { cmd_id: CmdId::Auth, flags: 0, req_id: 0, payload: Bytes::from(payload) };
    let resp = send_recv(stream, frame).await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "AUTH failed");
}

fn frame_set(key: &[u8], value: Value, ttl_ms: u64) -> Frame {
    let payload = rmp_serde::to_vec(&SetRequest { key: key.to_vec(), value, ttl_ms }).unwrap();
    Frame { cmd_id: CmdId::Set, flags: 0, req_id: 0, payload: Bytes::from(payload) }
}

fn frame_get(key: &[u8]) -> Frame {
    let payload = rmp_serde::to_vec(&GetRequest { key: key.to_vec() }).unwrap();
    Frame { cmd_id: CmdId::Get, flags: 0, req_id: 0, payload: Bytes::from(payload) }
}

fn frame_del(keys: Vec<Vec<u8>>) -> Frame {
    let payload = rmp_serde::to_vec(&DelRequest { keys }).unwrap();
    Frame { cmd_id: CmdId::Del, flags: 0, req_id: 0, payload: Bytes::from(payload) }
}

// ── Hash tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn hash_hset_hget_hdel_hgetall() {
    let config = start_server(16400).await;
    let aegis = Aegis::new(&config.tls).unwrap();
    let mut client = connect(&aegis, &config.client_addr()).await;
    auth(&mut client, &config.auth.cluster_secret).await;

    // HSET: set multiple fields at once
    let payload = rmp_serde::to_vec(&HSetRequest {
        key: b"user:1".to_vec(),
        pairs: vec![
            (b"name".to_vec(), b"alice".to_vec()),
            (b"age".to_vec(), b"30".to_vec()),
            (b"city".to_vec(), b"london".to_vec()),
        ],
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::HSet, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "HSET should succeed");
    let added: u64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(added, 3, "HSET should report 3 new fields");

    // HGET existing field
    let payload = rmp_serde::to_vec(&HGetRequest {
        key: b"user:1".to_vec(),
        field: b"name".to_vec(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::HGet, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "HGET should succeed");
    let val: Vec<u8> = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(val, b"alice");

    // HGET missing field → Error
    let payload = rmp_serde::to_vec(&HGetRequest {
        key: b"user:1".to_vec(),
        field: b"email".to_vec(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::HGet, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Error, "HGET missing field should return Error");

    // HGETALL
    let payload = rmp_serde::to_vec(&b"user:1".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::HGetAll, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "HGETALL should succeed");
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(pairs.len(), 3, "HGETALL should return 3 fields");

    // HDEL one field
    let payload = rmp_serde::to_vec(&HDelRequest {
        key: b"user:1".to_vec(),
        fields: vec![b"city".to_vec()],
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::HDel, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "HDEL should succeed");
    let removed: u64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(removed, 1);

    // HGETALL should now have 2 fields
    let payload = rmp_serde::to_vec(&b"user:1".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::HGetAll, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(pairs.len(), 2);

    // Wrong type: HGET on a String key → Error
    let _ = send_recv(&mut client, frame_set(b"str_key", Value::String(b"v".to_vec()), 0)).await;
    let payload = rmp_serde::to_vec(&HGetRequest {
        key: b"str_key".to_vec(),
        field: b"f".to_vec(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::HGet, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Error, "HGET on non-hash should return Error");
}

// ── List tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_lpush_rpush_lpop_rpop_lrange() {
    let config = start_server(16410).await;
    let aegis = Aegis::new(&config.tls).unwrap();
    let mut client = connect(&aegis, &config.client_addr()).await;
    auth(&mut client, &config.auth.cluster_secret).await;

    // LPUSH a, b, c → list is [c, b, a]
    let payload = rmp_serde::to_vec(&ListPushRequest {
        key: b"mylist".to_vec(),
        values: vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::LPush, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "LPUSH should succeed");
    let len: u64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(len, 3);

    // RPUSH d → list is [c, b, a, d]
    let payload = rmp_serde::to_vec(&ListPushRequest {
        key: b"mylist".to_vec(),
        values: vec![b"d".to_vec()],
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::RPush, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let len: u64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(len, 4);

    // LRANGE 0 -1 (full list)
    let payload = rmp_serde::to_vec(&LRangeRequest {
        key: b"mylist".to_vec(),
        start: 0,
        stop: -1,
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::LRange, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "LRANGE should succeed");
    let items: Vec<Vec<u8>> = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(items.len(), 4);
    assert_eq!(items[0], b"c");
    assert_eq!(items[3], b"d");

    // LRANGE 1 2 (slice)
    let payload = rmp_serde::to_vec(&LRangeRequest {
        key: b"mylist".to_vec(),
        start: 1,
        stop: 2,
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::LRange, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    let items: Vec<Vec<u8>> = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(items, vec![b"b".to_vec(), b"a".to_vec()]);

    // LPOP → "c"
    let payload = rmp_serde::to_vec(&b"mylist".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::LPop, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let popped: Vec<u8> = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(popped, b"c");

    // RPOP → "d"
    let payload = rmp_serde::to_vec(&b"mylist".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::RPop, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let popped: Vec<u8> = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(popped, b"d");

    // LPOP on empty list after DEL → Error
    let _ = send_recv(&mut client, frame_del(vec![b"mylist".to_vec()])).await;
    let payload = rmp_serde::to_vec(&b"mylist".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::LPop, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Error, "LPOP on missing key should return Error");
}

// ── Sorted Set tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn zset_zadd_zrank_zrange_zrem_zcard() {
    let config = start_server(16420).await;
    let aegis = Aegis::new(&config.tls).unwrap();
    let mut client = connect(&aegis, &config.client_addr()).await;
    auth(&mut client, &config.auth.cluster_secret).await;

    // ZADD three members
    let payload = rmp_serde::to_vec(&ZAddRequest {
        key: b"leaderboard".to_vec(),
        members: vec![
            ZSetMember::new(1000.0, b"alice"),
            ZSetMember::new(850.0, b"bob"),
            ZSetMember::new(1200.0, b"carol"),
        ],
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::ZAdd, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "ZADD should succeed");
    let added: u64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(added, 3);

    // ZRANK (alice) — 0-indexed ascending by score: bob(0) alice(1) carol(2)
    let payload = rmp_serde::to_vec(&ZRankRequest {
        key: b"leaderboard".to_vec(),
        member: b"alice".to_vec(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::ZRank, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let rank: u64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(rank, 1, "alice should be rank 1 (0-indexed)");

    // ZRANK missing member → Error
    let payload = rmp_serde::to_vec(&ZRankRequest {
        key: b"leaderboard".to_vec(),
        member: b"dave".to_vec(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::ZRank, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Error, "ZRANK of non-member should return Error");

    // ZRANGE 0 2 (all members, ascending score)
    let payload = rmp_serde::to_vec(&ZRangeRequest {
        key: b"leaderboard".to_vec(),
        start: 0,
        stop: 2,
        with_scores: false,
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::ZRange, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "ZRANGE should succeed");
    let members: Vec<Vec<u8>> = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(members.len(), 3);
    assert_eq!(members[0], b"bob");
    assert_eq!(members[2], b"carol");

    // ZADD update existing member (carol score → 500)
    let payload = rmp_serde::to_vec(&ZAddRequest {
        key: b"leaderboard".to_vec(),
        members: vec![ZSetMember::new(500.0, b"carol")],
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::ZAdd, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let added: u64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(added, 0, "updating existing member adds 0 new entries");

    // ZREM alice
    let payload = rmp_serde::to_vec(&ZRemRequest {
        key: b"leaderboard".to_vec(),
        members: vec![b"alice".to_vec()],
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::ZRem, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let removed: u64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(removed, 1);

    // ZCARD → 2 remaining
    let payload = rmp_serde::to_vec(&b"leaderboard".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::ZCard, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let card: u64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(card, 2);
}

// ── Counter tests ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn counter_incr_decr_incrby_decrby_getset() {
    let config = start_server(16430).await;
    let aegis = Aegis::new(&config.tls).unwrap();
    let mut client = connect(&aegis, &config.client_addr()).await;
    auth(&mut client, &config.auth.cluster_secret).await;

    // SET a Counter initial value
    let resp = send_recv(
        &mut client,
        frame_set(b"hits", Value::Counter(0), 0),
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "SET Counter should succeed");

    // INCR → 1
    let payload = rmp_serde::to_vec(&b"hits".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::Incr, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "INCR should succeed");
    let val: i64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(val, 1);

    // INCR again → 2
    let payload = rmp_serde::to_vec(&b"hits".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::Incr, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    let val: i64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(val, 2);

    // DECR → 1
    let payload = rmp_serde::to_vec(&b"hits".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::Decr, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    let val: i64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(val, 1);

    // INCRBY 100 → 101
    let payload = rmp_serde::to_vec(&IncrByRequest {
        key: b"hits".to_vec(),
        delta: 100,
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::IncrBy, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "INCRBY should succeed");
    let val: i64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(val, 101);

    // DECRBY 50 → 51
    let payload = rmp_serde::to_vec(&IncrByRequest {
        key: b"hits".to_vec(),
        delta: 50,
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::DecrBy, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    let val: i64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(val, 51);

    // INCRBYFLOAT 0.5 → 51.5 (stored as string representation)
    let payload = rmp_serde::to_vec(&IncrByFloatRequest {
        key: b"float_ctr".to_vec(),
        delta: 1.5,
    })
    .unwrap();
    // First SET a string "10"
    let _ = send_recv(
        &mut client,
        frame_set(b"float_ctr", Value::String(b"10".to_vec()), 0),
    )
    .await;
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::IncrByFloat, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "INCRBYFLOAT should succeed");

    // INCR auto-creates Counter(0) if key missing
    let _ = send_recv(&mut client, frame_del(vec![b"new_ctr".to_vec()])).await;
    let payload = rmp_serde::to_vec(&b"new_ctr".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::Incr, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "INCR on missing key should auto-create counter");
    let val: i64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(val, 1, "INCR on new key should return 1");

    // INCR on wrong type → Error
    let _ = send_recv(
        &mut client,
        frame_set(b"wrong_type", Value::Hash(vec![]), 0),
    )
    .await;
    let payload = rmp_serde::to_vec(&b"wrong_type".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::Incr, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Error, "INCR on non-counter/string should fail");
}

// ── JSON tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn json_set_get_del_exists_type_arrapend_numincrby() {
    let config = start_server(16440).await;
    let aegis = Aegis::new(&config.tls).unwrap();
    let mut client = connect(&aegis, &config.client_addr()).await;
    auth(&mut client, &config.auth.cluster_secret).await;

    // JSON.SET root
    let payload = rmp_serde::to_vec(&JsonSetRequest {
        key: b"doc".to_vec(),
        path: "$".to_string(),
        value: r#"{"name":"alice","score":42,"tags":["rust","cache"]}"#.to_string(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonSet, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "JSON.SET should succeed");

    // JSON.GET root
    let payload = rmp_serde::to_vec(&JsonGetRequest {
        key: b"doc".to_vec(),
        path: "$".to_string(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonGet, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "JSON.GET root should succeed");
    let raw: String = rmp_serde::from_slice(&resp.payload).unwrap();
    assert!(raw.contains("alice"), "JSON.GET root should contain name field");

    // JSON.GET field path
    let payload = rmp_serde::to_vec(&JsonGetRequest {
        key: b"doc".to_vec(),
        path: "$.name".to_string(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonGet, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "JSON.GET field should succeed");
    let val: String = rmp_serde::from_slice(&resp.payload).unwrap();
    assert!(val.contains("alice"));

    // JSON.GET missing path → Error
    let payload = rmp_serde::to_vec(&JsonGetRequest {
        key: b"doc".to_vec(),
        path: "$.missing".to_string(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonGet, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Error, "JSON.GET missing path should return Error");

    // JSON.SET field
    let payload = rmp_serde::to_vec(&JsonSetRequest {
        key: b"doc".to_vec(),
        path: "$.name".to_string(),
        value: r#""bob""#.to_string(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonSet, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "JSON.SET field should succeed");

    // Verify update
    let payload = rmp_serde::to_vec(&JsonGetRequest {
        key: b"doc".to_vec(),
        path: "$.name".to_string(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonGet, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    let val: String = rmp_serde::from_slice(&resp.payload).unwrap();
    assert!(val.contains("bob"), "name should be updated to bob");

    // JSON.DEL field
    let payload = rmp_serde::to_vec(&JsonDelRequest {
        key: b"doc".to_vec(),
        path: "$.name".to_string(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonDel, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "JSON.DEL should succeed");

    // JSON.EXISTS after del → false
    let payload = rmp_serde::to_vec(&JsonGetRequest {
        key: b"doc".to_vec(),
        path: "$.name".to_string(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonExists, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let exists: bool = rmp_serde::from_slice(&resp.payload).unwrap();
    assert!(!exists, "$.name should not exist after JSON.DEL");

    // JSON.EXISTS existing path → true
    let payload = rmp_serde::to_vec(&JsonGetRequest {
        key: b"doc".to_vec(),
        path: "$.score".to_string(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonExists, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    let exists: bool = rmp_serde::from_slice(&resp.payload).unwrap();
    assert!(exists, "$.score should still exist");

    // JSON.NUMINCRBY
    let payload = rmp_serde::to_vec(&JsonNumIncrByRequest {
        key: b"doc".to_vec(),
        path: "$.score".to_string(),
        delta: 8.0,
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonNumIncrBy, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "JSON.NUMINCRBY should succeed");
    let new_val: f64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert!((new_val - 50.0).abs() < 0.001, "score should be 50 after +8");

    // JSON.ARRAPPEND
    let payload = rmp_serde::to_vec(&JsonArrAppendRequest {
        key: b"doc".to_vec(),
        path: "$.tags".to_string(),
        value: r#""distributed""#.to_string(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonArrAppend, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "JSON.ARRAPPEND should succeed");

    // JSON.TYPE at various paths
    let payload = rmp_serde::to_vec(&JsonGetRequest {
        key: b"doc".to_vec(),
        path: "$.score".to_string(),
    })
    .unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::JsonType, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok, "JSON.TYPE should succeed");
    let type_name: String = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(type_name, "number");
}

// ── GET/SET on a JSON value (via frame_set) ───────────────────────────────────

#[tokio::test]
async fn json_value_stored_via_set_get() {
    let config = start_server(16450).await;
    let aegis = Aegis::new(&config.tls).unwrap();
    let mut client = connect(&aegis, &config.client_addr()).await;
    auth(&mut client, &config.auth.cluster_secret).await;

    let doc = mneme_common::JsonDoc { raw: r#"{"x":1}"#.into() };
    let resp = send_recv(
        &mut client,
        frame_set(b"jdoc", Value::Json(doc.clone()), 0),
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);

    let resp = send_recv(&mut client, frame_get(b"jdoc")).await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let got: Value = rmp_serde::from_slice(&resp.payload).unwrap();
    assert!(matches!(got, Value::Json(_)), "should retrieve a Json value");
}

// ── Expiry applies to all types ───────────────────────────────────────────────

#[tokio::test]
async fn ttl_applies_to_counter_and_hash() {
    let config = start_server(16460).await;
    let aegis = Aegis::new(&config.tls).unwrap();
    let mut client = connect(&aegis, &config.client_addr()).await;
    auth(&mut client, &config.auth.cluster_secret).await;

    // Counter with TTL
    let resp = send_recv(
        &mut client,
        frame_set(b"expiring_ctr", Value::Counter(99), 200), // 200 ms TTL
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);

    // Immediately readable
    let payload = rmp_serde::to_vec(&b"expiring_ctr".to_vec()).unwrap();
    let resp = send_recv(
        &mut client,
        Frame { cmd_id: CmdId::Incr, flags: 0, req_id: 0, payload: Bytes::from(payload) },
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);
    let val: i64 = rmp_serde::from_slice(&resp.payload).unwrap();
    assert_eq!(val, 100);

    // Wait for expiry
    sleep(Duration::from_millis(350)).await;

    // Should be gone (GET returns Error)
    let resp = send_recv(&mut client, frame_get(b"expiring_ctr")).await;
    assert_eq!(resp.cmd_id, CmdId::Error, "expired counter should not be found");

    // Hash with TTL
    let resp = send_recv(
        &mut client,
        frame_set(b"expiring_hash", Value::Hash(vec![(b"f".to_vec(), b"v".to_vec())]), 200),
    )
    .await;
    assert_eq!(resp.cmd_id, CmdId::Ok);

    sleep(Duration::from_millis(350)).await;
    let resp = send_recv(&mut client, frame_get(b"expiring_hash")).await;
    assert_eq!(resp.cmd_id, CmdId::Error, "expired hash should not be found");
}
