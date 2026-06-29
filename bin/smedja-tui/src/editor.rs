use crossterm::event::{PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{event::KeyboardEnhancementFlags, execute};

/// Resolves the editor binary to use for Ctrl-G composition.
///
/// Priority: `$VISUAL` → `$EDITOR` → `vi`.
pub(crate) fn resolve_editor() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_owned())
}

/// Opens the system `$EDITOR` with `initial_text` pre-filled.
///
/// Suspends the TUI (disables raw mode + leaves the alternate screen), runs
/// the editor, then restores the terminal.  Returns the trimmed file contents
/// on success, or `None` if the editor exited with a non-zero status or an
/// I/O error occurred.
pub(crate) fn open_in_editor(initial_text: &str) -> Option<String> {
    use std::{fs, io::Write as _, process::Command};

    let editor = resolve_editor();

    // Temp file keyed by PID; no external crate needed.
    let path = std::env::temp_dir().join(format!("smedja-edit-{}.md", std::process::id()));

    // Write the current input into the temp file.
    {
        let mut f = fs::File::create(&path).ok()?;
        f.write_all(initial_text.as_bytes()).ok()?;
    }

    // Suspend the TUI so the editor can own the terminal. Pop our kitty
    // keyboard flags first so the editor sees a clean legacy terminal.
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);

    let status = Command::new(&editor).arg(&path).status();

    // Restore the TUI regardless of editor outcome.
    let _ = execute!(std::io::stdout(), EnterAlternateScreen);
    let _ = enable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );

    let ok = status.is_ok_and(|s| s.success());
    if !ok {
        let _ = fs::remove_file(&path);
        return None;
    }

    let text = fs::read_to_string(&path).ok();
    let _ = fs::remove_file(&path);
    text.map(|s| s.trim_end_matches('\n').to_owned())
}
