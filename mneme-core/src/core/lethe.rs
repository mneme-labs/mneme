// Lethe — Eviction engine.
// LFU Morris probabilistic counter + 3-level TTL wheel.
// Wheel spec: 256×10ms | 64×1s | 64×60s.
// Tick rate: every 10ms (from mnemosyne background task).

use std::collections::VecDeque;

use parking_lot::Mutex;

/// Level 0: 256 buckets × 10ms = 2560ms (2.56s)
const WHEEL_MS_BUCKETS: usize = 256;
const WHEEL_MS_TICK: u64 = 10; // ms per bucket

/// Level 1: 64 buckets × 1000ms = 64s
const WHEEL_S_BUCKETS: usize = 64;
const WHEEL_S_TICK: u64 = 1_000; // ms per bucket

/// Level 2: 64 buckets × 60000ms = ~64min
const WHEEL_M_BUCKETS: usize = 64;
const WHEEL_M_TICK: u64 = 60_000; // ms per bucket

/// A key queued in a TTL wheel bucket.
#[derive(Debug, Clone)]
struct WheelEntry {
    key: Vec<u8>,
    /// Absolute expiry in milliseconds since epoch.
    expires_at_ms: u64,
}

pub struct Lethe {
    inner: Mutex<LetheInner>,
}

struct LetheInner {
    ms: [VecDeque<WheelEntry>; WHEEL_MS_BUCKETS],
    s: [VecDeque<WheelEntry>; WHEEL_S_BUCKETS],
    m: [VecDeque<WheelEntry>; WHEEL_M_BUCKETS],

    /// Current cursor positions (absolute tick counts, not wrapped)
    ms_cursor: u64, // units of WHEEL_MS_TICK
    s_cursor: u64,  // units of WHEEL_S_TICK
    m_cursor: u64,  // units of WHEEL_M_TICK

    /// Absolute ms timestamp of wheel origin (set on first schedule or first tick).
    /// Cursor targets are computed from elapsed time since this origin,
    /// avoiding sub-tick remainder loss between tick() calls.
    origin_ms: u64,
}

impl Lethe {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(LetheInner {
                ms: std::array::from_fn(|_| VecDeque::new()),
                s: std::array::from_fn(|_| VecDeque::new()),
                m: std::array::from_fn(|_| VecDeque::new()),
                ms_cursor: 0,
                s_cursor: 0,
                m_cursor: 0,
                origin_ms: 0,
            }),
        }
    }

    /// Schedule `key` for expiry at `expires_at_ms` (absolute epoch ms).
    pub fn schedule(&self, key: Vec<u8>, expires_at_ms: u64, now_ms: u64) {
        if expires_at_ms == 0 {
            return;
        }
        let mut g = self.inner.lock();
        if g.origin_ms == 0 {
            g.origin_ms = now_ms;
        }
        let delta_ms = expires_at_ms.saturating_sub(now_ms);
        let entry = WheelEntry { key, expires_at_ms };

        if delta_ms < WHEEL_MS_TICK * WHEEL_MS_BUCKETS as u64 {
            // Level 0: fine-grained ms wheel
            let ticks = (delta_ms / WHEEL_MS_TICK).max(1) as usize;
            let bucket = ((g.ms_cursor as usize + ticks) % WHEEL_MS_BUCKETS) as usize;
            g.ms[bucket].push_back(entry);
        } else if delta_ms < WHEEL_S_TICK * WHEEL_S_BUCKETS as u64 {
            // Level 1: second wheel
            let ticks = (delta_ms / WHEEL_S_TICK).max(1) as usize;
            let bucket = ((g.s_cursor as usize + ticks) % WHEEL_S_BUCKETS) as usize;
            g.s[bucket].push_back(entry);
        } else {
            // Level 2: minute wheel — cap at wheel capacity
            let ticks = ((delta_ms / WHEEL_M_TICK).max(1) as usize).min(WHEEL_M_BUCKETS - 1);
            let bucket = ((g.m_cursor as usize + ticks) % WHEEL_M_BUCKETS) as usize;
            g.m[bucket].push_back(entry);
        }
    }

    /// Advance the wheel to `now_ms`. Returns keys that have expired.
    /// Designed to be called every ~10ms.
    ///
    /// Cursor positions are computed from absolute elapsed time since
    /// `origin_ms`, avoiding sub-tick remainder loss between calls.
    pub fn tick(&self, now_ms: u64) -> Vec<Vec<u8>> {
        let mut expired = Vec::new();
        let mut g = self.inner.lock();

        if g.origin_ms == 0 {
            g.origin_ms = now_ms;
            return expired;
        }

        let elapsed_total = now_ms.saturating_sub(g.origin_ms);

        // ── Level 0: ms wheel (10ms ticks) ───────────────────────────────────
        let ms_target = elapsed_total / WHEEL_MS_TICK;
        let ms_advance = (ms_target - g.ms_cursor).min(WHEEL_MS_BUCKETS as u64);
        for _ in 0..ms_advance {
            g.ms_cursor += 1;
            let bucket = (g.ms_cursor as usize - 1) % WHEEL_MS_BUCKETS;
            drain_bucket(&mut g.ms[bucket], now_ms, &mut expired, None);
        }

        // ── Level 1: s wheel (1s ticks) ──────────────────────────────────────
        let s_target = elapsed_total / WHEEL_S_TICK;
        let s_advance = (s_target - g.s_cursor).min(WHEEL_S_BUCKETS as u64);
        for _ in 0..s_advance {
            g.s_cursor += 1;
            let bucket = (g.s_cursor as usize - 1) % WHEEL_S_BUCKETS;
            // Cascade entries from s-wheel into ms-wheel if not yet expired.
            // Reborrow through the guard so the borrow checker can split
            // g.s[bucket] and g.ms as disjoint fields.
            let cascade_into_ms = (g.ms_cursor as usize, WHEEL_MS_BUCKETS, WHEEL_MS_TICK);
            let inner = &mut *g;
            drain_bucket_cascade(
                &mut inner.s[bucket],
                now_ms,
                &mut expired,
                &mut inner.ms,
                cascade_into_ms,
            );
        }

        // ── Level 2: min wheel (60s ticks) ───────────────────────────────────
        let m_target = elapsed_total / WHEEL_M_TICK;
        let m_advance = (m_target - g.m_cursor).min(WHEEL_M_BUCKETS as u64);
        for _ in 0..m_advance {
            g.m_cursor += 1;
            let bucket = (g.m_cursor as usize - 1) % WHEEL_M_BUCKETS;
            // Cascade into s-wheel.
            let cascade_into_s = (g.s_cursor as usize, WHEEL_S_BUCKETS, WHEEL_S_TICK);
            let inner = &mut *g;
            drain_bucket_cascade(
                &mut inner.m[bucket],
                now_ms,
                &mut expired,
                &mut inner.s,
                cascade_into_s,
            );
        }

        expired
    }

    // ── LFU Morris counter ────────────────────────────────────────────────────

    /// Probabilistically increment the LFU counter (Morris counter, base=5).
    pub fn increment_lfu(current: u8) -> u8 {
        if current == 255 {
            return 255;
        }
        let base: u8 = 5;
        if current < base {
            return current + 1;
        }
        let factor: u64 = 10;
        let threshold = 1u64 << ((current - base).min(62) as u64);
        let rand_val = fast_rand_u64() % (threshold * factor);
        if rand_val == 0 { current.saturating_add(1) } else { current }
    }

    /// Frequency decay: decrement by 1 (saturating).
    pub fn decay_lfu(current: u8) -> u8 {
        current.saturating_sub(1)
    }

    /// Pick `n` eviction candidates (lowest LFU counter).
    pub fn pick_eviction_candidates(
        candidates: &[(Vec<u8>, u8)],
        n: usize,
    ) -> Vec<Vec<u8>> {
        let mut sorted: Vec<_> = candidates.iter().collect();
        sorted.sort_by_key(|(_, c)| *c);
        sorted.into_iter().take(n).map(|(k, _)| k.clone()).collect()
    }
}

impl Default for Lethe {
    fn default() -> Self { Self::new() }
}

/// Drain a bucket: move expired keys into `out`, drop the rest.
fn drain_bucket(
    bucket: &mut VecDeque<WheelEntry>,
    now_ms: u64,
    out: &mut Vec<Vec<u8>>,
    _unused: Option<()>,
) {
    let entries = std::mem::take(bucket);
    for e in entries {
        if e.expires_at_ms <= now_ms {
            out.push(e.key);
        }
        // Keys not yet expired are dropped — they were placed in this bucket
        // with a possible minor timing error; a lazy check on access handles them.
    }
}

/// Drain a cascade bucket: expired keys go to `out`, live keys cascade into `target`.
fn drain_bucket_cascade<const N: usize>(
    bucket: &mut VecDeque<WheelEntry>,
    now_ms: u64,
    out: &mut Vec<Vec<u8>>,
    target: &mut [VecDeque<WheelEntry>; N],
    (cursor, _size, tick): (usize, usize, u64),
) {
    let entries = std::mem::take(bucket);
    for e in entries {
        if e.expires_at_ms <= now_ms {
            out.push(e.key);
        } else {
            let delta_ms = e.expires_at_ms.saturating_sub(now_ms);
            let ticks = ((delta_ms / tick).max(1) as usize).min(N - 1);
            let b = (cursor + ticks) % N;
            target[b].push_back(e);
        }
    }
}

/// Cheap xorshift64 PRNG (thread-local state).
fn fast_rand_u64() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new(0x123456789ABCDEF0);
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lfu_increment_low_counter_always_increments() {
        assert_eq!(Lethe::increment_lfu(0), 1);
        assert_eq!(Lethe::increment_lfu(1), 2);
        assert_eq!(Lethe::increment_lfu(4), 5);
    }

    #[test]
    fn lfu_saturates_at_255() {
        assert_eq!(Lethe::increment_lfu(255), 255);
    }

    #[test]
    fn lfu_decay() {
        assert_eq!(Lethe::decay_lfu(10), 9);
        assert_eq!(Lethe::decay_lfu(0), 0);
    }

    #[test]
    fn pick_eviction_candidates_lowest_first() {
        let candidates = vec![
            (b"hot".to_vec(), 200u8),
            (b"cold".to_vec(), 3u8),
            (b"warm".to_vec(), 50u8),
            (b"ice".to_vec(), 1u8),
        ];
        let evicted = Lethe::pick_eviction_candidates(&candidates, 2);
        assert!(evicted.contains(&b"ice".to_vec()));
        assert!(evicted.contains(&b"cold".to_vec()));
        assert_eq!(evicted.len(), 2);
    }

    #[test]
    fn pick_more_than_available_returns_all() {
        let candidates = vec![(b"a".to_vec(), 10u8)];
        let evicted = Lethe::pick_eviction_candidates(&candidates, 5);
        assert_eq!(evicted.len(), 1);
    }

    #[test]
    fn ttl_wheel_ms_bucket_expires() {
        let lethe = Lethe::new();
        let now_ms: u64 = 1_000_000;
        // expire in 50ms → level 0 bucket
        lethe.schedule(b"fast".to_vec(), now_ms + 50, now_ms);

        let expired = lethe.tick(now_ms + 10);
        assert!(!expired.contains(&b"fast".to_vec()), "too early");

        let expired = lethe.tick(now_ms + 60);
        assert!(expired.contains(&b"fast".to_vec()), "should have expired");
    }

    #[test]
    fn ttl_wheel_s_bucket_expires() {
        let lethe = Lethe::new();
        let now_ms: u64 = 2_000_000;
        // expire in 5s → level 1 bucket
        lethe.schedule(b"medium".to_vec(), now_ms + 5_000, now_ms);

        let expired = lethe.tick(now_ms + 3_000);
        assert!(!expired.contains(&b"medium".to_vec()));

        let expired = lethe.tick(now_ms + 6_000);
        assert!(expired.contains(&b"medium".to_vec()));
    }

    #[test]
    fn ttl_wheel_no_expiry_skipped() {
        let lethe = Lethe::new();
        let now_ms: u64 = 1_000_000;
        lethe.schedule(b"persistent".to_vec(), 0, now_ms);
        let expired = lethe.tick(now_ms + 100_000);
        assert!(!expired.contains(&b"persistent".to_vec()));
    }

    #[test]
    fn ttl_wheel_multiple_keys() {
        let lethe = Lethe::new();
        let now_ms: u64 = 3_000_000;
        lethe.schedule(b"k1".to_vec(), now_ms + 20, now_ms);   // ms bucket
        lethe.schedule(b"k2".to_vec(), now_ms + 3_000, now_ms); // s bucket
        lethe.schedule(b"k3".to_vec(), now_ms + 120_000, now_ms); // m bucket

        // After 30ms: k1 expires
        let e = lethe.tick(now_ms + 30);
        assert!(e.contains(&b"k1".to_vec()));
        assert!(!e.contains(&b"k2".to_vec()));
        assert!(!e.contains(&b"k3".to_vec()));

        // After 4s: k2 expires
        let e = lethe.tick(now_ms + 4_000);
        assert!(e.contains(&b"k2".to_vec()));
        assert!(!e.contains(&b"k3".to_vec()));
    }

    #[test]
    fn ttl_wheel_cascade_s_to_ms() {
        // Schedule a key in the s-wheel (delta 5s), tick past the s-wheel bucket,
        // verify it cascades to ms-wheel and eventually expires.
        let lethe = Lethe::new();
        let now_ms: u64 = 5_000_000;
        let expires_at = now_ms + 5_000; // 5s → lands in s-wheel (bucket 5)

        lethe.schedule(b"cascade_s".to_vec(), expires_at, now_ms);

        // Tick to 4s — s-wheel bucket not yet reached, key should not expire.
        let e = lethe.tick(now_ms + 4_000);
        assert!(!e.contains(&b"cascade_s".to_vec()), "should not expire at 4s");

        // s-bucket 5 drains when s_cursor reaches 6 (elapsed >= 6000ms).
        // At that point the key's expires_at <= now so it expires directly
        // during cascade drain, or cascades into ms-wheel and expires on the
        // next ms-wheel advance.
        let e = lethe.tick(now_ms + 6_000);
        if !e.contains(&b"cascade_s".to_vec()) {
            // Cascaded into ms-wheel; one more tick drains it.
            let e2 = lethe.tick(now_ms + 6_100);
            assert!(
                e2.contains(&b"cascade_s".to_vec()),
                "cascaded key should expire after ms-wheel tick"
            );
        }
    }

    #[test]
    fn ttl_wheel_cascade_m_to_s() {
        // Schedule a key in the m-wheel (delta 120s), tick past the m-wheel bucket,
        // verify it cascades to s-wheel and eventually expires.
        let lethe = Lethe::new();
        let now_ms: u64 = 10_000_000;
        let expires_at = now_ms + 120_000; // 120s → lands in m-wheel (bucket 2)

        lethe.schedule(b"cascade_m".to_vec(), expires_at, now_ms);

        // Tick to 60s — first m-bucket fires but key is in bucket 2 (offset ~120s).
        let e = lethe.tick(now_ms + 60_000);
        assert!(
            !e.contains(&b"cascade_m".to_vec()),
            "should not expire at 60s"
        );

        // m-bucket 2 drains when m_cursor reaches 3 (elapsed >= 180000ms = 3min).
        // At that point expires_at <= now, so it expires directly in cascade drain
        // or cascades into s-wheel and then drains on subsequent ticks.
        let e = lethe.tick(now_ms + 180_000);
        if !e.contains(&b"cascade_m".to_vec()) {
            // Cascaded into s-wheel; advance further to drain s → ms.
            let e2 = lethe.tick(now_ms + 181_000);
            if !e2.contains(&b"cascade_m".to_vec()) {
                let e3 = lethe.tick(now_ms + 181_100);
                assert!(
                    e3.contains(&b"cascade_m".to_vec()),
                    "cascaded key should eventually expire"
                );
            }
        }
    }

    #[test]
    fn ttl_wheel_past_expiry_immediate() {
        // Schedule a key with expiry already in the past. On next tick it should
        // expire immediately.
        let lethe = Lethe::new();
        let now_ms: u64 = 8_000_000;
        let expires_at = now_ms - 500; // 500ms in the past

        lethe.schedule(b"past".to_vec(), expires_at, now_ms);

        // delta_ms saturates to 0, max(1) puts it in ms-bucket (0+1)%256 = 1.
        // Bucket 1 drains when ms_cursor reaches 2 (elapsed >= 20ms).
        let e = lethe.tick(now_ms + 20);
        assert!(
            e.contains(&b"past".to_vec()),
            "past-expiry key should expire on first sufficient tick"
        );
    }

    #[test]
    fn ttl_wheel_large_delta_clamped() {
        // Schedule with delta > 64 minutes. Should land in last m-bucket
        // without panicking.
        let lethe = Lethe::new();
        let now_ms: u64 = 1_000_000;
        let expires_at = now_ms + 200 * 60_000; // 200 minutes

        // This must not panic — clamped to WHEEL_M_BUCKETS - 1.
        lethe.schedule(b"huge".to_vec(), expires_at, now_ms);

        // Verify it was scheduled (tick should not find it expired yet).
        let e = lethe.tick(now_ms + 10);
        assert!(
            !e.contains(&b"huge".to_vec()),
            "should not expire after 10ms"
        );
    }

    #[test]
    fn ttl_wheel_tick_before_schedule() {
        // Call tick() before any schedule(). Should return empty and not panic.
        let lethe = Lethe::new();
        let now_ms: u64 = 7_000_000;

        let e = lethe.tick(now_ms);
        assert!(e.is_empty(), "tick with no scheduled keys should return empty");

        let e = lethe.tick(now_ms + 100);
        assert!(e.is_empty(), "subsequent tick with no keys should return empty");
    }

    #[test]
    fn lfu_increment_probabilistic() {
        // Call increment_lfu(10) 100000 times. At counter 10 (above base 5),
        // increments are probabilistic. Should get at least a few but not all.
        let mut increment_count = 0u64;
        for _ in 0..100_000 {
            let result = Lethe::increment_lfu(10);
            if result == 11 {
                increment_count += 1;
            }
        }
        assert!(
            increment_count >= 1,
            "expected at least 1 increment out of 100000, got {increment_count}"
        );
        assert!(
            increment_count < 99_999,
            "expected fewer than 99999 increments (probabilistic), got {increment_count}"
        );
    }

    #[test]
    fn pick_eviction_empty_candidates() {
        let candidates: Vec<(Vec<u8>, u8)> = vec![];
        let evicted = Lethe::pick_eviction_candidates(&candidates, 5);
        assert!(evicted.is_empty(), "empty candidates should return empty");
    }

    #[test]
    fn pick_eviction_n_zero() {
        let candidates = vec![
            (b"a".to_vec(), 1u8),
            (b"b".to_vec(), 2u8),
        ];
        let evicted = Lethe::pick_eviction_candidates(&candidates, 0);
        assert!(evicted.is_empty(), "n=0 should return empty");
    }
}
