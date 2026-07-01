use std::path::PathBuf;
pub(crate) fn default_ingot_path() -> PathBuf {
    let data_home = std::env::var("XDG_DATA_HOME").map_or_else(
        |_| {
            std::env::var("HOME").map_or_else(
                |_| PathBuf::from(".local/share"),
                |h| PathBuf::from(h).join(".local/share"),
            )
        },
        PathBuf::from,
    );
    let dir = data_home.join("smedja");
    // Best-effort directory creation — open() will surface the error if it fails.
    let _ = std::fs::create_dir_all(&dir);
    dir.join("ingot.db")
}

pub(crate) fn default_socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(base).join("smdjad.sock")
}

pub(crate) fn xdg_config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME").map_or_else(
        |_| {
            std::env::var("HOME").map_or_else(
                |_| PathBuf::from(".config"),
                |h| PathBuf::from(h).join(".config"),
            )
        },
        PathBuf::from,
    )
}
