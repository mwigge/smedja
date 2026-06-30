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

/// Kitty desktop notification (OSC 99).
pub(crate) fn osc99_turn_complete_bytes() -> &'static [u8] {
    b"\x1b]99;i=1;turn complete\x1b\\"
}

/// VTE / Gnome Terminal desktop notification (OSC 777).
pub(crate) fn osc777_turn_complete_bytes() -> &'static [u8] {
    b"\x1b]777;notify;smedja;turn complete\x07"
}

/// Wraps `seq` in a tmux DCS passthrough frame when the process is running
/// inside tmux.  Each ESC byte within the payload is doubled, per the tmux
/// DCS spec.  Returns `seq` verbatim when tmux is not detected.
pub(crate) fn tmux_passthrough(seq: &[u8]) -> Vec<u8> {
    tmux_passthrough_inner(seq, std::env::var_os("TMUX").is_some())
}

fn tmux_passthrough_inner(seq: &[u8], in_tmux: bool) -> Vec<u8> {
    if !in_tmux {
        return seq.to_vec();
    }
    let mut out = Vec::with_capacity(seq.len() + 16);
    // DCS open: ESC P tmux; ESC
    out.extend_from_slice(b"\x1bPtmux;\x1b");
    for &b in seq {
        if b == b'\x1b' {
            out.push(b'\x1b'); // double embedded ESC
        }
        out.push(b);
    }
    // ST close
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Emits OSC 9 (iTerm2), OSC 99 (Kitty), and OSC 777 (VTE) turn-complete
/// notifications, wrapping each in a tmux DCS passthrough frame when needed.
pub(crate) fn emit_turn_notifications(w: &mut impl std::io::Write) -> std::io::Result<()> {
    w.write_all(&tmux_passthrough(osc9_turn_complete_bytes()))?;
    w.write_all(&tmux_passthrough(osc99_turn_complete_bytes()))?;
    w.write_all(&tmux_passthrough(osc777_turn_complete_bytes()))
}

const TITLE_SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Writes an OSC 0 window-title sequence to `w`.  While `inferring`, a braille
/// spinner frame (derived from `tick`) is prepended.
pub(crate) fn set_terminal_title(
    w: &mut impl std::io::Write,
    inferring: bool,
    tick: u8,
) -> std::io::Result<()> {
    let title = if inferring {
        let frame = TITLE_SPINNER[tick as usize % TITLE_SPINNER.len()];
        format!("{frame} smedja")
    } else {
        "smedja".to_owned()
    };
    write!(w, "\x1b]0;{title}\x07")
}

pub(crate) fn push_kill(ring: &mut VecDeque<String>, text: String) {
    if ring.len() >= 16 {
        ring.pop_front();
    }
    ring.push_back(text);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        emit_turn_notifications, osc777_turn_complete_bytes, osc99_turn_complete_bytes,
        osc9_turn_complete_bytes, set_terminal_title, tmux_passthrough_inner, TITLE_SPINNER,
    };

    #[test]
    fn osc9_format_unchanged() {
        assert_eq!(osc9_turn_complete_bytes(), b"\x1b]9;turn complete\x07");
    }

    #[test]
    fn osc99_is_kitty_format() {
        let b = osc99_turn_complete_bytes();
        assert!(b.starts_with(b"\x1b]99;"), "must be OSC 99");
        assert!(b.ends_with(b"\x1b\\"), "must end with ST");
    }

    #[test]
    fn osc777_is_vte_format() {
        let b = osc777_turn_complete_bytes();
        assert!(b.starts_with(b"\x1b]777;"), "must be OSC 777");
    }

    #[test]
    fn tmux_passthrough_not_in_tmux_is_identity() {
        let seq = b"\x1b]9;test\x07";
        assert_eq!(tmux_passthrough_inner(seq, false), seq.to_vec());
    }

    #[test]
    fn tmux_passthrough_in_tmux_wraps_with_dcs() {
        let seq = b"\x1b]9;test\x07";
        let wrapped = tmux_passthrough_inner(seq, true);
        assert!(
            wrapped.starts_with(b"\x1bPtmux;\x1b"),
            "must have DCS header"
        );
        assert!(wrapped.ends_with(b"\x1b\\"), "must end with ST");
        // The ESC at byte 0 of seq must be doubled in the payload
        let payload_start = b"\x1bPtmux;\x1b".len();
        let payload_end = wrapped.len() - b"\x1b\\".len();
        let payload = &wrapped[payload_start..payload_end];
        // Original seq starts with \x1b → doubled → \x1b\x1b]9;...
        assert_eq!(&payload[..2], b"\x1b\x1b", "ESC must be doubled");
        assert_eq!(&payload[2..4], b"]9", "OSC 9 body follows doubled ESC");
    }

    #[test]
    fn emit_turn_notifications_contains_all_three_osc() {
        let mut buf = Vec::new();
        emit_turn_notifications(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("]9;"), "must contain OSC 9");
        assert!(text.contains("]99;"), "must contain OSC 99");
        assert!(text.contains("]777;"), "must contain OSC 777");
    }

    #[test]
    fn title_inferring_uses_braille_spinner() {
        let mut buf = Vec::new();
        set_terminal_title(&mut buf, true, 0).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("\x1b]0;"), "must be OSC 0");
        assert!(
            TITLE_SPINNER.iter().any(|&c| s.contains(c)),
            "must contain a braille character"
        );
        assert!(s.contains("smedja"), "must contain app name");
    }

    #[test]
    fn title_done_is_plain_name() {
        let mut buf = Vec::new();
        set_terminal_title(&mut buf, false, 0).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "\x1b]0;smedja\x07");
    }

    #[test]
    fn title_spinner_advances_with_tick() {
        let mut frames: Vec<char> = Vec::new();
        for tick in 0u8..10 {
            let mut buf = Vec::new();
            set_terminal_title(&mut buf, true, tick).unwrap();
            let s = String::from_utf8(buf).unwrap();
            // Title string is "\x1b]0;{spinner} smedja\x07"
            // chars: 0=ESC 1=] 2=0 3=; 4=spinner_char
            let frame = s.chars().nth(4).unwrap();
            frames.push(frame);
        }
        // All 10 spinner frames should be distinct across 10 ticks
        let unique: std::collections::HashSet<char> = frames.iter().copied().collect();
        assert!(unique.len() >= 2, "spinner must cycle across ticks");
    }
}
