//! Streaming turn plumbing: the background NDJSON reader and the
//! `StreamEvent` dispatch applied on each event-loop tick.
//!
//! Split out of `main.rs` verbatim; behaviour is unchanged.

use super::*;

/// Connects to the smdjad stream socket and forwards NDJSON events to `tx`
/// until the terminal `done` or `error` event is received.
pub(crate) async fn start_stream_reader(
    sock_path: PathBuf,
    task_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = match UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "stream socket connect failed");
            let _ = tx.send(StreamEvent::Error {
                message: format!("stream unavailable: {e}"),
            });
            return;
        }
    };
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    let req = format!("{{\"task_id\":\"{task_id}\"}}\n");
    if writer.write_all(req.as_bytes()).await.is_err() {
        let _ = tx.send(StreamEvent::Error {
            message: "stream handshake failed".to_owned(),
        });
        return;
    }

    // After a successful `done` the daemon emits one trailing Tier-1 quality
    // snapshot a beat later (the post-turn gate runs once the turn completes).
    // Keep reading within a bounded grace window so that snapshot arrives instead
    // of being cut off by the terminal `done`; a failed turn (`error`) has no
    // trailing snapshot, so stop at once. The window matches the daemon's
    // `QUALITY_GRACE_SECS`.
    const QUALITY_GRACE: std::time::Duration = std::time::Duration::from_secs(8);
    let mut line = String::new();
    let mut grace_until: Option<tokio::time::Instant> = None;
    loop {
        line.clear();
        let read = reader.read_line(&mut line);
        let outcome = if let Some(dl) = grace_until {
            let remaining = dl.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, read).await {
                Ok(r) => r,
                Err(_) => break, // grace elapsed with no trailing snapshot
            }
        } else {
            read.await
        };
        match outcome {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<StreamEvent>(trimmed) {
                    let is_quality = matches!(ev, StreamEvent::Quality { .. });
                    let is_error = matches!(ev, StreamEvent::Error { .. });
                    let is_done = matches!(ev, StreamEvent::Done { .. });
                    let _ = tx.send(ev);
                    if is_quality || is_error {
                        break;
                    }
                    if is_done {
                        // Hold open briefly for the trailing quality snapshot.
                        grace_until = Some(tokio::time::Instant::now() + QUALITY_GRACE);
                    }
                }
            }
        }
    }
}

/// Applies a single streamed [`StreamEvent`] to `state`, mirroring the inline
/// dispatch that previously lived in the event loop. Returns `true` when the
/// event terminates the turn (`Done`/`Error`), so the caller finalises it.
pub(crate) fn apply_stream_event(
    state: &mut AppState,
    event: StreamEvent,
    pending_output_save: &mut Option<(OutputType, String)>,
) -> bool {
    let mut turn_done = false;
    match event {
        StreamEvent::Delta { text } => {
            // Live-line liveness: bump the moving token counter and
            // mark fresh output so the stall detector stays quiet.
            state.last_stream_activity = Some(std::time::Instant::now());
            #[allow(clippy::cast_possible_truncation)]
            let est = (text.chars().count() / 4) as u32;
            state.live_tokens = state.live_tokens.saturating_add(est);
            // First delta of the turn: emit the assistant author
            // chip on its own line so the response never merges
            // into the preceding line (which broke ```fences →
            // syntax highlighting), and start a fresh body line.
            if !state.assistant_open {
                // Tint the chip with the active agent's stable colour so
                // interleaved multi-agent output is attributable; fall
                // back to the runner brand colour for a solo turn.
                let color = state
                    .active_agent_name
                    .as_deref()
                    .and_then(|a| state.fleet.color_for(a))
                    .unwrap_or_else(|| theme::runner_color(&state.runner));
                let label = theme::runner_label(&state.runner).to_lowercase();
                // Thin `▏` bar: the assistant owns the content width, so its
                // boundary marker stays quiet next to the user's heavy `▌`.
                push_author_chip(
                    &mut state.main_panel,
                    "\u{258f}",
                    &label,
                    color,
                    state.no_color,
                );
                state.main_panel.push_line(String::new());
                state.assistant_open = true;
            }
            // A tool-result marker (↳) resolves the running tool
            // card to ✓ (ok) or ✗ (error).
            if text.contains('\u{21b3}') {
                if let Some((idx, name, inp)) = state.pending_tool.take() {
                    let ok = !text.contains("error");
                    let status = if ok {
                        tool_call::CardStatus::Ok
                    } else {
                        tool_call::CardStatus::Failed
                    };
                    let elapsed_s = state.tool_started_at.map(|t| t.elapsed().as_secs_f32());
                    let card =
                        tool_call::tool_card_line(&name, &inp, state.no_color, status, elapsed_s);
                    state.main_panel.replace_styled_line(idx, card);
                    // Close the tool's trace span at the current turn offset.
                    let end_ms = state.turn_submitted_at.map_or(0, |t| {
                        u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
                    });
                    state.current_trace.settle_last_tool(end_ms, ok);
                    state.tool_started_at = None;
                }
            }
            // The tool card resolved above (or, for a passthrough external CLI,
            // the dim "⏵ execute · … · ✓" banner) already shows a successful
            // tool call, so drop the redundant "↳ ok · …" result echo — one dim
            // line per tool call, matching native cards. A failure
            // ("↳ error · …") is kept so its detail survives.
            let body = strip_ok_result_echo(&text);
            // Split on newlines so each line is a separate panel entry.
            let mut remaining = body.as_ref();
            loop {
                if let Some(pos) = remaining.find('\n') {
                    let chunk = &remaining[..pos];
                    if !chunk.is_empty() {
                        state.main_panel.push_delta(chunk);
                    }
                    // The line is now complete — classify it
                    // (syntax-highlight code, colour diffs) then
                    // open a fresh tail line for the next chunk.
                    state.main_panel.finalize_last_line();
                    state.main_panel.push_line(String::new());
                    remaining = &remaining[pos + 1..];
                    if let Some(ref mut block) = state.current_block {
                        block.push_text(chunk);
                        block.push_text("\n");
                    }
                } else {
                    if !remaining.is_empty() {
                        state.main_panel.push_delta(remaining);
                        if let Some(ref mut block) = state.current_block {
                            block.push_text(remaining);
                        }
                    }
                    break;
                }
            }
            // Scan accumulated turn text for new numbered plan steps.
            if let Some(ref block) = state.current_block {
                let new = plan_panel::extract_new_steps(&block.content, state.plan_steps.len());
                state.plan_steps.extend(new);
            }
        }
        StreamEvent::Started { agent_name } => {
            if let Some(name) = agent_name {
                // Register the agent in the fleet roster (id keyed by
                // name today; the roster supports many agents).
                state.fleet.upsert(&name, &name);
                state.active_agent_name = Some(name);
            }
        }
        StreamEvent::Thinking { text } => {
            state.last_stream_activity = Some(std::time::Instant::now());
            #[allow(clippy::cast_possible_truncation)]
            let est = (text.chars().count() / 4) as u32;
            state.live_tokens = state.live_tokens.saturating_add(est);
            state.current_thinking.push_str(&text);
            let elapsed_s = state
                .turn_submitted_at
                .map_or(0.0, |t| t.elapsed().as_secs_f32());
            // Merge consecutive reasoning into the last step.
            if let Some(thoughts_panel::ThinkingStep::Reasoning {
                text: ref mut t, ..
            }) = state.thinking_steps.last_mut()
            {
                t.push_str(&text);
            } else {
                state
                    .thinking_steps
                    .push(thoughts_panel::ThinkingStep::Reasoning {
                        text: text.clone(),
                        elapsed_s,
                    });
            }
        }
        StreamEvent::ToolCall { name, input, full } => {
            let full_str = full.as_deref().unwrap_or(&input);
            state.last_stream_activity = Some(std::time::Instant::now());
            state.tool_started_at = Some(std::time::Instant::now());
            let elapsed_s = state
                .turn_submitted_at
                .map_or(0.0, |t| t.elapsed().as_secs_f32());
            // Open a trace span for this tool at the current turn offset.
            let start_ms = state.turn_submitted_at.map_or(0, |t| {
                u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
            });
            state.current_trace.push_tool(name.clone(), start_ms);
            // Reflect the activity on the active agent's roster row and
            // advance the plan step tracker as tool progress moves on.
            if !state.plan_steps.is_empty() {
                state.plan_current =
                    (state.plan_current + 1).min(state.plan_steps.len().saturating_sub(1));
            }
            if let Some(agent) = state.active_agent_name.clone() {
                state.fleet.set_activity(&agent, &name, &input);
                #[allow(clippy::cast_possible_truncation)]
                let total = state.plan_steps.len() as u16;
                #[allow(clippy::cast_possible_truncation)]
                let cur = (state.plan_current as u16).saturating_add(1).min(total);
                state.fleet.set_step(&agent, cur, total);
            }
            state
                .thinking_steps
                .push(thoughts_panel::ThinkingStep::Tool {
                    name: name.clone(),
                    preview: input.chars().take(60).collect(),
                    elapsed_s,
                });
            // Card starts "running" (◷); resolved to ✓/✗ when its
            // result arrives.
            let card = tool_call_card(&name, &input, state.no_color, '\u{25f7}');
            state.main_panel.push_styled_line(card);
            // Record the card's line + full args for right-click
            // expansion and the /tools inspector.
            let line_idx = state.main_panel.len().saturating_sub(1);
            // Bounded push via the free helper (not `push_tool_detail`)
            // so only `tool_details` is borrowed — `state.stream_rx` is
            // mutably borrowed as `rx` for the duration of this match.
            push_capped(
                &mut state.tool_details,
                (line_idx, name.clone(), full_str.to_owned()),
                TOOL_DETAILS_CAP,
            );
            state.pending_tool = Some((line_idx, name.clone(), input.clone()));
            if let Some(ref mut block) = state.current_block {
                block.push_text(&format!("▶ {name}: {input}"));
                block.push_text("\n");
            }
        }
        StreamEvent::Done {
            output_tok,
            input_tok,
            traceparent,
        } => {
            // Classify the final streamed line (responses needn't end
            // with a newline) and close the assistant block.
            state.main_panel.finalize_last_line();
            state.assistant_open = false;
            let elapsed_s = state
                .turn_submitted_at
                .map_or(0.0, |start| start.elapsed().as_secs_f32());
            state
                .thinking_steps
                .push(thoughts_panel::ThinkingStep::Answer { elapsed_s });
            // Any tool still marked "running" at turn end is settled.
            if let Some((idx, name, inp)) = state.pending_tool.take() {
                let elapsed = state.tool_started_at.map(|t| t.elapsed().as_secs_f32());
                let card = tool_call::tool_card_line(
                    &name,
                    &inp,
                    state.no_color,
                    tool_call::CardStatus::Ok,
                    elapsed,
                );
                state.main_panel.replace_styled_line(idx, card);
                state.tool_started_at = None;
            }
            let output_tok = u64::from(output_tok);
            let input_tok = u64::from(input_tok.unwrap_or(0));
            let turn_ms = state.turn_submitted_at.map_or(0, |inst| {
                u64::try_from(inst.elapsed().as_millis()).unwrap_or(u64::MAX)
            });
            state.turn_submitted_at = None;
            state.last_traceparent.clone_from(&traceparent);
            // Close the trace waterfall, mark the plan complete, and
            // flip the active agent's roster row to done.
            state.current_trace.finish(turn_ms, true);
            state.plan_current = state.plan_steps.len();
            if let Some(ref agent) = state.active_agent_name {
                state
                    .fleet
                    .set_status(agent, fleet_panel::AgentStatus::Done);
            }

            // Track latency samples for p95/p99 in the obs panel.
            if turn_ms > 0 {
                if state.latency_samples.len() >= LATENCY_SAMPLE_CAP {
                    state.latency_samples.pop_front();
                }
                state.latency_samples.push_back(turn_ms);
                state.obs_snapshot.latency_samples = state.latency_samples.clone();
            }
            // Accumulate session token totals.
            state.session_tokens_in = state.session_tokens_in.saturating_add(input_tok);
            state.session_tokens_out = state.session_tokens_out.saturating_add(output_tok);
            state.obs_snapshot.tokens_input = state.session_tokens_in;
            state.obs_snapshot.tokens_output = state.session_tokens_out;

            let block_content = if let Some(mut block) = state.current_block.take() {
                block.complete(turn_ms);
                let content = block.content.clone();
                state.block_store.push(block);
                content
            } else {
                String::new()
            };

            let footer = if let Some(ref tp_str) = traceparent {
                if state.otlp_configured {
                    format!("↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str}")
                } else {
                    format!(
                        "↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str} · traces not exported (set SMEDJA_OTLP_ENDPOINT)"
                    )
                }
            } else {
                format!("↳ {input_tok}↑ {output_tok}↓ tokens · {turn_ms}ms")
            };
            state.main_panel.push_line(footer);

            // Emit a collapsible trace badge when thinking steps or tool
            // calls were recorded — meaningful for all providers.
            let n_steps = state.thinking_steps.len();
            if n_steps > 1 {
                let label = if state
                    .thinking_steps
                    .iter()
                    .any(|s| matches!(s, thoughts_panel::ThinkingStep::Reasoning { .. }))
                {
                    "thinking"
                } else {
                    "trace"
                };
                // Chrome, not content — keep the trace badge dim.
                crate::push_chrome_line(
                    &mut state.main_panel,
                    format!("\u{254c} {label} ({n_steps} steps) [T to expand] \u{254c}"),
                );
            }

            if let Some(output_type) = state.pending_output_type.take() {
                *pending_output_save = Some((output_type, block_content));
            }

            let _ = emit_osc9(&mut std::io::stdout());

            turn_done = true;
        }
        StreamEvent::Error { message } => {
            let (label, hint) = classify_turn_error(&message);
            let header = format_turn_error(&state.runner, label, &message);
            let display = if hint.is_empty() {
                header
            } else {
                format!("{header}\n  \u{2192} {hint}")
            };
            // push_system_message cannot be called here because `state.stream_rx`
            // is mutably borrowed via `ref mut rx` for the entire enclosing block.
            // Emit to the main panel directly instead.
            for line in display.lines() {
                state.main_panel.push_line(line.to_owned());
            }
            if let Some(mut block) = state.current_block.take() {
                block.fail();
                state.block_store.push(block);
            }
            // Close the trace and roster as failed.
            let turn_ms = state.turn_submitted_at.map_or(0, |inst| {
                u64::try_from(inst.elapsed().as_millis()).unwrap_or(u64::MAX)
            });
            state.current_trace.finish(turn_ms, false);
            if let Some(ref agent) = state.active_agent_name {
                state
                    .fleet
                    .set_status(agent, fleet_panel::AgentStatus::Failed);
            }
            turn_done = true;
        }
        StreamEvent::Quality {
            score,
            tdd_pass,
            clean_pass,
            file_advisories,
            skill_advisories,
            llm_reviewed,
            suggested_command,
        } => {
            // Carry the trend history across snapshots (each Quality event
            // replaces the whole snapshot) and append this turn's score, capped so
            // the sparkline shows a rolling recent window.
            const TREND_CAP: usize = 32;
            let mut trend = std::mem::take(&mut state.quality_snapshot.trend);
            trend.push(score);
            if trend.len() > TREND_CAP {
                trend.drain(0..trend.len() - TREND_CAP);
            }
            // Update quality panel snapshot from the post-turn gate evaluation.
            state.quality_snapshot = quality_panel::QualitySnapshot {
                score,
                scored: true,
                tdd_pass,
                clean_pass,
                llm_reviewed,
                file_advisories,
                skill_advisories,
                suggested_command,
                trend,
            };
            // Feed the real running average consumed by the value panel.
            state.quality_score_sum = state.quality_score_sum.saturating_add(u64::from(score));
            state.quality_score_count = state.quality_score_count.saturating_add(1);
            if llm_reviewed {
                state.quality_review_in_progress = false;
            }
            // CoworkGate: two consecutive turns below 60.
            if score < 60 {
                state.consecutive_low_quality = state.consecutive_low_quality.saturating_add(1);
                if state.consecutive_low_quality >= 2 {
                    state.pending_cowork.push(cowork_widget::CoworkItem {
                        id: format!("quality-gate-{score}"),
                        tool: "quality-gate".to_owned(),
                        step_n: 0,
                        args_display: format!(
                            "Score {score}/100 for 2 consecutive turns — address findings?"
                        ),
                        reasoning: "Quality score below 60 for 2 turns.".to_owned(),
                    });
                }
            } else {
                state.consecutive_low_quality = 0;
            }
        }
        StreamEvent::BufferOverflow { lost } => {
            let s = if lost == 1 { "" } else { "s" };
            state.main_panel.push_line(format!(
                "[stream] {lost} event{s} dropped — output may be incomplete"
            ));
        }
        StreamEvent::CoworkRequest {
            approval_id,
            tool,
            step_n,
            args_display,
            reasoning,
        } => {
            let already_known = state.pending_cowork.iter().any(|i| i.id == approval_id);
            if !already_known {
                state.pending_cowork.push(cowork_widget::CoworkItem {
                    id: approval_id,
                    tool,
                    step_n,
                    args_display,
                    reasoning,
                });
            }
        }
        StreamEvent::Usage {
            input_tok,
            output_tok,
        } => {
            // Authoritative running token count for the live line.
            state.last_stream_activity = Some(std::time::Instant::now());
            state.live_tokens = state.live_tokens.max(output_tok);
            // Feed the obs panel's throughput bar live, before `Done` commits
            // the turn into the session totals. Providers split usage across
            // several events (input on message_start, output on message_delta),
            // so track a per-field high-water mark and add it on top of the
            // already-committed session totals. `Done` later sets the obs totals
            // absolutely from the session counters, so there is no double count.
            state.turn_tokens_in = state.turn_tokens_in.max(u64::from(input_tok));
            state.turn_tokens_out = state.turn_tokens_out.max(u64::from(output_tok));
            state.obs_snapshot.tokens_input =
                state.session_tokens_in.saturating_add(state.turn_tokens_in);
            state.obs_snapshot.tokens_output = state
                .session_tokens_out
                .saturating_add(state.turn_tokens_out);
        }
        StreamEvent::Unknown
        | StreamEvent::ToolCallChunk { .. }
        | StreamEvent::ToolCallUpdate { .. } => {}
    }
    turn_done
}

/// Removes the redundant `↳ ok · …` tool-result echo from a streamed assistant
/// chunk, keeping every other line (including the `↳ error · …` failure echo,
/// whose detail must survive). The tool card already conveys a successful call,
/// so echoing it doubles the line; collapsing it to the card alone gives one
/// dim line per tool call. Borrows when there is nothing to strip.
fn strip_ok_result_echo(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("\u{21b3} ok") {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for seg in text.split_inclusive('\n') {
        if seg.trim_start().starts_with("\u{21b3} ok") {
            continue;
        }
        out.push_str(seg);
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod echo_tests {
    use super::strip_ok_result_echo;

    #[test]
    fn ok_result_echo_is_dropped_but_failures_and_content_survive() {
        // The "↳ ok · …" echo (as framed by the daemon) collapses away.
        let s = strip_ok_result_echo("\n\u{21b3} ok \u{00b7} [git status]\n");
        assert!(!s.contains("\u{21b3} ok"), "ok echo dropped: {s:?}");

        // A failure echo keeps its detail.
        let f = strip_ok_result_echo("\n\u{21b3} error \u{00b7} permission denied\n");
        assert!(f.contains("\u{21b3} error"), "failure kept: {f:?}");

        // Answer content interleaved with an ok echo keeps the content.
        let mixed = strip_ok_result_echo("Findings\n\u{21b3} ok \u{00b7} done\nDetails\n");
        assert!(mixed.contains("Findings") && mixed.contains("Details"));
        assert!(!mixed.contains("\u{21b3} ok"));

        // Nothing to strip → borrows without allocating.
        assert!(matches!(
            strip_ok_result_echo("plain text"),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}
