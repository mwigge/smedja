//! `st-pty` — PTY session management, VT emulation, scrollback, and copy mode.
//!
//! Spawns a child shell via [`portable_pty`], feeds its output through a
//! [`vte::Parser`] that mutates a shared [`CellGrid`], and exposes the grid
//! for rendering.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::assigning_clones,
    clippy::single_char_lifetime_names,
    clippy::equatable_if_let,
    clippy::match_like_matches_macro,
    clippy::doc_markdown,
    clippy::many_single_char_names,
    clippy::needless_range_loop,
    clippy::float_cmp,
    clippy::float_cmp_const
)]

use thiserror::Error;

mod cell;
mod color;
mod grid;
mod session;
mod vt;

pub use cell::{Cell, CellFlags};
pub use color::Color;
pub use grid::{
    parse_osc777, parse_osc7_uri, parse_osc9, BlockMarker, CellGrid, MarkerKind, MouseMode,
    Notification,
};
pub use session::{CopyMode, PtySession};
pub use vt::{render_vt_snapshot, render_vt_stale_cell_count, snapshot_grid, snapshot_hash};

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors produced by PTY operations.
#[derive(Debug, Error)]
pub enum PtyError {
    /// PTY system call failed.
    #[error("pty error: {0}")]
    Pty(String),
    /// I/O error on the PTY master fd.
    #[error("pty I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Clipboard error.
    #[error("clipboard error: {0}")]
    Clipboard(String),
}

#[cfg(test)]
mod tests;
