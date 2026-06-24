//! Cross-turn stable-prefix drift tracking and cache-breakpoint selection.
//!
//! [`WorkingMemory`] is rebuilt per turn and seals its stable prefix exactly
//! once, with no memory of the prior turn's boundary. As a session lengthens the
//! genuinely-stable region (system prompt, skills, settled early turns) can grow,
//! yet the sealed boundary stays fixed — so cache hits silently degrade. The
//! [`CacheAligner`] is a per-session observer that carries the prior boundary and
//! a per-message digest across turns, classifies how the prefix drifted, and
//! emits a [`CacheHint`] describing a safe cache breakpoint.

use std::hash::{Hash as _, Hasher as _};

use crate::types::Message;
use crate::WorkingMemory;

/// How the sealed stable prefix changed relative to the previous turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drift {
    /// Same boundary and the same per-message digests as last turn.
    Unchanged,
    /// The boundary advanced and every prior message is byte-identical.
    Grown,
    /// A message inside the prior boundary changed content.
    Mutated,
}

/// A provider-neutral cache hint produced by the aligner.
///
/// `breakpoint` is the number of leading messages that are safe to treat as a
/// stable cache prefix this turn. It never exceeds the current
/// [`WorkingMemory::stable_prefix`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheHint {
    /// Number of leading messages that form the safe stable cache prefix.
    pub breakpoint: usize,
    /// How the prefix drifted since the previous turn.
    pub drift: Drift,
}

/// Per-session observer that tracks stable-prefix drift and selects cache
/// breakpoints across turns.
///
/// The aligner is advisory: a wrong guess degrades to a smaller (or absent)
/// cache prefix, never to a wrong response.
#[derive(Debug, Default)]
pub struct CacheAligner {
    /// Per-message digests for the messages inside the last sealed boundary.
    /// Empty before the first [`CacheAligner::align`] call.
    prior_digests: Vec<u64>,
    /// Whether at least one turn has been observed.
    seen: bool,
}

/// Computes a deterministic digest of a single message (role + content).
fn digest_message(message: &Message) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    message.role.as_str().hash(&mut hasher);
    message.content.hash(&mut hasher);
    hasher.finish()
}

impl CacheAligner {
    /// Creates a fresh aligner with no observed history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Observes the freshly-sealed `memory` and returns a [`CacheHint`].
    ///
    /// The breakpoint is the longest leading run of messages whose digests are
    /// unchanged versus the previous turn, capped at the current
    /// [`WorkingMemory::stable_prefix`]. On the first turn (no history) the whole
    /// sealed prefix is treated as stable. When a message inside the prior
    /// boundary mutated, the breakpoint is truncated to just before the first
    /// changed message; if that leaves nothing stable the hint reports a
    /// zero-length breakpoint.
    ///
    /// Calling `align` updates the aligner's stored state for the next turn.
    #[must_use]
    pub fn align(&mut self, memory: &WorkingMemory) -> CacheHint {
        let prefix_len = memory.stable_prefix();
        let messages = memory.messages();
        let current: Vec<u64> = messages[..prefix_len].iter().map(digest_message).collect();

        let hint = if self.seen {
            self.classify(prefix_len, &current)
        } else {
            // First observed turn: the entire sealed prefix is stable.
            CacheHint {
                breakpoint: prefix_len,
                drift: Drift::Unchanged,
            }
        };

        self.prior_digests = current;
        self.seen = true;
        hint
    }

    /// Classifies drift against the stored prior digests and selects a breakpoint.
    fn classify(&self, prefix_len: usize, current: &[u64]) -> CacheHint {
        // Longest leading run of digest-stable messages shared with the prior turn.
        let stable_run = current
            .iter()
            .zip(self.prior_digests.iter())
            .take_while(|(now, then)| now == then)
            .count();

        let mutated = stable_run < self.prior_digests.len();
        if mutated {
            // A message inside the prior boundary changed: never place the
            // breakpoint on or past it.
            let breakpoint = stable_run.min(prefix_len);
            return CacheHint {
                breakpoint,
                drift: Drift::Mutated,
            };
        }

        // No mutation. The breakpoint is the full current prefix (capped).
        let breakpoint = prefix_len;
        let drift = if breakpoint > self.prior_digests.len() {
            Drift::Grown
        } else {
            Drift::Unchanged
        };
        CacheHint { breakpoint, drift }
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheAligner, Drift};
    use crate::types::Message;
    use crate::WorkingMemory;

    fn sealed(prefix: &[&str]) -> WorkingMemory {
        let mut m = WorkingMemory::new(4096);
        for content in prefix {
            m.push(Message::system(*content));
        }
        m.seal_prefix();
        m
    }

    #[test]
    fn fresh_aligner_reports_unchanged_at_full_prefix() {
        let mem = sealed(&["sys", "skills"]);
        let mut aligner = CacheAligner::new();
        let hint = aligner.align(&mem);
        assert_eq!(hint.drift, Drift::Unchanged);
        assert_eq!(hint.breakpoint, mem.stable_prefix());
    }

    #[test]
    fn grown_prefix_with_stable_messages_advances_breakpoint() {
        let mut aligner = CacheAligner::new();
        let first = sealed(&["sys", "skills"]);
        let first_hint = aligner.align(&first);
        assert_eq!(first_hint.breakpoint, 2);

        // Next turn: same leading messages, prefix grew by one settled turn.
        let second = sealed(&["sys", "skills", "settled turn"]);
        let hint = aligner.align(&second);
        assert_eq!(hint.drift, Drift::Grown);
        assert_eq!(hint.breakpoint, 3);
    }

    #[test]
    fn mutated_message_truncates_breakpoint_before_change() {
        let mut aligner = CacheAligner::new();
        let first = sealed(&["sys", "skills", "context"]);
        let _ = aligner.align(&first);

        // Second turn: index 1 mutated; breakpoint must stop before it.
        let second = sealed(&["sys", "CHANGED", "context"]);
        let hint = aligner.align(&second);
        assert_eq!(hint.drift, Drift::Mutated);
        assert_eq!(hint.breakpoint, 1);
    }

    #[test]
    fn mutated_first_message_yields_zero_breakpoint() {
        let mut aligner = CacheAligner::new();
        let first = sealed(&["sys", "skills"]);
        let _ = aligner.align(&first);

        let second = sealed(&["CHANGED", "skills"]);
        let hint = aligner.align(&second);
        assert_eq!(hint.drift, Drift::Mutated);
        assert_eq!(hint.breakpoint, 0);
    }

    #[test]
    fn hint_carries_breakpoint_and_drift() {
        let mem = sealed(&["sys"]);
        let mut aligner = CacheAligner::new();
        let hint = aligner.align(&mem);
        // The hint exposes both a breakpoint index and a provider-neutral drift.
        assert_eq!(hint.breakpoint, 1);
        assert_eq!(hint.drift, Drift::Unchanged);
    }

    #[test]
    fn breakpoint_never_exceeds_stable_prefix_and_zero_when_empty() {
        let mut aligner = CacheAligner::new();
        // Empty sealed prefix → breakpoint 0.
        let empty = sealed(&[]);
        let hint = aligner.align(&empty);
        assert_eq!(hint.breakpoint, 0);
        assert!(hint.breakpoint <= empty.stable_prefix());
    }

    #[test]
    fn unchanged_prefix_reports_unchanged() {
        let mut aligner = CacheAligner::new();
        let first = sealed(&["sys", "skills"]);
        let _ = aligner.align(&first);
        let second = sealed(&["sys", "skills"]);
        let hint = aligner.align(&second);
        assert_eq!(hint.drift, Drift::Unchanged);
        assert_eq!(hint.breakpoint, 2);
    }
}
