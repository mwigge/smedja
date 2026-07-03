use crate::blocks;
use crate::history::{dirs_tui_history_path, PROMPT_HISTORY_CAP};
use crate::messages::{push_action_log, push_author_chip, push_system_message};
use crate::session::start_stream_reader;
use crate::state::{AppState, Message, Role};
use crate::theme::{self, palette};
use anyhow::Result;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::json;
use smedja_rpc::client::Client;

pub(crate) async fn submit(input: &str, state: &mut AppState, client: &mut Client) -> Result<()> {
    let text = input.trim().to_owned();
    if text.is_empty() {
        return Ok(());
    }
    if state.turn_in_flight {
        push_system_message(state, "a turn is already in flight — press Esc to cancel");
        return Ok(());
    }
    state.prompt_history.push(text.clone());
    if state.prompt_history.len() > PROMPT_HISTORY_CAP {
        state.prompt_history.remove(0);
    }
    // Append-only write so history survives an unclean shutdown.
    // The full rewrite in save_history is still called on clean exit.
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let path = dirs_tui_history_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&path)
        {
            if let Ok(line) = serde_json::to_string(&text) {
                let _ = writeln!(f, "{line}");
            }
        }
    }
    state.history_idx = None;
    state.saved_input.clear();
    let user_msg = Message {
        role: Role::User,
        text: text.clone(),
    };
    // Author chip + message body. Reset the assistant chip latch so the next
    // response emits its own "▌ <runner>" boundary on a fresh line.
    let you_accent = palette().accent;
    push_author_chip(&mut state.main_panel, "you", you_accent, state.no_color);
    state.main_panel.push_line(user_msg.text.clone());
    state.assistant_open = false;
    state.messages.push(user_msg);
    state.turn_n += 1;
    state.turn_submitted_at = Some(std::time::Instant::now());
    state.current_block = Some(blocks::TurnBlock::new(state.turn_n));
    let resp = client
        .call(
            "turn.submit",
            json!({
                "session_id": state.session_id,
                "content": text,
            }),
        )
        .await;
    let reply = match resp {
        Ok(ref v) => {
            let task_id = v["task_id"].as_str().unwrap_or("?").to_owned();
            state.pending_task_id = Some(task_id.clone());
            state.turn_in_flight = true;
            state.last_poll = Some(std::time::Instant::now());
            state.current_thinking.clear();
            state.thinking_steps.clear();
            state.thinking_expanded = false;
            state.active_agent_name = None;
            state.plan_steps.clear();

            // Start streaming reader; events arrive via unbounded channel.
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            state.stream_rx = Some(rx);
            let sock = state.stream_sock_path.clone();
            let tid = task_id.clone();
            tokio::spawn(start_stream_reader(sock, tid, tx));

            // Dim provider hint — visible immediately while the turn is in flight,
            // before the first token arrives.
            {
                let p = palette();
                let label = theme::runner_label(&state.runner).to_lowercase();
                let model_part = state
                    .model
                    .as_deref()
                    .map_or_else(String::new, |m| format!(" · {m}"));
                state.main_panel.push_styled_line(Line::from(Span::styled(
                    format!("↪ {label}{model_part}"),
                    Style::default().fg(p.text_dim),
                )));
            }

            // "queued" is operational noise — route it to the actions log, not
            // the message box (keeps the conversation clean). Lead with the
            // session id (same 12-char form as the session rail) so the queued
            // task can be tied back to its session; the task id is a separate
            // per-turn handle and is shown short, after.
            let sid = &state.session_id[..state.session_id.len().min(12)];
            let short_task = &task_id[..task_id.len().min(8)];
            push_action_log(state, format!("queued · session {sid} · task {short_task}"));
            None
        }
        Err(ref e) => Some(format!("error: {e}")),
    };
    // Only genuine errors surface in the message panel now.
    if let Some(text) = reply {
        let sys_msg = Message {
            role: Role::System,
            text,
        };
        state.main_panel.push_line(sys_msg.text.clone());
        state.messages.push(sys_msg);
    }
    Ok(())
}
