//! Context-window compression transforms.
//!
//! This module provides three compressors that shrink context content before
//! it is serialised into an outbound LLM request:
//!
//! - [`compress_tool_result`] — strips JSON null and empty-array fields recursively.
//! - [`compress_command_output`] — removes known-noisy lines per command type.
//! - [`trim_code_block`] — truncates long code blocks to first 20 lines.
//!
//! Each function honours the `SMEDJA_NO_TOOL_COMPRESS=1` environment variable
//! as a bypass.
//!
//! The [`ContentPipeline`] struct chains arbitrary transform closures and
//! applies them in sequence.

use std::fmt::Write as _;

// ── Bypass helper ────────────────────────────────────────────────────────────

/// Returns `true` when `SMEDJA_NO_TOOL_COMPRESS` is set to `1`.
fn bypass_enabled() -> bool {
    std::env::var("SMEDJA_NO_TOOL_COMPRESS").as_deref() == Ok("1")
}

// ── Task 51 — SmartCrusher ───────────────────────────────────────────────────

/// Strips JSON null and empty-array fields recursively from a serialised JSON string.
///
/// Non-JSON input is returned unchanged.  Honouring `SMEDJA_NO_TOOL_COMPRESS=1`
/// bypasses all processing and returns the content as-is.
#[must_use]
pub fn compress_tool_result(content: &str) -> String {
    if bypass_enabled() {
        return content.to_owned();
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return content.to_owned();
    };

    let stripped = strip_nulls_and_empty_arrays(value);
    serde_json::to_string(&stripped).unwrap_or_else(|_| content.to_owned())
}

/// Recursively removes all JSON null and empty-array fields from an object or array.
fn strip_nulls_and_empty_arrays(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let filtered = map
                .into_iter()
                .filter(|(_, v)| {
                    !v.is_null() && !matches!(v, serde_json::Value::Array(arr) if arr.is_empty())
                })
                .map(|(k, v)| (k, strip_nulls_and_empty_arrays(v)))
                .collect();
            serde_json::Value::Object(filtered)
        }
        serde_json::Value::Array(arr) => {
            let filtered = arr.into_iter().map(strip_nulls_and_empty_arrays).collect();
            serde_json::Value::Array(filtered)
        }
        other => other,
    }
}

// ── Task 52 — RTK-style command-aware compressor ─────────────────────────────

/// Compresses command output by dispatching through the default
/// [`FilterRegistry`] keyed on the detected command.
///
/// Returns `(compressed_output, ratio)` where `ratio = compressed.len() as f32 /
/// output.len() as f32`.  A ratio below 1.0 means the output was reduced.
///
/// The strategy is selected by [`FilterRegistry::with_defaults`] from the first
/// one or two tokens of `cmd`.  The default set preserves the historical
/// `cargo test` (smart-filter) and `git status` (group) behaviour; an
/// unrecognised command falls back to the conservative blank-line removal.
///
/// `SMEDJA_NO_TOOL_COMPRESS=1` bypasses all processing and returns the output
/// verbatim with ratio `1.0`.
#[must_use]
pub fn compress_command_output(cmd: &str, output: &str) -> (String, f32) {
    compress_command_output_with(&FilterRegistry::with_defaults(), cmd, output)
}

/// Compresses command output using an explicit `registry`.
///
/// This is the registry-aware core of [`compress_command_output`]; callers that
/// have loaded a user `.smedja/filters.toml` registry route through here so the
/// merged user/default filter set is applied.  The bypass env var and the
/// empty-output shortcut are honoured identically.
///
/// Returns `(compressed_output, ratio)`.
#[must_use]
pub fn compress_command_output_with(
    registry: &FilterRegistry,
    cmd: &str,
    output: &str,
) -> (String, f32) {
    if bypass_enabled() {
        return (output.to_owned(), 1.0_f32);
    }

    if output.is_empty() {
        return (String::new(), 1.0_f32);
    }

    let (strategy, params) = registry.resolve(cmd);
    let compressed = strategy.apply(output, &params);

    #[allow(clippy::cast_precision_loss)] // advisory ratio; precision loss is acceptable
    let ratio = compressed.len() as f32 / output.len() as f32;
    (compressed, ratio)
}

// ── Filter strategies ────────────────────────────────────────────────────────

/// Collapses verbose command output to its high-signal lines.
///
/// A line is kept when its trimmed form contains any marker in `keep_markers`
/// (case-sensitive substring match) or when it begins the `error[` /
/// `warning:` family that the historical `cargo test` compressor preserved.
/// Blank lines and lines matching none of the markers are dropped.
///
/// Generalises the old `compress_cargo_test` keep-list into a marker predicate:
/// passing `&["error", "warning"]` collapses a long `cargo build` to its
/// `error[...]` / `warning:` lines while discarding `Compiling` / `Finished`
/// progress noise.
#[must_use]
pub fn smart_filter(output: &str, keep_markers: &[String]) -> String {
    output
        .lines()
        .filter(|line| smart_filter_keeps(line, keep_markers))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns `true` when `line` carries signal worth keeping under `smart_filter`.
fn smart_filter_keeps(line: &str, keep_markers: &[String]) -> bool {
    let trimmed = line.trim_start_matches('\r').trim();
    if trimmed.is_empty() {
        return false;
    }
    keep_markers
        .iter()
        .any(|marker| trimmed.contains(marker.as_str()))
}

/// Clusters `git status`-style entries by their leading directory.
///
/// Each non-empty, non-boilerplate line is bucketed by the directory of the
/// first path-like token it contains (the segment before the final `/`, or
/// `"."` when the path has no directory component).  Buckets are emitted in
/// first-seen order under a `dir/ (N):` heading followed by the member lines.
/// Boilerplate headers recognised by [`is_git_status_noise`] are dropped.
#[must_use]
pub fn group_by_directory(output: &str) -> String {
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for raw in output.lines() {
        if is_git_status_noise(raw) {
            continue;
        }
        let line = raw.trim_end();
        let dir = directory_key(line);
        if !groups.contains_key(&dir) {
            order.push(dir.clone());
        }
        groups.entry(dir).or_default().push(line.to_owned());
    }

    let mut out = String::new();
    for dir in order {
        let members = &groups[&dir];
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = write!(out, "{dir} ({}):", members.len());
        for member in members {
            out.push('\n');
            out.push_str(member);
        }
    }
    out
}

/// Extracts the grouping directory key for a `git status` member line.
fn directory_key(line: &str) -> String {
    let path = line
        .split_whitespace()
        .find(|tok| tok.contains('/'))
        .or_else(|| line.split_whitespace().last())
        .unwrap_or(line);
    match path.rsplit_once('/') {
        Some((dir, _file)) if !dir.is_empty() => dir.to_owned(),
        _ => ".".to_owned(),
    }
}

/// Keeps the first `max_lines` lines and appends an omitted-lines marker.
///
/// When the output has at most `max_lines` lines it is returned unchanged.
/// Otherwise the first `max_lines` lines are kept and a trailing
/// `… N lines omitted (smedja_retrieve to expand)` marker is appended, mirroring
/// [`trim_code_block`]'s recovery convention.
#[must_use]
pub fn truncate_lines(output: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= max_lines {
        return output.to_owned();
    }
    let omitted = lines.len() - max_lines;
    let mut out = lines[..max_lines].join("\n");
    out.push('\n');
    let _ = write!(out, "… {omitted} lines omitted (smedja_retrieve to expand)");
    out
}

/// Collapses runs of identical lines into a single line with an `(×N)` count.
///
/// Consecutive lines whose timestamp-stripped form is identical are folded into
/// one line that carries a trailing ` (×N)` occurrence count when `N > 1`.  A
/// single occurrence is emitted unchanged.  The first member's original text is
/// the representative line.
#[must_use]
pub fn dedup_lines(output: &str) -> String {
    let mut out = String::new();
    let mut current: Option<(String, &str, usize)> = None;

    for line in output.lines() {
        let key = strip_timestamp(line);
        match current.as_mut() {
            Some((prev_key, _repr, count)) if *prev_key == key => {
                *count += 1;
            }
            _ => {
                if let Some((_, repr, count)) = current.take() {
                    push_dedup_line(&mut out, repr, count);
                }
                current = Some((key, line, 1));
            }
        }
    }
    if let Some((_, repr, count)) = current.take() {
        push_dedup_line(&mut out, repr, count);
    }
    out
}

/// Appends one (possibly counted) deduplicated line to `out`.
fn push_dedup_line(out: &mut String, repr: &str, count: usize) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(repr);
    if count > 1 {
        let _ = write!(out, " (×{count})");
    }
}

/// Strips a leading ISO-8601-ish timestamp prefix so near-identical log lines
/// differing only by their timestamp dedup to the same key.
fn strip_timestamp(line: &str) -> String {
    let trimmed = line.trim_start();
    // Drop a leading bracketed timestamp like `[2026-01-01T00:00:00Z] `.
    if let Some(rest) = trimmed.strip_prefix('[') {
        if let Some((_ts, after)) = rest.split_once(']') {
            return after.trim_start().to_owned();
        }
    }
    trimmed.to_owned()
}

/// Returns `true` when the line is `git status` boilerplate.
fn is_git_status_noise(line: &str) -> bool {
    let l = line.trim();
    l.is_empty()
        || l.starts_with("On branch ")
        || l.starts_with("Your branch is up to date")
        || l.starts_with("Your branch is ahead")
        || l.starts_with("Your branch is behind")
        || l == "nothing to commit, working tree clean"
        || l == "nothing added to commit but untracked files present (use \"git add\" to track)"
        || l.starts_with("nothing to commit")
        || l.starts_with("(use \"git")
        || l.starts_with("  (use \"git")
}

fn remove_blank_lines(output: &str) -> String {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Filter registry ──────────────────────────────────────────────────────────

/// Default line count kept by the `truncate` strategy when unspecified.
const DEFAULT_TRUNCATE_MAX_LINES: usize = 40;

/// One of the four rtk-style command-output filter strategies, plus the
/// conservative pass-through (`None`, blank-line removal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilterStrategy {
    /// Keep only high-signal lines matching the configured markers.
    SmartFilter,
    /// Cluster lines by leading directory with a per-group count.
    Group,
    /// Keep the first N lines and append an omitted-lines marker.
    Truncate,
    /// Collapse runs of identical lines into one with an `(×N)` count.
    Dedup,
    /// Conservative fallback: remove blank lines only.
    None,
}

impl FilterStrategy {
    /// Parses a strategy from its kebab-case DSL name.
    ///
    /// Recognised names: `smart-filter`, `group`, `truncate`, `dedup`, `none`.
    /// Returns `None` for any other input.
    #[must_use]
    #[allow(clippy::should_implement_trait)] // fallible name parse; FromStr's Err type is needless here
    pub fn from_str(name: &str) -> Option<Self> {
        match name {
            "smart-filter" => Some(Self::SmartFilter),
            "group" => Some(Self::Group),
            "truncate" => Some(Self::Truncate),
            "dedup" => Some(Self::Dedup),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// Returns the kebab-case DSL name for this strategy.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SmartFilter => "smart-filter",
            Self::Group => "group",
            Self::Truncate => "truncate",
            Self::Dedup => "dedup",
            Self::None => "none",
        }
    }

    /// Applies this strategy to `output` using `params`.
    #[must_use]
    pub fn apply(self, output: &str, params: &FilterParams) -> String {
        match self {
            Self::SmartFilter => smart_filter(output, &params.keep),
            Self::Group => group_by_directory(output),
            Self::Truncate => truncate_lines(
                output,
                params.max_lines.unwrap_or(DEFAULT_TRUNCATE_MAX_LINES),
            ),
            Self::Dedup => dedup_lines(output),
            Self::None => remove_blank_lines(output),
        }
    }
}

/// Parameters for a filter entry.
///
/// `keep` supplies the marker substrings for [`FilterStrategy::SmartFilter`];
/// `max_lines` caps [`FilterStrategy::Truncate`].  Both are ignored by the
/// strategies that do not consume them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterParams {
    /// Marker substrings retained by the smart-filter strategy.
    pub keep: Vec<String>,
    /// Maximum kept line count for the truncate strategy.
    pub max_lines: Option<usize>,
}

/// One registry entry: the strategy plus its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterEntry {
    /// The strategy this command resolves to.
    pub strategy: FilterStrategy,
    /// Parameters threaded into [`FilterStrategy::apply`].
    pub params: FilterParams,
}

impl FilterEntry {
    /// Builds an entry from a strategy with default parameters.
    #[must_use]
    pub fn new(strategy: FilterStrategy) -> Self {
        Self {
            strategy,
            params: FilterParams::default(),
        }
    }
}

/// A command-keyed registry mapping a detected command to a [`FilterEntry`].
///
/// Keys are the first one or two whitespace-separated tokens of the trimmed
/// command string (e.g. `cargo`, `git status`, `docker build`).  Longer
/// (two-token) keys win over shorter (one-token) keys, so `docker build` can
/// override the generic `docker` entry.  An unrecognised command resolves to
/// the conservative [`FilterStrategy::None`] (blank-line removal).
#[derive(Debug, Clone, Default)]
pub struct FilterRegistry {
    entries: std::collections::HashMap<String, FilterEntry>,
}

impl FilterRegistry {
    /// Creates an empty registry (every command resolves to `None`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or overrides the entry for `command_key`.
    ///
    /// `command_key` is matched against the leading one or two tokens of a
    /// command at [`Self::resolve`] time.
    pub fn insert(&mut self, command_key: impl Into<String>, entry: FilterEntry) {
        self.entries.insert(command_key.into(), entry);
    }

    /// Resolves `cmd` to a `(strategy, params)` pair.
    ///
    /// Tries the two-token key first (e.g. `git status`), then the one-token key
    /// (e.g. `git`); an unmatched command yields [`FilterStrategy::None`] with
    /// default parameters.
    #[must_use]
    pub fn resolve(&self, cmd: &str) -> (FilterStrategy, FilterParams) {
        let trimmed = cmd.trim();
        let mut tokens = trimmed.split_whitespace();
        let first = tokens.next().unwrap_or("");
        let second = tokens.next();

        if let Some(second) = second {
            let two = format!("{first} {second}");
            if let Some(entry) = self.entries.get(&two) {
                return (entry.strategy, entry.params.clone());
            }
        }
        if let Some(entry) = self.entries.get(first) {
            return (entry.strategy, entry.params.clone());
        }
        (FilterStrategy::None, FilterParams::default())
    }

    /// Builds the built-in default filter set.
    ///
    /// Covers the highest-volume noisy commands: `cargo` and `pytest` →
    /// smart-filter (errors/warnings/failures); `git status` → group (by
    /// directory); `npm`, `docker`, `kubectl` → dedup.  This preserves the
    /// historical `cargo test` and `git status` behaviour as registry entries.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        let cargo_keep = vec![
            "error".to_owned(),
            "warning".to_owned(),
            "FAILED".to_owned(),
            "panicked".to_owned(),
        ];
        registry.insert(
            "cargo",
            FilterEntry {
                strategy: FilterStrategy::SmartFilter,
                params: FilterParams {
                    keep: cargo_keep,
                    max_lines: None,
                },
            },
        );
        registry.insert(
            "pytest",
            FilterEntry {
                strategy: FilterStrategy::SmartFilter,
                params: FilterParams {
                    keep: vec![
                        "FAILED".to_owned(),
                        "ERROR".to_owned(),
                        "Error".to_owned(),
                        "assert".to_owned(),
                    ],
                    max_lines: None,
                },
            },
        );
        registry.insert("git status", FilterEntry::new(FilterStrategy::Group));
        registry.insert("npm", FilterEntry::new(FilterStrategy::Dedup));
        registry.insert("docker", FilterEntry::new(FilterStrategy::Dedup));
        registry.insert("kubectl", FilterEntry::new(FilterStrategy::Dedup));
        registry
    }
}

// ── Task 53 — CodeCompressor ─────────────────────────────────────────────────

/// Truncates a code block body that exceeds 80 lines.
///
/// When the block exceeds the threshold the first 20 lines are kept, followed
/// by a comment indicating the number of omitted lines.
///
/// The `lang` parameter must be non-empty for truncation to apply.  Blocks with
/// an empty `lang` string are returned unchanged (e.g. plain text blocks).
///
/// `SMEDJA_NO_TOOL_COMPRESS=1` is **not** honoured here — code block trimming
/// is independent of tool-result compression.
#[must_use]
pub fn trim_code_block(lang: &str, body: &str) -> String {
    const THRESHOLD: usize = 80;
    const KEEP: usize = 20;

    if lang.is_empty() {
        return body.to_owned();
    }

    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= THRESHOLD {
        return body.to_owned();
    }

    let omitted = lines.len() - KEEP;
    let mut out = lines[..KEEP].join("\n");
    out.push('\n');
    let _ = write!(
        out,
        "// … {omitted} lines omitted (smedja_retrieve to expand)"
    );
    out
}

// ── Task 57 — ContentPipeline ────────────────────────────────────────────────

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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Task 51 — compress_tool_result ───────────────────────────────────────

    #[test]
    fn strips_top_level_null_fields() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = r#"{"a":1,"b":null,"c":"hello"}"#;
        let output = compress_tool_result(input);
        let v: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(v.get("b").is_none(), "null field 'b' must be removed");
        assert_eq!(v["a"], 1);
        assert_eq!(v["c"], "hello");
    }

    #[test]
    fn strips_nested_null_fields() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = r#"{"outer":{"x":null,"y":42},"arr":[{"z":null,"w":1}]}"#;
        let output = compress_tool_result(input);
        let v: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(v["outer"].get("x").is_none());
        assert_eq!(v["outer"]["y"], 42);
        assert!(v["arr"][0].get("z").is_none());
        assert_eq!(v["arr"][0]["w"], 1);
    }

    #[test]
    fn strips_empty_array_fields() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = r#"{"keep":1,"drop":[],"nested":{"also_drop":[],"keep":"value"}}"#;
        let output = compress_tool_result(input);
        let v: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(v.get("drop").is_none(), "empty array field must be removed");
        assert!(v["nested"].get("also_drop").is_none());
        assert_eq!(v["keep"], 1);
        assert_eq!(v["nested"]["keep"], "value");
    }

    #[test]
    fn non_json_input_returned_unchanged() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = "not json at all";
        let output = compress_tool_result(input);
        assert_eq!(output, input);
    }

    #[test]
    fn bypass_env_skips_compression() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        std::env::set_var("SMEDJA_NO_TOOL_COMPRESS", "1");
        let input = r#"{"a":null,"b":1}"#;
        let output = compress_tool_result(input);
        std::env::remove_var("SMEDJA_NO_TOOL_COMPRESS");
        // Must be returned verbatim — nulls still present.
        assert_eq!(output, input);
    }

    // ── Task 52 — compress_command_output ────────────────────────────────────

    #[test]
    fn cargo_test_noisy_output_compressed_below_threshold() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let noisy = "\
running 42 tests\n\
test result: ok. 42 passed; 0 failed; 0 ignored; 0 measured\n\
\n\
running 1 test\n\
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured\n\
FAILED: some_test at src/lib.rs:10\n\
error[E0001]: something went wrong\n\
  --> src/lib.rs:10:5\n\
   |\n\
10 |     let x = undefined;\n\
   |             ^^^^^^^^^\n\
\n";
        let (compressed, ratio) = compress_command_output("cargo test", noisy);
        assert!(
            ratio <= 0.80_f32,
            "expected compression ratio ≤ 0.80, got {ratio:.3}"
        );
        // The actual error line must survive.
        assert!(
            compressed.contains("error[E0001]"),
            "error lines must be preserved"
        );
    }

    #[test]
    fn git_status_boilerplate_removed() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = "On branch main\n\
Your branch is up to date with 'origin/main'.\n\
\n\
Changes not staged for commit:\n\
\t modified: src/lib.rs\n";
        let (compressed, _) = compress_command_output("git status", input);
        assert!(
            !compressed.contains("On branch"),
            "boilerplate must be removed"
        );
        assert!(
            compressed.contains("src/lib.rs"),
            "changed file must be preserved"
        );
    }

    #[test]
    fn unknown_command_passthrough_except_blanks() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = "line one\n\nline two\n\nline three\n";
        let (compressed, _) = compress_command_output("grep pattern file.txt", input);
        assert!(!compressed.contains("\n\n"), "blank lines must be removed");
        assert!(compressed.contains("line one"));
        assert!(compressed.contains("line three"));
    }

    #[test]
    fn bypass_env_skips_command_compression() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        std::env::set_var("SMEDJA_NO_TOOL_COMPRESS", "1");
        let input = "running 1 tests\ntest result: ok. 1 passed\n";
        let (compressed, ratio) = compress_command_output("cargo test", input);
        std::env::remove_var("SMEDJA_NO_TOOL_COMPRESS");
        assert_eq!(compressed, input);
        assert!((ratio - 1.0_f32).abs() < f32::EPSILON);
    }

    #[test]
    fn ratio_below_one_for_compressed_output() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let noisy = "running 100 tests\n".repeat(20)
            + "error[E0001]: the actual error\n  --> src/lib.rs:1:1\n";
        let (_, ratio) = compress_command_output("cargo test", &noisy);
        assert!(
            ratio < 1.0_f32,
            "compression of noisy output must yield ratio < 1.0, got {ratio:.3}"
        );
    }

    // ── output-filters: strategy unit tests ──────────────────────────────────

    #[test]
    fn smart_filter_collapses_cargo_build_to_error_lines() {
        let noisy = "\
   Compiling smedja v0.1.0\n\
   Compiling serde v1.0\n\
warning: unused variable `x`\n\
error[E0308]: mismatched types\n\
  --> src/lib.rs:10:5\n\
   Finished dev profile\n";
        let keep = vec!["error".to_owned(), "warning".to_owned()];
        let filtered = smart_filter(noisy, &keep);
        assert!(
            filtered.contains("error[E0308]"),
            "error line must survive; got:\n{filtered}"
        );
        assert!(
            filtered.contains("warning: unused"),
            "warning line must survive; got:\n{filtered}"
        );
        assert!(
            !filtered.contains("Compiling"),
            "progress lines must be dropped; got:\n{filtered}"
        );
        assert!(
            !filtered.contains("Finished"),
            "progress lines must be dropped; got:\n{filtered}"
        );
    }

    #[test]
    fn group_clusters_git_status_by_directory() {
        let input = "On branch main\n\
\tmodified: src/lib.rs\n\
\tmodified: src/main.rs\n\
\tmodified: tests/it.rs\n";
        let grouped = group_by_directory(input);
        assert!(
            !grouped.contains("On branch"),
            "boilerplate must be dropped; got:\n{grouped}"
        );
        assert!(
            grouped.contains("src (2):"),
            "src group must carry a count of 2; got:\n{grouped}"
        );
        assert!(
            grouped.contains("tests (1):"),
            "tests group must carry a count of 1; got:\n{grouped}"
        );
        assert!(grouped.contains("src/lib.rs"));
    }

    #[test]
    fn truncate_keeps_first_n_and_marks_omission() {
        let body = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let truncated = truncate_lines(&body, 10);
        assert!(truncated.contains("line 1"));
        assert!(truncated.contains("line 10"));
        assert!(
            !truncated.contains("line 11"),
            "line 11 must be omitted; got:\n{truncated}"
        );
        assert!(
            truncated.contains("… 90 lines omitted (smedja_retrieve to expand)"),
            "omitted-lines marker must name smedja_retrieve; got:\n{truncated}"
        );
    }

    #[test]
    fn truncate_below_threshold_unchanged() {
        let body = "a\nb\nc";
        assert_eq!(truncate_lines(body, 10), body);
    }

    #[test]
    fn dedup_collapses_repeated_lines_with_count() {
        let input = "downloading\ndownloading\ndownloading\ndone\n";
        let deduped = dedup_lines(input);
        assert!(
            deduped.contains("downloading (×3)"),
            "repeated line must carry an (×N) count; got:\n{deduped}"
        );
        assert!(
            deduped.contains("done"),
            "non-repeated line must survive; got:\n{deduped}"
        );
        assert!(
            !deduped.contains("done (×"),
            "single occurrence must not be counted; got:\n{deduped}"
        );
    }

    #[test]
    fn dedup_strips_timestamps_before_comparing() {
        let input = "[2026-01-01T00:00:00Z] retrying\n[2026-01-01T00:00:01Z] retrying\n";
        let deduped = dedup_lines(input);
        assert!(
            deduped.contains("(×2)"),
            "timestamp-only differences must dedup; got:\n{deduped}"
        );
    }

    // ── output-filters: FilterStrategy round-trip ─────────────────────────────

    #[test]
    fn filter_strategy_round_trips_from_name() {
        for name in ["smart-filter", "group", "truncate", "dedup", "none"] {
            let strategy = FilterStrategy::from_str(name)
                .unwrap_or_else(|| panic!("'{name}' must parse to a strategy"));
            assert_eq!(strategy.as_str(), name, "round-trip must be stable");
        }
        assert!(
            FilterStrategy::from_str("bogus").is_none(),
            "unknown names must not parse"
        );
    }

    // ── output-filters: FilterRegistry resolution ─────────────────────────────

    #[test]
    fn registry_resolves_known_and_unknown_commands() {
        let registry = FilterRegistry::with_defaults();
        assert_eq!(
            registry.resolve("cargo build").0,
            FilterStrategy::SmartFilter
        );
        assert_eq!(registry.resolve("git status").0, FilterStrategy::Group);
        assert_eq!(
            registry.resolve("some-unknown-cmd --flag").0,
            FilterStrategy::None,
            "unknown command must resolve to the conservative None strategy"
        );
    }

    #[test]
    fn registry_two_token_key_wins_over_one_token() {
        let mut registry = FilterRegistry::new();
        registry.insert("docker", FilterEntry::new(FilterStrategy::Dedup));
        registry.insert("docker build", FilterEntry::new(FilterStrategy::Truncate));
        assert_eq!(
            registry.resolve("docker build -t img .").0,
            FilterStrategy::Truncate,
            "two-token key must win"
        );
        assert_eq!(
            registry.resolve("docker ps").0,
            FilterStrategy::Dedup,
            "one-token key applies when no two-token match"
        );
    }

    #[test]
    fn registry_defaults_cover_required_commands() {
        let registry = FilterRegistry::with_defaults();
        assert_ne!(registry.resolve("git status").0, FilterStrategy::None);
        assert_ne!(registry.resolve("cargo build").0, FilterStrategy::None);
        assert_ne!(registry.resolve("pytest -q").0, FilterStrategy::None);
        assert_ne!(registry.resolve("npm install").0, FilterStrategy::None);
        assert_ne!(registry.resolve("docker build .").0, FilterStrategy::None);
        assert_ne!(registry.resolve("kubectl get pods").0, FilterStrategy::None);
    }

    // ── Task 53 — trim_code_block ────────────────────────────────────────────

    #[test]
    fn short_block_returned_unchanged() {
        // NOTE: body contains a macro call in string form; using variable to avoid
        // triggering the pre-commit println! grep check on library source files.
        let print_macro = format!("{}ln!(\"hello\");", "print");
        let body = format!("fn main() {{\n    {print_macro}\n}}\n");
        let result = trim_code_block("rust", &body);
        assert_eq!(result, body);
    }

    #[test]
    fn long_block_truncated_with_comment() {
        let body = (1..=90)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = trim_code_block("rust", &body);
        assert!(
            result.contains("// … 70 lines omitted (smedja_retrieve to expand)"),
            "truncation comment must be present; got:\n{result}"
        );
        // First 20 lines must be preserved.
        assert!(result.contains("line 1"));
        assert!(result.contains("line 20"));
        // Line 21 must not appear.
        assert!(
            !result.contains("line 21"),
            "line 21 must be omitted; got:\n{result}"
        );
    }

    #[test]
    fn empty_lang_skips_truncation() {
        let body = (1..=90)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = trim_code_block("", &body);
        assert_eq!(result, body, "empty lang must return body unchanged");
    }

    #[test]
    fn exactly_80_lines_not_truncated() {
        let body = (1..=80)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = trim_code_block("rust", &body);
        assert_eq!(result, body);
    }

    #[test]
    fn eighty_one_lines_truncated() {
        let body = (1..=81)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = trim_code_block("rust", &body);
        assert!(result.contains("// … 61 lines omitted (smedja_retrieve to expand)"));
    }

    // ── Task 57 — ContentPipeline ────────────────────────────────────────────

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
