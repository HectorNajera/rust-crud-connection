use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{config::Config, error::AppError};

/// Per-client-IP token bucket: each client may send `burst` requests at once,
/// refilled at `per_second` tokens per second.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Inner>,
}

struct Inner {
    per_second: f64,
    burst: f64,
    trust_proxy: bool,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

enum Decision {
    Allowed { remaining: u64 },
    Limited { retry_after: Duration },
}

impl RateLimiter {
    /// Returns `None` when rate limiting is disabled.
    pub fn from_config(cfg: &Config) -> Option<Self> {
        if !cfg.rate_limit_enabled || cfg.rate_limit_per_second <= 0.0 {
            return None;
        }
        Some(Self {
            inner: Arc::new(Inner {
                per_second: cfg.rate_limit_per_second,
                burst: f64::from(cfg.rate_limit_burst.max(1)),
                trust_proxy: cfg.rate_limit_trust_proxy,
                buckets: Mutex::new(HashMap::new()),
            }),
        })
    }

    fn check(&self, ip: IpAddr, now: Instant) -> Decision {
        let inner = &self.inner;
        let mut buckets = inner.buckets.lock().unwrap();
        let bucket = buckets.entry(ip).or_insert(Bucket { tokens: inner.burst, last: now });

        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * inner.per_second).min(inner.burst);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Decision::Allowed { remaining: bucket.tokens as u64 }
        } else {
            let wait = (1.0 - bucket.tokens) / inner.per_second;
            Decision::Limited { retry_after: Duration::from_secs_f64(wait) }
        }
    }

    /// Drops buckets that have been idle long enough to be full again;
    /// a fresh bucket behaves the same, so this only frees memory.
    fn evict_idle(&self, now: Instant) {
        let refill = Duration::from_secs_f64(self.inner.burst / self.inner.per_second);
        self.inner
            .buckets
            .lock()
            .unwrap()
            .retain(|_, b| now.saturating_duration_since(b.last) < refill);
    }

    pub fn spawn_eviction_task(&self) {
        let limiter = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                limiter.evict_idle(Instant::now());
            }
        });
    }

    fn client_ip(&self, req: &Request) -> Option<IpAddr> {
        if self.inner.trust_proxy {
            if let Some(ip) = forwarded_for(req.headers()) {
                return Some(ip);
            }
        }
        req.extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip())
    }
}

/// First address in `X-Forwarded-For` (the original client).
fn forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

pub async fn middleware(State(limiter): State<RateLimiter>, req: Request, next: Next) -> Response {
    let Some(ip) = limiter.client_ip(&req) else {
        return next.run(req).await;
    };
    let limit = HeaderValue::from(limiter.inner.burst as u64);

    match limiter.check(ip, Instant::now()) {
        Decision::Allowed { remaining } => {
            let mut res = next.run(req).await;
            let headers = res.headers_mut();
            headers.insert("x-ratelimit-limit", limit);
            headers.insert("x-ratelimit-remaining", HeaderValue::from(remaining));
            res
        }
        Decision::Limited { retry_after } => {
            tracing::debug!(%ip, "rate limit exceeded");
            let secs = retry_after.as_secs_f64().ceil().max(1.0) as u64;
            let mut res = AppError::RateLimited(secs).into_response();
            let headers = res.headers_mut();
            headers.insert("x-ratelimit-limit", limit);
            headers.insert("x-ratelimit-remaining", HeaderValue::from(0));
            res
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(per_second: f64, burst: u32) -> RateLimiter {
        RateLimiter {
            inner: Arc::new(Inner {
                per_second,
                burst: f64::from(burst),
                trust_proxy: false,
                buckets: Mutex::new(HashMap::new()),
            }),
        }
    }

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    #[test]
    fn allows_burst_then_limits() {
        let l = limiter(1.0, 3);
        let now = Instant::now();
        for _ in 0..3 {
            assert!(matches!(l.check(IP, now), Decision::Allowed { .. }));
        }
        assert!(matches!(l.check(IP, now), Decision::Limited { .. }));
    }

    #[test]
    fn refills_over_time() {
        let l = limiter(2.0, 1);
        let now = Instant::now();
        assert!(matches!(l.check(IP, now), Decision::Allowed { .. }));
        assert!(matches!(l.check(IP, now), Decision::Limited { .. }));
        let later = now + Duration::from_millis(500);
        assert!(matches!(l.check(IP, later), Decision::Allowed { .. }));
    }

    #[test]
    fn clients_are_independent() {
        let l = limiter(1.0, 1);
        let now = Instant::now();
        let other: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(matches!(l.check(IP, now), Decision::Allowed { .. }));
        assert!(matches!(l.check(other, now), Decision::Allowed { .. }));
    }

    #[test]
    fn parses_forwarded_for() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7, 10.0.0.1"));
        assert_eq!(forwarded_for(&h), Some("203.0.113.7".parse().unwrap()));
    }
}
