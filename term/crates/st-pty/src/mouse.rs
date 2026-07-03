//! Mouse-reporting protocol modes.

/// Which mouse-reporting protocol the application has enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    None,
    /// `?1000h` — click events only (press + release).
    X10,
    /// `?1002h` — button events (click + drag while button held).
    ButtonEvent,
    /// `?1003h` — any motion (click + all mouse movement).
    AnyEvent,
}
