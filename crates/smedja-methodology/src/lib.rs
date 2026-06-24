//! `smedja-methodology` — pure diff-analysis gates enforcing development
//! workflow discipline.
//!
//! Gates are pure functions over text: no I/O, no async. A diff goes in;
//! the gate either passes it (`Ok(())`) or returns a [`MethodologyViolation`]
//! explaining what went wrong.
//!
//! # Quick start
//!
//! ```rust
//! use smedja_methodology::{tdd, clean};
//!
//! let diff = "+fn foo() {}\n+#[test]\n+fn test_foo() {}\n";
//! assert!(tdd::check(diff).is_ok());
//! assert!(clean::check(diff).is_ok());
//! ```

pub mod clean;
pub mod config;
pub mod tdd;
pub mod types;

pub use config::{MethodologyConfig, MethodologyConfigError};
pub use types::{GateResult, MethodologyViolation, Mode, SessionConfig};
