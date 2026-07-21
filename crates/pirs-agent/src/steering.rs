//! Steering — queue messages into a *running* agent and have them injected at the
//! next safe boundary.
//!
//! The agent loop already re-polls its steering hook at every turn boundary (and
//! drains it once before the first turn of each phase). What was missing was an
//! ergonomic, shareable queue to feed that hook. [`SteeringQueue`] is that: a
//! cloneable handle backed by one shared buffer. Anything holding a clone can
//! [`push`](SteeringQueue::push) a message while the agent runs; the loop picks it
//! up at its next turn (mid-phase) or at the start of the next phase (between
//! phases), because [`as_hook`](SteeringQueue::as_hook) plugs straight into
//! [`Hooks::get_steering_messages`](crate::events::Hooks).
//!
//! This is the strategy-path counterpart to the [`Agent`](crate::agent::Agent)'s
//! built-in `steer()` — the same idea, exposed as a standalone primitive so the
//! phase-driven executor (and any other consumer that drives [`run_agent_loop`]
//! directly) can be steered too.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use pirs_ai::Message;

use crate::events::MessageSourceHook;

/// How much of the queue a single poll consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    /// Drain everything queued so far at each boundary (the default): a burst of
    /// steering messages all land together.
    #[default]
    All,
    /// Release one message per boundary, so multi-step steering is paced across
    /// turns instead of dumped at once.
    OneAtATime,
}

/// A cloneable handle to a shared steering buffer. Clones share one queue.
///
/// Optional **scope keys** (e.g. `telegram:chat:123`) isolate multi-chat steers
/// so one channel cannot inject into another session's queue.
#[derive(Clone, Default)]
pub struct SteeringQueue {
    inner: Arc<Mutex<VecDeque<String>>>,
    mode: QueueMode,
    /// When set, only steers with matching scope are drained (session hygiene).
    scope: Option<String>,
    /// Scoped buckets: key → messages (shared across clones of the same base).
    scoped: Arc<Mutex<std::collections::HashMap<String, VecDeque<String>>>>,
}

impl SteeringQueue {
    /// An empty queue that drains all pending messages at each boundary.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty queue with an explicit [`QueueMode`].
    pub fn with_mode(mode: QueueMode) -> Self {
        SteeringQueue {
            inner: Arc::new(Mutex::new(VecDeque::new())),
            mode,
            scope: None,
            scoped: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Bind this handle to a session scope key (channel/chat identity).
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// Queue a message for injection at the running agent's next boundary. FIFO.
    pub fn push(&self, text: impl Into<String>) {
        let text = text.into();
        if let Some(scope) = &self.scope {
            self.scoped
                .lock()
                .unwrap()
                .entry(scope.clone())
                .or_default()
                .push_back(text);
        } else {
            self.inner.lock().unwrap().push_back(text);
        }
    }

    /// Push into an explicit scope without rebinding this handle.
    pub fn push_scoped(&self, scope: &str, text: impl Into<String>) {
        self.scoped
            .lock()
            .unwrap()
            .entry(scope.to_string())
            .or_default()
            .push_back(text.into());
    }

    /// Nothing pending for this handle's scope (or global queue)?
    pub fn is_empty(&self) -> bool {
        if let Some(scope) = &self.scope {
            self.scoped
                .lock()
                .unwrap()
                .get(scope)
                .map(|q| q.is_empty())
                .unwrap_or(true)
        } else {
            self.inner.lock().unwrap().is_empty()
        }
    }

    /// Number of messages still queued for this handle.
    pub fn len(&self) -> usize {
        if let Some(scope) = &self.scope {
            self.scoped
                .lock()
                .unwrap()
                .get(scope)
                .map(|q| q.len())
                .unwrap_or(0)
        } else {
            self.inner.lock().unwrap().len()
        }
    }

    /// Take pending messages per the queue's [`QueueMode`], as user [`Message`]s.
    /// This is what the loop's steering hook calls each time it polls.
    pub fn drain(&self) -> Vec<Message> {
        if let Some(scope) = &self.scope {
            let mut map = self.scoped.lock().unwrap();
            let q = map.entry(scope.clone()).or_default();
            match self.mode {
                QueueMode::All => q.drain(..).map(Message::user).collect(),
                QueueMode::OneAtATime => q.pop_front().map(Message::user).into_iter().collect(),
            }
        } else {
            let mut q = self.inner.lock().unwrap();
            match self.mode {
                QueueMode::All => q.drain(..).map(Message::user).collect(),
                QueueMode::OneAtATime => q.pop_front().map(Message::user).into_iter().collect(),
            }
        }
    }

    /// A [`MessageSourceHook`] over this queue — assign it to
    /// [`Hooks::get_steering_messages`](crate::events::Hooks) so the loop drains
    /// the queue at every turn/phase boundary.
    pub fn as_hook(&self) -> MessageSourceHook {
        let this = self.clone();
        Arc::new(move || this.drain())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(msgs: &[Message]) -> Vec<String> {
        use pirs_ai::UserContent;
        msgs.iter()
            .map(|m| match m {
                Message::User(u) => match &u.content {
                    UserContent::Text(t) => t.clone(),
                    UserContent::Blocks(b) => b
                        .iter()
                        .filter_map(|x| x.as_text())
                        .collect::<Vec<_>>()
                        .join(""),
                },
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn push_then_drain_is_fifo() {
        let q = SteeringQueue::new();
        assert!(q.is_empty());
        q.push("first");
        q.push("second");
        assert_eq!(q.len(), 2);
        let drained = q.drain();
        assert_eq!(texts(&drained), vec!["first", "second"]);
        assert!(q.is_empty(), "drain-all empties the queue");
    }

    #[test]
    fn one_at_a_time_paces_across_polls() {
        let q = SteeringQueue::with_mode(QueueMode::OneAtATime);
        q.push("a");
        q.push("b");
        assert_eq!(texts(&q.drain()), vec!["a"]);
        assert_eq!(texts(&q.drain()), vec!["b"]);
        assert!(q.drain().is_empty());
    }

    #[test]
    fn clones_share_one_buffer() {
        let q = SteeringQueue::new();
        let handle = q.clone();
        handle.push("from clone");
        // The original sees the clone's push — one shared queue.
        assert_eq!(q.len(), 1);
        assert_eq!(texts(&q.drain()), vec!["from clone"]);
    }

    #[test]
    fn as_hook_drains_the_queue_when_called() {
        let q = SteeringQueue::new();
        let hook = q.as_hook();
        q.push("steer me");
        let first = hook(); // simulates a turn-boundary poll
        assert_eq!(texts(&first), vec!["steer me"]);
        // A second poll with nothing queued yields nothing.
        assert!(hook().is_empty());
    }

    #[test]
    fn scoped_steers_do_not_cross_sessions() {
        let base = SteeringQueue::new();
        let a = base.clone().with_scope("chat:a");
        let b = base.clone().with_scope("chat:b");
        a.push("only-a");
        b.push("only-b");
        assert_eq!(texts(&a.drain()), vec!["only-a"]);
        assert_eq!(texts(&b.drain()), vec!["only-b"]);
        assert!(a.is_empty());
        // Global queue stays empty when using scopes.
        assert!(base.is_empty());
    }
}
