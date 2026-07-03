//! Structured audit findings: parsing from model output, de-duplication, and
//! deterministic markdown rendering.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Severity of an [`AuditFinding`], ordered most-to-least severe for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Severity {
    /// A critical defect (security hole, data loss, crash).
    Critical,
    /// A high-impact defect.
    High,
    /// A medium-impact defect.
    Medium,
    /// A low-impact defect or smell.
    Low,
    /// Informational note.
    Info,
}

impl Severity {
    /// Returns the lowercase wire string for this severity.
    #[must_use]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Info => "info",
        }
    }

    /// Returns the title-case section heading for this severity.
    #[must_use]
    fn heading(self) -> &'static str {
        match self {
            Self::Critical => "Critical",
            Self::High => "High",
            Self::Medium => "Medium",
            Self::Low => "Low",
            Self::Info => "Info",
        }
    }

    /// Parses a severity from a case-insensitive string.
    #[must_use]
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "critical" => Some(Self::Critical),
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            "info" | "informational" => Some(Self::Info),
            _ => None,
        }
    }

    /// The fixed rendering order, most severe first.
    const ORDER: [Self; 5] = [
        Self::Critical,
        Self::High,
        Self::Medium,
        Self::Low,
        Self::Info,
    ];
}

/// A single structured review finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AuditFinding {
    /// How severe the finding is.
    pub(crate) severity: Severity,
    /// Workspace-relative file the finding concerns.
    pub(crate) file: String,
    /// Optional 1-based line number.
    pub(crate) line: Option<u32>,
    /// Short rule slug (e.g. `error-handling`, `unwrap-in-lib`).
    pub(crate) rule: String,
    /// One-sentence rationale.
    pub(crate) rationale: String,
}

/// Parses a single finding from a JSON object, returning `None` when any
/// required field is missing or malformed (tolerant, non-fatal).
fn parse_finding_object(obj: &Value) -> Option<AuditFinding> {
    let severity = Severity::parse(obj.get("severity")?.as_str()?)?;
    let file = obj.get("file")?.as_str()?.trim();
    if file.is_empty() {
        return None;
    }
    let rule = obj.get("rule")?.as_str()?.trim();
    if rule.is_empty() {
        return None;
    }
    let rationale = obj.get("rationale")?.as_str()?.trim();
    if rationale.is_empty() {
        return None;
    }
    let line = obj
        .get("line")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    Some(AuditFinding {
        severity,
        file: file.to_owned(),
        line,
        rule: rule.to_owned(),
        rationale: rationale.to_owned(),
    })
}

/// Parses findings from model output, tolerantly skipping malformed objects.
///
/// Scans `text` for the first JSON array (optionally inside a ```` ```json ````
/// fence) and parses each element as an [`AuditFinding`]; elements that fail to
/// parse are skipped without failing the parse.
#[must_use]
pub(crate) fn parse_findings(text: &str) -> Vec<AuditFinding> {
    let Some(array) = first_json_array(text) else {
        return Vec::new();
    };
    array.iter().filter_map(parse_finding_object).collect()
}

/// Finds the first JSON array value embedded anywhere in `text`.
///
/// For each `[` byte a streaming deserializer attempts to read a JSON array,
/// ignoring trailing text. This tolerates fenced code blocks and surrounding
/// prose without a brace-counting scanner.
fn first_json_array(text: &str) -> Option<Vec<Value>> {
    use serde::de::Deserialize as _;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'[' {
            let mut de = serde_json::Deserializer::from_str(&text[i..]);
            if let Ok(Value::Array(arr)) = Value::deserialize(&mut de) {
                return Some(arr);
            }
        }
    }
    None
}

/// De-duplicates findings on `(file, line, rule)`, or `(file, rule)` when line
/// is absent. The first occurrence wins; its rationale is retained.
#[must_use]
pub(crate) fn dedup_findings(findings: Vec<AuditFinding>) -> Vec<AuditFinding> {
    let mut seen: HashSet<(String, Option<u32>, String)> = HashSet::new();
    let mut out = Vec::with_capacity(findings.len());
    for finding in findings {
        let key = (finding.file.clone(), finding.line, finding.rule.clone());
        if seen.insert(key) {
            out.push(finding);
        }
    }
    out
}

/// Renders findings into a deterministic markdown report.
///
/// The report leads with a per-severity count header, then sections ordered
/// Critical → High → Medium → Low → Info; each finding renders as a
/// `` `file:line` — **rule** — rationale `` line. Findings are not re-ordered
/// within a severity, so identical input renders byte-identically.
#[must_use]
pub(crate) fn render_report(findings: &[AuditFinding]) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    out.push_str("# Audit Report\n\n");
    out.push_str("## Summary\n\n");
    for severity in Severity::ORDER {
        let count = findings.iter().filter(|f| f.severity == severity).count();
        let _ = writeln!(out, "- {}: {count}", severity.heading());
    }
    out.push('\n');

    for severity in Severity::ORDER {
        let matching: Vec<&AuditFinding> =
            findings.iter().filter(|f| f.severity == severity).collect();
        if matching.is_empty() {
            continue;
        }
        let _ = writeln!(out, "## {}\n", severity.heading());
        for finding in matching {
            let location = match finding.line {
                Some(line) => format!("{}:{line}", finding.file),
                None => finding.file.clone(),
            };
            let _ = writeln!(
                out,
                "- `{location}` — **{}** — {}",
                finding.rule, finding.rationale
            );
        }
        out.push('\n');
    }
    out
}

/// Returns the per-severity counts as a JSON object keyed by severity slug.
#[must_use]
pub(crate) fn severity_counts(findings: &[AuditFinding]) -> Value {
    let mut counts = serde_json::Map::new();
    for severity in Severity::ORDER {
        let count = findings.iter().filter(|f| f.severity == severity).count();
        counts.insert(severity.as_str().to_owned(), json!(count));
    }
    Value::Object(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_findings() -> Vec<AuditFinding> {
        vec![
            AuditFinding {
                severity: Severity::Critical,
                file: "src/a.rs".to_owned(),
                line: Some(10),
                rule: "sql-injection".to_owned(),
                rationale: "interpolated SQL".to_owned(),
            },
            AuditFinding {
                severity: Severity::Low,
                file: "src/b.rs".to_owned(),
                line: None,
                rule: "naming".to_owned(),
                rationale: "abbreviated name".to_owned(),
            },
        ]
    }

    // ── finding parsing ───────────────────────────────────────────────────────

    #[test]
    fn parses_fenced_json_array_of_findings() {
        let text = "Here are my findings:\n```json\n[\
            {\"severity\":\"high\",\"file\":\"src/a.rs\",\"line\":12,\"rule\":\"unwrap-in-lib\",\"rationale\":\"uses unwrap\"}\
            ]\n```\nDone.";
        let findings = parse_findings(text);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::High);
        assert_eq!(findings[0].file, "src/a.rs");
        assert_eq!(findings[0].line, Some(12));
        assert_eq!(findings[0].rule, "unwrap-in-lib");
    }

    #[test]
    fn skips_malformed_finding_keeps_valid_siblings() {
        let text = "[\
            {\"severity\":\"bogus\",\"file\":\"x\",\"rule\":\"r\",\"rationale\":\"y\"},\
            {\"file\":\"only-file\"},\
            {\"severity\":\"low\",\"file\":\"src/b.rs\",\"rule\":\"naming\",\"rationale\":\"unclear name\"}\
            ]";
        let findings = parse_findings(text);
        assert_eq!(findings.len(), 1, "only the valid finding survives");
        assert_eq!(findings[0].file, "src/b.rs");
    }

    #[test]
    fn dedup_on_file_line_rule_first_wins() {
        let findings = vec![
            AuditFinding {
                severity: Severity::High,
                file: "a.rs".to_owned(),
                line: Some(3),
                rule: "r".to_owned(),
                rationale: "first".to_owned(),
            },
            AuditFinding {
                severity: Severity::Low,
                file: "a.rs".to_owned(),
                line: Some(3),
                rule: "r".to_owned(),
                rationale: "second".to_owned(),
            },
        ];
        let deduped = dedup_findings(findings);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].rationale, "first", "first occurrence wins");
    }

    #[test]
    fn dedup_on_file_rule_when_line_absent() {
        let findings = vec![
            AuditFinding {
                severity: Severity::High,
                file: "a.rs".to_owned(),
                line: None,
                rule: "r".to_owned(),
                rationale: "first".to_owned(),
            },
            AuditFinding {
                severity: Severity::High,
                file: "a.rs".to_owned(),
                line: None,
                rule: "r".to_owned(),
                rationale: "second".to_owned(),
            },
        ];
        assert_eq!(dedup_findings(findings).len(), 1);
    }

    // ── report rendering ──────────────────────────────────────────────────────

    #[test]
    fn report_has_count_header_and_severity_sections() {
        let report = render_report(&sample_findings());
        assert!(report.contains("## Summary"), "must have a summary header");
        assert!(report.contains("Critical: 1"), "must count critical");
        assert!(report.contains("Low: 1"), "must count low");
        assert!(
            report.contains("## Critical"),
            "must have a Critical section"
        );
        assert!(
            report.contains("`src/a.rs:10` — **sql-injection** — interpolated SQL"),
            "must render the finding line; got:\n{report}"
        );
        assert!(
            report.contains("`src/b.rs` — **naming** — abbreviated name"),
            "lineless finding must render without a colon; got:\n{report}"
        );
        // Critical section must precede Low.
        let crit = report.find("## Critical").unwrap();
        let low = report.find("## Low").unwrap();
        assert!(crit < low, "Critical must precede Low");
    }

    #[test]
    fn report_is_deterministic() {
        let findings = sample_findings();
        assert_eq!(render_report(&findings), render_report(&findings));
    }

    #[test]
    fn severity_counts_covers_all_levels() {
        let counts = severity_counts(&sample_findings());
        assert_eq!(counts["critical"], 1);
        assert_eq!(counts["high"], 0);
        assert_eq!(counts["low"], 1);
    }
}
