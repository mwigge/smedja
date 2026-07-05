//! Auto-activation selector — promotes bundle advisories to selection.
//!
//! At turn start the orchestrator injects only the cheap L1 index
//! ([`crate::Bundle::l1_index`]). This module then decides which items' full
//! bodies are worth inlining, by matching each item against:
//!
//! - the turn text — a case-insensitive substring match on the item's
//!   `trigger_phrases`, its `name`, or significant words of its `description`;
//! - the turn's touched files — a glob match of the item's `paths` patterns.
//!
//! It is the same signal idea as `smedja_methodology::skill_inject`, promoted
//! from a warn-only backstop to a turn-start selection.

use crate::bundle::{BundleItem, BundleKind};

/// Selects the items whose triggers, name, description, or path globs match the
/// current turn. Agents are never selected (they are routing targets, not
/// inline context). Order and de-duplication follow the input `items` order.
#[must_use]
pub fn select<'a>(
    items: &'a [BundleItem],
    turn_text: &str,
    touched_files: &[String],
) -> Vec<&'a BundleItem> {
    let haystack = turn_text.to_lowercase();
    items
        .iter()
        .filter(|item| item.kind != BundleKind::Agent && matches(item, &haystack, touched_files))
        .collect()
}

/// Whether a single item matches the (already lowercased) turn text or any
/// touched file.
fn matches(item: &BundleItem, turn_lower: &str, touched_files: &[String]) -> bool {
    // 1. Explicit trigger phrases.
    if item
        .triggers
        .iter()
        .any(|t| !t.trim().is_empty() && turn_lower.contains(&t.to_lowercase()))
    {
        return true;
    }
    // 2. The item's own name mentioned in the turn.
    if word_present(turn_lower, &item.name.to_lowercase()) {
        return true;
    }
    // 3. Significant description words (length > 4) mentioned in the turn.
    if item
        .description
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 4)
        .any(|w| word_present(turn_lower, &w.to_lowercase()))
    {
        return true;
    }
    // 4. Path globs against the turn's touched files.
    item.paths
        .iter()
        .any(|pat| touched_files.iter().any(|f| glob_match(pat, f)))
}

/// Whether `needle` appears in `haystack` bounded by non-alphanumeric edges, so
/// `sql` matches `run sql` but not `mysql`. Both arguments must be lowercase.
fn word_present(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let nlen = needle.len();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let at = start + pos;
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric() && bytes[at - 1] != b'_';
        let after = at + nlen;
        let after_ok =
            after == bytes.len() || !bytes[after].is_ascii_alphanumeric() && bytes[after] != b'_';
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
        if start >= bytes.len() {
            break;
        }
    }
    false
}

/// Matches a glob `pattern` against a `path`, with `/`-aware semantics:
///
/// - `**` matches any run of characters, including `/`.
/// - `*` matches any run of characters except `/`.
/// - `?` matches exactly one character except `/`.
/// - every other character matches itself.
///
/// A leading `**/` also matches zero path segments, so `**/*.rs` matches
/// `main.rs`.
#[must_use]
pub fn glob_match(pattern: &str, path: &str) -> bool {
    // Normalise a leading `**/` so it can match zero segments too.
    if let Some(rest) = pattern.strip_prefix("**/") {
        if glob_match(rest, path) {
            return true;
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = path.chars().collect();
    glob_rec(&p, &s)
}

/// Recursive backtracking matcher over char slices.
fn glob_rec(p: &[char], s: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('*') => {
            if p.get(1) == Some(&'*') {
                // `**` — consume any characters (including `/`).
                let rest = &p[2..];
                // A `**/` prefix should also be able to match zero segments.
                let rest = rest.strip_prefix(&['/']).unwrap_or(rest);
                (0..=s.len()).any(|i| glob_rec(rest, &s[i..]))
            } else {
                // `*` — consume characters within a single segment (not `/`).
                let rest = &p[1..];
                let mut i = 0;
                loop {
                    if glob_rec(rest, &s[i..]) {
                        return true;
                    }
                    if i >= s.len() || s[i] == '/' {
                        return false;
                    }
                    i += 1;
                }
            }
        }
        Some('?') => match s.first() {
            Some(&c) if c != '/' => glob_rec(&p[1..], &s[1..]),
            _ => false,
        },
        Some(&c) => match s.first() {
            Some(&d) if c == d => glob_rec(&p[1..], &s[1..]),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::bundle::BundleItem;

    fn skill(name: &str, triggers: &[&str], paths: &[&str], desc: &str) -> BundleItem {
        BundleItem {
            kind: BundleKind::Skill,
            name: name.to_owned(),
            description: desc.to_owned(),
            triggers: triggers.iter().map(ToString::to_string).collect(),
            paths: paths.iter().map(ToString::to_string).collect(),
            path: PathBuf::from(format!("/x/{name}.md")),
            body: String::new(),
            supporting_files: Vec::new(),
            agent: None,
        }
    }

    // --- glob ---------------------------------------------------------------

    #[test]
    fn glob_star_stays_within_segment() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
    }

    #[test]
    fn glob_double_star_crosses_segments() {
        assert!(glob_match("src/**/*.rs", "src/a/b/main.rs"));
        assert!(glob_match("**/*.sql", "db/migrations/001.sql"));
        assert!(
            glob_match("**/*.sql", "001.sql"),
            "leading **/ matches zero segments"
        );
    }

    #[test]
    fn glob_question_matches_single_non_slash() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
    }

    #[test]
    fn glob_non_match() {
        assert!(!glob_match("*.rs", "main.py"));
        assert!(!glob_match("src/*.rs", "tests/main.rs"));
    }

    // --- selection ----------------------------------------------------------

    #[test]
    fn selects_by_trigger_phrase() {
        let items = vec![skill("pg", &["postgres", "sql"], &[], "db patterns")];
        let sel = select(&items, "please write a postgres migration", &[]);
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].name, "pg");
    }

    #[test]
    fn selects_by_name_word_boundary() {
        let items = vec![skill("ponytail", &[], &[], "review lens")];
        assert_eq!(select(&items, "apply the ponytail lens", &[]).len(), 1);
        // A substring inside a larger word must NOT match.
        assert!(select(&items, "myponytailish", &[]).is_empty());
    }

    #[test]
    fn selects_by_touched_path_glob() {
        let items = vec![skill("sqlrules", &[], &["**/*.sql"], "sql")];
        let touched = vec!["db/migrations/001.sql".to_owned()];
        assert_eq!(select(&items, "unrelated turn text", &touched).len(), 1);
    }

    #[test]
    fn selects_by_significant_description_word() {
        let items = vec![skill("x", &[], &[], "kubernetes deployment helper")];
        assert_eq!(select(&items, "help me with kubernetes", &[]).len(), 1);
    }

    #[test]
    fn no_match_selects_nothing() {
        let items = vec![skill("pg", &["postgres"], &["**/*.sql"], "db")];
        assert!(select(&items, "write a rust struct", &["main.rs".to_owned()]).is_empty());
    }

    #[test]
    fn agents_are_never_selected() {
        let mut a = skill("reviewer", &["review"], &[], "reviews");
        a.kind = BundleKind::Agent;
        assert!(select(&[a], "please review this", &[]).is_empty());
    }

    #[test]
    fn preserves_input_order_and_dedupes() {
        let items = vec![
            skill("a", &["alpha"], &[], "first"),
            skill("b", &["alpha"], &[], "second"),
        ];
        let sel = select(&items, "alpha alpha", &[]);
        assert_eq!(sel.len(), 2);
        assert_eq!(sel[0].name, "a");
        assert_eq!(sel[1].name, "b");
    }
}
