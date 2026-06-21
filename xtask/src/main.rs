//! `cargo xtask` — build-time code generation helpers for smedja.
//!
//! # Usage
//!
//! ```
//! cargo xtask gen-rpc-types
//! ```
//!
//! Reads `crates/smedja-rpc/schema/types.json`, generates Rust types with
//! `typify`, and writes `crates/smedja-rpc/src/generated.rs`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use schemars::schema::RootSchema;
use typify::TypeSpace;

/// Cargo xtask entry point.
#[derive(Debug, Parser)]
#[command(name = "xtask", about = "smedja build-time code generation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Available xtask subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Generate `crates/smedja-rpc/src/generated.rs` from the JSON Schema.
    GenRpcTypes,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::GenRpcTypes => gen_rpc_types(),
    }
}

/// Reads the JSON Schema at `crates/smedja-rpc/schema/types.json` relative to
/// the workspace root, generates Rust types via `typify`, and writes the result
/// to `crates/smedja-rpc/src/generated.rs`.
///
/// The workspace root is inferred from the `CARGO_MANIFEST_DIR` environment
/// variable that Cargo sets for the xtask crate, falling back to the current
/// working directory.
///
/// # Errors
///
/// Returns an error if the schema file cannot be read, the JSON cannot be
/// parsed as a `RootSchema`, code generation fails, or the output file cannot
/// be written.
fn gen_rpc_types() -> Result<()> {
    let workspace_root = workspace_root();

    let schema_path = workspace_root
        .join("crates")
        .join("smedja-rpc")
        .join("schema")
        .join("types.json");

    let out_path = workspace_root
        .join("crates")
        .join("smedja-rpc")
        .join("src")
        .join("generated.rs");

    eprintln!("Reading schema: {}", schema_path.display());
    let json_str = std::fs::read_to_string(&schema_path)
        .with_context(|| format!("cannot read {}", schema_path.display()))?;

    let schema: RootSchema = serde_json::from_str(&json_str)
        .with_context(|| format!("cannot parse {} as JSON Schema", schema_path.display()))?;

    let mut type_space = TypeSpace::default();
    type_space
        .add_root_schema(schema)
        .context("typify failed to process schema")?;

    let generated = format!("{}", type_space.to_stream());

    // If typify produced no types (e.g. the schema only defines primitives),
    // we emit hand-written newtype wrappers so the file is always non-empty and
    // provides the expected public API.
    let content = if generated.trim().is_empty() {
        hand_written_types()
    } else {
        format!("{}\n\n{}", file_header(), generated)
    };

    eprintln!("Writing: {}", out_path.display());
    std::fs::write(&out_path, content)
        .with_context(|| format!("cannot write {}", out_path.display()))?;

    eprintln!("Done.");
    Ok(())
}

/// Returns the hand-written newtype wrappers that are emitted when typify
/// produces no output for the given schema.
fn hand_written_types() -> String {
    format!(
        "{}\n{}",
        file_header(),
        r"use serde::{Deserialize, Serialize};

/// Opaque identifier for an interactive session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// Creates a new [`SessionId`] from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque identifier for a single turn within a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(pub String);

impl TurnId {
    /// Creates a new [`TurnId`] from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque identifier for an async task tracked in smedja-ingot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    /// Creates a new [`TaskId`] from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
"
    )
}

/// Returns the standard file header comment for generated files.
fn file_header() -> &'static str {
    "// @generated — do not edit by hand; regenerate with `cargo xtask gen-rpc-types`\n\
     #![allow(clippy::all, unused_imports)]"
}

/// Returns the workspace root path.
///
/// Cargo sets `CARGO_MANIFEST_DIR` to the xtask crate directory; the workspace
/// root is one level up.  Falls back to the current working directory when the
/// variable is not set (e.g. when running the binary directly outside of Cargo).
fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points to xtask/; workspace root is one level up.
    std::env::var("CARGO_MANIFEST_DIR").map_or_else(
        |_| PathBuf::from("."),
        |d| {
            PathBuf::from(d)
                .parent()
                .unwrap_or(&PathBuf::from("."))
                .to_path_buf()
        },
    )
}
