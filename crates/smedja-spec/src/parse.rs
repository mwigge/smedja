//! The delta parser/merger — the keystone primitive of the native engine.
//!
//! This module turns the OpenSpec markdown surface into the typed model and
//! back:
//!
//! - [`parse_delta`] reads a change's delta spec (`## ADDED Requirements` /
//!   `## MODIFIED Requirements` / `## REMOVED Requirements` sections, each of
//!   `### Requirement:` + `#### Scenario:` blocks) into a [`Delta`].
//! - [`parse_spec`] reads a capability's source spec into a [`Spec`], preserving
//!   its header preamble.
//! - [`render_delta`] / [`render_spec`] render the typed model back to markdown,
//!   so parse→render→parse round-trips.
//! - [`parse_pending_slices`] / [`task_counts`] read `tasks.md` — the single code
//!   path the loop's slice reader also routes through.

use crate::model::{Delta, DeltaOp, Requirement, Scenario, Spec};

/// Prefix that opens a requirement block.
const REQ_PREFIX: &str = "### Requirement:";
/// Prefix that opens a scenario block.
const SCN_PREFIX: &str = "#### Scenario:";

/// Returns the requirement name if `line` is a `### Requirement:` header.
fn requirement_header(line: &str) -> Option<&str> {
    line.trim().strip_prefix(REQ_PREFIX).map(str::trim)
}

/// Returns the scenario name if `line` is a `#### Scenario:` header.
fn scenario_header(line: &str) -> Option<&str> {
    line.trim().strip_prefix(SCN_PREFIX).map(str::trim)
}

/// Returns the delta operation if `line` is an `## ADDED/MODIFIED/REMOVED
/// Requirements` header.
fn op_header(line: &str) -> Option<DeltaOp> {
    let rest = line.trim().strip_prefix("## ")?;
    let word = rest.split_whitespace().next()?.to_ascii_uppercase();
    match word.as_str() {
        "ADDED" => Some(DeltaOp::Added),
        "MODIFIED" => Some(DeltaOp::Modified),
        "REMOVED" => Some(DeltaOp::Removed),
        _ => None,
    }
}

/// Joins lines, trimming surrounding blank lines while preserving interior
/// structure, so parse→render→parse is stable.
fn join_trim(lines: &[&str]) -> String {
    lines.join("\n").trim().to_owned()
}

/// Parses a block of requirement markdown into `(preamble, requirements)`.
///
/// The preamble is everything before the first `### Requirement:` header. Each
/// requirement absorbs its statement text up to its first `#### Scenario:`, and
/// each scenario absorbs the lines up to the next scenario or requirement.
fn parse_requirement_block(md: &str) -> (String, Vec<Requirement>) {
    let mut preamble: Vec<&str> = Vec::new();
    let mut requirements: Vec<Requirement> = Vec::new();

    let mut cur_req: Option<Requirement> = None;
    let mut cur_req_text: Vec<&str> = Vec::new();
    let mut cur_scn_name: Option<String> = None;
    let mut cur_scn_body: Vec<&str> = Vec::new();

    // Finalises the in-progress scenario into the in-progress requirement.
    fn flush_scenario(
        cur_req: &mut Option<Requirement>,
        cur_scn_name: &mut Option<String>,
        cur_scn_body: &mut Vec<&str>,
    ) {
        if let Some(name) = cur_scn_name.take() {
            if let Some(req) = cur_req.as_mut() {
                req.scenarios
                    .push(Scenario::new(name, join_trim(cur_scn_body)));
            }
        }
        cur_scn_body.clear();
    }

    for line in md.lines() {
        if let Some(name) = requirement_header(line) {
            flush_scenario(&mut cur_req, &mut cur_scn_name, &mut cur_scn_body);
            if let Some(mut req) = cur_req.take() {
                req.text = join_trim(&cur_req_text);
                requirements.push(req);
            }
            cur_req_text.clear();
            cur_req = Some(Requirement::new(name, String::new()));
        } else if let Some(name) = scenario_header(line) {
            flush_scenario(&mut cur_req, &mut cur_scn_name, &mut cur_scn_body);
            cur_scn_name = Some(name.to_owned());
        } else if cur_scn_name.is_some() {
            cur_scn_body.push(line);
        } else if cur_req.is_some() {
            cur_req_text.push(line);
        } else {
            preamble.push(line);
        }
    }

    flush_scenario(&mut cur_req, &mut cur_scn_name, &mut cur_scn_body);
    if let Some(mut req) = cur_req.take() {
        req.text = join_trim(&cur_req_text);
        requirements.push(req);
    }

    (join_trim(&preamble), requirements)
}

/// Parses a capability's source spec markdown into a [`Spec`].
///
/// The header text before the first requirement is preserved as the spec's
/// preamble so merges keep the title/purpose intact.
#[must_use]
pub fn parse_spec(capability: &str, md: &str) -> Spec {
    let (preamble, requirements) = parse_requirement_block(md);
    let preamble = if preamble.is_empty() {
        format!("# {capability} Specification\n\n## Requirements")
    } else {
        preamble
    };
    Spec {
        capability: capability.to_owned(),
        preamble,
        requirements,
    }
}

/// Parses a change's delta spec markdown into a [`Delta`].
///
/// Requirements are bucketed by the `## ADDED/MODIFIED/REMOVED Requirements`
/// section they appear under; requirements before any such header are ignored.
#[must_use]
pub fn parse_delta(capability: &str, md: &str) -> Delta {
    let mut delta = Delta::new(capability);
    let mut current_op: Option<DeltaOp> = None;
    let mut buffer: Vec<&str> = Vec::new();

    // Parses the accumulated buffer into the section for `op`.
    fn flush(delta: &mut Delta, op: Option<DeltaOp>, buffer: &mut Vec<&str>) {
        if let Some(op) = op {
            let (_, reqs) = parse_requirement_block(&buffer.join("\n"));
            match op {
                DeltaOp::Added => delta.added.extend(reqs),
                DeltaOp::Modified => delta.modified.extend(reqs),
                DeltaOp::Removed => delta.removed.extend(reqs),
            }
        }
        buffer.clear();
    }

    for line in md.lines() {
        if let Some(op) = op_header(line) {
            flush(&mut delta, current_op, &mut buffer);
            current_op = Some(op);
        } else {
            buffer.push(line);
        }
    }
    flush(&mut delta, current_op, &mut buffer);

    delta
}

/// Renders a single requirement back to markdown.
#[must_use]
pub fn render_requirement(req: &Requirement) -> String {
    let mut out = format!("{REQ_PREFIX} {}\n", req.name);
    if !req.text.trim().is_empty() {
        out.push_str(req.text.trim());
        out.push('\n');
    }
    for scn in &req.scenarios {
        out.push_str(&format!("\n{SCN_PREFIX} {}\n", scn.name));
        if !scn.body.trim().is_empty() {
            out.push_str(scn.body.trim());
            out.push('\n');
        }
    }
    out
}

/// Renders a delta back to markdown, emitting only non-empty sections in
/// `ADDED`, `MODIFIED`, `REMOVED` order.
#[must_use]
pub fn render_delta(delta: &Delta) -> String {
    let mut sections: Vec<String> = Vec::new();
    for (op, reqs) in [
        (DeltaOp::Added, &delta.added),
        (DeltaOp::Modified, &delta.modified),
        (DeltaOp::Removed, &delta.removed),
    ] {
        if reqs.is_empty() {
            continue;
        }
        let mut section = format!("{}\n", op.header());
        for req in reqs {
            section.push('\n');
            section.push_str(&render_requirement(req));
        }
        sections.push(section);
    }
    let body = sections.join("\n");
    if body.ends_with('\n') {
        body
    } else {
        format!("{body}\n")
    }
}

/// Renders a capability spec back to markdown: preamble followed by each
/// requirement.
#[must_use]
pub fn render_spec(spec: &Spec) -> String {
    let mut out = spec.preamble.trim().to_owned();
    out.push('\n');
    for req in &spec.requirements {
        out.push('\n');
        out.push_str(&render_requirement(req));
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Extracts the pending slices (`- [ ] ` lines) from `tasks.md`.
///
/// This is the single code path the loop's slice reader routes through, so the
/// engine and the loop agree on what counts as a pending slice.
#[must_use]
pub fn parse_pending_slices(tasks_md: &str) -> Vec<String> {
    tasks_md
        .lines()
        .filter_map(|l| l.strip_prefix("- [ ] ").map(|s| s.trim().to_owned()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Returns `(done, total)` task-item counts from `tasks.md`, where a task item
/// is a `- [ ]` (pending) or `- [x]`/`- [X]` (done) line.
#[must_use]
pub fn task_counts(tasks_md: &str) -> (usize, usize) {
    let mut done = 0;
    let mut total = 0;
    for raw in tasks_md.lines() {
        let line = raw.trim_start();
        if line.starts_with("- [ ] ") {
            total += 1;
        } else if line.starts_with("- [x] ") || line.starts_with("- [X] ") {
            total += 1;
            done += 1;
        }
    }
    (done, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DELTA: &str = "\
## ADDED Requirements

### Requirement: Native Engine
The daemon SHALL provide a native OpenSpec engine.

#### Scenario: Create a change
- WHEN spec_create is called
- THEN the change is scaffolded

#### Scenario: Validate a change
- WHEN spec_validate runs
- THEN structural errors are reported

## MODIFIED Requirements

### Requirement: Legacy Path
The TUI MUST call the daemon RPC instead of an external binary.

#### Scenario: TUI spec command
- WHEN /spec list is run
- THEN it calls spec.list over RPC

## REMOVED Requirements

### Requirement: External Binary
";

    #[test]
    fn parse_delta_buckets_requirements_by_operation() {
        let delta = parse_delta("engine", SAMPLE_DELTA);
        assert_eq!(delta.capability, "engine");
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.modified.len(), 1);
        assert_eq!(delta.removed.len(), 1);
        assert_eq!(delta.added[0].name, "Native Engine");
        assert_eq!(delta.added[0].scenarios.len(), 2);
        assert_eq!(delta.modified[0].name, "Legacy Path");
        assert_eq!(delta.removed[0].name, "External Binary");
        assert!(delta.added[0].is_normative());
    }

    #[test]
    fn delta_parse_render_round_trips() {
        // parse → render → parse must yield the identical typed model, proving
        // the render output is itself parseable (the keystone round-trip).
        let first = parse_delta("engine", SAMPLE_DELTA);
        let rendered = render_delta(&first);
        let second = parse_delta("engine", &rendered);
        assert_eq!(first, second, "delta must survive a render round-trip");
    }

    #[test]
    fn spec_parse_render_round_trips_and_keeps_preamble() {
        let md = "\
# Engine Specification

## Purpose
The single source of truth for engine requirements.

### Requirement: Parse Deltas
The engine SHALL parse delta specs.

#### Scenario: A well-formed delta
- WHEN a delta is parsed
- THEN its requirements are bucketed
";
        let spec = parse_spec("engine", md);
        assert!(spec.preamble.contains("## Purpose"));
        assert_eq!(spec.requirements.len(), 1);
        let rendered = render_spec(&spec);
        let reparsed = parse_spec("engine", &rendered);
        assert_eq!(spec, reparsed, "spec must survive a render round-trip");
    }

    #[test]
    fn pending_slices_reads_only_unchecked_items() {
        let tasks = "\
## 1. Group

- [ ] Slice one
- [x] already done
- [ ] Slice two
";
        assert_eq!(
            parse_pending_slices(tasks),
            vec!["Slice one".to_owned(), "Slice two".to_owned()]
        );
        assert_eq!(task_counts(tasks), (1, 3));
    }
}
