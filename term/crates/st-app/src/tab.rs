//! `tab.rs` — Tab bar, pane model, and tab navigation for smedja.
//!
//! A [`TabBar`] owns zero or more [`Tab`]s, each of which contains one or more
//! [`Pane`]s split via a [`crate::split::SplitLayout`].  The invariant is that
//! there is always at least one tab.

// Public API surface used by the event loop, tests, and future PTY wiring.
// Binary crates warn on pub items not referenced from main(); suppress those
// warnings for this module since all items here are live API, not dead code.
#![allow(dead_code)]

use uuid::Uuid;

// ── Pane ─────────────────────────────────────────────────────────────────────

/// A single terminal pane within a tab.
///
/// A pane is backed by a PTY session once it has been spawned.  The `pty`
/// field is `None` before `spawn` is called (e.g. immediately after a split
/// is created but before the shell starts).
pub struct Pane {
    /// Stable identifier for this pane.
    pub id: Uuid,
    /// Whether this pane is currently zoomed (fills the full tab area).
    pub zoomed: bool,
}

impl Pane {
    /// Creates a new [`Pane`] with a fresh UUID.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4(),
            zoomed: false,
        }
    }
}

impl Default for Pane {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tab ───────────────────────────────────────────────────────────────────────

/// A tab containing one or more split panes.
pub struct Tab {
    /// Stable identifier for this tab.
    pub id: Uuid,
    /// Display title — typically the running command or a user-assigned name.
    pub title: String,
    /// Ordered list of panes within this tab.
    pub panes: Vec<Pane>,
    /// Index into `panes` of the currently focused pane.
    pub active_pane: usize,
}

impl Tab {
    /// Creates a new [`Tab`] with a single default pane.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            panes: vec![Pane::new()],
            active_pane: 0,
        }
    }

    /// Returns a reference to the active pane, if any.
    #[must_use]
    pub fn active_pane(&self) -> Option<&Pane> {
        self.panes.get(self.active_pane)
    }

    /// Returns a mutable reference to the active pane, if any.
    pub fn active_pane_mut(&mut self) -> Option<&mut Pane> {
        self.panes.get_mut(self.active_pane)
    }

    /// Adds a new pane to this tab and returns its index.
    pub fn push_pane(&mut self) -> usize {
        self.panes.push(Pane::new());
        self.panes.len() - 1
    }

    /// Removes the pane at `idx`, preserving at least one pane.
    ///
    /// If `idx` is the active pane, `active_pane` is adjusted to the preceding
    /// pane (or 0 if removing the first).  If removing the pane would leave the
    /// tab empty, the call is a no-op.
    pub fn remove_pane(&mut self, idx: usize) {
        if self.panes.len() <= 1 || idx >= self.panes.len() {
            return;
        }
        self.panes.remove(idx);
        // Clamp active_pane to valid range.
        if self.active_pane >= self.panes.len() {
            self.active_pane = self.panes.len() - 1;
        } else if idx < self.active_pane {
            self.active_pane -= 1;
        }
    }

    /// Toggles the zoom state of the active pane.
    pub fn toggle_zoom(&mut self) {
        // Un-zoom all other panes first so only one can be zoomed at a time.
        let active = self.active_pane;
        for (i, pane) in self.panes.iter_mut().enumerate() {
            pane.zoomed = if i == active { !pane.zoomed } else { false };
        }
    }
}

// ── TabBar ───────────────────────────────────────────────────────────────────

/// The multiplexer tab bar — owns all tabs and the currently active tab index.
///
/// Invariant: `tabs` always contains at least one element and `active < tabs.len()`.
pub struct TabBar {
    /// All open tabs, in display order.
    pub tabs: Vec<Tab>,
    /// Index of the currently focused tab.
    pub active: usize,
}

impl TabBar {
    /// Creates a new [`TabBar`] with a single default tab titled `"1"`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tabs: vec![Tab::new("1")],
            active: 0,
        }
    }

    /// Opens a new tab with the given title and returns a mutable reference to it.
    pub fn open_tab(&mut self, title: impl Into<String>) -> &mut Tab {
        self.tabs.push(Tab::new(title));
        let idx = self.tabs.len() - 1;
        self.active = idx;
        &mut self.tabs[idx]
    }

    /// Closes the tab at `idx`.
    ///
    /// If `idx` is out of range the call is a no-op.  The last remaining tab
    /// is never removed — a [`TabBar`] always holds at least one tab.
    pub fn close_tab(&mut self, idx: usize) {
        if self.tabs.len() <= 1 || idx >= self.tabs.len() {
            return;
        }
        self.tabs.remove(idx);
        // Keep active in bounds.
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
    }

    /// Renames the tab at `idx`.
    pub fn rename_tab(&mut self, idx: usize, title: impl Into<String>) {
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.title = title.into();
        }
    }

    /// Moves focus to the next tab, wrapping around.
    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    /// Moves focus to the previous tab, wrapping around.
    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active = self.active.checked_sub(1).unwrap_or(self.tabs.len() - 1);
        }
    }

    /// Returns a reference to the active tab, or `None` if the bar is empty.
    #[must_use]
    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// Returns a mutable reference to the active tab, or `None` if the bar is empty.
    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }
}

impl Default for TabBar {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_bar_open_tab_increments_count() {
        let mut bar = TabBar::new();
        assert_eq!(bar.tabs.len(), 1);
        bar.open_tab("shell");
        assert_eq!(bar.tabs.len(), 2);
    }

    #[test]
    fn tab_bar_close_tab_decrements_count() {
        let mut bar = TabBar::new();
        bar.open_tab("second");
        assert_eq!(bar.tabs.len(), 2);
        bar.close_tab(0);
        assert_eq!(bar.tabs.len(), 1);
    }

    #[test]
    fn tab_bar_close_last_tab_keeps_one() {
        let mut bar = TabBar::new();
        assert_eq!(bar.tabs.len(), 1);
        bar.close_tab(0);
        // Invariant: never drops below 1.
        assert_eq!(bar.tabs.len(), 1);
    }

    #[test]
    fn tab_bar_next_wraps_around() {
        let mut bar = TabBar::new();
        bar.open_tab("b");
        bar.open_tab("c");
        bar.active = 2; // last tab
        bar.next_tab();
        assert_eq!(bar.active, 0, "next_tab should wrap from last to first");
    }

    #[test]
    fn tab_bar_rename_changes_title() {
        let mut bar = TabBar::new();
        bar.rename_tab(0, "htop");
        assert_eq!(bar.tabs[0].title, "htop");
    }

    #[test]
    fn tab_bar_prev_wraps_from_first_to_last() {
        let mut bar = TabBar::new();
        bar.open_tab("b");
        bar.active = 0;
        bar.prev_tab();
        assert_eq!(bar.active, 1, "prev_tab should wrap from first to last");
    }

    #[test]
    fn tab_active_pane_returns_none_when_empty() {
        // Constructing a tab with no panes (bypassing new()) to test the guard.
        let tab = Tab {
            id: Uuid::new_v4(),
            title: "empty".into(),
            panes: vec![],
            active_pane: 0,
        };
        assert!(tab.active_pane().is_none());
    }

    #[test]
    fn tab_toggle_zoom_sets_zoomed() {
        let mut tab = Tab::new("test");
        assert!(!tab.panes[0].zoomed);
        tab.toggle_zoom();
        assert!(tab.panes[0].zoomed);
        tab.toggle_zoom();
        assert!(!tab.panes[0].zoomed);
    }

    #[test]
    fn tab_remove_pane_keeps_at_least_one() {
        let mut tab = Tab::new("t");
        assert_eq!(tab.panes.len(), 1);
        tab.remove_pane(0);
        assert_eq!(tab.panes.len(), 1);
    }

    #[test]
    fn tab_push_pane_increments_count() {
        let mut tab = Tab::new("t");
        tab.push_pane();
        assert_eq!(tab.panes.len(), 2);
    }
}
