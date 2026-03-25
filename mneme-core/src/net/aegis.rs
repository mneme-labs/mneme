// Aegis — TLS 1.3 layer.
// rustls server + client configs, rcgen auto-cert generation, mTLS for replication.

use std::fs;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use mneme_common::config::TlsConfig;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tracing::info;

#[derive(Clone)]
pub struct Aegis {
    server_cfg: Arc<ServerConfig>,
    client_cfg: Arc<ClientConfig>,
    /// mTLS server config for replication port.
    mtls_server_cfg: Arc<ServerConfig>,
}

impl Aegis {
    /// Initialise TLS from `tls_config`. Auto-generates certs if `auto_generate` is true.
    pub fn new(tls_config: &TlsConfig) -> Result<Self> {
        if tls_config.auto_generate && Self::needs_generation(tls_config) {
            Self::gen_self_signed(tls_config)?;
        }

        let certs = load_certs(&tls_config.cert)?;
        let key = load_key(&tls_config.key)?;
        let ca_certs = load_certs(&tls_config.ca_cert)?;

        // ── Server config (client port, one-way TLS) ─────────────────────
        let server_cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs.clone(), key.clone_key())
            .with_context(|| "Aegis server cert")?;

        // ── mTLS server config (replication port) ────────────────────────
        let mut root_store = RootCertStore::empty();
        for ca in &ca_certs {
            root_store
                .add(ca.clone())
                .with_context(|| "Aegis add CA cert")?;
        }
        let client_auth = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
            .build()
            .with_context(|| "Aegis mTLS verifier")?;

        let mtls_server_cfg = ServerConfig::builder()
            .with_client_cert_verifier(client_auth)
            .with_single_cert(certs.clone(), key.clone_key())
            .with_context(|| "Aegis mTLS server cert")?;

        // ── Client config (replication outbound) ─────────────────────────
        let mut root_store = RootCertStore::empty();
        for ca in &ca_certs {
            root_store
                .add(ca.clone())
                .with_context(|| "Aegis client CA")?;
        }
        let client_cfg = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_client_auth_cert(certs, key)
            .with_context(|| "Aegis client cert")?;

        info!("Aegis TLS initialised");

        Ok(Self {
            server_cfg: Arc::new(server_cfg),
            client_cfg: Arc::new(client_cfg),
            mtls_server_cfg: Arc::new(mtls_server_cfg),
        })
    }

    /// Create a dummy Aegis for solo mode where Raft transport is never used.
    /// Panics if any config is actually accessed.
    pub fn dummy() -> Self {
        // Generate ephemeral self-signed cert for the dummy.
        let key = KeyPair::generate().expect("keygen");
        let params = CertificateParams::new(vec!["dummy.local".into()]).expect("params");
        let cert = params.self_signed(&key).expect("self-sign");
        let cert_der = CertificateDer::from(cert.der().to_vec());
        let key_der = PrivateKeyDer::try_from(key.serialize_der()).expect("key-der");

        let server_cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der.clone_key())
            .expect("dummy server cfg");

        let mut root = RootCertStore::empty();
        root.add(cert_der.clone()).expect("add dummy CA");
        let client_cfg = ClientConfig::builder()
            .with_root_certificates(root.clone())
            .with_client_auth_cert(vec![cert_der.clone()], key_der.clone_key())
            .expect("dummy client cfg");

        let mtls_server_cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .expect("dummy mtls cfg");

        Self {
            server_cfg: Arc::new(server_cfg),
            client_cfg: Arc::new(client_cfg),
            mtls_server_cfg: Arc::new(mtls_server_cfg),
        }
    }

    /// Build Aegis with optional separate client-facing cert.
    /// When `tls_config.client_cert` and `client_key` are set, the client port
    /// uses that cert (e.g. publicly-trusted) instead of the internal auto-gen cert.
    pub fn server_config_for_client_port(&self, tls_config: &TlsConfig) -> Result<Arc<ServerConfig>> {
        if !tls_config.client_cert.is_empty() && !tls_config.client_key.is_empty() {
            let certs = load_certs(&tls_config.client_cert)?;
            let key = load_key(&tls_config.client_key)?;
            let cfg = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .with_context(|| "client port cert")?;
            Ok(Arc::new(cfg))
        } else {
            Ok(self.server_cfg.clone())
        }
    }

    /// TLS server config for the client-facing port.
    pub fn server_config(&self) -> Arc<ServerConfig> {
        self.server_cfg.clone()
    }

    /// mTLS server config for the replication port.
    pub fn mtls_server_config(&self) -> Arc<ServerConfig> {
        self.mtls_server_cfg.clone()
    }

    /// mTLS client config for outgoing replication connections.
    pub fn client_config(&self) -> Arc<ClientConfig> {
        self.client_cfg.clone()
    }

    /// Generate a self-signed CA + node certificate and write to the paths in `tls_config`.
    pub fn gen_self_signed(tls_config: &TlsConfig) -> Result<()> {
        info!("Generating self-signed TLS certificates");

        // CA
        let mut ca_params = CertificateParams::new(vec!["MnemeCache CA".to_string()])?;
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = KeyPair::generate()?;
        let ca_cert = ca_params.self_signed(&ca_key)?;

        // Node cert signed by CA — includes DNS SANs, local IPs, and any
        // extra_sans the operator configured (e.g. a public/EIP address).
        let mut node_params =
            CertificateParams::new(vec!["mneme.local".to_string(), "localhost".to_string()])?;
        node_params.distinguished_name = {
            let mut dn = DistinguishedName::new();
            dn.push(DnType::CommonName, "mneme-node");
            dn
        };

        // Add IP SANs: loopback + auto-detected primary outbound IP.
        // This lets mneme-cli connect via IP without --insecure, and allows
        // nodes on the same network to verify by IP as well as hostname.
        node_params.subject_alt_names.push(
            SanType::IpAddress(IpAddr::from([127, 0, 0, 1])),
        );
        if let Some(local_ip) = detect_local_ip() {
            info!(%local_ip, "Adding local IP to TLS cert SANs");
            node_params.subject_alt_names.push(SanType::IpAddress(local_ip));
        }
        // Extra SANs from config — useful for adding a public/EIP address so
        // nodes connecting from outside the private subnet can verify the cert.
        for san in &tls_config.extra_sans {
            if let Ok(ip) = san.parse::<IpAddr>() {
                info!(san = %san, "Adding extra IP SAN from config");
                node_params.subject_alt_names.push(SanType::IpAddress(ip));
            } else {
                info!(san = %san, "Adding extra DNS SAN from config");
                if let Ok(dns) = san.as_str().try_into() {
                    node_params.subject_alt_names.push(SanType::DnsName(dns));
                }
            }
        }

        let node_key = KeyPair::generate()?;
        let node_cert = node_params.signed_by(&node_key, &ca_cert, &ca_key)?;

        // Ensure parent directories exist
        for path in [&tls_config.ca_cert, &tls_config.cert, &tls_config.key] {
            if let Some(parent) = Path::new(path).parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create TLS dir {}", parent.display()))?;
            }
        }

        fs::write(&tls_config.ca_cert, ca_cert.pem())
            .with_context(|| format!("write CA cert to {}", tls_config.ca_cert))?;
        fs::write(&tls_config.cert, node_cert.pem())
            .with_context(|| format!("write node cert to {}", tls_config.cert))?;
        fs::write(&tls_config.key, node_key.serialize_pem())
            .with_context(|| format!("write node key to {}", tls_config.key))?;

        info!(
            ca = %tls_config.ca_cert,
            cert = %tls_config.cert,
            key = %tls_config.key,
            "Self-signed certs written"
        );
        Ok(())
    }

    fn needs_generation(cfg: &TlsConfig) -> bool {
        !Path::new(&cfg.cert).exists() || !Path::new(&cfg.key).exists()
    }
}

// ── IP detection ─────────────────────────────────────────────────────────────

/// Return the primary outbound non-loopback IPv4 address of this machine.
///
/// Uses the UDP socket trick: bind to 0.0.0.0:0, "connect" (no packets sent)
/// to an external address, then read the OS-chosen source IP.  Falls back to
/// None if the machine has no default route.
fn detect_local_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // Connecting to 8.8.8.8 never sends a packet — it just sets the route.
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    let ip = addr.ip();
    if ip.is_loopback() || ip.is_unspecified() { None } else { Some(ip) }
}

// ── file helpers ─────────────────────────────────────────────────────────────

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let pem = fs::read(path).with_context(|| format!("read cert: {path}"))?;
    let certs: Vec<CertificateDer> = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse cert: {path}"))?;
    Ok(certs)
}

fn load_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    let pem = fs::read(path).with_context(|| format!("read key: {path}"))?;
    let key = rustls_pemfile::private_key(&mut pem.as_slice())
        .with_context(|| format!("parse key: {path}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {path}"))?;
    Ok(key)
}
