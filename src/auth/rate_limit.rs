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
}
