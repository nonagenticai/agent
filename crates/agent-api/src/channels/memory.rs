//! In-process, bounded state for the phase-A edge (B11): the recent turns of each channel conversation (so a Telegram
//! or web chat is multi-turn although the interaction seam is stateless) and the provider message ids
//! already taken (providers retry). Both are lost on a restart and are per replica; B4's conversation tables replace
//! the history, B11 phase B the dedupe (Redis `NX`), so neither grows a persistence story here.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::connector::ChannelKind;
use crate::interaction::model::{Message, Role};

/// Messages kept per conversation (user + assistant), oldest dropped first.
pub const MAX_TURN_MESSAGES: usize = 20;
/// Characters kept per conversation; older messages are dropped until it fits.
pub const MAX_HISTORY_CHARS: usize = 16_000;
/// Conversations kept; the least recently used is dropped beyond it.
pub const MAX_CONVERSATIONS: usize = 2_000;
/// A conversation idle this long starts again from nothing.
pub const HISTORY_TTL: Duration = Duration::from_secs(2 * 60 * 60);
/// Provider message ids remembered for dedupe, and for how long (providers retry within minutes to hours,
/// but a retry that late is answered again rather than held in memory).
pub const MAX_SEEN: usize = 20_000;
pub const SEEN_TTL: Duration = Duration::from_secs(60 * 60);

type Key = (ChannelKind, String);

struct Conversation {
    messages: Vec<Message>,
    touched: Instant,
}

#[derive(Default)]
pub struct ConversationMemory {
    inner: Mutex<HashMap<Key, Conversation>>,
}

impl ConversationMemory {
    /// The kept turns of a conversation (empty when unknown or expired).
    pub fn history(&self, kind: ChannelKind, conversation: &str) -> Vec<Message> {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let key = (kind, conversation.to_string());
        match m.get(&key) {
            Some(c) if c.touched.elapsed() < HISTORY_TTL => c.messages.clone(),
            Some(_) => {
                m.remove(&key);
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// Record one answered turn.
    pub fn record(&self, kind: ChannelKind, conversation: &str, user: &str, assistant: &str) {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= MAX_CONVERSATIONS && !m.contains_key(&(kind, conversation.to_string())) {
            m.retain(|_, c| c.touched.elapsed() < HISTORY_TTL);
            if m.len() >= MAX_CONVERSATIONS
                && let Some(oldest) = m
                    .iter()
                    .min_by_key(|(_, c)| c.touched)
                    .map(|(k, _)| k.clone())
            {
                m.remove(&oldest);
            }
        }
        let c = m
            .entry((kind, conversation.to_string()))
            .or_insert_with(|| Conversation {
                messages: Vec::new(),
                touched: Instant::now(),
            });
        if c.touched.elapsed() >= HISTORY_TTL {
            c.messages.clear();
        }
        c.touched = Instant::now();
        c.messages.push(Message::text(Role::User, user));
        c.messages.push(Message::text(Role::Assistant, assistant));
        let excess = c.messages.len().saturating_sub(MAX_TURN_MESSAGES);
        c.messages.drain(..excess);
        while c.messages.len() > 2
            && c.messages
                .iter()
                .map(|m| m.joined().chars().count())
                .sum::<usize>()
                > MAX_HISTORY_CHARS
        {
            c.messages.drain(..2);
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Default)]
pub struct Dedupe {
    inner: Mutex<HashMap<Key, Instant>>,
}

impl Dedupe {
    /// True the first time `(kind, id)` is seen within [`SEEN_TTL`]; false for a retry.
    pub fn first_time(&self, kind: ChannelKind, id: &str) -> bool {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let key = (kind, id.to_string());
        if m.get(&key).is_some_and(|at| at.elapsed() < SEEN_TTL) {
            return false;
        }
        if m.len() >= MAX_SEEN {
            m.retain(|_, at| at.elapsed() < SEEN_TTL);
            if m.len() >= MAX_SEEN
                && let Some(oldest) = m.iter().min_by_key(|(_, at)| **at).map(|(k, _)| k.clone())
            {
                m.remove(&oldest);
            }
        }
        m.insert(key, Instant::now());
        true
    }

    /// Forget `(kind, id)`: a message the edge took but could not hand to core (R6: answered 503), so the provider's
    /// retry is processed rather than dropped as a duplicate.
    pub fn forget(&self, kind: ChannelKind, id: &str) {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        m.remove(&(kind, id.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_keeps_turns_per_conversation_and_kind() {
        let m = ConversationMemory::default();
        m.record(ChannelKind::Telegram, "c1", "hi", "hello");
        m.record(ChannelKind::Telegram, "c1", "and?", "more");
        assert_eq!(m.history(ChannelKind::Telegram, "c1").len(), 4);
        assert!(m.history(ChannelKind::WebChat, "c1").is_empty());
        assert!(m.history(ChannelKind::Telegram, "c2").is_empty());
        assert_eq!(m.history(ChannelKind::Telegram, "c1")[0].role, Role::User);
    }

    #[test]
    fn history_is_bounded_by_messages_and_characters() {
        let m = ConversationMemory::default();
        for i in 0..30 {
            m.record(ChannelKind::WebChat, "c", &format!("q{i}"), "a");
        }
        let h = m.history(ChannelKind::WebChat, "c");
        assert_eq!(h.len(), MAX_TURN_MESSAGES);
        assert_eq!(h[0].joined(), "q20");
        let big = "x".repeat(MAX_HISTORY_CHARS / 2);
        m.record(ChannelKind::WebChat, "d", &big, "a");
        m.record(ChannelKind::WebChat, "d", &big, "a");
        m.record(ChannelKind::WebChat, "d", &big, "a");
        let total: usize = m
            .history(ChannelKind::WebChat, "d")
            .iter()
            .map(|x| x.joined().chars().count())
            .sum();
        assert!(total <= MAX_HISTORY_CHARS);
    }

    #[test]
    fn dedupe_takes_an_id_once_per_kind() {
        let d = Dedupe::default();
        assert!(d.first_time(ChannelKind::WebChat, "Ev1"));
        assert!(!d.first_time(ChannelKind::WebChat, "Ev1"));
        assert!(d.first_time(ChannelKind::Telegram, "Ev1"));
        d.forget(ChannelKind::WebChat, "Ev1");
        assert!(
            d.first_time(ChannelKind::WebChat, "Ev1"),
            "a forgotten id is new again"
        );
    }
}
