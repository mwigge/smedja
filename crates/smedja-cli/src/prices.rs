use super::*;
use crate::paths::xdg_config_dir;

pub(crate) fn dispatch_prices(action: PricesCmd) -> Result<()> {
    match action {
        PricesCmd::Update { file } => {
            if let Some(src) = file {
                // Copy file to daemon config dir; daemon reloads on next request.
                let dest = xdg_config_dir().join("smedja").join("prices.toml");
                std::fs::copy(&src, &dest)?;
                println!("prices.toml updated \u{2192} {}", dest.display());
            } else {
                println!("prices.toml is read from the daemon's config directory at startup");
            }
        }
    }
    Ok(())
}
