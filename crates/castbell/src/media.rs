use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const TTL: Duration = Duration::from_secs(60);

/// Pure expiry decision: `now - at >= ttl`.
pub fn expired(at: Instant, now: Instant, ttl: Duration) -> bool {
    now.duration_since(at) >= ttl
}

#[derive(Debug)]
pub struct ThumbnailCache {
    inner: Mutex<HashMap<String, (Instant, Vec<u8>)>>,
}

impl ThumbnailCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, id: impl Into<String>, bytes: Vec<u8>) {
        self.inner
            .lock()
            .unwrap()
            .insert(id.into(), (Instant::now(), bytes));
    }

    pub fn get(&self, id: &str) -> Option<Vec<u8>> {
        let mut m = self.inner.lock().unwrap();
        match m.get(id) {
            Some((at, b)) if !expired(*at, Instant::now(), TTL) => Some(b.clone()),
            Some(_) => {
                m.remove(id);
                None
            }
            None => None,
        }
    }

    pub fn remove(&self, id: &str) {
        self.inner.lock().unwrap().remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_after_ttl() {
        let at = Instant::now();
        let now = at + Duration::from_secs(61);
        assert!(expired(at, now, TTL));
    }

    #[test]
    fn not_expired_before_ttl() {
        let at = Instant::now();
        let now = at + Duration::from_secs(59);
        assert!(!expired(at, now, TTL));
    }

    #[test]
    fn expired_at_boundary() {
        let at = Instant::now();
        let now = at + TTL;
        assert!(expired(at, now, TTL));
    }

    #[test]
    fn cache_insert_get_remove() {
        let c = ThumbnailCache::new();
        c.insert("id1", vec![1, 2, 3]);
        assert_eq!(c.get("id1"), Some(vec![1, 2, 3]));
        c.remove("id1");
        assert_eq!(c.get("id1"), None);
    }

    #[test]
    fn cache_miss_unknown() {
        let c = ThumbnailCache::new();
        assert_eq!(c.get("nope"), None);
    }
}
