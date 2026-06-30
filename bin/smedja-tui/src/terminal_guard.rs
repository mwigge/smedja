use std::io::{stdout, Write as _};

use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};

pub(crate) struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Reset terminal title so the host shell's title is restored after exit.
        // OSC 0 with an empty string resets to the terminal's default title.
        let _ = write!(stdout(), "\x1b]0;\x07");
        // Pop the kitty keyboard flags we pushed at startup before tearing down
        // raw mode / alt screen, so the host terminal is left in legacy state.
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
        let _ = disable_raw_mode();
        let _ = execute!(
            stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}
