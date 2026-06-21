//! `split.rs` — Taffy-based pane split layout for smedja-term.
//!
//! [`SplitLayout`] maintains a flexbox layout tree (via [`taffy`]) that maps
//! pane IDs to screen rectangles.  Each split operation inserts a new flex
//! container around the target pane and a sibling leaf for the new pane.

// Public API surface used by the event loop and tests.  Binary crates warn on
// pub items not referenced directly from main(); suppress here since all items
// are live API consumed by the event loop methods.
#![allow(dead_code)]

use std::collections::HashMap;

use taffy::prelude::*;
use taffy::tree::TaffyError;
use uuid::Uuid;

/// Type alias for pane identifiers.
pub type PaneId = Uuid;

/// Split direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    /// Side-by-side (left | right).
    Horizontal,
    /// Stacked (top / bottom).
    Vertical,
}

/// A Taffy-backed layout tree mapping [`PaneId`]s to screen rectangles.
pub struct SplitLayout {
    /// The Taffy layout tree.
    pub tree: TaffyTree<PaneId>,
    /// The root node of the tree.
    pub root: NodeId,
    /// Mapping from [`PaneId`] to the Taffy leaf node that represents it.
    pub pane_nodes: HashMap<PaneId, NodeId>,
}

impl SplitLayout {
    /// Creates a new [`SplitLayout`] with a single root pane.
    ///
    /// # Panics
    ///
    /// Panics if the initial Taffy node cannot be created (unreachable in
    /// practice — Taffy only errors on resource exhaustion).
    #[must_use]
    pub fn new(root_pane: PaneId) -> Self {
        let mut tree: TaffyTree<PaneId> = TaffyTree::new();

        // The root pane is a leaf that fills all available space.
        let leaf_style = Style {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            ..Default::default()
        };

        let leaf = tree
            .new_leaf_with_context(leaf_style, root_pane)
            .expect("taffy: failed to create root leaf");

        // Wrap in a flex row container so splits always have a consistent parent.
        let container_style = Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            size: Size {
                width: length(100.0),
                height: length(100.0),
            },
            ..Default::default()
        };

        let root = tree
            .new_with_children(container_style, &[leaf])
            .expect("taffy: failed to create root container");

        let mut pane_nodes = HashMap::new();
        pane_nodes.insert(root_pane, leaf);

        Self {
            tree,
            root,
            pane_nodes,
        }
    }

    /// Splits the pane identified by `pane` in the given direction.
    ///
    /// The existing pane and the new `new_pane` share the space equally via
    /// `flex_grow: 1.0`.  The parent container's flex direction is set to
    /// match `dir`.
    ///
    /// # Errors
    ///
    /// Returns [`TaffyError`] if a Taffy node operation fails.
    #[allow(clippy::too_many_lines)]
    pub fn split(
        &mut self,
        pane: PaneId,
        dir: SplitDirection,
        new_pane: PaneId,
    ) -> Result<(), TaffyError> {
        let Some(&existing_leaf) = self.pane_nodes.get(&pane) else {
            return Ok(()); // unknown pane — no-op
        };

        let leaf_style = Style {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            ..Default::default()
        };
        let new_leaf = self.tree.new_leaf_with_context(leaf_style, new_pane)?;

        // Find the parent of the existing leaf.
        let parent = self.find_parent(existing_leaf);

        let flex_dir = match dir {
            SplitDirection::Horizontal => FlexDirection::Row,
            SplitDirection::Vertical => FlexDirection::Column,
        };

        if let Some(parent_id) = parent {
            // Insert the new sibling immediately after the existing leaf.
            let siblings = self.tree.children(parent_id)?;
            let pos = siblings
                .iter()
                .position(|&n| n == existing_leaf)
                .unwrap_or(siblings.len());

            // Update the parent flex direction to match the split direction.
            let mut parent_style = self.tree.style(parent_id)?.clone();
            parent_style.flex_direction = flex_dir;
            self.tree.set_style(parent_id, parent_style)?;

            self.tree
                .insert_child_at_index(parent_id, pos + 1, new_leaf)?;
        } else {
            // existing_leaf is the root — wrap both in a new container.
            let container_style = Style {
                display: Display::Flex,
                flex_direction: flex_dir,
                flex_grow: 1.0,
                size: Size {
                    width: length(100.0),
                    height: length(100.0),
                },
                ..Default::default()
            };
            let container = self
                .tree
                .new_with_children(container_style, &[existing_leaf, new_leaf])?;
            // Replace the root.
            self.root = container;
        }

        self.pane_nodes.insert(new_pane, new_leaf);
        Ok(())
    }

    /// Removes the pane identified by `pane` from the layout tree.
    ///
    /// If `pane` is unknown or is the only pane remaining, the call is a no-op.
    pub fn remove_pane(&mut self, pane: PaneId) {
        if self.pane_nodes.len() <= 1 {
            return;
        }
        let Some(&leaf) = self.pane_nodes.get(&pane) else {
            return;
        };
        let _ = self.tree.remove(leaf);
        self.pane_nodes.remove(&pane);
    }

    /// Computes the layout for all panes given the available pixel dimensions.
    ///
    /// After this call, [`pane_rect`] will return the computed geometry for
    /// each pane.
    pub fn compute_layout(&mut self, available_width: f32, available_height: f32) {
        let available = Size {
            width: AvailableSpace::Definite(available_width),
            height: AvailableSpace::Definite(available_height),
        };
        let _ = self.tree.compute_layout(self.root, available);
    }

    /// Returns the computed [`Layout`] rectangle for the pane identified by
    /// `pane`, or `None` if `pane` is unknown or layout has not been computed.
    #[must_use]
    pub fn pane_rect(&self, pane: PaneId) -> Option<Layout> {
        let &node = self.pane_nodes.get(&pane)?;
        self.tree.layout(node).ok().copied()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Walks the tree to find the parent of `node`.
    fn find_parent(&self, node: NodeId) -> Option<NodeId> {
        self.tree.parent(node)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_id() -> PaneId {
        Uuid::new_v4()
    }

    #[test]
    fn split_layout_new_has_one_pane() {
        let id = fresh_id();
        let layout = SplitLayout::new(id);
        assert_eq!(layout.pane_nodes.len(), 1);
        assert!(layout.pane_nodes.contains_key(&id));
    }

    #[test]
    fn split_layout_horizontal_split_creates_two_panes() {
        let id1 = fresh_id();
        let id2 = fresh_id();
        let mut layout = SplitLayout::new(id1);
        layout.split(id1, SplitDirection::Horizontal, id2).unwrap();
        assert_eq!(layout.pane_nodes.len(), 2);
        assert!(layout.pane_nodes.contains_key(&id1));
        assert!(layout.pane_nodes.contains_key(&id2));
    }

    #[test]
    fn split_layout_vertical_split_creates_two_panes() {
        let id1 = fresh_id();
        let id2 = fresh_id();
        let mut layout = SplitLayout::new(id1);
        layout.split(id1, SplitDirection::Vertical, id2).unwrap();
        assert_eq!(layout.pane_nodes.len(), 2);
    }

    #[test]
    fn split_layout_remove_pane_reduces_count() {
        let id1 = fresh_id();
        let id2 = fresh_id();
        let mut layout = SplitLayout::new(id1);
        layout.split(id1, SplitDirection::Horizontal, id2).unwrap();
        assert_eq!(layout.pane_nodes.len(), 2);
        layout.remove_pane(id2);
        assert_eq!(layout.pane_nodes.len(), 1);
    }

    #[test]
    fn split_layout_remove_last_pane_keeps_one() {
        let id = fresh_id();
        let mut layout = SplitLayout::new(id);
        layout.remove_pane(id); // should be no-op
        assert_eq!(layout.pane_nodes.len(), 1);
    }

    #[test]
    fn split_layout_compute_layout_does_not_panic() {
        let id1 = fresh_id();
        let id2 = fresh_id();
        let mut layout = SplitLayout::new(id1);
        layout.split(id1, SplitDirection::Horizontal, id2).unwrap();
        // Must not panic even with zero or non-zero dimensions.
        layout.compute_layout(1200.0, 800.0);
        layout.compute_layout(0.0, 0.0);
    }

    #[test]
    fn split_layout_pane_rect_returns_none_for_unknown() {
        let id = fresh_id();
        let layout = SplitLayout::new(id);
        assert!(layout.pane_rect(fresh_id()).is_none());
    }

    #[test]
    fn split_layout_pane_rect_after_compute() {
        let id = fresh_id();
        let mut layout = SplitLayout::new(id);
        layout.compute_layout(800.0, 600.0);
        // After compute, the root pane should have a non-None rect.
        let rect = layout.pane_rect(id);
        assert!(rect.is_some());
    }
}
