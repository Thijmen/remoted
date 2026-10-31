//! Recent events for the portal's activity list (`GET /api/activity`): agents coming and going,
//! sessions, terminals and file browsers opened, device and update requests. Kept in memory,
//! newest first and at most [`CAPACITY`], so the list starts empty after a restart. The audit log
//! stays the record; this is what the portal shows.

use std::collections::VecDeque;

use parking_lot::Mutex;
use serde::Serialize;

use crate::util::unix_ms;

/// Events kept; the oldest go first.
pub const CAPACITY: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    /// Unix ms.
    pub at_ms: u64,
    pub event: &'static str,
    pub machine: String,
    /// Who did it; absent for the agent's own events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl Event {
    pub fn new(event: &'static str, machine: &str) -> Self {
        Event {
            at_ms: unix_ms(),
            event,
            machine: machine.to_owned(),
            user: None,
            from: None,
            to: None,
            duration_ms: None,
        }
    }

    pub fn by(mut self, user: &str) -> Self {
        self.user = Some(user.to_owned());
        self
    }

    pub fn versions(mut self, from: &str, to: &str) -> Self {
        self.from = Some(from.to_owned());
        self.to = Some(to.to_owned());
        self
    }

    pub fn lasted(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }
}

#[derive(Default)]
pub struct Activity {
    events: Mutex<VecDeque<Event>>,
}

impl Activity {
    pub fn record(&self, event: Event) {
        let mut events = self.events.lock();
        if events.len() == CAPACITY {
            events.pop_back();
        }
        events.push_front(event);
    }

    /// Newest first.
    pub fn recent(&self) -> Vec<Event> {
        self.events.lock().iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_and_bounded() {
        let a = Activity::default();
        for i in 0..CAPACITY + 5 {
            a.record(Event::new("agent_online", &format!("m{i}")));
        }
        let recent = a.recent();
        assert_eq!(recent.len(), CAPACITY);
        assert_eq!(recent[0].machine, format!("m{}", CAPACITY + 4));
        assert_eq!(recent[CAPACITY - 1].machine, "m5");
    }

    #[test]
    fn json_leaves_out_what_an_event_does_not_have() {
        let mut e = Event::new("session_end", "mbp")
            .by("a@b.test")
            .lasted(61_000);
        e.at_ms = 7;
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            serde_json::json!({"at_ms": 7, "event": "session_end", "machine": "mbp", "user": "a@b.test", "duration_ms": 61_000})
        );
        let mut e = Event::new("agent_updated", "pc").versions("1.8.0", "1.9.1");
        e.at_ms = 8;
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            serde_json::json!({"at_ms": 8, "event": "agent_updated", "machine": "pc", "from": "1.8.0", "to": "1.9.1"})
        );
    }
}
