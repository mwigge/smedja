//! Background bridge connecting the terminal to the `smdjad` agent daemon.
//!
//! Spawns a fire-and-forget thread that streams pane events into the shared
//! pane state and agent manager, which the status bar and agent-block overlay
//! read each render frame.

use tracing::debug;

use st_agent::{AgentChunk, SharedAgentManager, SharedPaneState};

/// Spawns a background thread that connects to smdjad and streams pane events
/// into `state`, which the status bar modules read each render frame.
///
/// The thread is fire-and-forget: if smdjad is absent or the connection drops,
/// it exits silently and the status bar simply shows no agent context.
#[allow(clippy::too_many_lines)]
pub(crate) fn spawn_agent_bridge(
    state: SharedPaneState,
    agent_manager: SharedAgentManager,
    pane_id: String,
) {
    std::thread::Builder::new()
        .name("st-agent".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                if !st_agent::socket_exists().await {
                    debug!("agent bridge: smdjad socket absent — skipping");
                    return;
                }
                let Ok(mut client) = st_agent::SmdjadClient::connect_agent().await else {
                    return;
                };
                if client.subscribe_pane(&pane_id).await.is_err() {
                    return;
                }
                // Current turn identifier, used as the AgentSession block_id.
                let mut current_turn_id = String::new();
                let mut current_model = String::new();
                while let Ok(Some(ev)) = client.next_event().await {
                    let mut s = state.0.write().await;
                    match ev {
                        st_agent::PaneEvent::TurnStart {
                            tier,
                            model,
                            turn_id,
                            ..
                        } => {
                            if !tier.is_empty() {
                                s.tier = Some(tier);
                            }
                            if !model.is_empty() {
                                s.model = Some(model.clone());
                                current_model = model;
                            }
                            s.is_agent_turn = true;
                            current_turn_id = turn_id;
                        }
                        ref turn_end @ st_agent::PaneEvent::TurnEnd { .. } => {
                            // Accumulate token/latency counters and the cumulative
                            // token-economy figures into pane state (logic lives in
                            // st-agent so it stays unit-testable without a GPU).
                            s.apply_turn_end(turn_end);
                            // Mark the session done.
                            if !current_turn_id.is_empty() {
                                let mut mgr = agent_manager
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                let session = mgr.session_mut(&current_turn_id, &current_model);
                                session.push_chunk(&AgentChunk {
                                    block_id: current_turn_id.clone(),
                                    text: String::new(),
                                    done: true,
                                    approval_required: false,
                                });
                            }
                        }
                        st_agent::PaneEvent::ToolCall { tool_name, .. } => {
                            s.active_task = Some(tool_name.clone());
                            // Record tool call as a content line.
                            if !current_turn_id.is_empty() {
                                let mut mgr = agent_manager
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                mgr.session_mut(&current_turn_id, &current_model)
                                    .push_chunk(&AgentChunk {
                                        block_id: current_turn_id.clone(),
                                        text: format!("[tool: {tool_name}]"),
                                        done: false,
                                        approval_required: false,
                                    });
                            }
                        }
                        st_agent::PaneEvent::StreamDelta { text } => {
                            if !current_turn_id.is_empty() {
                                let mut mgr = agent_manager
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                mgr.session_mut(&current_turn_id, &current_model)
                                    .push_chunk(&AgentChunk {
                                        block_id: current_turn_id.clone(),
                                        text,
                                        done: false,
                                        approval_required: false,
                                    });
                            }
                        }
                        st_agent::PaneEvent::ToolResult { tool_name, outcome } => {
                            if !current_turn_id.is_empty() {
                                let mut mgr = agent_manager
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                mgr.session_mut(&current_turn_id, &current_model)
                                    .push_chunk(&AgentChunk {
                                        block_id: current_turn_id.clone(),
                                        text: format!("[{tool_name}: {outcome}]"),
                                        done: false,
                                        approval_required: false,
                                    });
                            }
                        }
                        st_agent::PaneEvent::ApprovalPrompt {
                            tool_name, prompt, ..
                        } => {
                            if !current_turn_id.is_empty() {
                                let mut mgr = agent_manager
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                mgr.session_mut(&current_turn_id, &current_model)
                                    .push_chunk(&AgentChunk {
                                        block_id: current_turn_id.clone(),
                                        text: format!(
                                            "[approval required: {tool_name} — {prompt}]"
                                        ),
                                        done: false,
                                        approval_required: true,
                                    });
                            }
                        }
                    }
                }
            });
        })
        .ok();
}
