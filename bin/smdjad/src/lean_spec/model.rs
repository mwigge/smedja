//! Slice→umbrella pointer metadata, the umbrella vault namespace, and
//! detail chunking.
//!
//! These are the pure, dependency-free primitives of the lean-spec machinery: a
//! slice's pointer back to its umbrella, the `umbrella:<id>` namespace all of an
//! umbrella's chunks live under, and the paragraph-boundary chunker that keeps
//! each stored vault entry small enough for cold recall to discriminate.

/// A slice's pointer back to its umbrella.
///
/// The link is metadata — an `umbrella_id` and a `slice_n` — modelled on the
/// vault payload convention, NOT a `parent` field in the `OpenSpec` change
/// manifest (which stays flat: `schema` + `created` only). A slice carries this
/// pointer instead of restating the umbrella's Why or design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlicePointer {
    /// Identifier of the umbrella this slice belongs to.
    pub umbrella_id: String,
    /// 1-based ordinal of this slice within the umbrella's slice list.
    pub slice_n: u32,
}

impl SlicePointer {
    /// Creates a pointer to slice `slice_n` of umbrella `umbrella_id`.
    #[must_use]
    pub(crate) fn new(umbrella_id: impl Into<String>, slice_n: u32) -> Self {
        Self {
            umbrella_id: umbrella_id.into(),
            slice_n,
        }
    }

    /// Renders the pointer as the JSON payload a slice records, reusing the
    /// vault payload-kind convention (`{"kind":"slice", ...}`).
    #[must_use]
    pub(crate) fn to_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": "slice",
            "umbrella_id": self.umbrella_id,
            "slice_n": self.slice_n,
        })
    }

    /// Returns the umbrella namespace this pointer resolves to.
    #[must_use]
    pub(crate) fn umbrella_namespace(&self) -> String {
        umbrella_namespace(&self.umbrella_id)
    }
}

/// Builds the vault namespace that holds an umbrella's chunks.
///
/// All of an umbrella's design-detail chunks live under `umbrella:<id>`, so a
/// slice can resolve and recall its umbrella by id with a single namespace
/// scope.
#[must_use]
pub(crate) fn umbrella_namespace(umbrella_id: &str) -> String {
    format!("umbrella:{umbrella_id}")
}

/// Splits `detail` into chunks no longer than `max_chars` on paragraph
/// boundaries.
///
/// Umbrella design detail is large and variable; chunking keeps each vault entry
/// small enough that cold recall can surface the relevant fragment rather than
/// the whole document. Paragraphs (`\n\n`-separated) are packed greedily; a lone
/// paragraph longer than `max_chars` becomes its own chunk rather than being
/// split mid-word. Returns an empty `Vec` for blank input.
#[must_use]
pub(crate) fn chunk_detail(detail: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for paragraph in detail
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        // A fresh oversized paragraph stands alone rather than being split mid-word.
        if current.is_empty() {
            current.push_str(paragraph);
        } else if current.len() + 2 + paragraph.len() <= max_chars {
            current.push_str("\n\n");
            current.push_str(paragraph);
        } else {
            chunks.push(std::mem::take(&mut current));
            current.push_str(paragraph);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_records_umbrella_pointer_as_payload_metadata() {
        // Task 2.1/2.2: a slice records umbrella_id + slice_n as pointer
        // metadata (the vault payload convention), not a manifest parent field.
        let pointer = SlicePointer::new("alpha", 3);
        let payload = pointer.to_payload();
        assert_eq!(payload["kind"], serde_json::json!("slice"));
        assert_eq!(payload["umbrella_id"], serde_json::json!("alpha"));
        assert_eq!(payload["slice_n"], serde_json::json!(3));
        // The pointer resolves to the umbrella's namespace by id alone.
        assert_eq!(pointer.umbrella_namespace(), "umbrella:alpha");
    }
}
