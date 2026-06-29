use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Simple in-memory rate limiter using a sliding window.
///
/// Tracks request timestamps per key (e.g., username or IP) and rejects
/// requests that exceed `max_requests` within the configured `window`.
pub struct RateLimiter {
    requests: Mutex<HashMap<String, Vec<Instant>>>,
    max_requests: usize,
    window: Duration,
}

impl RateLimiter {
    /// Create a new rate limiter.
    ///
    /// - `max_requests`: maximum number of requests allowed per window.
    /// - `window_secs`: window duration in seconds.
    pub fn new(max_requests: usize, window_secs: u64) -> Self {
        Self {
            requests: Mutex::new(HashMap::new()),
            max_requests,
            window: Duration::from_secs(window_secs),
        }
    }

    /// Check whether a request for the given key is allowed.
    ///
    /// Returns `true` if the request is allowed, `false` if rate limited.
    pub fn check(&self, key: &str) -> bool {
        let now = Instant::now();
        let cutoff = now - self.window;

        let mut map = self.requests.lock().unwrap();

        // Opportunistic eviction: keys are attacker-controlled (usernames/IPs),
        // so the map could grow unbounded under a spray of distinct keys. When it
        // gets large, drop keys whose window has fully expired — bounding memory
        // without paying an O(n) sweep on every call.
        if map.len() > 1024 {
            map.retain(|_, timestamps| timestamps.iter().any(|t| *t > cutoff));
        }

        let timestamps = map.entry(key.to_string()).or_default();

        // Remove expired entries
        timestamps.retain(|t| *t > cutoff);

        if timestamps.len() >= self.max_requests {
            false
        } else {
            timestamps.push(now);
            true
        }
    }

    /// Whether `key` has already reached the limit within the window, WITHOUT
    /// recording a new attempt. Use this to reject before doing expensive work
    /// (e.g. Argon2) while only counting *failures* via `record_failure`, so a
    /// legitimate burst of successful auths (e.g. a `docker push`, which makes
    /// many authenticated requests in a row) is never throttled.
    pub fn is_limited(&self, key: &str) -> bool {
        let cutoff = Instant::now() - self.window;
        let mut map = self.requests.lock().unwrap();
        match map.get_mut(key) {
            Some(timestamps) => {
                timestamps.retain(|t| *t > cutoff);
                timestamps.len() >= self.max_requests
            }
            None => false,
        }
    }

    /// Record one attempt (a failed one) for `key`, evicting expired entries.
    pub fn record_failure(&self, key: &str) {
        let now = Instant::now();
        let cutoff = now - self.window;
        let mut map = self.requests.lock().unwrap();
        if map.len() > 1024 {
            map.retain(|_, timestamps| timestamps.iter().any(|t| *t > cutoff));
        }
        let timestamps = map.entry(key.to_string()).or_default();
        timestamps.retain(|t| *t > cutoff);
        timestamps.push(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter_allows_within_limit() {
        let limiter = RateLimiter::new(3, 60);
        assert!(limiter.check("user1"));
        assert!(limiter.check("user1"));
        assert!(limiter.check("user1"));
        // 4th should be blocked
        assert!(!limiter.check("user1"));
    }

    #[test]
    fn test_rate_limiter_different_keys() {
        let limiter = RateLimiter::new(2, 60);
        assert!(limiter.check("user1"));
        assert!(limiter.check("user1"));
        assert!(!limiter.check("user1"));
        // Different key should still be allowed
        assert!(limiter.check("user2"));
        assert!(limiter.check("user2"));
        assert!(!limiter.check("user2"));
    }

    #[test]
    fn test_is_limited_only_after_recorded_failures() {
        let limiter = RateLimiter::new(3, 60);
        // is_limited never records, so it can be polled freely.
        assert!(!limiter.is_limited("u"));
        assert!(!limiter.is_limited("u"));
        limiter.record_failure("u");
        limiter.record_failure("u");
        assert!(!limiter.is_limited("u"), "2 failures < limit of 3");
        limiter.record_failure("u");
        assert!(limiter.is_limited("u"), "3 failures reaches the limit");
    }
}
