//! Canonical shared types for the smedja workspace.
//!
//! Provides [`Runner`], [`Tier`], and [`Complexity`] as the single source of
//! truth for all crates that need to interoperate on model routing, plus
//! domain value types ([`Timestamp`], [`Microdollars`], [`SessionId`],
//! [`TurnId`], [`ToolOutcome`], [`WorkspaceRoot`]) shared across the workspace.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The model runner backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runner {
    /// Anthropic Claude (cloud).
    Claude,
    /// `OpenAI` Codex (cloud).
    Codex,
    /// Kimi / Moonshot AI (cloud).
    Kimi,
    /// Google Gemini (cloud).
    Gemini,
    /// Local model running on device — no cloud egress.
    Local,
    /// GitHub Copilot (cloud).
    Copilot,
    /// `MiniMax` (cloud).
    Minimax,
    /// Berget (cloud).
    Berget,
    /// Poolside (cloud, `pool` CLI).
    Pool,
}

/// The execution tier that controls latency vs. capability trade-offs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Low latency, small context window, cheap.
    Fast,
    /// Local model running on device — no cloud egress.
    Local,
    /// High capability, large context window, higher latency.
    Deep,
}

impl Tier {
    /// Capability rank of this tier — higher means more capable (larger context
    /// window / higher quality). The single source of truth for every tier
    /// capability comparison in the workspace: `Local < Fast < Deep`.
    ///
    /// Ordering rationale: `Deep` is the strongest hosted tier. `Fast` is a
    /// hosted low-latency tier — still a cloud model, so more capable than a
    /// device-bound one. `Local` runs on-device with the smallest model and is
    /// the least capable. This matches the descending "Deep → Fast → Local"
    /// implementation ladder used by the assayer.
    #[must_use]
    pub fn capability_rank(self) -> u8 {
        match self {
            Self::Local => 0,
            Self::Fast => 1,
            Self::Deep => 2,
        }
    }
}

/// Estimated complexity of the task being assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Complexity {
    /// Trivial change: config tweak, one-liner fix, doc update.
    Simple,
    /// Moderate change: single module, a few functions, straightforward logic.
    Coding,
    /// High-effort change: cross-module, design-sensitive, or multi-step.
    Complex,
}

/// A point in time stored as microseconds since the Unix epoch.
///
/// This is the canonical timestamp representation across the workspace.
/// [`Timestamp::from_secs_f64`] and [`Timestamp::as_secs_f64`] bridge the
/// legacy `f64`-seconds representation used by older call sites and during
/// data migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// Captures the current wall-clock time as microseconds since the epoch.
    ///
    /// Saturates to `i64::MAX` if the clock is set before the Unix epoch or
    /// the elapsed microsecond count overflows `i64`.
    #[must_use]
    pub fn now() -> Self {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
            });
        Self(micros)
    }

    /// Constructs a timestamp from a raw microsecond count.
    #[must_use]
    pub fn from_micros(micros: i64) -> Self {
        Self(micros)
    }

    /// Returns the raw microsecond count.
    #[must_use]
    pub fn as_micros(&self) -> i64 {
        self.0
    }

    /// Constructs a timestamp from fractional seconds since the epoch.
    ///
    /// Bridges the legacy `f64`-seconds representation.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // rounded micros fit i64 for any realistic timestamp
    pub fn from_secs_f64(seconds: f64) -> Self {
        Self((seconds * 1_000_000.0).round() as i64)
    }

    /// Returns the timestamp as fractional seconds since the epoch.
    ///
    /// Bridges the legacy `f64`-seconds representation.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // micros -> f64 loses precision only beyond 2^53 micros (~285 years)
    pub fn as_secs_f64(&self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }
}

/// A monetary amount stored as millionths of a US dollar (microdollars).
///
/// Integer storage keeps totals exact; [`Microdollars::from_usd_f64`] and
/// [`Microdollars::as_usd_f64`] bridge the `f64`-dollars representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Microdollars(pub i64);

impl Microdollars {
    /// Constructs an amount from a dollar value.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // rounded microdollars fit i64 for any realistic cost
    pub fn from_usd_f64(usd: f64) -> Self {
        Self((usd * 1_000_000.0).round() as i64)
    }

    /// Returns the amount in dollars.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // microdollars -> f64 loses precision only beyond 2^53
    pub fn as_usd_f64(&self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    /// Constructs an amount from a raw microdollar count.
    #[must_use]
    pub fn from_micros(micros: i64) -> Self {
        Self(micros)
    }

    /// Returns the raw microdollar count.
    #[must_use]
    pub fn as_micros(&self) -> i64 {
        self.0
    }

    /// Adds two amounts, returning [`None`] on overflow.
    #[must_use]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    /// Sums an iterator of amounts for an exact total.
    ///
    /// Saturates rather than wrapping on overflow.
    #[must_use]
    pub fn sum<I: IntoIterator<Item = Self>>(amounts: I) -> Self {
        let total = amounts
            .into_iter()
            .fold(0_i64, |acc, amount| acc.saturating_add(amount.0));
        Self(total)
    }
}

/// Identifies a single conversational session.
///
/// A simple, non-validating wrapper around [`String`] — `new` does not reject
/// empty values. It is a distinct type from [`TurnId`], so a `SessionId`
/// cannot be passed where a `TurnId` is expected and vice versa.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Wraps a session identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Identifies a single turn within a session.
///
/// A simple, non-validating wrapper around [`String`] — `new` does not reject
/// empty values. It is a distinct type from [`SessionId`], so a `TurnId`
/// cannot be passed where a `SessionId` is expected and vice versa.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(String);

impl TurnId {
    /// Wraps a turn identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The result of executing a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolOutcome {
    /// The tool completed successfully, carrying its output.
    Success(String),
    /// The tool ran but reported a failure, carrying the error message.
    Failure(String),
    /// The tool did not complete within its time budget.
    Timeout,
    /// The tool was blocked because approval was denied, carrying the reason.
    ApprovalDenied(String),
}

impl ToolOutcome {
    /// Returns `true` only for the [`ToolOutcome::Success`] variant.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }
}

/// A canonicalised filesystem root for a workspace.
///
/// [`WorkspaceRoot::new`] canonicalises the supplied path, returning the
/// canonicalisation [`std::io::Error`] directly on failure (for example when
/// the path does not exist), so no extra error type or dependency is needed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    /// Canonicalises `path` and stores the canonical form.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] if the path cannot be
    /// canonicalised (for example, it does not exist or is inaccessible).
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self(path.as_ref().canonicalize()?))
    }

    /// Returns the canonical workspace root path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Returns the canonical workspace root path as a string slice, if it is
    /// valid UTF-8.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        self.0.to_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_serde_roundtrip() {
        for runner in [
            Runner::Claude,
            Runner::Codex,
            Runner::Kimi,
            Runner::Gemini,
            Runner::Local,
            Runner::Copilot,
            Runner::Minimax,
            Runner::Berget,
            Runner::Pool,
        ] {
            let json = serde_json::to_string(&runner).expect("serialise runner");
            let back: Runner = serde_json::from_str(&json).expect("deserialise runner");
            assert_eq!(runner, back);
        }
    }

    #[test]
    fn tier_serde_roundtrip() {
        for tier in [Tier::Fast, Tier::Local, Tier::Deep] {
            let json = serde_json::to_string(&tier).expect("serialise tier");
            let back: Tier = serde_json::from_str(&json).expect("deserialise tier");
            assert_eq!(tier, back);
        }
    }

    #[test]
    fn tier_capability_rank_orders_local_fast_deep() {
        // The single source of truth: Local < Fast < Deep. Every consumer
        // (assayer descent, provider-pool rotation) derives from this.
        assert!(Tier::Local.capability_rank() < Tier::Fast.capability_rank());
        assert!(Tier::Fast.capability_rank() < Tier::Deep.capability_rank());
        // Ranks are distinct so no two tiers ever compare equal.
        assert_eq!(Tier::Local.capability_rank(), 0);
        assert_eq!(Tier::Fast.capability_rank(), 1);
        assert_eq!(Tier::Deep.capability_rank(), 2);
    }

    #[test]
    fn complexity_serde_roundtrip() {
        for complexity in [Complexity::Simple, Complexity::Coding, Complexity::Complex] {
            let json = serde_json::to_string(&complexity).expect("serialise complexity");
            let back: Complexity = serde_json::from_str(&json).expect("deserialise complexity");
            assert_eq!(complexity, back);
        }
    }

    #[test]
    fn timestamp_micros_roundtrip() {
        let timestamp = Timestamp::from_micros(1_700_000_000_000_000);
        assert_eq!(timestamp.as_micros(), 1_700_000_000_000_000);
    }

    #[test]
    fn timestamp_secs_f64_bridge() {
        let timestamp = Timestamp::from_secs_f64(1.5);
        assert_eq!(timestamp.as_micros(), 1_500_000);
        assert!((timestamp.as_secs_f64() - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn timestamp_now_is_after_epoch() {
        assert!(Timestamp::now().as_micros() > 0);
    }

    #[test]
    fn timestamp_serializes_as_inner_i64() {
        let json = serde_json::to_string(&Timestamp::from_micros(42)).expect("serialise");
        assert_eq!(json, "42");
        let back: Timestamp = serde_json::from_str("42").expect("deserialise");
        assert_eq!(back, Timestamp::from_micros(42));
    }

    #[test]
    fn timestamp_orders_by_micros() {
        assert!(Timestamp::from_micros(1) < Timestamp::from_micros(2));
    }

    #[test]
    fn microdollars_usd_bridge() {
        let amount = Microdollars::from_usd_f64(1.25);
        assert_eq!(amount.as_micros(), 1_250_000);
        assert!((amount.as_usd_f64() - 1.25).abs() < f64::EPSILON);
    }

    #[test]
    fn microdollars_micros_roundtrip() {
        let amount = Microdollars::from_micros(999);
        assert_eq!(amount.as_micros(), 999);
    }

    #[test]
    fn microdollars_checked_add_detects_overflow() {
        let max = Microdollars::from_micros(i64::MAX);
        assert_eq!(max.checked_add(Microdollars::from_micros(1)), None);
        assert_eq!(
            Microdollars::from_micros(2).checked_add(Microdollars::from_micros(3)),
            Some(Microdollars::from_micros(5))
        );
    }

    #[test]
    fn microdollars_sum_totals_exactly() {
        let total = Microdollars::sum([
            Microdollars::from_micros(10),
            Microdollars::from_micros(20),
            Microdollars::from_micros(30),
        ]);
        assert_eq!(total, Microdollars::from_micros(60));
    }

    #[test]
    fn microdollars_sum_saturates_on_overflow() {
        let total = Microdollars::sum([
            Microdollars::from_micros(i64::MAX),
            Microdollars::from_micros(i64::MAX),
        ]);
        assert_eq!(total, Microdollars::from_micros(i64::MAX));
    }

    #[test]
    fn microdollars_serializes_as_inner_i64() {
        let json = serde_json::to_string(&Microdollars::from_micros(7)).expect("serialise");
        assert_eq!(json, "7");
    }

    #[test]
    fn session_id_wraps_and_displays() {
        let id = SessionId::new("abc");
        assert_eq!(id.as_str(), "abc");
        assert_eq!(id.to_string(), "abc");
    }

    #[test]
    fn turn_id_wraps_and_displays() {
        let id = TurnId::new(String::from("turn-1"));
        assert_eq!(id.as_str(), "turn-1");
        assert_eq!(id.to_string(), "turn-1");
    }

    #[test]
    fn ids_serialize_transparently() {
        let json = serde_json::to_string(&SessionId::new("s")).expect("serialise");
        assert_eq!(json, "\"s\"");
        let back: TurnId = serde_json::from_str("\"t\"").expect("deserialise");
        assert_eq!(back.as_str(), "t");
    }

    #[test]
    fn tool_outcome_is_success_only_for_success() {
        assert!(ToolOutcome::Success("ok".into()).is_success());
        assert!(!ToolOutcome::Failure("boom".into()).is_success());
        assert!(!ToolOutcome::Timeout.is_success());
        assert!(!ToolOutcome::ApprovalDenied("nope".into()).is_success());
    }

    #[test]
    fn tool_outcome_serde_roundtrip() {
        for outcome in [
            ToolOutcome::Success("out".into()),
            ToolOutcome::Failure("err".into()),
            ToolOutcome::Timeout,
            ToolOutcome::ApprovalDenied("reason".into()),
        ] {
            let json = serde_json::to_string(&outcome).expect("serialise outcome");
            let back: ToolOutcome = serde_json::from_str(&json).expect("deserialise outcome");
            assert_eq!(outcome, back);
        }
    }

    #[test]
    fn workspace_root_canonicalises_existing_path() {
        let root = WorkspaceRoot::new(".").expect("canonicalise current dir");
        assert!(root.path().is_absolute());
        assert!(root.as_str().is_some());
    }

    #[test]
    fn workspace_root_rejects_missing_path() {
        let result = WorkspaceRoot::new("/this/path/should/not/exist/smedja-xyz");
        assert!(result.is_err());
    }
}
