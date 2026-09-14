use st_agent::{AgentChunk, SharedAgentManager, SharedPaneState};
use tracing::debug;

#[allow(clippy::too_many_lines)] // linear bridge-setup pipeline; splitting hurts readability
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
                                    approval_id: None,
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
                                        approval_id: None,
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
                                        approval_id: None,
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
                                        approval_id: None,
                                    });
                            }
                        }
                        st_agent::PaneEvent::ApprovalPrompt {
                            tool_name,
                            prompt,
                            approval_id,
                            turn_id,
                            ..
                        } => {
                            // File the prompt under the event's own turn when
                            // the wire carries one; the pane's current turn is
                            // only a fallback (a prompt for a different turn —
                            // or one arriving between turns — would otherwise be
                            // misfiled or dropped).
                            let session_key = turn_id
                                .as_deref()
                                .filter(|t| !t.is_empty())
                                .unwrap_or(&current_turn_id);
                            if !session_key.is_empty() {
                                let mut mgr = agent_manager
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                // The prompt text carries the scrubbed args
                                // summary (or the agent's reasoning); y/n while
                                // this is pending answers the gate via
                                // `SmdjadClient::send_approval`. Pre-v4 payloads
                                // carry no approval_id and cannot be answered,
                                // so the hint is only shown when one exists.
                                let hint = if approval_id.is_some() { " (y/n)" } else { "" };
                                let chunk = AgentChunk {
                                    block_id: session_key.to_owned(),
                                    text: format!(
                                        "[approval required: {tool_name} — {prompt}]{hint}"
                                    ),
                                    done: false,
                                    approval_required: true,
                                    approval_id,
                                };
                                // A prompt for ANOTHER pane's turn (smdjad
                                // broadcasts gate prompts to every pane) is
                                // filed passively: marking it active would
                                // hijack the y/n interception away from the
                                // prompt actually rendered in this window.
                                if session_key == current_turn_id {
                                    mgr.session_mut(session_key, &current_model)
                                        .push_chunk(&chunk);
                                } else {
                                    mgr.session_mut_passive(session_key, &current_model)
                                        .push_chunk(&chunk);
                                }
                            }
                        }
                        st_agent::PaneEvent::ApprovalResolved { approval_id } => {
                            // Session-agnostic resolution notice: the gate was
                            // answered by another client (or timed out), so the
                            // matching local prompt must stop intercepting y/n
                            // regardless of which session/turn filed it.
                            let mut mgr = agent_manager
                                .0
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if let Some(block_id) = mgr.clear_approval_by_id(&approval_id) {
                                mgr.session_mut_passive(&block_id, "")
                                    .push_chunk(&AgentChunk {
                                        block_id: block_id.clone(),
                                        text: "[approval resolved elsewhere]".to_owned(),
                                        done: false,
                                        approval_required: false,
                                        approval_id: None,
                                    });
                            }
                        }
                    }
                }
            });
        })
        .ok();
}
