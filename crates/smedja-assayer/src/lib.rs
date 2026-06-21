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
//! use smedja_assayer::{Assayer, Complexity, Role, Runner, Tier};
//!
//! let assayer = Assayer::default_rules();
//! let route = assayer.route(Role::Review, Complexity::Coding);
//! assert_eq!(route.runner, Runner::Claude);
//! assert_eq!(route.tier, Tier::Deep);
//! ```

pub mod assayer;
pub mod types;

pub use assayer::{Assayer, RoutingRule};
pub use types::{Complexity, Role, Route, Runner, Tier};
