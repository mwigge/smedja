//! Line-oriented filter strategies used by the command-output compressor.

use std::fmt::Write as _;

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

pub(super) fn remove_blank_lines(output: &str) -> String {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
