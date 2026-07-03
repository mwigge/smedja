//! Human-in-the-loop gate for tool calls in cowork mode.

mod gate;
mod policy;
mod rules;

pub use gate::{ApprovalId, ApprovalPrompt, CoworkGate, Decision};
pub use policy::{evaluate, PermissionDecision, PermissionMode};
pub use rules::{evaluate_permission_rules, load_permission_rules, PermissionRule, RuleMode};
