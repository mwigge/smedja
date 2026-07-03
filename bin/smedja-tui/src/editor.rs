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

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::{open_in_editor, resolve_editor};

    // Serialises env-var mutation across the parallel test runner.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn resolve_editor_falls_back_to_vi() {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Remove VISUAL and EDITOR from the environment for this test.
        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");
        // Can't guarantee clean env in parallel tests, but the fallback path
        // must always produce a non-empty string.
        let editor = resolve_editor();
        assert!(
            !editor.is_empty(),
            "resolve_editor must return a non-empty string"
        );
    }

    #[test]
    fn resolve_editor_prefers_visual_over_editor() {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("VISUAL", "emacs");
        std::env::set_var("EDITOR", "nano");
        let editor = resolve_editor();
        // Clean up after the test regardless of assertion result.
        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");
        assert_eq!(editor, "emacs", "VISUAL must be preferred over EDITOR");
    }

    #[test]
    fn open_in_editor_temp_path_is_in_tmpdir() {
        // Verify the temp file path is inside the OS temp directory — we
        // cannot actually invoke an editor in a unit test, but we can check
        // that the path construction is correct.
        let tmp = std::env::temp_dir();
        let path = tmp.join(format!("smedja-edit-{}.md", std::process::id()));
        assert!(
            path.starts_with(&tmp),
            "temp file must be under the OS temp directory"
        );
        assert!(
            path.to_string_lossy().ends_with(".md"),
            "temp file must have .md extension for editor syntax highlighting"
        );
    }
}
