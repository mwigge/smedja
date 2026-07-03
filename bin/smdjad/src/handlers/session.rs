//! Session RPC handlers:
//! `session.create/list/search/get/delete/fork/set_model/set_runner/set_mode/set_title/`
//! `context/token_usage/takeover` and `runner.list`.
//!
//! The handlers are split across focused submodules and re-exported here so the
//! router keeps referring to them as `handlers::session::<name>`.

mod create;
mod fork;
mod query;
mod settings;
mod takeover;

pub(crate) use create::create;
pub(crate) use fork::fork;
pub(crate) use query::{context, delete, get, history, list, search, token_usage};
pub(crate) use settings::{runner_list, set_mode, set_model, set_runner, set_tier, set_title};
pub(crate) use takeover::takeover;
