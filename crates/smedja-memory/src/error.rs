/// Errors produced by the `smedja-memory` crate.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// The requested message index is out of bounds.
    #[error("index {index} out of bounds (len {len})")]
    IndexOutOfBounds {
        /// The index that was requested.
        index: usize,
        /// The current length of the message store.
        len: usize,
    },

    /// An attempt was made to replace the mutable window with a slice that
    /// would shrink into the stable prefix.
    #[error("replacement length {replacement} would shrink below stable prefix {stable_prefix}")]
    ReplacementShrinksBelowPrefix {
        /// Length of the proposed replacement slice.
        replacement: usize,
        /// The current stable prefix boundary.
        stable_prefix: usize,
    },
}
