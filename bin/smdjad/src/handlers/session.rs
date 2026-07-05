//! Session RPC handlers:
//! `session.create/list/search/get/delete/fork/set_model/set_runner/set_mode/set_title/`
//! `context/token_usage/takeover` and `runner.list`.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use smedja_ingot::{Checkpoint, Session, Task};
use smedja_rpc::{codes, RpcError};
use smedja_types::Timestamp;
use smedja_vault::VaultEntry;
use uuid::Uuid;

use crate::handlers::HandlerState;
use crate::{ingot_err, missing_param};

mod lifecycle;
mod mutate;
mod query;

pub(crate) use lifecycle::*;
pub(crate) use mutate::*;
pub(crate) use query::*;

#[cfg(test)]
mod tests;
