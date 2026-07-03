//! Task 57 — `ContentPipeline`: ordered chains of content transforms.

use super::code::trim_code_block;
use super::command::compress_command_output;
use super::crusher::compress_tool_result;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pipeline_returns_unchanged() {
        let pipeline = ContentPipeline::new();
        let result = pipeline.run("hello");
        assert_eq!(result, "hello");
    }

    #[test]
    fn pipeline_applies_transforms_in_order() {
        let append = |suffix: &'static str| move |content: &str| format!("{content}{suffix}");

        let pipeline = ContentPipeline::new()
            .push(append(" A"))
            .push(append(" B"))
            .push(append(" C"));

        let result = pipeline.run("start");
        assert_eq!(result, "start A B C");
    }

    #[test]
    fn smart_crusher_transform_strips_nulls() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let pipeline = ContentPipeline::new().push(|c: &str| compress_tool_result(c));
        let input = r#"{"keep":1,"drop":null}"#;
        let output = pipeline.run(input);
        let v: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(v.get("drop").is_none());
        assert_eq!(v["keep"], 1);
    }

    #[test]
    fn command_compressor_transform_removes_blanks() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let pipeline = ContentPipeline::new().push(command_compressor("echo hello"));
        let result = pipeline.run("line one\n\nline two\n");
        assert!(!result.contains("\n\n"), "blank lines must be removed");
    }

    #[test]
    fn code_compressor_transform_truncates_long_block() {
        let pipeline = ContentPipeline::new().push(code_compressor("python"));
        let body = (1..=90)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = pipeline.run(&body);
        assert!(result.contains("// … 70 lines omitted (smedja_retrieve to expand)"));
    }

    #[test]
    fn pipeline_default_produces_same_as_new() {
        let a = ContentPipeline::new();
        let b = ContentPipeline::default();
        assert_eq!(a.run("test"), b.run("test"));
    }
}
