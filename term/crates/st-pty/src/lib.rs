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

mod apc;
mod cell;
mod color;
mod copy;
mod csi;
mod error;
mod esc;
mod grid;
mod grid_ops;
mod marker;
mod mouse;
mod notification;
mod osc;
mod session;
mod sgr;
mod snapshot;
mod vt;

pub use cell::{Cell, CellFlags};
pub use color::Color;
pub use copy::CopyMode;
pub use error::PtyError;
pub use grid::CellGrid;
pub use marker::{BlockMarker, MarkerKind};
pub use mouse::MouseMode;
pub use notification::{parse_osc777, parse_osc7_uri, parse_osc9, Notification};
pub use session::PtySession;
pub use snapshot::{render_vt_snapshot, render_vt_stale_cell_count, snapshot_grid, snapshot_hash};
