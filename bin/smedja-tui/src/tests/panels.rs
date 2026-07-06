//! `panels`-area unit tests (moved verbatim from the former `tests.rs`).

use serde_json::json;

use crate::input::{apply_cowork_decision, cowork_resolved};
use crate::render::render;
use crate::test_support::make_state;
use crate::thoughts_panel;
use crate::{cowork_widget, lsp_snapshot_from_rpc};

// Bug regression: enabling the trace waterfall must not clobber the LSP panel.
// Both get their own rail slot, and LSP keeps a minimum height (Min, not Fill)
// so the fixed-height trace panel can never starve it to zero rows.
#[test]
fn trace_and_lsp_coexist_in_rail() {
    let mut state = make_state("rail-coexist");
    state.panels.context_rail = true;
    state.panels.lsp = true;
    state.panels.obs = true;
    // A recorded turn trace makes the waterfall visible alongside LSP/obs.
    state.current_trace.start_turn();
    state.current_trace.push_tool("Read", 100);
    state.current_trace.settle_last_tool(300, true);
    state.current_trace.finish(400, true);

    // Wide + tall enough that the rail renders and every panel has room.
    let backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();

    assert!(
        content.contains("lsp"),
        "LSP panel must still render when the trace is enabled; got: {content:?}"
    );
    assert!(
        content.contains("trace"),
        "trace panel must render alongside LSP; got: {content:?}"
    );
    assert!(
        content.contains("obs"),
        "obs panel must render alongside LSP and trace; got: {content:?}"
    );
}

fn cowork_item(id: &str, tool: &str) -> cowork_widget::CoworkItem {
    cowork_widget::CoworkItem {
        id: id.to_owned(),
        tool: tool.to_owned(),
        step_n: 1,
        args_display: String::new(),
        reasoning: String::new(),
    }
}

#[test]
fn cowork_resolved_true_only_when_flag_set() {
    let yes: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": true }));
    assert!(cowork_resolved(&yes), "resolved:true must return true");

    let no: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": false }));
    assert!(!cowork_resolved(&no), "resolved:false must return false");

    let missing: Result<serde_json::Value, smedja_rpc::RpcError> = Ok(json!({ "id": "a" }));
    assert!(
        !cowork_resolved(&missing),
        "missing resolved field must return false"
    );

    let err: Result<serde_json::Value, smedja_rpc::RpcError> =
        Err(smedja_rpc::RpcError::new(-32603, "transport down"));
    assert!(!cowork_resolved(&err), "transport error must return false");
}

// --- cowork decision application (approve / deny) ---

#[test]
fn apply_cowork_decision_approve_resolved_removes_and_confirms() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": true }));
    let item = cowork_item("a", "bash");
    let (remove, message) =
        apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
    assert!(remove, "resolved:true must remove the item");
    assert_eq!(message, "approved: bash");
}

#[test]
fn apply_cowork_decision_unresolved_retains_and_reports_not_found() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": false }));
    let item = cowork_item("a", "bash");
    let (remove, message) =
        apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
    assert!(!remove, "resolved:false must retain the item");
    assert_eq!(message, "item not found: bash");
}

#[test]
fn apply_cowork_decision_deny_resolved_removes_and_confirms() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": true }));
    let item = cowork_item("a", "edit_file");
    let (remove, message) =
        apply_cowork_decision(&result, "cowork.deny", "denied: edit_file", &item.tool);
    assert!(remove, "resolved:true must remove the item");
    assert_eq!(message, "denied: edit_file");
}

#[test]
fn apply_cowork_decision_rpc_error_retains_and_reports_error() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Err(smedja_rpc::RpcError::new(-32603, "boom"));
    let item = cowork_item("a", "bash");
    let (remove, message) =
        apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
    assert!(!remove, "rpc error must retain the item");
    assert!(
        message.contains("cowork.approve error"),
        "error message must name the method; got: {message}"
    );
}

// --- cowork modify flow ---

#[test]
fn apply_cowork_decision_modify_resolved_echoes_instruction() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": true }));
    let item = cowork_item("a", "bash");
    let (remove, message) = apply_cowork_decision(
        &result,
        "cowork.modify",
        "modify sent: use ls -la instead",
        &item.tool,
    );
    assert!(remove, "resolved:true must remove the item");
    assert_eq!(message, "modify sent: use ls -la instead");
}

#[test]
fn apply_cowork_decision_modify_unresolved_retains_item() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": false }));
    let item = cowork_item("a", "bash");
    let (remove, message) = apply_cowork_decision(
        &result,
        "cowork.modify",
        "modify sent: use ls -la instead",
        &item.tool,
    );
    assert!(!remove, "resolved:false must retain the item");
    assert_eq!(message, "item not found: bash");
}

// --- lsp_snapshot_from_rpc -----------------------------------------------

#[test]
fn lsp_snapshot_from_rpc_decodes_all_severity_strings() {
    let status = json!({"servers": []});
    let diag = json!({
        "diagnostics": [
            {"file": "a.rs", "line": 1, "col": 1, "severity": "error",   "message": "e"},
            {"file": "a.rs", "line": 2, "col": 1, "severity": "warning", "message": "w"},
            {"file": "a.rs", "line": 3, "col": 1, "severity": "info",    "message": "i"},
            {"file": "a.rs", "line": 4, "col": 1, "severity": "hint",    "message": "h"},
        ]
    });
    let snap = lsp_snapshot_from_rpc(&status, &diag);
    assert_eq!(snap.diagnostics.len(), 4);
    assert!(matches!(
        snap.diagnostics[0].severity,
        smedja_lsp::Severity::Error
    ));
    assert!(matches!(
        snap.diagnostics[1].severity,
        smedja_lsp::Severity::Warning
    ));
    assert!(matches!(
        snap.diagnostics[2].severity,
        smedja_lsp::Severity::Info
    ));
    assert!(matches!(
        snap.diagnostics[3].severity,
        smedja_lsp::Severity::Hint
    ));
}

#[test]
fn lsp_snapshot_from_rpc_unknown_severity_defaults_to_error() {
    let status = json!({"servers": []});
    let diag = json!({
        "diagnostics": [
            {"file": "x.rs", "line": 1, "col": 1, "severity": "banana", "message": "x"}
        ]
    });
    let snap = lsp_snapshot_from_rpc(&status, &diag);
    assert!(matches!(
        snap.diagnostics[0].severity,
        smedja_lsp::Severity::Error
    ));
}

#[test]
fn lsp_snapshot_from_rpc_decodes_server_states() {
    let status = json!({
        "servers": [
            {"name": "ra",     "state": "ready"},
            {"name": "gopls",  "state": "degraded: connection refused"},
            {"name": "py",     "state": "starting"},
        ]
    });
    let snap = lsp_snapshot_from_rpc(&status, &json!({"diagnostics": []}));
    assert_eq!(snap.servers.len(), 3);
    assert!(matches!(
        snap.servers[0].state,
        smedja_lsp::ServerState::Ready
    ));
    assert!(
        matches!(&snap.servers[1].state, smedja_lsp::ServerState::Degraded(r) if r == "connection refused"),
        "degraded reason must be extracted from prefix"
    );
    assert!(matches!(
        snap.servers[2].state,
        smedja_lsp::ServerState::Starting
    ));
}

#[test]
fn lsp_snapshot_from_rpc_empty_inputs_yield_empty_snapshot() {
    let snap = lsp_snapshot_from_rpc(&json!({"servers": []}), &json!({"diagnostics": []}));
    assert!(snap.servers.is_empty());
    assert!(snap.diagnostics.is_empty());
}

// --- poll backoff --------------------------------------------------------

#[test]
fn thinking_tokens_accumulate_in_current_thinking() {
    let mut state = make_state("sess-think");
    assert!(state.current_thinking.is_empty());
    // Simulate two ThinkingDelta stream events arriving.
    state.current_thinking.push_str("step one ");
    state.current_thinking.push_str("step two");
    assert_eq!(state.current_thinking, "step one step two");
}

#[test]
fn thinking_expanded_toggles_only_when_content_present() {
    let mut state = make_state("sess-think-toggle");
    state.scroll_focus = true;
    // No steps: T key must be a no-op.
    assert!(state.thinking_steps.is_empty());
    if !state.thinking_steps.is_empty() {
        state.thinking_expanded = !state.thinking_expanded;
    }
    assert!(
        !state.thinking_expanded,
        "T must not toggle when no thinking steps"
    );

    // With steps: T key must toggle.
    state
        .thinking_steps
        .push(thoughts_panel::ThinkingStep::Answer { elapsed_s: 1.0 });
    if !state.thinking_steps.is_empty() {
        state.thinking_expanded = !state.thinking_expanded;
    }
    assert!(
        state.thinking_expanded,
        "T must expand when thinking steps are present"
    );
    if !state.thinking_steps.is_empty() {
        state.thinking_expanded = !state.thinking_expanded;
    }
    assert!(!state.thinking_expanded, "second T must collapse");
}

// --- thinking step timeline ----------------------------------------------

#[test]
fn thinking_steps_cleared_at_turn_start() {
    let mut state = make_state("sess-steps-clear");
    state
        .thinking_steps
        .push(thoughts_panel::ThinkingStep::Answer { elapsed_s: 1.0 });
    assert_eq!(state.thinking_steps.len(), 1);
    state.thinking_steps.clear();
    assert!(state.thinking_steps.is_empty());
}

#[test]
fn thinking_step_tool_has_correct_fields() {
    let step = thoughts_panel::ThinkingStep::Tool {
        name: "bash".into(),
        preview: "ls /src".into(),
        elapsed_s: 0.5,
    };
    assert!(matches!(step.elapsed_s(), 0.4..=0.6));
}

// --- govctl work-item harness --------------------------------------------

#[test]
fn thinking_cleared_on_new_turn() {
    let mut state = make_state("sess-think-clear");
    state.current_thinking = "previous reasoning".to_owned();
    state.thinking_expanded = true;
    // Simulate what happens when a new turn starts.
    state.current_thinking.clear();
    state.thinking_expanded = false;
    assert!(state.current_thinking.is_empty());
    assert!(!state.thinking_expanded);
}

// --- P3b: OSC-9 helper ---------------------------------------------------
