//! `Ingot` operation groups.
//!
//! Each submodule holds a cohesive slice of [`crate::Ingot`]'s inherent
//! methods. They live in descendant modules so they can reach the private
//! `conn` field while keeping `lib.rs` small.

mod audit;
mod checkpoints;
mod cost;
mod jsonl;
mod loops;
mod mcp;
mod sessions;
mod tasks;
