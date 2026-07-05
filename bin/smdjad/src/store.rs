//! Persistent-store bootstrap and the daemon home-directory primitive.

use std::path::PathBuf;

use smedja_ingot::Ingot;
use smedja_vault::Vault;

pub(crate) fn open_ingot() -> anyhow::Result<Ingot> {
    // Try to open the persistent store under ~/.local/share/smedja/smedja.db.
    // If the data directory cannot be created, fall back to in-memory.
    let data_dir = dirs_home()
        .map(|h| h.join(".local").join("share").join("smedja"))
        .filter(|d| std::fs::create_dir_all(d).is_ok());

    if let Some(dir) = data_dir {
        let db_path = dir.join("smedja.db");
        let ingot = Ingot::open(&db_path).map_err(anyhow::Error::from)?;
        // Ensure the database file is only readable by the owner.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(ingot)
    } else {
        tracing::error!("cannot create data directory; using in-memory store — all session data will be lost on restart");
        Ingot::open_in_memory().map_err(anyhow::Error::from)
    }
}

pub(crate) fn open_vault() -> Vault {
    // Mirror the ingot path: ~/.local/share/smedja/vault.db.
    // Falls back to an in-memory vault if the directory cannot be created.
    let vault_path = dirs_home()
        .map(|h| h.join(".local").join("share").join("smedja"))
        .filter(|d| std::fs::create_dir_all(d).is_ok())
        .map(|dir| dir.join("vault.db"));

    if let Some(path) = vault_path {
        match Vault::open(&path) {
            Ok(v) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
                return v;
            }
            Err(e) => tracing::warn!(error = %e, "vault open failed; using in-memory vault"),
        }
    } else {
        tracing::warn!("cannot create vault data directory; using in-memory vault");
    }
    Vault::open_in_memory().expect("in-memory vault must open")
}

/// Returns the user's home directory, or `None` if it cannot be determined.
pub(crate) fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}
