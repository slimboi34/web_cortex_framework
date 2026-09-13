//! Multi-turn agent sessions.
//!
//! A session is a conversation kept between requests so an agent can be
//! talked to rather than only asked. Sessions are in-memory, bounded, and
//! keyed by *principal* as well as by the client-supplied id, so two callers
//! presenting the same `session_id` never see each other's history.
//!
//! Ephemeral by design: a session does not survive a restart, and the runtime
//! says so rather than pretending otherwise. Durable conversation state is
//! application data — put it in a table.

use super::Conversation;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct SessionStore {
    inner: Mutex<HashMap<String, Entry>>,
    capacity: usize,
    ttl: Duration,
}

struct Entry {
    conversation: Conversation,
    touched: Instant,
}

impl SessionStore {
    pub fn new(capacity: usize, ttl_secs: u64) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
            ttl: Duration::from_secs(ttl_secs.max(1)),
        }
    }

    pub fn key(agent: &str, principal_id: &str, session_id: &str) -> String {
        format!("{agent}\u{1}{principal_id}\u{1}{session_id}")
    }

    pub fn load(&self, key: &str) -> Option<Conversation> {
        let mut map = self.lock();
        let now = Instant::now();
        match map.get_mut(key) {
            Some(e) if now.duration_since(e.touched) <= self.ttl => {
                e.touched = now;
                Some(e.conversation.clone())
            }
            Some(_) => {
                map.remove(key);
                None
            }
            None => None,
        }
    }

    pub fn save(&self, key: String, conversation: Conversation) {
        let mut map = self.lock();
        let now = Instant::now();
        map.retain(|_, e| now.duration_since(e.touched) <= self.ttl);
        if map.len() >= self.capacity && !map.contains_key(&key) {
            // Evict the least recently touched entry.
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.touched)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(key, Entry { conversation, touched: now });
    }

    pub fn forget(&self, key: &str) -> bool {
        self.lock().remove(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Message;

    fn conv(n: usize) -> Conversation {
        Conversation {
            messages: (0..n)
                .map(|i| Message { role: "user".into(), content: serde_json::json!(i) })
                .collect(),
        }
    }

    #[test]
    fn round_trips_and_is_keyed_by_principal() {
        let s = SessionStore::new(10, 60);
        s.save(SessionStore::key("a", "u1", "s"), conv(2));
        assert_eq!(s.load(&SessionStore::key("a", "u1", "s")).unwrap().messages.len(), 2);
        assert!(s.load(&SessionStore::key("a", "u2", "s")).is_none(), "another principal must not see it");
    }

    #[test]
    fn capacity_evicts_the_least_recently_used() {
        let s = SessionStore::new(2, 60);
        s.save("a".into(), conv(1));
        s.save("b".into(), conv(1));
        let _ = s.load("a");
        s.save("c".into(), conv(1));
        assert!(s.load("b").is_none(), "b was the least recently touched");
        assert!(s.load("a").is_some());
        assert_eq!(s.len(), 2);
    }
}
