//! Per-IP rate limiting for the REST API.
//!
//! The API is reachable from the LAN, so without a limit a single client
//! can hammer it — opening a socket per request as fast as it can.  The
//! existing `MAX_IN_FLIGHT` cap only bounds concurrency, not request
//! rate.
//!
//! Mechanism
//! ---------
//! A token bucket per source IP.  Each bucket holds `capacity` tokens and
//! refills at `refill_per_second` up to that capacity.  A request costs
//! one token; a client with no tokens left gets a 429 with a
//! `Retry-After` header.
//!
//! Two buckets are kept per client: one for reads and one for mutations.
//! Reads are cheap and frequent — a Crestron polls `/state` every second
//! and must never be throttled.  Mutations are rare and expensive (they
//! travel through the main loop), so they get a tighter budget.
//!
//! Buckets are evicted once they have been idle long enough, so a
//! long-running server does not accumulate an entry per client that has
//! ever connected.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Which budget a request draws from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bucket {
    /// Read-only endpoints (`/state`, `/health`, `/inputs`).
    Read,
    /// Endpoints that change state.
    Write,
}

/// Limits for one bucket.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum tokens the bucket can hold — the burst size.
    pub capacity: u32,
    /// Tokens added per second.
    pub refill_per_second: f64,
}

impl Default for Limits {
    fn default() -> Self {
        // Reads: a 1 Hz poll with room to spare, plus a burst for a
        // client that polls faster.
        Self {
            capacity: 20,
            refill_per_second: 10.0,
        }
    }
}

/// Limits for the mutation bucket.
pub fn default_write_limits() -> Limits {
    // Writes: 5 per second sustained, 10 in a burst.  A Crestron sends
    // one command at a time, so this is generous.
    Limits {
        capacity: 10,
        refill_per_second: 5.0,
    }
}

/// One client's bucket state.
#[derive(Debug)]
struct BucketState {
    /// Tokens currently available.
    tokens: f64,
    /// When the bucket was last refilled.
    last_refill: Instant,
    /// When the client last made a request — used for eviction.
    last_seen: Instant,
}

impl BucketState {
    fn new(now: Instant, limits: &Limits) -> Self {
        Self {
            tokens: limits.capacity as f64,
            last_refill: now,
            last_seen: now,
        }
    }

    /// Add the tokens accrued since the last refill, capped at capacity.
    fn refill(&mut self, now: Instant, limits: &Limits) {
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens =
                (self.tokens + elapsed * limits.refill_per_second).min(limits.capacity as f64);
            self.last_refill = now;
        }
        self.last_seen = now;
    }
}

/// The set of buckets, one pair per source IP.
#[derive(Debug)]
pub struct RateLimiter {
    clients: Mutex<HashMap<String, ClientBuckets>>,
    read: Limits,
    write: Limits,
    /// A client idle for this long is dropped.
    idle_timeout: Duration,
}

/// One client's two buckets.
#[derive(Debug)]
struct ClientBuckets {
    read: BucketState,
    write: BucketState,
}

/// The outcome of a rate-limit check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Allowed.
    Allow,
    /// Rejected; the value is how long the client should wait, in
    /// seconds.
    Reject(u64),
}

impl RateLimiter {
    /// Build a limiter with the given limits.
    pub fn new(read: Limits, write: Limits) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            read,
            write,
            idle_timeout: Duration::from_secs(300),
        }
    }

    /// Build a limiter with the default limits.
    pub fn with_defaults() -> Self {
        Self::new(Limits::default(), default_write_limits())
    }

    /// Charge one request from `client`'s bucket.
    pub fn check(&self, client: &str, bucket: Bucket) -> Decision {
        let now = Instant::now();
        let limits = match bucket {
            Bucket::Read => self.read,
            Bucket::Write => self.write,
        };

        let mut clients = self.clients.lock().unwrap_or_else(|p| p.into_inner());

        // Drop clients that have gone quiet, so the map does not grow
        // without bound.
        if clients.len() > 64 {
            clients.retain(|_, c| now.duration_since(c.read.last_seen) < self.idle_timeout);
        }

        let entry = clients
            .entry(client.to_string())
            .or_insert_with(|| ClientBuckets {
                read: BucketState::new(now, &self.read),
                write: BucketState::new(now, &self.write),
            });

        let state = match bucket {
            Bucket::Read => &mut entry.read,
            Bucket::Write => &mut entry.write,
        };
        state.refill(now, &limits);

        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            Decision::Allow
        } else {
            // Seconds until one token is available.
            let deficit = 1.0 - state.tokens;
            let wait = (deficit / limits.refill_per_second).ceil() as u64;
            Decision::Reject(wait.max(1))
        }
    }

    /// Number of clients currently tracked — for diagnostics and tests.
    #[allow(dead_code)]
    pub fn tracked_clients(&self) -> usize {
        self.clients.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

/// Decide which bucket a request path draws from.
///
/// Anything under `/api/v1/` that is not a read endpoint is a mutation.
///
/// The bucket is chosen by path, not by method, so a mutation endpoint
/// draws on the write budget however it is called.  That matters now
/// that the mutations are POST-only: a GET on one of them is refused
/// with 405 before it can reach the limiter, and a GET that somehow
/// slipped through would still not be given the read budget.
///
/// The portal routes are not limited at all — they are browser traffic
/// and the portal polls gently.
pub fn bucket_for(path: &str) -> Option<Bucket> {
    if !path.starts_with("/api/v1/") {
        return None;
    }
    match path {
        "/api/v1/health" | "/api/v1/state" | "/api/v1/inputs" | "/api/v1/saves" => {
            Some(Bucket::Read)
        }
        _ => Some(Bucket::Write),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_endpoint_uses_the_read_bucket() {
        assert_eq!(bucket_for("/api/v1/state"), Some(Bucket::Read));
        assert_eq!(bucket_for("/api/v1/health"), Some(Bucket::Read));
        assert_eq!(bucket_for("/api/v1/inputs"), Some(Bucket::Read));
        assert_eq!(bucket_for("/api/v1/saves"), Some(Bucket::Read));
    }

    #[test]
    fn a_mutation_endpoint_uses_the_write_bucket() {
        assert_eq!(bucket_for("/api/v1/tool"), Some(Bucket::Write));
        assert_eq!(bucket_for("/api/v1/undo"), Some(Bucket::Write));
        assert_eq!(bucket_for("/api/v1/save"), Some(Bucket::Write));
        assert_eq!(bucket_for("/api/v1/input"), Some(Bucket::Write));
    }

    #[test]
    fn the_portal_routes_are_not_rate_limited() {
        assert_eq!(bucket_for("/"), None);
        assert_eq!(bucket_for("/portal/login"), None);
        assert_eq!(bucket_for("/index.html"), None);
    }

    #[test]
    fn requests_within_the_budget_are_allowed() {
        let rl = RateLimiter::new(
            Limits {
                capacity: 5,
                refill_per_second: 1.0,
            },
            Limits {
                capacity: 5,
                refill_per_second: 1.0,
            },
        );
        for _ in 0..5 {
            assert_eq!(rl.check("1.2.3.4", Bucket::Read), Decision::Allow);
        }
    }

    #[test]
    fn a_client_over_the_budget_is_rejected() {
        let rl = RateLimiter::new(
            Limits {
                capacity: 3,
                refill_per_second: 0.1,
            },
            Limits {
                capacity: 3,
                refill_per_second: 0.1,
            },
        );
        for _ in 0..3 {
            assert_eq!(rl.check("1.2.3.4", Bucket::Read), Decision::Allow);
        }
        let d = rl.check("1.2.3.4", Bucket::Read);
        assert!(matches!(d, Decision::Reject(_)), "expected a rejection");
        if let Decision::Reject(secs) = d {
            assert!(secs >= 1, "the retry hint must be at least a second");
        }
    }

    #[test]
    fn clients_are_limited_independently() {
        let rl = RateLimiter::new(
            Limits {
                capacity: 2,
                refill_per_second: 0.1,
            },
            Limits {
                capacity: 2,
                refill_per_second: 0.1,
            },
        );
        assert_eq!(rl.check("a", Bucket::Read), Decision::Allow);
        assert_eq!(rl.check("a", Bucket::Read), Decision::Allow);
        // A is now over budget...
        assert!(matches!(rl.check("a", Bucket::Read), Decision::Reject(_)));
        // ...but B is untouched.
        assert_eq!(rl.check("b", Bucket::Read), Decision::Allow);
    }

    #[test]
    fn the_read_and_write_buckets_are_separate() {
        let rl = RateLimiter::new(
            Limits {
                capacity: 2,
                refill_per_second: 0.1,
            },
            Limits {
                capacity: 10,
                refill_per_second: 5.0,
            },
        );
        assert_eq!(rl.check("a", Bucket::Read), Decision::Allow);
        assert_eq!(rl.check("a", Bucket::Read), Decision::Allow);
        assert!(matches!(rl.check("a", Bucket::Read), Decision::Reject(_)));
        // The write bucket still has plenty of budget.
        assert_eq!(rl.check("a", Bucket::Write), Decision::Allow);
    }

    #[test]
    fn a_one_hertz_poll_is_never_throttled() {
        // The default read budget must comfortably cover a Crestron
        // polling every second, plus headroom for a faster client.
        let rl = RateLimiter::with_defaults();
        for _ in 0..20 {
            assert_eq!(rl.check("crestron", Bucket::Read), Decision::Allow);
        }
    }
}
