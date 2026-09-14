//! Bounded-age discovery cache. Scheduling and async execution belong to callers.
use crate::AgentSessionDiscovery;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct CachedDiscovery {
    observed_at: Instant,
    discovery: AgentSessionDiscovery,
}

pub struct DiscoveryCache {
    ttl: Duration,
    cache: Mutex<Option<CachedDiscovery>>,
}
impl DiscoveryCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            cache: Mutex::new(None),
        }
    }
    pub fn get(&self) -> Option<AgentSessionDiscovery> {
        let cache = self
            .cache
            .lock()
            .expect("agent session cache lock should hold");
        cache
            .as_ref()
            .filter(|cached| cached.observed_at.elapsed() < self.ttl)
            .map(|cached| cached.discovery.clone())
    }
    pub fn store(&self, discovery: AgentSessionDiscovery) {
        let mut cache = self
            .cache
            .lock()
            .expect("agent session cache lock should hold");
        *cache = Some(CachedDiscovery {
            observed_at: Instant::now(),
            discovery,
        });
    }
}
