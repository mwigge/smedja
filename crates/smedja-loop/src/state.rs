//! Loop state machine states.
//!
//! These map 1:1 to the `status` column in the `loops` `SQLite` table and to the
//! `LoopEvent` variants emitted via bellows.

use serde::{Deserialize, Serialize};

/// State machine state for a loop run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopState {
    /// Initial state — the loop has been created but not yet started.
    Planning,
    /// The loop is dividing the work envelope into slices.
    Slicing,
    /// A verification command is running against the current slice.
    Verifying,
    /// A reviewer role is assessing the slice output.
    Reviewing,
    /// A fix role is addressing reviewer feedback.
    Fixed,
    /// All slices completed successfully.
    Complete,
    /// The loop exceeded its attempt limit or a fatal error occurred.
    Failed,
    /// The `loop.json` policy file was modified after load.
    PolicyTampered,
    /// The loop was explicitly cancelled.
    Retired,
}

impl LoopState {
    /// Returns the string representation used in the database `status` column.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::Slicing => "slicing",
            Self::Verifying => "verifying",
            Self::Reviewing => "reviewing",
            Self::Fixed => "fixed",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::PolicyTampered => "policy_tampered",
            Self::Retired => "retired",
        }
    }

    /// Returns `true` when the state is a terminal state that will not advance further.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Failed | Self::PolicyTampered | Self::Retired
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_are_identified_correctly() {
        assert!(LoopState::Complete.is_terminal());
        assert!(LoopState::Failed.is_terminal());
        assert!(LoopState::PolicyTampered.is_terminal());
        assert!(LoopState::Retired.is_terminal());
        assert!(!LoopState::Planning.is_terminal());
        assert!(!LoopState::Slicing.is_terminal());
        assert!(!LoopState::Verifying.is_terminal());
        assert!(!LoopState::Reviewing.is_terminal());
        assert!(!LoopState::Fixed.is_terminal());
    }

    #[test]
    fn as_str_matches_serde_rename() {
        // Serde serialises to lowercase; as_str must agree with those values.
        assert_eq!(LoopState::Planning.as_str(), "planning");
        assert_eq!(LoopState::Slicing.as_str(), "slicing");
        assert_eq!(LoopState::Complete.as_str(), "complete");
        assert_eq!(LoopState::Failed.as_str(), "failed");
        assert_eq!(LoopState::PolicyTampered.as_str(), "policy_tampered");
        assert_eq!(LoopState::Retired.as_str(), "retired");
    }

    #[test]
    fn state_round_trips_through_json() {
        let state = LoopState::Verifying;
        let json = serde_json::to_string(&state).unwrap();
        let restored: LoopState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, restored);
    }
}
