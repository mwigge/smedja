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

/// Writes `text` to the system clipboard, mirroring `read_clipboard_text`'s
/// mechanism order (pbcopy → wl-copy → xclip → xsel → arboard). Returns
/// false when every mechanism failed.
pub(crate) fn write_clipboard_text(text: &str) -> bool {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    // Copy tools keep running to serve the selection (X11/Wayland clipboard
    // ownership), so the payload is piped to stdin and the child reaped on a
    // side thread — waiting for it on the event loop would block forever.
    let try_cmd = |cmd: &str, args: &[&str]| -> bool {
        let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let write_ok = child
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        // `stdin` is dropped here, closing the pipe so the tool sees EOF.
        if !write_ok {
            let _ = child.kill();
            return false;
        }
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        true
    };

    #[cfg(target_os = "macos")]
    if try_cmd("pbcopy", &[]) {
        return true;
    }
    #[cfg(not(target_os = "macos"))]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && try_cmd("wl-copy", &[]) {
            return true;
        }
        if try_cmd("xclip", &["-selection", "clipboard", "-i"]) {
            return true;
        }
        if try_cmd("xsel", &["-b", "-i"]) {
            return true;
        }
    }
    // Last resort: arboard (works on most setups, flaky on some Wayland ones).
    arboard::Clipboard::new()
        .ok()
        .is_some_and(|mut cb| cb.set_text(text).is_ok())
}
