//! Composable content-transform pipeline and the built-in transform constructors.

use super::{compress_command_output, compress_tool_result, trim_code_block};

/// A single content transform: a boxed function from content to content.
///
/// Transforms are plain function values rather than trait objects, so callers
/// can register closures directly via [`ContentPipeline::push`].
pub type Transform = Box<dyn Fn(&str) -> String + Send + Sync>;

/// Ordered pipeline of [`Transform`] closures.
///
/// Transforms are applied in the order they were registered via
/// [`ContentPipeline::push`].  An empty pipeline returns the content unchanged.
pub struct ContentPipeline {
    transforms: Vec<Transform>,
}

impl ContentPipeline {
    /// Creates an empty pipeline.
    #[must_use]
    pub fn new() -> Self {
        Self {
            transforms: Vec::new(),
        }
    }

    /// Appends a transform closure to the end of the pipeline.
    ///
    /// Returns `self` to allow method chaining.
    #[must_use]
    pub fn push(mut self, transform: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.transforms.push(Box::new(transform));
        self
    }

    /// Runs all transforms in order and returns the final content.
    #[must_use]
    pub fn run(&self, content: &str) -> String {
        self.transforms
            .iter()
            .fold(content.to_owned(), |acc, transform| transform(&acc))
    }
}

impl Default for ContentPipeline {
    fn default() -> Self {
        Self::new()
    }
}

// ── Transform constructors ────────────────────────────────────────────────────

/// Builds a transform that strips JSON null fields recursively from tool-result
/// content (wraps [`compress_tool_result`]).
#[must_use]
pub fn smart_crusher() -> Transform {
    Box::new(|content: &str| compress_tool_result(content))
}

/// Builds a transform that removes known-noisy lines for `cmd`
/// (wraps [`compress_command_output`]).
#[must_use]
pub fn command_compressor(cmd: impl Into<String>) -> Transform {
    let cmd = cmd.into();
    Box::new(move |content: &str| {
        let (compressed, _ratio) = compress_command_output(&cmd, content);
        compressed
    })
}

/// Builds a transform that truncates code blocks of language `lang` exceeding
/// 80 lines (wraps [`trim_code_block`]).
#[must_use]
pub fn code_compressor(lang: impl Into<String>) -> Transform {
    let lang = lang.into();
    Box::new(move |content: &str| trim_code_block(&lang, content))
}
