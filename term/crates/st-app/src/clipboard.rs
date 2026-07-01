pub(crate) fn read_clipboard_text() -> Option<String> {
    use std::process::Command;
    let try_cmd = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = Command::new(cmd).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };

    #[cfg(target_os = "macos")]
    if let Some(t) = try_cmd("pbpaste", &[]) {
        return Some(t);
    }
    #[cfg(not(target_os = "macos"))]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            if let Some(t) = try_cmd("wl-paste", &["--no-newline"]) {
                return Some(t);
            }
        }
        if let Some(t) = try_cmd("xclip", &["-selection", "clipboard", "-o"]) {
            return Some(t);
        }
        if let Some(t) = try_cmd("xsel", &["-b", "-o"]) {
            return Some(t);
        }
    }
    // Last resort: arboard (works on most setups, flaky on some Wayland ones).
    arboard::Clipboard::new().ok()?.get_text().ok()
}
