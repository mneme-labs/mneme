// mneme-bench: latency benchmarks.
// Run with: cargo bench --bench latency

use std::collections::HashMap;
use std::hint::black_box;
use std::sync::Arc;

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use mneme_common::{CmdId, Frame, Value};
use mneme_core::auth::argus::Argus;
use mneme_core::core::iris::Iris;
use mneme_core::core::lethe::Lethe;
use parking_lot::RwLock;

// ── Frame encode / decode ─────────────────────────────────────────────────────

fn bench_frame_encode(c: &mut Criterion) {
    let payload = rmp_serde::to_vec(b"benchmark-key").unwrap();
    let frame = Frame {
        cmd_id: CmdId::Get,
        flags: 0,
        req_id: 0, payload: Bytes::from(payload),
    };

    c.bench_function("frame_encode", |b| {
        b.iter(|| black_box(frame.encode()))
    });
}

fn bench_frame_decode(c: &mut Criterion) {
    let payload = rmp_serde::to_vec(b"benchmark-key").unwrap();
    let frame = Frame {
        cmd_id: CmdId::Get,
        flags: 0,
        req_id: 0, payload: Bytes::from(payload),
    };
    let wire = frame.encode();

    c.bench_function("frame_decode", |b| {
        b.iter(|| black_box(Frame::decode(&wire).unwrap()))
    });
}

// ── CRC16 / slot routing ──────────────────────────────────────────────────────

fn bench_slot_for(c: &mut Criterion) {
    let keys: Vec<Vec<u8>> = (0u32..256)
        .map(|i| format!("user:session:{i}:data").into_bytes())
        .collect();

    let mut group = c.benchmark_group("slot_routing");
    group.throughput(Throughput::Elements(keys.len() as u64));

    group.bench_function("slot_for_256_keys", |b| {
        b.iter(|| {
            for key in &keys {
                black_box(Iris::slot_for(key));
            }
        })
    });
    group.finish();
}

fn bench_iris_route(c: &mut Criterion) {
    let iris = Iris::new(1);
    let key = b"hot:key:42";

    c.bench_function("iris_route_local", |b| {
        b.iter(|| black_box(iris.route(key)))
    });
}

// ── LFU counter ───────────────────────────────────────────────────────────────

fn bench_lfu_increment(c: &mut Criterion) {
    let mut group = c.benchmark_group("lfu");

    for start in [0u8, 5, 50, 100, 200, 254] {
        group.bench_with_input(
            BenchmarkId::new("increment", start),
            &start,
            |b, &counter| {
                b.iter(|| black_box(Lethe::increment_lfu(counter)))
            },
        );
    }
    group.finish();
}

fn bench_lfu_pick_candidates(c: &mut Criterion) {
    let candidates: Vec<(Vec<u8>, u8)> = (0u32..1024)
        .map(|i| (format!("key:{i}").into_bytes(), (i % 256) as u8))
        .collect();

    c.bench_function("lfu_pick_32_from_1024", |b| {
        b.iter(|| black_box(Lethe::pick_eviction_candidates(&candidates, 32)))
    });
}

// ── Argus token operations ────────────────────────────────────────────────────

fn bench_token_issue(c: &mut Criterion) {
    let argus = Argus::new(b"bench-secret-key-for-testing-1234");

    c.bench_function("argus_issue", |b| {
        b.iter(|| black_box(argus.issue(1, 3600).unwrap()))
    });
}

fn bench_token_verify(c: &mut Criterion) {
    let argus = Argus::new(b"bench-secret-key-for-testing-1234");
    let token = argus.issue(42, 3600).unwrap();

    c.bench_function("argus_verify", |b| {
        b.iter(|| black_box(argus.verify(&token).unwrap()))
    });
}

fn bench_cluster_tag(c: &mut Criterion) {
    let argus = Argus::new(b"bench-secret-key-for-testing-1234");
    let msg = b"node-heartbeat-payload-data";
    let sig = argus.sign(msg).unwrap();

    c.bench_function("argus_cluster_tag_verify", |b| {
        b.iter(|| black_box(argus.verify_cluster_tag(msg, &sig)))
    });
}

// ── In-memory shard pool ──────────────────────────────────────────────────────
// Simulates the hot path inside Mnemosyne (no TLS, no I/O).

fn bench_shard_get_set(c: &mut Criterion) {
    // Simplified single-shard map (mirrors Mnemosyne's inner loop).
    let map: Arc<RwLock<HashMap<Vec<u8>, Value>>> = Arc::new(RwLock::new(HashMap::new()));

    // Pre-populate
    {
        let mut g = map.write();
        for i in 0u32..10_000 {
            g.insert(
                format!("key:{i}").into_bytes(),
                Value::String(format!("value:{i}").into_bytes()),
            );
        }
    }

    let mut group = c.benchmark_group("shard_pool");

    group.bench_function("get_hit", |b| {
        b.iter(|| {
            let g = map.read();
            black_box(g.get(b"key:42" as &[u8]))
        })
    });

    group.bench_function("set", |b| {
        let v = Value::String(b"bench-value".to_vec());
        b.iter(|| {
            let mut g = map.write();
            black_box(g.insert(b"bench:key".to_vec(), v.clone()))
        })
    });

    group.finish();
}

// ── TTL wheel scheduling ──────────────────────────────────────────────────────

fn bench_ttl_schedule(c: &mut Criterion) {
    let lethe = Lethe::new();
    let now_ms: u64 = 1_000_000_000;

    let mut group = c.benchmark_group("ttl_wheel");

    group.bench_function("schedule_1s_bucket", |b| {
        b.iter(|| {
            black_box(lethe.schedule(
                b"expkey".to_vec(),
                now_ms + 1000,
                now_ms,
            ))
        })
    });

    group.bench_function("tick_empty", |b| {
        let lethe2 = Lethe::new();
        let mut t = now_ms;
        b.iter(|| {
            t += 1000;
            black_box(lethe2.tick(t))
        })
    });

    group.finish();
}

// ── registry ──────────────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_frame_encode,
    bench_frame_decode,
    bench_slot_for,
    bench_iris_route,
    bench_lfu_increment,
    bench_lfu_pick_candidates,
    bench_token_issue,
    bench_token_verify,
    bench_cluster_tag,
    bench_shard_get_set,
    bench_ttl_schedule,
);
criterion_main!(benches);
