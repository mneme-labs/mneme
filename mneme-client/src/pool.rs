// Pontus — MnemePool: connection pool with health check + auto-reconnect.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rustls::ClientConfig;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::{info, warn};

use crate::conn::MnemeConn;

pub struct PoolConfig {
    pub addr: String,
    /// Additional Core addresses for HA failover.  When the primary `addr` is
    /// unreachable or returns `LeaderRedirect`, the pool tries these in order.
    pub addrs: Vec<String>,
    pub tls_ca_cert: Option<String>,   // None = insecure dev mode
    /// TLS SNI / server name sent during the handshake.
    /// Must match the CN or SAN in the server certificate.
    /// Comes from `config.tls.server_name` — never hardcode "mneme.local".
    pub server_name: String,
    pub token: String,
    pub min_idle: usize,
    pub max_size: usize,
    pub acquire_timeout: Duration,
    pub health_interval: Duration,
    pub idle_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:6379".into(),
            addrs: Vec::new(),
            tls_ca_cert: None,
            server_name: "mneme.local".into(),
            token: String::new(),
            min_idle: 2,
            max_size: 20,
            acquire_timeout: Duration::from_millis(200),
            health_interval: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(300),
        }
    }
}

struct PooledConn {
    conn: MnemeConn,
    #[allow(dead_code)]
    created_at: Instant,
    last_used: Instant,
}

pub struct MnemePool {
    config: Arc<PoolConfig>,
    idle: Arc<Mutex<VecDeque<PooledConn>>>,
    tls_config: Arc<ClientConfig>,
}

impl MnemePool {
    /// Build a pool. Connects `min_idle` connections eagerly.
    pub async fn new(config: PoolConfig) -> Result<Self> {
        let tls_config = build_tls_config(config.tls_ca_cert.as_deref())?;
        let pool = Self {
            config: Arc::new(config),
            idle: Arc::new(Mutex::new(VecDeque::new())),
            tls_config: Arc::new(tls_config),
        };

        // Warm up min_idle connections
        for _ in 0..pool.config.min_idle {
            match pool.open_conn().await {
                Ok(c) => pool.idle.lock().push_back(PooledConn {
                    conn: c,
                    created_at: Instant::now(),
                    last_used: Instant::now(),
                }),
                Err(e) => warn!("Pool warm-up failed: {e}"),
            }
        }

        // Health check background task
        {
            let pool2_idle = pool.idle.clone();
            let pool2_cfg = pool.config.clone();
            let pool2_tls = pool.tls_config.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(pool2_cfg.health_interval);
                loop {
                    interval.tick().await;

                    // 1. Drain idle connections for health checking.
                    //    Remove expired-by-timeout connections immediately;
                    //    collect remaining ones for PING validation.
                    let to_check: Vec<PooledConn> = {
                        let mut idle = pool2_idle.lock();
                        let all: Vec<PooledConn> = idle.drain(..).collect();
                        all.into_iter()
                            .filter(|c| c.last_used.elapsed() < pool2_cfg.idle_timeout)
                            .collect()
                    };

                    // 2. PING each connection; keep only those that respond.
                    let mut healthy = Vec::new();
                    for pc in to_check {
                        let deadline = tokio::time::timeout(
                            Duration::from_secs(2),
                            pc.conn.ping(),
                        );
                        if deadline.await.map(|r| r.is_ok()).unwrap_or(false) {
                            healthy.push(pc);
                        } else {
                            warn!("Pool health check: connection failed PING, dropping");
                        }
                    }

                    // 3. Return healthy connections to the pool.
                    {
                        let mut idle = pool2_idle.lock();
                        for pc in healthy {
                            idle.push_back(pc);
                        }
                    }

                    // 4. Refill to min_idle.
                    let count = pool2_idle.lock().len();
                    for _ in count..pool2_cfg.min_idle {
                        if let Ok(conn) = open_conn_raw(&pool2_cfg.addr, &pool2_cfg.server_name, &pool2_tls, &pool2_cfg.token).await {
                            pool2_idle.lock().push_back(PooledConn {
                                conn,
                                created_at: Instant::now(),
                                last_used: Instant::now(),
                            });
                        }
                    }
                }
            });
        }

        info!(addr = %pool.config.addr, "MnemePool ready");
        Ok(pool)
    }

    /// Acquire a connection. Blocks up to `acquire_timeout`.
    pub async fn acquire(&self) -> Result<PoolGuard<'_>> {
        let deadline = Instant::now() + self.config.acquire_timeout;
        loop {
            {
                let mut idle = self.idle.lock();
                if let Some(mut pc) = idle.pop_front() {
                    pc.last_used = Instant::now();
                    return Ok(PoolGuard { conn: Some(pc.conn), pool: self });
                }
            }
            // Open a new connection if pool not exhausted
            if let Ok(conn) = self.open_conn().await {
                return Ok(PoolGuard { conn: Some(conn), pool: self });
            }
            if Instant::now() >= deadline {
                anyhow::bail!("Pool acquire timeout");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn open_conn(&self) -> Result<MnemeConn> {
        match open_conn_raw(&self.config.addr, &self.config.server_name, &self.tls_config, &self.config.token).await {
            Ok(c) => Ok(c),
            Err(e) if !self.config.addrs.is_empty() => {
                warn!(primary = %self.config.addr, "primary dial failed ({e:#}), trying failover addrs");
                open_conn_multi(&self.config.addrs, &self.config.server_name, &self.tls_config, &self.config.token).await
            }
            Err(e) => Err(e),
        }
    }

    /// Return a connection to the pool (called by PoolGuard on drop).
    fn release(&self, conn: MnemeConn) {
        let mut idle = self.idle.lock();
        if idle.len() < self.config.max_size {
            idle.push_back(PooledConn {
                conn,
                created_at: Instant::now(),
                last_used: Instant::now(),
            });
        }
        // else: conn is dropped
    }
}

async fn open_conn_raw(addr: &str, server_name_str: &str, tls_config: &ClientConfig, token: &str) -> Result<MnemeConn> {
    let conn = dial_addr(addr, server_name_str, tls_config).await?;
    if !token.is_empty() {
        conn.auth_token(token).await.context("auth")?;
    }
    Ok(conn)
}

/// Try multiple addresses in order, returning the first successful connection.
async fn open_conn_multi(
    addrs: &[String],
    server_name_str: &str,
    tls_config: &ClientConfig,
    token: &str,
) -> Result<MnemeConn> {
    let mut last_err = anyhow::anyhow!("no addresses to try");
    for addr in addrs {
        match dial_addr(addr, server_name_str, tls_config).await {
            Ok(conn) => {
                if !token.is_empty() {
                    conn.auth_token(token).await.context("auth")?;
                }
                return Ok(conn);
            }
            Err(e) => {
                warn!(addr, "failover dial failed: {e:#}");
                last_err = e;
            }
        }
    }
    Err(last_err)
}

async fn dial_addr(addr: &str, server_name_str: &str, tls_config: &ClientConfig) -> Result<MnemeConn> {
    // clone() shares the Arc<dyn ClientSessionStore> inside config.resumption,
    // so all pool connections benefit from in-memory TLS session resumption.
    let connector = TlsConnector::from(Arc::new(tls_config.clone()));
    let server_name = rustls::pki_types::ServerName::try_from(server_name_str.to_owned())
        .map_err(|_| anyhow::anyhow!("invalid TLS server name: {server_name_str}"))?;
    let tcp = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect to {addr}"))?;
    tcp.set_nodelay(true)?;
    let tls = connector.connect(server_name, tcp).await.context("TLS handshake")?;
    Ok(MnemeConn::new(tls))
}

fn build_tls_config(ca_cert_path: Option<&str>) -> Result<ClientConfig> {
    if let Some(path) = ca_cert_path {
        let ca_data = std::fs::read(path).with_context(|| format!("read CA: {path}"))?;
        let mut root_store = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut ca_data.as_slice()) {
            root_store.add(cert?)?;
        }
        Ok(ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth())
    } else {
        // Insecure dev mode
        Ok(ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth())
    }
}

/// RAII guard — returns conn to pool on drop.
pub struct PoolGuard<'a> {
    /// Option so we can `take()` on drop to move into pool.
    conn: Option<MnemeConn>,
    pool: &'a MnemePool,
}

impl<'a> PoolGuard<'a> {
    /// Borrow the inner connection.
    pub fn conn(&self) -> &MnemeConn {
        self.conn.as_ref().expect("conn already taken")
    }
}

impl<'a> std::ops::Deref for PoolGuard<'a> {
    type Target = MnemeConn;
    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().expect("conn already taken")
    }
}

impl<'a> Drop for PoolGuard<'a> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.release(conn);
        }
    }
}

#[derive(Debug)]
struct NoVerifier;
impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(&self, _: &rustls::pki_types::CertificateDer, _: &[rustls::pki_types::CertificateDer], _: &rustls::pki_types::ServerName, _: &[u8], _: rustls::pki_types::UnixTime) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &rustls::pki_types::CertificateDer, _: &rustls::DigitallySignedStruct) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _: &[u8], _: &rustls::pki_types::CertificateDer, _: &rustls::DigitallySignedStruct) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}