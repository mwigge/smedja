//! Native tool-body implementations dispatched from [`super::execute_tool`].
//!
//! Each submodule owns a cohesive group of the tools that `execute_tool`
//! handles in-process (rather than forwarding to an MCP server):
//! filesystem/exec ([`fs`]), vault ([`vault`]), graph query ([`graph`]),
//! SRE observability ([`sre`]), and web fetch ([`web`]).
//!
//! Handlers that guard their input with early returns return
//! `Result<String, String>`: the `Ok` body flows through the executor's
//! output-scan return path, while an `Err` is returned to the caller verbatim,
//! preserving the original control flow where those guards used `return`.

pub(crate) mod fs;
pub(crate) mod graph;
pub(crate) mod sre;
pub(crate) mod vault;
pub(crate) mod web;
