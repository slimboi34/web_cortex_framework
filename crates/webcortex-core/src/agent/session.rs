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

use super::{Conversation, Message};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Most messages one session keeps. The store is capped by count, but each
/// conversation grew by every turn, so one caller talking for long enough
/// held unbounded memory — and made every later turn resend all of it.
pub const MAX_SESSION_MESSAGES: usize = 400;

/// Most serialised bytes one session keeps (images included).
pub const MAX_SESSION_BYTES: usize = 8 * 1024 * 1024;

/// True for the message that starts a turn: a user message that is not a
/// batch of tool results. Cutting there keeps every `tool_use` beside its
/// `tool_result`.
fn starts_a_turn(m: &Message) -> bool {
    m.role == "user"
        && !m.content.as_array().is_some_and(|blocks| {
            blocks.iter().any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        })
}

fn approx_bytes(content: &Value) -> usize {
    serde_json::to_vec(content).map(|v| v.len()).unwrap_or(0)
}

/// Drop the oldest whole turns until the conversation is within
/// [`MAX_SESSION_MESSAGES`] and [`MAX_SESSION_BYTES`]. The newest turn is kept
/// even when it alone is over, since without it the session has no current
/// question. Returns how many messages were dropped.
pub fn bound(conversation: &mut Conversation) -> usize {
    let messages = &conversation.messages;
    let n = messages.len();
    let sizes: Vec<usize> = messages.iter().map(|m| approx_bytes(&m.content)).collect();
    let total: usize = sizes.iter().fold(0usize, |a, b| a.saturating_add(*b));
    if n <= MAX_SESSION_MESSAGES && total <= MAX_SESSION_BYTES {
        return 0;
    }
    let mut tail = 0usize;
    let mut fitting = None;
    let mut newest = None;
    for i in (0..n).rev() {
        tail = tail.saturating_add(sizes[i]);
        let fits = n - i <= MAX_SESSION_MESSAGES && tail <= MAX_SESSION_BYTES;
        if starts_a_turn(&messages[i]) {
            newest.get_or_insert(i);
            if fits {
                fitting = Some(i);
            }
        }
        if !fits {
            break;
        }
    }
    let Some(cut) = fitting.or(newest).filter(|c| *c > 0) else { return 0 };
    conversation.messages.drain(..cut);
    cut
}

pub struct SessionStore {
    inner: Mutex<HashMap<String, Entry>>,
    /// One lock per session with a turn in flight. Two turns on one session
    /// each loaded the same history and the second to finish overwrote the
    /// first; a turn now waits for the one ahead of it.
    turns: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Source of [`Entry::revision`]: store-wide, so a session that is
    /// forgotten and saved again never repeats an earlier revision.
    revisions: AtomicU64,
    capacity: usize,
    ttl: Duration,
}

struct Entry {
    conversation: Conversation,
    touched: Instant,
    revision: u64,
}

/// Held for the length of one turn on a session; see [`SessionStore::lock_turn`].
pub struct TurnGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl SessionStore {
    pub fn new(capacity: usize, ttl_secs: u64) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            revisions: AtomicU64::new(0),
            capacity: capacity.max(1),
            ttl: Duration::from_secs(ttl_secs.max(1)),
        }
    }

    pub fn key(agent: &str, principal_id: &str, session_id: &str) -> String {
        format!("{agent}\u{1}{principal_id}\u{1}{session_id}")
    }

    pub fn load(&self, key: &str) -> Option<Conversation> {
        self.load_with_revision(key).0
    }

    /// The session and its revision, which changes on every save. A missing
    /// or expired session is revision 0.
    pub fn load_with_revision(&self, key: &str) -> (Option<Conversation>, u64) {
        let mut map = self.lock();
        let now = Instant::now();
        match map.get_mut(key) {
            Some(e) if now.duration_since(e.touched) <= self.ttl => {
                e.touched = now;
                (Some(e.conversation.clone()), e.revision)
            }
            Some(_) => {
                map.remove(key);
                (None, 0)
            }
            None => (None, 0),
        }
    }

    /// Wait for any other turn on `key` to finish, then hold the session until
    /// the guard drops. Turns on one session run one after another, in the
    /// order they arrived (tokio's mutex is fair).
    pub async fn lock_turn(&self, key: &str) -> TurnGuard {
        let lock = {
            let mut turns = match self.turns.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            // Drop the locks no turn holds or waits on, so the map is bounded
            // by the turns in flight rather than by every session ever seen.
            turns.retain(|k, l| k == key || Arc::strong_count(l) > 1);
            turns.entry(key.to_string()).or_default().clone()
        };
        TurnGuard { _guard: lock.lock_owned().await }
    }

    pub fn save(&self, key: String, mut conversation: Conversation) {
        let dropped = bound(&mut conversation);
        if dropped > 0 {
            tracing::debug!(dropped, "session over its size bound; the oldest turns were dropped");
        }
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
        let revision = self.revisions.fetch_add(1, Ordering::Relaxed) + 1;
        map.insert(key, Entry { conversation, touched: now, revision });
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

    fn turn(i: usize) -> Vec<Message> {
        vec![
            Message { role: "user".into(), content: serde_json::json!(format!("question {i}")) },
            Message {
                role: "assistant".into(),
                content: serde_json::json!([{"type": "tool_use", "id": format!("t{i}"), "name": "x", "input": {}}]),
            },
            Message {
                role: "user".into(),
                content: serde_json::json!([{"type": "tool_result", "tool_use_id": format!("t{i}"), "content": "ok"}]),
            },
            Message { role: "assistant".into(), content: serde_json::json!([{"type": "text", "text": "done"}]) },
        ]
    }

    #[test]
    fn a_long_session_keeps_its_newest_whole_turns() {
        let s = SessionStore::new(10, 60);
        let mut c = Conversation::default();
        for i in 0..(MAX_SESSION_MESSAGES / 4 + 50) {
            c.messages.extend(turn(i));
        }
        s.save("k".into(), c);
        let kept = s.load("k").unwrap().messages;
        assert!(kept.len() <= MAX_SESSION_MESSAGES, "{}", kept.len());
        assert!(starts_a_turn(&kept[0]), "the cut lands on the start of a turn");
        assert_eq!(kept.last().unwrap().content[0]["text"], "done");
        assert_eq!(kept[kept.len() - 4].content, serde_json::json!(format!("question {}", MAX_SESSION_MESSAGES / 4 + 49)));
    }

    #[test]
    fn an_oversized_newest_turn_is_kept_alone() {
        let big = "x".repeat(MAX_SESSION_BYTES + 10);
        let mut c = Conversation::default();
        c.messages.extend(turn(0));
        c.messages.push(Message { role: "user".into(), content: serde_json::json!(big) });
        c.messages.push(Message { role: "assistant".into(), content: serde_json::json!([{"type": "text", "text": "ok"}]) });
        assert_eq!(bound(&mut c), 4);
        assert_eq!(c.messages.len(), 2);
        let mut small = Conversation::default();
        small.messages.extend(turn(0));
        assert_eq!(bound(&mut small), 0, "a small session is untouched");
    }

    #[test]
    fn every_save_moves_the_revision() {
        let s = SessionStore::new(10, 60);
        assert_eq!(s.load_with_revision("k").1, 0);
        s.save("k".into(), conv(1));
        let first = s.load_with_revision("k").1;
        s.save("k".into(), conv(2));
        let second = s.load_with_revision("k").1;
        assert!(first > 0 && second > first);
        s.forget("k");
        s.save("k".into(), conv(1));
        assert!(s.load_with_revision("k").1 > second, "a re-created session never repeats a revision");
    }

    #[tokio::test]
    async fn turns_on_one_session_run_one_at_a_time() {
        let s = Arc::new(SessionStore::new(10, 60));
        let first = s.lock_turn("k").await;
        let waiting = tokio::spawn({
            let s = s.clone();
            async move { s.lock_turn("k").await; }
        });
        let _other = s.lock_turn("other").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "a second turn on the same session must wait");
        drop(first);
        tokio::time::timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap();
        let _ = s.lock_turn("k").await;
        assert!(s.turns.lock().unwrap().len() <= 2, "idle locks are dropped");
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
