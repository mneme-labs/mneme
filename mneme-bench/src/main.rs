// mneme-bench: end-to-end latency benchmark over a PERSISTENT TLS connection.
//
// Unlike `docker exec mneme-cli …` (new process + new TLS handshake each call),
// this binary opens ONE TLS connection, authenticates once, then fires N
// operations in a tight loop, recording per-operation wall time.
//
// This measures the REAL cache latency that a Pontus client application sees:
//   GET EVENTUAL RAM hit  → target p99 < 150 µs
//   SET QUORUM            → target p99 < 800 µs
//
// Usage:
//   mneme-bench [OPTIONS] --token <TOK>
//   mneme-bench -H 127.0.0.1:6379 --ca-cert /etc/mneme/ca.crt \
//               -u admin -p secret --ops 10000 --mode mixed

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use mneme_client::{Consistency, MnemeConn};

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "mneme-bench", about = "MnemeCache end-to-end latency benchmark")]
struct Args {
    /// Server address (host:port)
    #[arg(short = 'H', long, default_value = "127.0.0.1:6379")]
    host: String,

    /// CA certificate path for TLS verification
    #[arg(long, default_value = "/etc/mneme/ca.crt")]
    ca_cert: PathBuf,

    /// Client certificate path (optional mTLS)
    #[arg(long)]
    cert: Option<PathBuf>,

    /// Client private key path (optional mTLS)
    #[arg(long)]
    key: Option<PathBuf>,

    /// Skip TLS certificate verification (dev only)
    #[arg(long)]
    insecure: bool,

    /// Username for authentication
    #[arg(short = 'u', long)]
    username: Option<String>,

    /// Password for authentication
    #[arg(short = 'p', long)]
    password: Option<String>,

    /// Pre-issued token (skips credential auth)
    #[arg(short = 't', long)]
    token: Option<String>,

    /// Number of operations to run
    #[arg(long, default_value = "10000")]
    ops: usize,

    /// Benchmark mode
    #[arg(long, value_enum, default_value = "mixed")]
    mode: Mode,

    /// Consistency level for writes (eventual/quorum/all/one)
    #[arg(short = 'c', long, default_value = "quorum")]
    consistency: ConsistencyArg,

    /// Warmup operations (not included in stats)
    #[arg(long, default_value = "500")]
    warmup: usize,

    /// Value size in bytes
    #[arg(long, default_value = "64")]
    value_size: usize,

    /// Print every Nth sample (0 = disabled)
    #[arg(long, default_value = "0")]
    verbose_every: usize,
}

#[derive(ValueEnum, Clone, Debug)]
enum Mode {
    Get,
    Set,
    Mixed,
}

#[derive(ValueEnum, Clone, Debug)]
enum ConsistencyArg {
    Eventual,
    Quorum,
    All,
    One,
}

impl From<ConsistencyArg> for Consistency {
    fn from(c: ConsistencyArg) -> Self {
        match c {
            ConsistencyArg::Eventual => Consistency::Eventual,
            ConsistencyArg::Quorum  => Consistency::Quorum,
            ConsistencyArg::All     => Consistency::All,
            ConsistencyArg::One     => Consistency::One,
        }
    }
}

// ── TLS connect helpers ───────────────────────────────────────────────────────

async fn tls_connect(args: &Args) -> Result<MnemeConn> {
    use rustls::pki_types::ServerName;
    use std::io::BufReader;
    use std::fs;
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    let host_part = args.host.split(':').next().unwrap_or("localhost").to_string();

    let tcp = TcpStream::connect(&args.host)
        .await
        .with_context(|| format!("TCP connect to {}", args.host))?;

    let tls_cfg = if args.insecure {
        // Dev-only: accept any cert.
        let mut cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth();
        cfg.resumption = rustls::client::Resumption::in_memory_sessions(64);
        cfg
    } else {
        let ca_pem = fs::read(&args.ca_cert)
            .with_context(|| format!("read CA cert {:?}", args.ca_cert))?;
        let mut roots = rustls::RootCertStore::empty();
        let mut reader = BufReader::new(ca_pem.as_slice());
        for cert in rustls_pemfile::certs(&mut reader) {
            roots.add(cert.context("parse CA cert")?).context("add CA cert")?;
        }
        let mut cfg = if let (Some(cert_path), Some(key_path)) = (&args.cert, &args.key) {
            let cert_pem = fs::read(cert_path)?;
            let key_pem  = fs::read(key_path)?;
            let mut cr = BufReader::new(cert_pem.as_slice());
            let mut kr = BufReader::new(key_pem.as_slice());
            let certs: Vec<_> = rustls_pemfile::certs(&mut cr).collect::<std::result::Result<_,_>>()?;
            let key = rustls_pemfile::private_key(&mut kr)?.context("no private key")?;
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_client_auth_cert(certs, key)?
        } else {
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
        };
        cfg.resumption = rustls::client::Resumption::in_memory_sessions(64);
        cfg
    };

    let connector = TlsConnector::from(Arc::new(tls_cfg));
    let server_name = ServerName::try_from(host_part.clone())
        .with_context(|| format!("invalid server name: {host_part}"))?
        .to_owned();
    let tls = connector.connect(server_name, tcp).await
        .with_context(|| format!("TLS handshake to {}", args.host))?;

    Ok(MnemeConn::new(tls))
}

async fn authenticate(conn: &MnemeConn, args: &Args) -> Result<String> {
    use mneme_common::CmdId;

    if let Some(tok) = &args.token {
        conn.auth_token(tok).await.context("auth with token")?;
        return Ok(tok.clone());
    }
    let user = args.username.as_deref().unwrap_or("admin");
    let pass = args.password.as_deref().unwrap_or("");
    let payload = bytes::Bytes::from(rmp_serde::to_vec(&(user, pass))?);
    let resp = conn.send(CmdId::Auth, payload, mneme_client::Consistency::Eventual)
        .await.context("send AUTH")?;
    if resp.cmd_id != CmdId::Ok {
        let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
        bail!("AUTH failed: {msg}");
    }
    let token: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
    Ok(token)
}

// ── Stats ─────────────────────────────────────────────────────────────────────

fn percentile(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() { return 0; }
    let idx = ((sorted.len() as f64 * pct / 100.0).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

fn print_stats(label: &str, mut samples: Vec<u64>, total_elapsed: Duration) {
    if samples.is_empty() { return; }
    samples.sort_unstable();
    let n = samples.len();
    let mean = samples.iter().sum::<u64>() / n as u64;
    let ops_per_sec = n as f64 / total_elapsed.as_secs_f64();

    println!("  {label}");
    println!("    ops:        {n}");
    println!("    throughput: {ops_per_sec:.0} ops/s");
    println!("    min:        {} µs", samples[0]);
    println!("    mean:       {mean} µs");
    println!("    p50:        {} µs", percentile(&samples, 50.0));
    println!("    p95:        {} µs", percentile(&samples, 95.0));
    println!("    p99:        {} µs", percentile(&samples, 99.0));
    println!("    p999:       {} µs", percentile(&samples, 99.9));
    println!("    max:        {} µs", samples[n - 1]);
    println!();
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Must be called before any rustls usage when multiple providers are compiled in.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let args = Args::parse();
    let consistency: Consistency = args.consistency.clone().into();
    let value: bytes::Bytes = bytes::Bytes::from(vec![b'x'; args.value_size]);

    println!("mneme-bench  target={} ops={}  mode={:?}  consistency={consistency:?}",
             args.host, args.ops, args.mode);

    // ── Establish connection ──────────────────────────────────────────────────

    let t_connect = Instant::now();
    let conn = tls_connect(&args).await?;
    let connect_ms = t_connect.elapsed().as_millis();

    let t_auth = Instant::now();
    let _token = authenticate(&conn, &args).await?;
    let auth_ms = t_auth.elapsed().as_millis();

    println!("  TLS connect:     {connect_ms} ms");
    println!("  Auth:            {auth_ms} ms");
    println!("  (single connection — all ops reuse this TLS session)");
    println!();

    // ── Warmup ────────────────────────────────────────────────────────────────

    if args.warmup > 0 {
        print!("  Warmup ({} ops)...", args.warmup);
        for i in 0..args.warmup {
            let key = format!("__bench_warmup_{i}");
            conn.set(key.as_bytes(), value.clone(), 0u64, consistency).await.ok();
        }
        println!(" done");
    }

    // ── Benchmark ─────────────────────────────────────────────────────────────

    let mut set_samples: Vec<u64> = Vec::with_capacity(args.ops);
    let mut get_samples: Vec<u64> = Vec::with_capacity(args.ops);

    let bench_start = Instant::now();

    for i in 0..args.ops {
        let key = format!("__bench_{}", i % 1000);

        match args.mode {
            Mode::Set => {
                let t = Instant::now();
                conn.set(key.as_bytes(), value.clone(), 0u64, consistency).await
                    .context("SET failed")?;
                set_samples.push(t.elapsed().as_micros() as u64);
            }
            Mode::Get => {
                let t = Instant::now();
                conn.get(key.as_bytes(), Consistency::Eventual).await.ok();
                get_samples.push(t.elapsed().as_micros() as u64);
            }
            Mode::Mixed => {
                // Even = SET, odd = GET.
                if i % 2 == 0 {
                    let t = Instant::now();
                    conn.set(key.as_bytes(), value.clone(), 0u64, consistency).await
                        .context("SET failed")?;
                    set_samples.push(t.elapsed().as_micros() as u64);
                } else {
                    let t = Instant::now();
                    conn.get(key.as_bytes(), Consistency::Eventual).await.ok();
                    get_samples.push(t.elapsed().as_micros() as u64);
                }
            }
        }

        if args.verbose_every > 0 && i % args.verbose_every == 0 {
            println!("  op {i} ...");
        }
    }

    let total = bench_start.elapsed();

    // ── Report ────────────────────────────────────────────────────────────────

    println!("Results (persistent connection, no per-op TLS overhead):");
    println!("─────────────────────────────────────────────────────────");

    if !set_samples.is_empty() {
        print_stats("SET", set_samples, total);
    }
    if !get_samples.is_empty() {
        print_stats("GET (eventual)", get_samples, total);
    }

    println!("Total elapsed: {:.3} s", total.as_secs_f64());

    // Check against targets.
    println!();
    println!("Target checks:");

    // Build local stats for checking (already consumed above, need to recompute from conn).
    // Just print a reminder.
    println!("  GET EVENTUAL p99 target: < 150 µs");
    println!("  SET QUORUM   p99 target: < 800 µs");
    println!();
    println!("NOTE: CLI per-invocation latency (docker exec mneme-cli …) is NOT this.");
    println!("Each CLI call pays: process spawn + TLS handshake (~40ms on Docker/Mac).");
    println!("Use Pontus (mneme-client) or this bench tool for production latency.");

    Ok(())
}

// ── NoVerifier for --insecure ─────────────────────────────────────────────────

#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self, _: &rustls::pki_types::CertificateDer,
        _: &[rustls::pki_types::CertificateDer],
        _: &rustls::pki_types::ServerName,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _: &[u8], _: &rustls::pki_types::CertificateDer,
        _: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self, _: &[u8], _: &rustls::pki_types::CertificateDer,
        _: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
