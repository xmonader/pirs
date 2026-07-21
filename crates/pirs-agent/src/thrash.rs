//! Loop / mistake detectors — stop thrash without requiring `--weak` packs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Signature of a tool call for identity thrash detection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolSignature {
    pub name: String,
    /// Stable args fingerprint (JSON or hash string).
    pub args_key: String,
}

impl ToolSignature {
    pub fn new(name: impl Into<String>, args: &serde_json::Value) -> Self {
        Self {
            name: name.into(),
            args_key: compact_args(args),
        }
    }
}

fn compact_args(v: &serde_json::Value) -> String {
    // Stable, short: serialize without whitespace.
    let s = v.to_string();
    if s.len() <= 400 {
        s
    } else {
        format!("{}…", &s[..400])
    }
}

/// Tracks repeated identical tool signatures.
#[derive(Debug, Clone)]
pub struct LoopDetectionTracker {
    window: usize,
    /// Stop when the same signature appears this many times in the window.
    max_repeats: usize,
    recent: VecDeque<ToolSignature>,
}

impl Default for LoopDetectionTracker {
    fn default() -> Self {
        Self::new(12, 3)
    }
}

impl LoopDetectionTracker {
    pub fn new(window: usize, max_repeats: usize) -> Self {
        Self {
            window: window.max(1),
            max_repeats: max_repeats.max(2),
            recent: VecDeque::new(),
        }
    }

    /// Record a tool call. Returns a stop message if thrashing.
    pub fn observe(&mut self, sig: ToolSignature) -> Option<String> {
        self.recent.push_back(sig.clone());
        while self.recent.len() > self.window {
            self.recent.pop_front();
        }
        let count = self.recent.iter().filter(|s| **s == sig).count();
        if count >= self.max_repeats {
            Some(format!(
                "loop detection: tool `{}` with identical args repeated {count} times \
                 in the last {} calls — stopping to avoid thrash. Change approach, \
                 re-read the file, or ask the user.",
                sig.name, self.window
            ))
        } else {
            None
        }
    }

    pub fn reset(&mut self) {
        self.recent.clear();
    }
}

/// Tracks consecutive tool/API failure streaks.
#[derive(Debug, Clone)]
pub struct MistakeTracker {
    max_consecutive: usize,
    streak: usize,
}

impl Default for MistakeTracker {
    fn default() -> Self {
        Self::new(5)
    }
}

impl MistakeTracker {
    pub fn new(max_consecutive: usize) -> Self {
        Self {
            max_consecutive: max_consecutive.max(2),
            streak: 0,
        }
    }

    /// Record success (`false`) or failure (`true`). Returns stop message if streak hit.
    pub fn observe_error(&mut self, is_error: bool) -> Option<String> {
        if is_error {
            self.streak += 1;
            if self.streak >= self.max_consecutive {
                return Some(format!(
                    "mistake limit: {} consecutive tool/API failures — stopping. \
                     Review errors, fix the approach, or raise the limit.",
                    self.streak
                ));
            }
        } else {
            self.streak = 0;
        }
        None
    }

    pub fn streak(&self) -> usize {
        self.streak
    }

    pub fn reset(&mut self) {
        self.streak = 0;
    }
}

/// Shared thrash state for the agent loop (thread-safe).
#[derive(Clone, Default)]
pub struct ThrashGuard {
    inner: Arc<Mutex<ThrashInner>>,
}

#[derive(Default)]
struct ThrashInner {
    loops: LoopDetectionTracker,
    mistakes: MistakeTracker,
    /// When set, next tool batch / turn should stop.
    stop_message: Option<String>,
}

impl ThrashGuard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(loop_repeats: usize, mistake_streak: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ThrashInner {
                loops: LoopDetectionTracker::new(12, loop_repeats),
                mistakes: MistakeTracker::new(mistake_streak),
                stop_message: None,
            })),
        }
    }

    pub fn observe_tool_start(&self, name: &str, args: &serde_json::Value) -> Option<String> {
        let mut g = self.inner.lock().unwrap();
        if let Some(msg) = g.loops.observe(ToolSignature::new(name, args)) {
            g.stop_message = Some(msg.clone());
            return Some(msg);
        }
        None
    }

    pub fn observe_tool_end(&self, is_error: bool) -> Option<String> {
        let mut g = self.inner.lock().unwrap();
        if let Some(msg) = g.mistakes.observe_error(is_error) {
            g.stop_message = Some(msg.clone());
            return Some(msg);
        }
        None
    }

    pub fn take_stop(&self) -> Option<String> {
        self.inner.lock().unwrap().stop_message.take()
    }

    pub fn peek_stop(&self) -> Option<String> {
        self.inner.lock().unwrap().stop_message.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn loop_detector_trips_on_identical_calls() {
        let mut t = LoopDetectionTracker::new(10, 3);
        let sig = ToolSignature::new("read", &json!({"path": "a.rs"}));
        assert!(t.observe(sig.clone()).is_none());
        assert!(t.observe(sig.clone()).is_none());
        let msg = t.observe(sig).unwrap();
        assert!(msg.contains("loop detection"));
        assert!(msg.contains("read"));
    }

    #[test]
    fn different_args_do_not_trip() {
        let mut t = LoopDetectionTracker::new(10, 3);
        assert!(t
            .observe(ToolSignature::new("read", &json!({"path": "a"})))
            .is_none());
        assert!(t
            .observe(ToolSignature::new("read", &json!({"path": "b"})))
            .is_none());
        assert!(t
            .observe(ToolSignature::new("read", &json!({"path": "c"})))
            .is_none());
    }

    #[test]
    fn mistake_tracker_trips_on_streak() {
        let mut m = MistakeTracker::new(3);
        assert!(m.observe_error(true).is_none());
        assert!(m.observe_error(true).is_none());
        let msg = m.observe_error(true).unwrap();
        assert!(msg.contains("mistake limit"));
        assert_eq!(m.streak(), 3);
    }

    #[test]
    fn success_resets_mistake_streak() {
        let mut m = MistakeTracker::new(3);
        assert!(m.observe_error(true).is_none());
        assert!(m.observe_error(true).is_none());
        assert!(m.observe_error(false).is_none());
        assert_eq!(m.streak(), 0);
        assert!(m.observe_error(true).is_none());
    }

    #[test]
    fn thrash_guard_shared() {
        let g = ThrashGuard::with_limits(3, 5);
        let args = json!({"x": 1});
        assert!(g.observe_tool_start("bash", &args).is_none());
        assert!(g.observe_tool_start("bash", &args).is_none());
        assert!(g.observe_tool_start("bash", &args).unwrap().contains("loop"));
        assert!(g.peek_stop().is_some());
    }
}
