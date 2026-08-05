//! Cross-cutting request concerns: CORS, security headers, rate limiting.
//!
//! All three are declared in the manifest rather than assembled by the
//! application author. A framework that makes security opt-in ships insecure
//! applications, so the defaults here are the safe ones and relaxing them is the
//! thing that takes a line of configuration.

use crate::manifest::{CorsConfig, RateLimitConfig, SecurityHeaders};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// A sharded token-bucket limiter keyed by principal (falling back to client IP).
///
/// Sharded because a single mutex over one map is a contention point at exactly
/// the moment you need the limiter most — when you are being flooded.
pub struct RateLimiter {
    shards: Vec<Mutex<BTreeMap<String, Bucket>>>,
    capacity: f64,
    refill_per_sec: f64,
    /// Buckets idle longer than this are evicted, so an attacker rotating keys
    /// cannot grow the map without bound.
    idle_eviction: Duration,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct RateDecision {
    pub allowed: bool,
    pub limit: u32,
    pub remaining: u32,
    pub retry_after_secs: u64,
}

const SHARDS: usize = 16;

impl RateLimiter {
    pub fn new(cfg: &RateLimitConfig) -> Self {
        let mut shards = Vec::with_capacity(SHARDS);
        for _ in 0..SHARDS {
            shards.push(Mutex::new(BTreeMap::new()));
        }
        Self {
            shards,
            capacity: cfg.burst.max(1) as f64,
            refill_per_sec: cfg.per_second.max(0.001),
            idle_eviction: Duration::from_secs(cfg.idle_eviction_secs),
        }
    }

    fn shard_for(&self, key: &str) -> &Mutex<BTreeMap<String, Bucket>> {
        // FNV-1a; cheap and good enough to spread keys across shards.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in key.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
        &self.shards[(hash as usize) % self.shards.len()]
    }

    pub fn check(&self, key: &str) -> RateDecision {
        let now = Instant::now();
        let shard = self.shard_for(key);
        let mut map = match shard.lock() {
            Ok(m) => m,
            // A poisoned lock must not become an outage; fail open on the
            // limiter rather than 500 every request behind it.
            Err(poisoned) => poisoned.into_inner(),
        };

        map.retain(|_, b| now.duration_since(b.last) < self.idle_eviction);

        let bucket = map.entry(key.to_string()).or_insert(Bucket {
            tokens: self.capacity,
            last: now,
        });

        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            RateDecision {
                allowed: true,
                limit: self.capacity as u32,
                remaining: bucket.tokens as u32,
                retry_after_secs: 0,
            }
        } else {
            let deficit = 1.0 - bucket.tokens;
            RateDecision {
                allowed: false,
                limit: self.capacity as u32,
                remaining: 0,
                retry_after_secs: (deficit / self.refill_per_sec).ceil() as u64,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

pub struct Cors {
    cfg: CorsConfig,
}

impl Cors {
    pub fn new(cfg: CorsConfig) -> Self {
        Self { cfg }
    }

    fn origin_allowed(&self, origin: &str) -> bool {
        // `*` with credentials is forbidden by the CORS spec and is a common
        // way to accidentally expose an authenticated API to every site on the
        // internet. Rejected at boot in `CorsConfig::validate`, not here.
        self.cfg.allow_origins.iter().any(|o| o == "*" || o == origin)
    }

    /// Headers to attach to a normal (non-preflight) response.
    pub fn headers_for(&self, request_origin: Option<&str>) -> Vec<(String, String)> {
        let Some(origin) = request_origin else {
            return Vec::new();
        };
        if !self.origin_allowed(origin) {
            return Vec::new();
        }

        let mut out = Vec::new();
        // Echo the concrete origin rather than `*` whenever credentials are in
        // play, and always `Vary` so caches do not serve one origin's response
        // to another.
        if self.cfg.allow_credentials {
            out.push(("access-control-allow-origin".into(), origin.to_string()));
            out.push(("access-control-allow-credentials".into(), "true".into()));
        } else if self.cfg.allow_origins.iter().any(|o| o == "*") {
            out.push(("access-control-allow-origin".into(), "*".into()));
        } else {
            out.push(("access-control-allow-origin".into(), origin.to_string()));
        }
        out.push(("vary".into(), "Origin".into()));

        if !self.cfg.expose_headers.is_empty() {
            out.push((
                "access-control-expose-headers".into(),
                self.cfg.expose_headers.join(", "),
            ));
        }
        out
    }

    /// Full preflight response, or `None` if this isn't a preflight we allow.
    pub fn preflight(&self, origin: Option<&str>) -> Option<Vec<(String, String)>> {
        let origin = origin?;
        if !self.origin_allowed(origin) {
            return None;
        }
        let mut out = self.headers_for(Some(origin));
        out.push((
            "access-control-allow-methods".into(),
            self.cfg.allow_methods.join(", "),
        ));
        out.push((
            "access-control-allow-headers".into(),
            self.cfg.allow_headers.join(", "),
        ));
        out.push(("access-control-max-age".into(), self.cfg.max_age_secs.to_string()));
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Security headers
// ---------------------------------------------------------------------------

/// Headers applied to every response unless explicitly disabled.
pub fn security_headers(cfg: &SecurityHeaders, is_https: bool) -> Vec<(String, String)> {
    if !cfg.enabled {
        return Vec::new();
    }
    let mut out = vec![
        ("x-content-type-options".into(), "nosniff".into()),
        ("x-frame-options".into(), cfg.frame_options.clone()),
        ("referrer-policy".into(), cfg.referrer_policy.clone()),
        (
            "cross-origin-opener-policy".into(),
            "same-origin".to_string(),
        ),
    ];
    if let Some(csp) = &cfg.content_security_policy {
        out.push(("content-security-policy".into(), csp.clone()));
    }
    // HSTS over plaintext would be ignored by browsers and is a footgun in
    // local development, so it is emitted only on TLS.
    if is_https && cfg.hsts_max_age_secs > 0 {
        out.push((
            "strict-transport-security".into(),
            format!("max-age={}; includeSubDomains", cfg.hsts_max_age_secs),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(per_second: f64, burst: u32) -> RateLimiter {
        RateLimiter::new(&RateLimitConfig {
            enabled: true,
            per_second,
            burst,
            idle_eviction_secs: 300,
        })
    }

    #[test]
    fn burst_is_allowed_then_refused() {
        let rl = limiter(1.0, 3);
        for i in 0..3 {
            assert!(rl.check("k").allowed, "request {i} should be allowed");
        }
        let d = rl.check("k");
        assert!(!d.allowed);
        assert!(d.retry_after_secs >= 1);
    }

    #[test]
    fn buckets_are_isolated_per_key() {
        let rl = limiter(1.0, 1);
        assert!(rl.check("a").allowed);
        assert!(!rl.check("a").allowed);
        assert!(rl.check("b").allowed, "one key must not exhaust another");
    }

    #[test]
    fn tokens_refill_over_time() {
        let rl = limiter(1000.0, 1);
        assert!(rl.check("k").allowed);
        assert!(!rl.check("k").allowed);
        std::thread::sleep(Duration::from_millis(5));
        assert!(rl.check("k").allowed, "bucket should have refilled");
    }

    fn cors(origins: &[&str], credentials: bool) -> Cors {
        Cors::new(CorsConfig {
            enabled: true,
            allow_origins: origins.iter().map(|s| s.to_string()).collect(),
            allow_credentials: credentials,
            ..CorsConfig::default()
        })
    }

    #[test]
    fn disallowed_origin_gets_no_cors_headers() {
        let c = cors(&["https://good.test"], false);
        assert!(c.headers_for(Some("https://evil.test")).is_empty());
        assert!(c.preflight(Some("https://evil.test")).is_none());
    }

    #[test]
    fn allowed_origin_is_echoed_and_varied() {
        let c = cors(&["https://good.test"], false);
        let h = c.headers_for(Some("https://good.test"));
        assert!(h.contains(&("access-control-allow-origin".into(), "https://good.test".into())));
        assert!(h.iter().any(|(k, v)| k == "vary" && v == "Origin"));
    }

    #[test]
    fn credentialed_cors_never_answers_with_a_wildcard() {
        let c = cors(&["*"], true);
        let h = c.headers_for(Some("https://any.test"));
        let origin = h.iter().find(|(k, _)| k == "access-control-allow-origin").unwrap();
        assert_eq!(origin.1, "https://any.test", "must echo, never '*', with credentials");
    }

    #[test]
    fn hsts_only_on_https() {
        let cfg = SecurityHeaders::default();
        let plain = security_headers(&cfg, false);
        let tls = security_headers(&cfg, true);
        assert!(!plain.iter().any(|(k, _)| k == "strict-transport-security"));
        assert!(tls.iter().any(|(k, _)| k == "strict-transport-security"));
    }

    #[test]
    fn nosniff_is_always_present_when_enabled() {
        let h = security_headers(&SecurityHeaders::default(), false);
        assert!(h.contains(&("x-content-type-options".into(), "nosniff".into())));
    }
}
