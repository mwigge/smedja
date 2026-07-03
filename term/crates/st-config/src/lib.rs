//! `st-config` — configuration for smedja.
//!
//! Loads `~/.config/smedja/config.toml` when present; otherwise returns
//! the built-in `forged_terminal` theme defaults.

pub mod contrast;
pub mod migrate;

mod colors;
mod config;
mod raw;
mod types;

use thiserror::Error;

pub use colors::{hex_to_rgba, ColorConfig};
pub use config::Config;
pub use types::{
    AccessibilityConfig, FontConfig, KeyAction, KeyBinding, LaunchEntry, WindowConfig,
};

/// Errors produced by config loading.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file exists but could not be read.
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    /// The config file exists but is not valid TOML.
    #[error("failed to parse config file: {0}")]
    Parse(#[from] toml::de::Error),
    /// A colour hex string was not a valid RGB triplet.
    #[error("invalid colour hex string '{0}'")]
    InvalidColor(String),
}
