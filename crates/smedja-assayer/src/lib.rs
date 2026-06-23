//! `smedja-assayer` — routing engine that maps role × complexity to a
//! `(Runner, Tier)` combination.
//!
//! Given an agent role and a task complexity estimate, the [`Assayer`] picks
//! the right runner and execution tier using an ordered list of
//! [`RoutingRule`]s. The first matching rule wins.
//!
//! # Quick start
//!
//! ```rust
//! use smedja_assayer::{AgentRole, Assayer, Complexity, Runner, Tier};
//!
//! let assayer = Assayer::default_rules();
//! let route = assayer.route(AgentRole::Review, Complexity::Coding);
//! assert_eq!(route.runner, Runner::Claude);
//! assert_eq!(route.tier, Tier::Deep);
//! ```

pub mod assayer;
pub mod bash_arity;
pub mod config;
pub mod parallel;
pub mod types;

pub use assayer::{Assayer, RoutingRule};
pub use bash_arity::{classify_bash, BashArity};
pub use config::load_rules;
pub use parallel::{Task, TaskStatus, WorktreePool};
pub use types::{AgentRole, Complexity, Route, RoutingDecision, Runner, Tier, MAX_ROLE_DEPTH};
