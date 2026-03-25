// Charon — Connection manager.
// Accept gate: rejects before TLS if at limit.
// Backpressure: tokio Semaphore caps in-flight requests.
// Idle timeout + TCP keepalive for half-open cleanup.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tracing::warn;

use crate::obs::aletheia::Aletheia;

pub struct Charon {
    max_total: usize,
    max_per_ip: u32,
    pub idle_timeout: Duration,
    pub tcp_keepalive: Duration,
    pub request_timeout: Duration,

    active_conns: Arc<std::sync::atomic::AtomicUsize>,
    per_ip: Arc<Mutex<HashMap<IpAddr, u32>>>,
    pub in_flight: Arc<Semaphore>,

    aletheia: Aletheia,
}

impl Charon {
    pub fn new(cfg: &mneme_common::config::ConnectionConfig, aletheia: Aletheia) -> Self {
        Self {
            max_total: cfg.max_total,
            max_per_ip: cfg.max_per_ip,
            idle_timeout: Duration::from_secs(cfg.idle_timeout_s),
            tcp_keepalive: Duration::from_secs(cfg.tcp_keepalive_s),
            request_timeout: Duration::from_millis(cfg.request_timeout_ms),
            active_conns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            per_ip: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(Semaphore::new(cfg.max_in_flight)),
            aletheia,
        }
    }

    /// Try to admit a new connection from `ip`.
    /// Returns a guard that decrements counters on drop.
    /// Call this BEFORE TLS handshake — cheap rejection.
    pub fn admit(&self, ip: IpAddr) -> Result<ConnGuard, AdmitError> {
        let total = self.active_conns.load(std::sync::atomic::Ordering::Relaxed);
        if total >= self.max_total {
            self.aletheia.record_conn_rejected("max_total");
            return Err(AdmitError::MaxTotal);
        }

        {
            let mut map = self.per_ip.lock();
            let count = map.entry(ip).or_insert(0);
            if *count >= self.max_per_ip {
                self.aletheia.record_conn_rejected("max_per_ip");
                return Err(AdmitError::MaxPerIp);
            }
            *count += 1;
        }

        self.active_conns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.aletheia.record_conn_accepted();
        self.aletheia.set_conn_active(
            self.active_conns.load(std::sync::atomic::Ordering::Relaxed)
        );

        Ok(ConnGuard {
            ip,
            active_conns: self.active_conns.clone(),
            per_ip: self.per_ip.clone(),
            aletheia: self.aletheia.clone(),
        })
    }

    /// Current active connection count.
    pub fn active(&self) -> usize {
        self.active_conns.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub enum AdmitError {
    MaxTotal,
    MaxPerIp,
}

impl std::fmt::Display for AdmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MaxTotal => write!(f, "ERR_MAX_CONNECTIONS"),
            Self::MaxPerIp => write!(f, "ERR_MAX_CONNECTIONS_PER_IP"),
        }
    }
}

/// RAII guard — decrements counters on drop.
pub struct ConnGuard {
    ip: IpAddr,
    active_conns: Arc<std::sync::atomic::AtomicUsize>,
    per_ip: Arc<Mutex<HashMap<IpAddr, u32>>>,
    aletheia: Aletheia,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.active_conns.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        let mut map = self.per_ip.lock();
        if let Some(count) = map.get_mut(&self.ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&self.ip);
            }
        }
        let active = self.active_conns.load(std::sync::atomic::Ordering::Relaxed);
        self.aletheia.set_conn_active(active);
    }
}

/// Graceful shutdown helper — waits for in-flight to drain.
pub async fn drain_in_flight(sem: &Semaphore, max: usize, timeout_secs: u64) {
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(timeout_secs);
    loop {
        let available = sem.available_permits();
        if available >= max {
            return; // all drained
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(
                in_flight = max - available,
                "Charon: drain timeout — force closing"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::aletheia::Aletheia;
    use mneme_common::config::ConnectionConfig;

    fn make_charon(max_total: usize, max_per_ip: u32) -> Charon {
        let mut cfg = ConnectionConfig::default();
        cfg.max_total = max_total;
        cfg.max_per_ip = max_per_ip;
        cfg.max_in_flight = 100;
        Charon::new(&cfg, Aletheia::new())
    }

    fn ip(s: &str) -> IpAddr { s.parse().unwrap() }

    #[test]
    fn admit_and_drop_decrements() {
        let c = make_charon(10, 5);
        assert_eq!(c.active(), 0);
        let g = c.admit(ip("1.2.3.4")).unwrap();
        assert_eq!(c.active(), 1);
        drop(g);
        assert_eq!(c.active(), 0);
    }

    #[test]
    fn max_total_enforced() {
        let c = make_charon(2, 10);
        let _g1 = c.admit(ip("1.1.1.1")).unwrap();
        let _g2 = c.admit(ip("2.2.2.2")).unwrap();
        assert!(matches!(c.admit(ip("3.3.3.3")), Err(AdmitError::MaxTotal)));
    }

    #[test]
    fn max_per_ip_enforced() {
        let c = make_charon(100, 2);
        let _g1 = c.admit(ip("1.2.3.4")).unwrap();
        let _g2 = c.admit(ip("1.2.3.4")).unwrap();
        assert!(matches!(c.admit(ip("1.2.3.4")), Err(AdmitError::MaxPerIp)));
        // Different IP still works
        assert!(c.admit(ip("5.6.7.8")).is_ok());
    }

    #[test]
    fn drop_frees_per_ip_slot() {
        let c = make_charon(100, 1);
        let g = c.admit(ip("1.2.3.4")).unwrap();
        assert!(c.admit(ip("1.2.3.4")).is_err());
        drop(g);
        assert!(c.admit(ip("1.2.3.4")).is_ok());
    }

    #[test]
    fn admit_returns_guard_under_limit() {
        let c = make_charon(10, 10);
        let result = c.admit(ip("10.0.0.1"));
        assert!(result.is_ok());
        assert_eq!(c.active(), 1);
    }

    #[test]
    fn admit_rejects_over_max_total() {
        let c = make_charon(1, 10);
        let _g = c.admit(ip("10.0.0.1")).unwrap();
        let result = c.admit(ip("10.0.0.2"));
        assert!(matches!(result, Err(AdmitError::MaxTotal)));
    }

    #[test]
    fn guard_drop_releases_slot() {
        let c = make_charon(1, 10);
        let g = c.admit(ip("10.0.0.1")).unwrap();
        assert_eq!(c.active(), 1);
        drop(g);
        assert_eq!(c.active(), 0);
        // Slot freed — next admit should succeed
        let result = c.admit(ip("10.0.0.2"));
        assert!(result.is_ok());
        assert_eq!(c.active(), 1);
    }

    #[test]
    fn per_ip_limit_enforced() {
        let c = make_charon(100, 2);
        let _g1 = c.admit(ip("10.0.0.1")).unwrap();
        let _g2 = c.admit(ip("10.0.0.1")).unwrap();
        let result = c.admit(ip("10.0.0.1"));
        assert!(matches!(result, Err(AdmitError::MaxPerIp)));
    }

    #[test]
    fn per_ip_different_ips_independent() {
        let c = make_charon(100, 2);
        // Fill IP A to its limit
        let _a1 = c.admit(ip("10.0.0.1")).unwrap();
        let _a2 = c.admit(ip("10.0.0.1")).unwrap();
        assert!(matches!(c.admit(ip("10.0.0.1")), Err(AdmitError::MaxPerIp)));
        // IP B should still have its own independent quota
        let _b1 = c.admit(ip("10.0.0.2")).unwrap();
        let _b2 = c.admit(ip("10.0.0.2")).unwrap();
        assert!(matches!(c.admit(ip("10.0.0.2")), Err(AdmitError::MaxPerIp)));
    }

    #[test]
    fn max_total_boundary_zero() {
        let c = make_charon(0, 10);
        // With max_total=0, every admit should be rejected
        let result = c.admit(ip("10.0.0.1"));
        assert!(matches!(result, Err(AdmitError::MaxTotal)));
        let result2 = c.admit(ip("10.0.0.2"));
        assert!(matches!(result2, Err(AdmitError::MaxTotal)));
    }
}