//! A simple shelf bin-packer for the glyph atlas.

// ── Shelf packer ─────────────────────────────────────────────────────────────

/// A simple shelf bin-packer for the glyph atlas.
///
/// Glyphs are packed left-to-right in horizontal shelves.  A new shelf starts
/// whenever the current one is full.
#[derive(Debug)]
pub struct ShelfPacker {
    current_x: u32,
    current_y: u32,
    shelf_height: u32,
    atlas_size: u32,
}

impl ShelfPacker {
    /// Creates a new [`ShelfPacker`] for an atlas of `atlas_size × atlas_size`.
    #[must_use]
    pub fn new(atlas_size: u32) -> Self {
        Self {
            current_x: 0,
            current_y: 0,
            shelf_height: 0,
            atlas_size,
        }
    }

    /// Allocates a `w × h` region in the atlas, returning the top-left `[x, y]`.
    ///
    /// Returns `None` when the atlas is full.
    pub fn alloc(&mut self, w: u32, h: u32) -> Option<[u32; 2]> {
        if w > self.atlas_size || h > self.atlas_size {
            return None;
        }
        // Need a new shelf?
        if self.current_x + w > self.atlas_size {
            self.current_y += self.shelf_height;
            self.current_x = 0;
            self.shelf_height = 0;
        }
        if self.current_y + h > self.atlas_size {
            return None; // Atlas full.
        }
        let pos = [self.current_x, self.current_y];
        self.current_x += w;
        self.shelf_height = self.shelf_height.max(h);
        Some(pos)
    }
}
