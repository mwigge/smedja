//! `smj prices` — manage model pricing.

use anyhow::Result;
use clap::Subcommand;

use crate::util::xdg_config_dir;

#[derive(Subcommand)]
pub(crate) enum PricesCmd {
    /// Update prices.toml from a local file or print current prices
    Update {
        /// Path to replacement prices.toml
        #[arg(long)]
        file: Option<std::path::PathBuf>,
    },
}

/// Dispatches a `smj prices` subcommand.
pub(crate) fn run(action: PricesCmd) -> Result<()> {
    match action {
        PricesCmd::Update { file } => {
            if let Some(src) = file {
                // ponytail: copy file to daemon config dir; daemon reloads on next request
                let dest = xdg_config_dir().join("smedja").join("prices.toml");
                std::fs::copy(&src, &dest)?;
                println!("prices.toml updated \u{2192} {}", dest.display());
            } else {
                // Print the embedded prices.toml location
                println!("prices.toml is read from the daemon's config directory at startup");
            }
        }
    }
    Ok(())
}
