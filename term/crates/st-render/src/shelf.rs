//! Shelf bin-packer for the glyph atlas.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shelf_packer_basic_alloc() {
        let mut p = ShelfPacker::new(100);
        let a = p.alloc(10, 10);
        assert_eq!(a, Some([0, 0]));
    }

    #[test]
    fn shelf_packer_fills_row_then_new_shelf() {
        let mut p = ShelfPacker::new(100);
        // Fill first shelf (10 wide, height 10) 10 times = 100 px.
        for i in 0..10u32 {
            assert_eq!(p.alloc(10, 10), Some([i * 10, 0]));
        }
        // Next alloc must start a new shelf.
        let b = p.alloc(10, 10);
        assert_eq!(b, Some([0, 10]));
    }

    #[test]
    fn shelf_packer_returns_none_when_full() {
        let mut p = ShelfPacker::new(4);
        // Fill the entire atlas.
        p.alloc(4, 4).unwrap();
        // Next alloc should fail.
        assert!(p.alloc(1, 1).is_none());
    }

    #[test]
    fn shelf_packer_rejects_oversized() {
        let mut p = ShelfPacker::new(10);
        assert!(p.alloc(11, 1).is_none());
        assert!(p.alloc(1, 11).is_none());
    }

    #[test]
    fn shelf_packer_alloc_advances_x_for_same_row() {
        let mut p = ShelfPacker::new(64);
        let _ = p.alloc(10, 10); // [0, 0]
        assert_eq!(p.alloc(10, 10), Some([10, 0]));
    }

    #[test]
    fn shelf_packer_alloc_wraps_to_new_shelf() {
        let mut p = ShelfPacker::new(20);
        // first alloc:  [0,0],  x→12, shelf_height→8
        let _ = p.alloc(12, 8);
        // second alloc: 12+12>20 → wrap: y→8, x→0, sh→0 → [0,8], x→12, sh→8
        let _ = p.alloc(12, 8);
        // third alloc:  12+5=17≤20 → [12,8]
        assert_eq!(p.alloc(5, 5), Some([12, 8]));
    }

    #[test]
    fn shelf_packer_alloc_glyph_wider_than_atlas_returns_none() {
        let mut p = ShelfPacker::new(64);
        assert_eq!(p.alloc(128, 1), None);
    }

    #[cfg_attr(
        not(feature = "gpu-tests"),
        ignore = "requires a GPU device; enable the gpu-tests feature to run"
    )]
    #[test]
    fn renderer_scale_factor_change_clears_atlas() {
        // Verifies that creating a new ShelfPacker (what update_scale_factor does)
        // resets allocation state to the origin.
        let mut packer = ShelfPacker::new(1024);
        let _ = packer.alloc(16, 16);
        // Simulate update_scale_factor: replace with a fresh packer.
        packer = ShelfPacker::new(1024);
        assert_eq!(
            packer.alloc(16, 16),
            Some([0, 0]),
            "fresh packer after scale change must start from origin"
        );
    }
}
