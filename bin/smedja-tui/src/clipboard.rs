use std::collections::VecDeque;

pub(crate) fn yank_to_clipboard(lines: &[String]) -> Result<&'static str, String> {
    use std::io::Write as _;
    let text = lines.join("\n");

    let candidates: &[(&str, &[&str])] = &[
        #[cfg(target_os = "macos")]
        ("pbcopy", &[]),
        #[cfg(not(target_os = "macos"))]
        ("wl-copy", &[]),
        #[cfg(not(target_os = "macos"))]
        ("xclip", &["-selection", "clipboard"]),
        #[cfg(not(target_os = "macos"))]
        ("xsel", &["--clipboard", "--input"]),
    ];

    for (bin, args) in candidates {
        let result = std::process::Command::new(bin)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .spawn();
        if let Ok(mut child) = result {
            let write_ok = child
                .stdin
                .take()
                .is_none_or(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
            if write_ok {
                // Reap the child in a background thread so we don't block
                // the TUI event loop (clipboard tools like wl-copy stay
                // alive serving paste requests until another owner takes over).
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(bin);
            }
        }
    }

    let tried: Vec<&str> = candidates.iter().map(|(b, _)| *b).collect();
    Err(format!(
        "clipboard unavailable — install one of: {}",
        tried.join(", ")
    ))
}

/// Reads text from the system clipboard using wl-paste (Wayland) or xclip/xsel (X11).
pub(crate) fn paste_from_clipboard() -> Option<String> {
    use std::process::Command;

    #[cfg(target_os = "macos")]
    {
        let out = Command::new("pbpaste").output().ok()?;
        if out.status.success() {
            return Some(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        return None;
    }

    #[cfg(not(target_os = "macos"))]
    {
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            let out = Command::new("wl-paste").arg("--no-newline").output().ok()?;
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
        // X11 fallbacks
        if let Ok(out) = Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .output()
        {
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
        if let Ok(out) = Command::new("xsel")
            .args(["--clipboard", "--output"])
            .output()
        {
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
        None
    }
}

pub(crate) fn osc9_turn_complete_bytes() -> &'static [u8] {
    b"\x1b]9;turn complete\x07"
}

pub(crate) fn emit_osc9(w: &mut impl std::io::Write) -> std::io::Result<()> {
    w.write_all(osc9_turn_complete_bytes())
}

pub(crate) fn push_kill(ring: &mut VecDeque<String>, text: String) {
    if ring.len() >= 16 {
        ring.pop_front();
    }
    ring.push_back(text);
}
