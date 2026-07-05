//! Auditor unit tests, moved verbatim from `auditor.rs`. `super::*` resolves
//! to the auditor module, whose (test-gated) re-exports expose the private
//! helpers these tests drive.

use super::*;

fn ingot() -> IngotHandle {
    IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap())
}

fn vault() -> Arc<Mutex<Vault>> {
    Arc::new(Mutex::new(Vault::open_in_memory().unwrap()))
}

fn embedder() -> Arc<dyn crate::embedder_port::Embedder> {
    Arc::new(crate::embedder_port::FnvEmbedder::new())
}

fn review_session() -> Session {
    Session {
        id: Uuid::new_v4(),
        created_at: Timestamp::from_micros(0),
        updated_at: Timestamp::from_micros(0),
        status: "active".to_owned(),
        task_id: None,
        mode: Some("review".to_owned()),
        title: String::new(),
        cowork_mode: false,
        workspace_root: None,
        model_override: None,
        runner_override: None,
    }
}

// ── scope resolution ──────────────────────────────────────────────────────

#[test]
fn resolve_scope_defaults_to_diff() {
    assert_eq!(resolve_scope(&json!({})), AuditScope::Diff);
    assert_eq!(resolve_scope(&json!({ "diff": true })), AuditScope::Diff);
}

#[test]
fn resolve_scope_path_arg_yields_path() {
    assert_eq!(
        resolve_scope(&json!({ "path": "src/lib.rs" })),
        AuditScope::Path {
            root: "src/lib.rs".to_owned()
        }
    );
}

#[test]
fn resolve_scope_branch_yields_branch_with_head_default() {
    assert_eq!(
        resolve_scope(&json!({ "branch": "main" })),
        AuditScope::Branch {
            base: "main".to_owned(),
            head: "HEAD".to_owned()
        }
    );
}

#[test]
fn resolve_scope_pr_yields_pr() {
    assert_eq!(
        resolve_scope(&json!({ "pr": "feature...main" })),
        AuditScope::Pr {
            reference: "feature...main".to_owned()
        }
    );
}

#[test]
fn resolve_scope_pr_takes_precedence_over_branch_and_path() {
    let params = json!({ "pr": "x", "branch": "main", "path": "src" });
    assert_eq!(
        resolve_scope(&params),
        AuditScope::Pr {
            reference: "x".to_owned()
        }
    );
}

// ── seed building ─────────────────────────────────────────────────────────

fn git_repo_with_change() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(path.join("a.txt"), "one\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "init"]);
    // An uncommitted change so `git diff HEAD` is non-empty.
    std::fs::write(path.join("a.txt"), "one\ntwo\n").unwrap();
    dir
}

#[tokio::test]
async fn diff_scope_seeds_from_unified_diff() {
    let repo = git_repo_with_change();
    let seed = build_seed(
        &AuditScope::Diff,
        repo.path(),
        &ingot(),
        &vault(),
        &embedder(),
    )
    .await
    .unwrap();
    assert!(!seed.trim().is_empty(), "seed must be non-empty");
    assert!(seed.contains("two"), "seed must contain the diff body");
}

#[tokio::test]
async fn path_scope_seeds_from_graph_and_listing() {
    let repo = git_repo_with_change();
    let seed = build_seed(
        &AuditScope::Path {
            root: ".".to_owned(),
        },
        repo.path(),
        &ingot(),
        &vault(),
        &embedder(),
    )
    .await
    .unwrap();
    assert!(!seed.trim().is_empty(), "path seed must be non-empty");
    assert!(seed.contains("File tree"), "path seed must list files");
    assert!(seed.contains("a.txt"), "path seed must include the file");
}

#[tokio::test]
async fn unresolvable_pr_ref_errors() {
    let repo = git_repo_with_change();
    let result = build_seed(
        &AuditScope::Pr {
            reference: "   ".to_owned(),
        },
        repo.path(),
        &ingot(),
        &vault(),
        &embedder(),
    )
    .await;
    assert!(result.is_err(), "empty PR ref must error");
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

// ── persistence ───────────────────────────────────────────────────────────

#[tokio::test]
async fn persists_findings_as_audit_events_with_markers() {
    let ig = ingot();
    let sid = Uuid::new_v4().to_string();
    let findings = sample_findings();
    persist_findings(&ig, &sid, &findings).await.unwrap();

    let events = ig.list_audit_events(&sid).await.unwrap();
    let starts = events
        .iter()
        .filter(|e| e.action_type == "turn_start")
        .count();
    let ends = events
        .iter()
        .filter(|e| e.action_type == "turn_end")
        .count();
    assert_eq!(starts, 1, "exactly one turn_start marker");
    assert_eq!(ends, 1, "exactly one turn_end marker");

    let finding_events: Vec<&AuditEvent> = events
        .iter()
        .filter(|e| e.action_type == "audit_finding")
        .collect();
    assert_eq!(finding_events.len(), 2);
    let crit = finding_events
        .iter()
        .find(|e| e.error_kind.as_deref() == Some("src/a.rs"))
        .unwrap();
    assert_eq!(crit.actor, "review");
    assert_eq!(crit.tier.as_deref(), Some("deep"));
    assert_eq!(crit.tool_name.as_deref(), Some("sql-injection"));
    assert_eq!(crit.operation_name.as_deref(), Some("critical"));
}

#[tokio::test]
async fn zero_findings_still_persists_markers() {
    let ig = ingot();
    let sid = Uuid::new_v4().to_string();
    persist_findings(&ig, &sid, &[]).await.unwrap();
    let events = ig.list_audit_events(&sid).await.unwrap();
    assert_eq!(events.len(), 2, "two markers, no findings");
    assert!(events.iter().any(|e| e.action_type == "turn_start"));
    assert!(events.iter().any(|e| e.action_type == "turn_end"));
}

// ── read-only loop ────────────────────────────────────────────────────────

/// A scripted runner that returns each response in order, recording the
/// observations it was fed back so a test can assert on rejection text.
struct ScriptedRunner {
    responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    seen: std::sync::Mutex<Vec<String>>,
}

impl ScriptedRunner {
    fn new(responses: Vec<&str>) -> Self {
        Self {
            responses: std::sync::Mutex::new(
                responses.into_iter().map(ToOwned::to_owned).collect(),
            ),
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl ReviewTurn for ScriptedRunner {
    async fn run_turn(&self, transcript: &[AdapterMessage]) -> Result<TurnOutput, RpcError> {
        if let Some(last) = transcript.last() {
            self.seen.lock().unwrap().push(last.content.clone());
        }
        let text = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        Ok(TurnOutput {
            text,
            input_tokens: 10,
            output_tokens: 10,
        })
    }
}

#[tokio::test]
async fn loop_rejects_non_allowlisted_tool_as_error_observation() {
    // First turn asks for a forbidden write_file tool; second turn emits
    // findings. The loop must feed back a rejection and never execute it.
    let runner = ScriptedRunner::new(vec![
        r#"{"tool":"write_file","input":{"path":"x","content":"y"}}"#,
        r#"[{"severity":"info","file":"a.rs","rule":"note","rationale":"ok"}]"#,
    ]);
    let session = review_session();
    let ws = tempfile::tempdir().unwrap();
    // Pre-create a file the forbidden write would have targeted.
    std::fs::write(ws.path().join("x"), "original").unwrap();

    let findings = run_audit_loop(
        &runner,
        "seed",
        ws.path(),
        &session,
        &ingot(),
        &vault(),
        &embedder(),
        &LoopBudget::default(),
    )
    .await
    .unwrap();

    assert_eq!(findings.len(), 1, "loop completes with the findings turn");
    // The file must be untouched: the write was never dispatched.
    let content = std::fs::read_to_string(ws.path().join("x")).unwrap();
    assert_eq!(content, "original", "forbidden write must not execute");
    // The rejection must have been fed back as an observation.
    let seen = runner.seen.lock().unwrap();
    assert!(
        seen.iter().any(|s| s.contains("not permitted")),
        "a rejection observation must be fed back; saw: {seen:?}"
    );
}

#[tokio::test]
async fn loop_dispatches_allowlisted_tool() {
    let runner = ScriptedRunner::new(vec![
        r#"{"tool":"list_files","input":{"path":"."}}"#,
        r#"[{"severity":"low","file":"f.rs","rule":"r","rationale":"x"}]"#,
    ]);
    let session = review_session();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("f.rs"), "fn main() {}").unwrap();

    let findings = run_audit_loop(
        &runner,
        "seed",
        ws.path(),
        &session,
        &ingot(),
        &vault(),
        &embedder(),
        &LoopBudget::default(),
    )
    .await
    .unwrap();
    assert_eq!(findings.len(), 1);
    let seen = runner.seen.lock().unwrap();
    assert!(
        seen.iter().any(|s| s.contains("f.rs")),
        "list_files observation must be fed back; saw: {seen:?}"
    );
}

#[tokio::test]
async fn loop_halts_at_max_iterations() {
    // The runner never emits findings: an endless tool-call loop. The
    // iteration bound must stop it.
    let responses: Vec<&str> =
        std::iter::repeat_n(r#"{"tool":"list_files","input":{"path":"."}}"#, 100).collect();
    let runner = ScriptedRunner::new(responses);
    let session = review_session();
    let ws = tempfile::tempdir().unwrap();

    let budget = LoopBudget {
        max_iterations: 3,
        token_budget: DEFAULT_TOKEN_BUDGET,
    };
    let findings = run_audit_loop(
        &runner,
        "seed",
        ws.path(),
        &session,
        &ingot(),
        &vault(),
        &embedder(),
        &budget,
    )
    .await
    .unwrap();
    assert!(findings.is_empty(), "no findings when none are emitted");
    // The runner was called at most max_iterations times.
    let calls = runner.seen.lock().unwrap().len();
    assert!(
        calls <= 3,
        "loop must halt at max_iterations; calls={calls}"
    );
}

// ── respond / report writing ──────────────────────────────────────────────

#[tokio::test]
async fn respond_inline_returns_report_and_counts() {
    let ws = tempfile::tempdir().unwrap();
    let resp = respond(&json!({}), &sample_findings(), ws.path())
        .await
        .unwrap();
    assert!(
        resp.get("report").is_some(),
        "inline report must be present"
    );
    assert!(resp["report"].as_str().unwrap().contains("## Summary"));
    assert_eq!(resp["counts"]["critical"], 1);
    assert!(resp["findings"].is_array());
}

#[tokio::test]
async fn respond_format_json_emits_typed_findings_without_loss() {
    let ws = tempfile::tempdir().unwrap();
    let resp = respond(&json!({ "format": "json" }), &sample_findings(), ws.path())
        .await
        .unwrap();
    assert!(
        resp.get("report").is_none(),
        "json format has no markdown body"
    );
    let arr = resp["findings"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["severity"], "critical");
    assert_eq!(arr[0]["file"], "src/a.rs");
    assert_eq!(arr[0]["line"], 10);
    assert_eq!(arr[0]["rule"], "sql-injection");
    assert_eq!(arr[0]["rationale"], "interpolated SQL");
}

#[tokio::test]
async fn respond_writes_report_to_path() {
    let ws = tempfile::tempdir().unwrap();
    // Canonicalise so the workspace-boundary check (which canonicalises the
    // root) accepts the not-yet-existing report path on platforms where the
    // temp dir is itself a symlink (e.g. /var → /private/var on macOS).
    let ws_root = ws.path().canonicalize().unwrap();
    let resp = respond(
        &json!({ "report": "report.md" }),
        &sample_findings(),
        &ws_root,
    )
    .await
    .unwrap();
    let path = resp["report_path"].as_str().unwrap();
    let written = std::fs::read_to_string(path).unwrap();
    assert!(
        written.contains("## Summary"),
        "report file must hold markdown"
    );
    assert!(
        resp.get("report").is_none(),
        "path mode does not inline the body"
    );
    assert_eq!(resp["counts"]["low"], 1);
}

#[tokio::test]
async fn full_pipeline_loop_persist_render_produces_markdown_report() {
    // End-to-end over the daemon side without a live provider: a scripted
    // review turn surfaces findings, which are persisted and rendered to a
    // markdown report with the per-severity header and severity sections.
    let runner = ScriptedRunner::new(vec![
        r#"{"tool":"list_files","input":{"path":"."}}"#,
        r#"[
                {"severity":"critical","file":"src/db.rs","line":7,"rule":"sql-injection","rationale":"interpolated query"},
                {"severity":"low","file":"src/util.rs","rule":"naming","rationale":"unclear name"}
            ]"#,
    ]);
    let session = review_session();
    let ws = tempfile::tempdir().unwrap();
    let ws_root = ws.path().canonicalize().unwrap();
    std::fs::write(ws_root.join("src.rs"), "fn main() {}").unwrap();
    let ig = ingot();

    let findings = run_audit_loop(
        &runner,
        "audit src/",
        &ws_root,
        &session,
        &ig,
        &vault(),
        &embedder(),
        &LoopBudget::default(),
    )
    .await
    .unwrap();
    assert_eq!(findings.len(), 2);

    persist_findings(&ig, &session.id.to_string(), &findings)
        .await
        .unwrap();
    let events = ig.list_audit_events(&session.id.to_string()).await.unwrap();
    assert!(events.iter().any(|e| e.action_type == "audit_finding"));

    let report = render_report(&findings);
    assert!(report.contains("## Summary"), "header present");
    assert!(report.contains("Critical: 1"));
    assert!(report.contains("## Critical"));
    assert!(report.contains("`src/db.rs:7` — **sql-injection**"));
}

#[test]
fn review_session_denies_write_bash() {
    // The session the loop runs under is in review mode, so the existing
    // gate denies write-arity bash.
    let session = review_session();
    assert!(
        !crate::executor::role_allows_write_bash_for_test(&session),
        "review-mode session must deny write-arity bash"
    );
}
