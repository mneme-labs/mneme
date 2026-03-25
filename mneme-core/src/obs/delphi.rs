// Delphi — SLOWLOG ring buffer + MONITOR broadcast stream.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// One SLOWLOG entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowEntry {
    /// Monotonic microseconds since process start.
    pub elapsed_us: u64,
    pub cmd: String,
    pub key: Vec<u8>,
    pub duration_us: u64,
}

/// One MONITOR event (every command, regardless of speed).
#[derive(Debug, Clone)]
pub struct MonitorEvent {
    pub elapsed_us: u64,
    pub cmd: String,
    pub key: Vec<u8>,
    pub duration_us: u64,
}

pub struct Delphi {
    inner: Arc<DelphiInner>,
}

struct DelphiInner {
    threshold_us: u64,
    ring: Mutex<RingBuf<SlowEntry>>,
    monitor_tx: broadcast::Sender<MonitorEvent>,
    start: Instant,
}

impl Delphi {
    pub fn new(threshold_us: u64, max_entries: usize) -> Self {
        let (monitor_tx, _) = broadcast::channel(4096);
        Self {
            inner: Arc::new(DelphiInner {
                threshold_us,
                ring: Mutex::new(RingBuf::new(max_entries)),
                monitor_tx,
                start: Instant::now(),
            }),
        }
    }

    /// Record a completed command. Adds to SLOWLOG if `duration_us >= threshold`.
    /// Always broadcasts to active MONITOR subscribers.
    pub fn record(&self, cmd: impl Into<String>, key: Vec<u8>, duration_us: u64) {
        let cmd = cmd.into();
        let elapsed_us = self.inner.start.elapsed().as_micros() as u64;

        // MONITOR broadcast (fire-and-forget; lagging receivers get dropped)
        let _ = self.inner.monitor_tx.send(MonitorEvent {
            elapsed_us,
            cmd: cmd.clone(),
            key: key.clone(),
            duration_us,
        });

        // SLOWLOG
        if duration_us >= self.inner.threshold_us {
            self.inner.ring.lock().push(SlowEntry {
                elapsed_us,
                cmd,
                key,
                duration_us,
            });
        }
    }

    /// Subscribe to the MONITOR stream. Returns a receiver that yields
    /// all subsequent commands. Receiver lag causes oldest events to be dropped.
    pub fn subscribe_monitor(&self) -> broadcast::Receiver<MonitorEvent> {
        self.inner.monitor_tx.subscribe()
    }

    /// Return up to `count` most recent SLOWLOG entries (newest first).
    pub fn get_slowlog(&self, count: usize) -> Vec<SlowEntry> {
        self.inner.ring.lock().recent(count)
    }

    /// Reset the SLOWLOG ring buffer.
    pub fn reset_slowlog(&self) {
        self.inner.ring.lock().clear();
    }

    /// Current number of active MONITOR subscribers.
    pub fn monitor_subscriber_count(&self) -> usize {
        self.inner.monitor_tx.receiver_count()
    }
}

impl Clone for Delphi {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

// ── fixed-capacity ring buffer ────────────────────────────────────────────────

struct RingBuf<T> {
    buf: Vec<Option<T>>,
    head: usize,
    len: usize,
    cap: usize,
}

impl<T: Clone> RingBuf<T> {
    fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            buf: vec![None; cap],
            head: 0,
            len: 0,
            cap,
        }
    }

    fn push(&mut self, item: T) {
        self.buf[self.head] = Some(item);
        self.head = (self.head + 1) % self.cap;
        if self.len < self.cap {
            self.len += 1;
        }
    }

    /// Return up to `n` most recent entries, newest first.
    fn recent(&self, n: usize) -> Vec<T> {
        let take = n.min(self.len);
        let mut out = Vec::with_capacity(take);
        for i in 0..take {
            let idx = (self.head + self.cap - 1 - i) % self.cap;
            if let Some(ref item) = self.buf[idx] {
                out.push(item.clone());
            }
        }
        out
    }

    fn clear(&mut self) {
        for slot in &mut self.buf {
            *slot = None;
        }
        self.head = 0;
        self.len = 0;
    }
}
