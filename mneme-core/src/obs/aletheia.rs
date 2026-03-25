// Aletheia — Metrics. Full metric set including Charon, Hermes, Iris, Themis, Aoide.

use std::net::SocketAddr;
use std::sync::Arc;

use prometheus::Encoder;
use anyhow::Result;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use once_cell::sync::Lazy;
use prometheus::{
    register_counter, register_counter_vec, register_gauge, register_histogram_vec,
    Counter, CounterVec, Gauge, HistogramVec, TextEncoder,
};
use tokio::net::TcpListener;
use tracing::{info, warn};

static CONN_ACCEPTED:  Lazy<Counter>    = Lazy::new(|| register_counter!("mneme_connections_accepted_total","Total accepted connections").unwrap());
static CONN_REJECTED:  Lazy<CounterVec> = Lazy::new(|| register_counter_vec!("mneme_connections_rejected_total","Rejected by reason",&["reason"]).unwrap());
static CONN_ACTIVE:    Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_connections_active","Active connections").unwrap());
static CONN_IDLE:      Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_connections_idle","Idle connections").unwrap());

static CMD_TOTAL:      Lazy<CounterVec>  = Lazy::new(|| register_counter_vec!("mneme_commands_total","Commands by type",&["cmd","result"]).unwrap());
static CMD_LATENCY:    Lazy<HistogramVec>= Lazy::new(|| register_histogram_vec!("mneme_command_latency_us","Latency µs",&["cmd"],vec![50.0,150.0,300.0,800.0,1200.0,5000.0,20000.0]).unwrap());
static IN_FLIGHT:      Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_requests_in_flight","In-flight requests").unwrap());

static POOL_USED:      Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_pool_bytes_used","Pool bytes used").unwrap());
static POOL_TOTAL:     Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_pool_bytes_total","Pool bytes total").unwrap());
static POOL_PRESSURE:  Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_pool_pressure_ratio","pool_used/pool_max").unwrap());
static EVICTIONS:      Lazy<CounterVec> = Lazy::new(|| register_counter_vec!("mneme_evictions_total","Evictions",&["reason"]).unwrap());
static COLD_FETCHES:   Lazy<Counter>    = Lazy::new(|| register_counter!("mneme_cold_fetches_total","Cold fetches from Oneiros").unwrap());

static REPL_FRAMES:    Lazy<CounterVec> = Lazy::new(|| register_counter_vec!("mneme_replication_frames_total","Repl frames",&["keeper_id"]).unwrap());
static REPL_ERRORS:    Lazy<CounterVec> = Lazy::new(|| register_counter_vec!("mneme_replication_errors_total","Repl errors",&["keeper_id"]).unwrap());
static KEEPER_CONNS:   Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_keeper_connections","Connected keepers").unwrap());

static WAL_BYTES:      Lazy<Counter>    = Lazy::new(|| register_counter!("mneme_wal_bytes_written_total","WAL bytes written").unwrap());
static WAL_ROTATIONS:  Lazy<Counter>    = Lazy::new(|| register_counter!("mneme_wal_rotations_total","WAL rotations").unwrap());

static CLUSTER_TERM:   Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_cluster_term","Raft term").unwrap());
static CLUSTER_LEADER: Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_cluster_leader","1 if leader").unwrap());
static CLUSTER_NODES:  Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_cluster_live_nodes","Live nodes").unwrap());
static ELECTIONS:      Lazy<Counter>    = Lazy::new(|| register_counter!("mneme_cluster_elections_total","Elections").unwrap());

static SLOT_MIGRATIONS:Lazy<Counter>    = Lazy::new(|| register_counter!("mneme_slot_migrations_total","Slot migrations").unwrap());

static HW_LLC:         Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_hw_l3_cache_misses_total","LLC misses").unwrap());
static HW_INSTR:       Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_hw_instructions_total","Instructions").unwrap());
static HW_TLB:         Lazy<Gauge>      = Lazy::new(|| register_gauge!("mneme_hw_tlb_misses_total","TLB misses").unwrap());

#[derive(Clone)]
pub struct Aletheia { inner: Arc<AletheiaInner> }

struct AletheiaInner { perf_enabled: bool }

impl Aletheia {
    pub fn new() -> Self {
        let _ = (&*CONN_ACCEPTED,&*CONN_REJECTED,&*CONN_ACTIVE,&*CONN_IDLE,
                 &*CMD_TOTAL,&*CMD_LATENCY,&*IN_FLIGHT,
                 &*POOL_USED,&*POOL_TOTAL,&*POOL_PRESSURE,&*EVICTIONS,&*COLD_FETCHES,
                 &*REPL_FRAMES,&*REPL_ERRORS,&*KEEPER_CONNS,
                 &*WAL_BYTES,&*WAL_ROTATIONS,
                 &*CLUSTER_TERM,&*CLUSTER_LEADER,&*CLUSTER_NODES,&*ELECTIONS,
                 &*SLOT_MIGRATIONS,&*HW_LLC,&*HW_INSTR,&*HW_TLB);
        #[cfg(target_os = "linux")]
        let perf_enabled = read_perf_counters().is_ok();
        #[cfg(not(target_os = "linux"))]
        let perf_enabled = false;
        Self { inner: Arc::new(AletheiaInner { perf_enabled }) }
    }

    // Charon
    pub fn record_conn_accepted(&self) { CONN_ACCEPTED.inc(); }
    pub fn record_conn_rejected(&self, reason: &str) { CONN_REJECTED.with_label_values(&[reason]).inc(); }
    pub fn set_conn_active(&self, n: usize) { CONN_ACTIVE.set(n as f64); }
    pub fn set_conn_idle(&self, n: usize) { CONN_IDLE.set(n as f64); }

    // Commands
    pub fn record_cmd(&self, cmd: &str, result: &str, latency_us: f64) {
        CMD_TOTAL.with_label_values(&[cmd, result]).inc();
        CMD_LATENCY.with_label_values(&[cmd]).observe(latency_us);
    }
    pub fn set_in_flight(&self, n: usize) { IN_FLIGHT.set(n as f64); }

    // Memory / Lethe
    pub fn set_pool_usage(&self, used: u64, total: u64) {
        POOL_USED.set(used as f64);
        POOL_TOTAL.set(total as f64);
        if total > 0 { POOL_PRESSURE.set(used as f64 / total as f64); }
    }
    pub fn record_eviction(&self, reason: &str) { EVICTIONS.with_label_values(&[reason]).inc(); }
    pub fn record_cold_fetch(&self) { COLD_FETCHES.inc(); }

    // Hermes
    pub fn set_keeper_connections(&self, n: usize) { KEEPER_CONNS.set(n as f64); }
    pub fn record_replication_frame(&self, keeper_id: &str) { REPL_FRAMES.with_label_values(&[keeper_id]).inc(); }
    pub fn record_replication_error(&self, keeper_id: &str) { REPL_ERRORS.with_label_values(&[keeper_id]).inc(); }

    // Aoide
    pub fn record_wal_write(&self, bytes: u64) { WAL_BYTES.inc_by(bytes as f64); }
    pub fn record_wal_rotation(&self) { WAL_ROTATIONS.inc(); }

    // Themis
    pub fn set_cluster_term(&self, term: u64) { CLUSTER_TERM.set(term as f64); }
    pub fn set_is_leader(&self, v: bool) { CLUSTER_LEADER.set(if v { 1.0 } else { 0.0 }); }
    pub fn set_live_nodes(&self, n: usize) { CLUSTER_NODES.set(n as f64); }
    pub fn record_election(&self) { ELECTIONS.inc(); }

    // Iris
    pub fn record_slot_migration(&self) { SLOT_MIGRATIONS.inc(); }

    // Hardware — Linux only (perf_event_open). No-op on macOS / Windows.
    pub fn sample_hw_counters(&self) {
        if !self.inner.perf_enabled { return; }
        #[cfg(target_os = "linux")]
        if let Ok((llc, instr, tlb)) = read_perf_counters() {
            HW_LLC.set(llc as f64);
            HW_INSTR.set(instr as f64);
            HW_TLB.set(tlb as f64);
        }
    }

    pub async fn serve_metrics(addr: SocketAddr) -> Result<()> {
        let listener = TcpListener::bind(addr).await?;
        info!(%addr, "Aletheia /metrics listening");
        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                if let Err(e) = http1::Builder::new()
                    .serve_connection(io, service_fn(handle_metrics))
                    .await
                { warn!("metrics: {e}"); }
            });
        }
    }
}

impl Default for Aletheia { fn default() -> Self { Self::new() } }

async fn handle_metrics(req: Request<hyper::body::Incoming>)
                        -> std::result::Result<Response<Full<Bytes>>, std::convert::Infallible>
{
    if req.uri().path() != "/metrics" {
        return Ok(Response::builder().status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("404\n"))).unwrap());
    }
    let enc = TextEncoder::new();
    match enc.encode_to_string(&prometheus::gather()) {
        Ok(body) => Ok(Response::builder().status(StatusCode::OK)
            .header("Content-Type", enc.format_type())
            .body(Full::new(Bytes::from(body))).unwrap()),
        Err(e) => Ok(Response::builder().status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Full::new(Bytes::from(e.to_string()))).unwrap()),
    }
}

/// Read LLC misses, instruction count, and TLB misses via perf_event_open.
/// Only compiled on Linux where the perf-event crate is available.
#[cfg(target_os = "linux")]
fn read_perf_counters() -> Result<(u64, u64, u64)> {
    use perf_event::Builder;
    use perf_event::events::Hardware;
    let mut llc  = Builder::new().kind(Hardware::CACHE_MISSES).build()?;
    let mut inst = Builder::new().kind(Hardware::INSTRUCTIONS).build()?;
    let mut tlb  = Builder::new().kind(Hardware::CACHE_REFERENCES).build()?;
    llc.enable()?;
    inst.enable()?;
    tlb.enable()?;
    let lv = llc.read()?;
    let iv = inst.read()?;
    let tv = tlb.read()?;
    llc.disable()?;
    inst.disable()?;
    tlb.disable()?;
    Ok((lv, iv, tv))
}

/// Stub for non-Linux platforms — hardware counters are unavailable.
#[cfg(not(target_os = "linux"))]
fn read_perf_counters() -> Result<(u64, u64, u64)> {
    anyhow::bail!("perf_event_open not available on this platform")
}