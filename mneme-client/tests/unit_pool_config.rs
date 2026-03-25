// mneme-client/tests/unit_pool_config.rs — Unit tests for PoolConfig defaults
// and construction. No network required.

use std::time::Duration;
use mneme_client::PoolConfig;

// ── Default values ────────────────────────────────────────────────────────────

#[test]
fn default_addr() {
    let cfg = PoolConfig::default();
    assert_eq!(cfg.addr, "127.0.0.1:6379");
}

#[test]
fn default_server_name() {
    let cfg = PoolConfig::default();
    assert_eq!(cfg.server_name, "mneme.local");
}

#[test]
fn default_tls_ca_cert_is_none() {
    let cfg = PoolConfig::default();
    assert!(cfg.tls_ca_cert.is_none(), "insecure dev mode by default");
}

#[test]
fn default_token_is_empty() {
    let cfg = PoolConfig::default();
    assert!(cfg.token.is_empty());
}

#[test]
fn default_pool_sizing() {
    let cfg = PoolConfig::default();
    assert!(cfg.min_idle < cfg.max_size, "min_idle must be less than max_size");
    assert!(cfg.min_idle >= 1);
    assert!(cfg.max_size >= 10);
}

#[test]
fn default_timeouts_are_positive() {
    let cfg = PoolConfig::default();
    assert!(cfg.acquire_timeout > Duration::ZERO);
    assert!(cfg.health_interval > Duration::ZERO);
    assert!(cfg.idle_timeout > Duration::ZERO);
}

#[test]
fn default_idle_timeout_exceeds_health_interval() {
    // idle_timeout should be longer than health_interval so connections aren't
    // evicted before the health check has a chance to keep them alive.
    let cfg = PoolConfig::default();
    assert!(cfg.idle_timeout > cfg.health_interval);
}

// ── Custom construction ────────────────────────────────────────────────────────

#[test]
fn custom_config_fields() {
    let cfg = PoolConfig {
        addr:            "10.0.0.1:6379".into(),
        addrs:           vec!["10.0.0.2:6379".into()],
        tls_ca_cert:     Some("/etc/mneme/ca.crt".into()),
        server_name:     "prod.mneme.local".into(),
        token:           "tok123".into(),
        min_idle:        4,
        max_size:        64,
        acquire_timeout: Duration::from_millis(500),
        health_interval: Duration::from_secs(15),
        idle_timeout:    Duration::from_secs(600),
    };

    assert_eq!(cfg.addr, "10.0.0.1:6379");
    assert_eq!(cfg.server_name, "prod.mneme.local");
    assert_eq!(cfg.tls_ca_cert.as_deref(), Some("/etc/mneme/ca.crt"));
    assert_eq!(cfg.token, "tok123");
    assert_eq!(cfg.min_idle, 4);
    assert_eq!(cfg.max_size, 64);
    assert_eq!(cfg.acquire_timeout, Duration::from_millis(500));
}

// ── Clone / debug ─────────────────────────────────────────────────────────────

// PoolConfig does not derive Clone (pool itself is not Clone),
// but we can verify the fields exist and are accessible.
#[test]
fn server_name_is_configurable() {
    // Per CLAUDE.md: TLS server_name comes from config, never hardcoded.
    let cfg = PoolConfig {
        server_name: "custom-server.example.com".into(),
        ..PoolConfig::default()
    };
    assert_eq!(cfg.server_name, "custom-server.example.com");
}
