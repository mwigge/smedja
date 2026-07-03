//! Bounded per-turn replay buffer for SSE `Last-Event-ID` reconnect support.

const REPLAY_CAP: usize = 512;

/// Bounded per-turn replay buffer for SSE `Last-Event-ID` reconnect support.
pub struct EventBuffer {
    // ponytail: global lock — per-turn sharding if contention observed
    inner: std::sync::Mutex<
        std::collections::HashMap<String, std::collections::VecDeque<(u64, String)>>,
    >,
    cap: usize,
}

impl Default for EventBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(std::collections::HashMap::new()),
            cap: REPLAY_CAP,
        }
    }

    #[cfg(test)]
    fn with_capacity(cap: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(std::collections::HashMap::new()),
            cap,
        }
    }

    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (another thread panicked while
    /// holding the lock).
    pub fn push(&self, turn_id: &str, seq: u64, data: String) {
        let mut map = self.inner.lock().expect("event buffer lock not poisoned");
        let q = map.entry(turn_id.to_owned()).or_default();
        q.push_back((seq, data));
        while q.len() > self.cap {
            q.pop_front();
        }
    }

    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn events_after(&self, turn_id: &str, last_seq: u64) -> Vec<(u64, String)> {
        let map = self.inner.lock().expect("event buffer lock not poisoned");
        map.get(turn_id)
            .map(|q| q.iter().filter(|(s, _)| *s > last_seq).cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::EventBuffer;

    #[test]
    fn event_buffer_stores_and_returns_events_after_seq() {
        let buf = EventBuffer::new();
        buf.push("t1", 1, "ev1".into());
        buf.push("t1", 2, "ev2".into());
        buf.push("t1", 3, "ev3".into());
        let after = buf.events_after("t1", 1);
        assert_eq!(after.len(), 2);
        assert_eq!(after[0], (2, "ev2".to_owned()));
        assert_eq!(after[1], (3, "ev3".to_owned()));
    }

    #[test]
    fn event_buffer_caps_at_max_size() {
        let buf = EventBuffer::with_capacity(3);
        for i in 1u64..=5 {
            buf.push("t", i, format!("ev{i}"));
        }
        let all = buf.events_after("t", 0);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, 3); // oldest kept is seq 3
    }

    #[test]
    fn event_buffer_empty_for_unknown_turn() {
        let buf = EventBuffer::new();
        assert!(buf.events_after("nosuch", 0).is_empty());
    }
}
