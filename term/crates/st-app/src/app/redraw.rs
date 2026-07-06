use super::*;

impl App {
    pub(super) fn handle_redraw_requested(&mut self) {
        // Flush any bytes the VT parser queued to write back to the
        // application (e.g. the kitty keyboard protocol query response).
        if let Some(pty) = &mut self.pty {
            let resp = std::mem::take(&mut pty.grid.lock().pending_responses);
            if !resp.is_empty() {
                if let Err(e) = pty.write_input(&resp) {
                    debug!("PTY response write error: {}", e);
                }
            }
        }
        if let (Some(pty), Some(renderer)) = (&self.pty, &mut self.renderer) {
            let dirty = pty.dirty.load(Ordering::Acquire);
            let sync_active = pty.grid.lock().synchronized_output;
            let occluded = self.occluded;
            debug!(dirty, sync_active, occluded, "RedrawRequested");
            if dirty && !sync_active {
                pty.dirty.store(false, Ordering::Release);
                let grid = pty.grid.lock();
                // Live screen (offset 0): use the cells' own stored
                // row/col exactly as before — no behaviour change to the
                // common path. Only when scrolled back do we stamp
                // positions, since scrollback rows carry stale indices.
                // The alt screen (full-screen apps like smedja-tui/vim)
                // has no scrollback view — always render the live grid
                // there, never a scrolled window. Position every cell by
                // its grid INDEX, not its stored .row/.col fields: those
                // can go stale after row shifts (scroll regions, IL/DL),
                // and the renderer positions by the field — a stale value
                // draws the cell at the wrong Y, overlapping the top rows.
                let cells: Vec<st_render::Cell> = if grid.scroll_offset <= 0 || grid.alt_screen {
                    grid.cells
                        .iter()
                        .enumerate()
                        .flat_map(|(r, row)| {
                            row.iter().enumerate().map(move |(c, cell)| {
                                render_cell(
                                    cell,
                                    u16::try_from(c).unwrap_or(u16::MAX),
                                    u16::try_from(r).unwrap_or(u16::MAX),
                                )
                            })
                        })
                        .collect()
                } else {
                    grid.visible_rows(grid.scroll_offset)
                        .iter()
                        .enumerate()
                        .flat_map(|(r, row)| {
                            row.iter().enumerate().map(move |(col, c)| {
                                render_cell(
                                    c,
                                    u16::try_from(col).unwrap_or(u16::MAX),
                                    u16::try_from(r).unwrap_or(u16::MAX),
                                )
                            })
                        })
                        .collect()
                };
                let non_blank = cells.iter().filter(|c| c.ch != ' ').count();
                drop(grid);
                debug!(
                    "update_cells: total={} non_blank={}",
                    cells.len(),
                    non_blank
                );

                // If all cells just went blank and we're inside the
                // post-resize suppress window, skip this frame.  The
                // child (ratatui) sends clear+redraw atomically; keeping
                // the old cell content avoids the grey flash while waiting
                // for the redraw to arrive.
                let in_suppress_window = self
                    .suppress_clear_until
                    .is_some_and(|t| std::time::Instant::now() < t);
                if non_blank == 0 && in_suppress_window {
                    // Keep dirty=true so we process the next PTY batch
                    // (the redraw content) without waiting for a new event.
                    pty.dirty.store(true, Ordering::Release);
                } else {
                    if non_blank > 0 {
                        self.suppress_clear_until = None;
                    }
                    renderer.update_cells(&cells);
                }
            }

            // Evaluate status bar modules and update the renderer.
            // The modules run in parallel (rayon + per-module threads)
            // within an 8 ms budget.  Live agent state comes from the
            // st-agent bridge running in its own thread.
            let (
                tier,
                model,
                active_task,
                input_tokens,
                output_tokens,
                latency_ms,
                traceparent,
                tokens_saved,
                efficiency_ratio,
            ) = {
                // Non-blocking try_read: if the lock is contended (agent
                // event writing) skip the update this frame.
                if let Ok(s) = self.pane_state.0.try_read() {
                    (
                        s.tier.clone(),
                        s.model.clone(),
                        s.active_task.clone(),
                        s.last_input_tokens,
                        s.last_output_tokens,
                        s.last_latency_ms,
                        s.last_traceparent.clone(),
                        s.tokens_saved,
                        s.efficiency_ratio,
                    )
                } else {
                    (None, None, None, None, None, None, None, None, None)
                }
            };

            // Read last exit code from OSC 133 D markers in the PTY grid.
            let last_exit_code = {
                let grid = pty.grid.lock();
                grid.block_markers.iter().rev().find_map(|m| {
                    if let st_pty::MarkerKind::CommandDone { exit_code } = m.kind {
                        exit_code
                    } else {
                        None
                    }
                })
            };

            // Read the most recent OSC 7 CWD marker from the PTY grid.
            let pty_cwd = {
                let grid = pty.grid.lock();
                grid.block_markers.iter().rev().find_map(|m| {
                    if let st_pty::MarkerKind::Osc7Cwd { ref path } = m.kind {
                        Some(path.clone())
                    } else {
                        None
                    }
                })
            };
            let cwd = pty_cwd.or_else(|| self.cwd.clone());

            // Context-gauge inputs. The last turn's input (prompt) token count is
            // the live context occupancy — it is everything the model just read —
            // and the window is the model family's published maximum.
            let context_used = input_tokens
                .map(|t| usize::try_from(t).unwrap_or(0))
                .unwrap_or(0);
            let context_window = model
                .as_deref()
                .map(st_statusbar::model_context_window)
                .unwrap_or(0);

            let sb_ctx = st_statusbar::ModuleContext {
                tier,
                model,
                context_used,
                context_window,
                active_task,
                last_exit_code,
                input_tokens,
                output_tokens,
                latency_ms,
                traceparent,
                session_id: Some(self.pane_id.clone()),
                cwd,
                interface: Some("tui".to_owned()),
                tokens_saved,
                efficiency_ratio,
            };

            let git_branch_disabled = self
                .starship_config
                .as_ref()
                .is_some_and(|c| c.git_branch_disabled);
            let git_branch_symbol = self
                .starship_config
                .as_ref()
                .and_then(|c| c.git_branch_symbol.clone());

            // App / session / cwd used to live in a separate TOP bar that
            // stole the first grid row and collided with full-screen apps'
            // own top row. They now lead the single bottom status bar.
            let mut sb_modules: Vec<Box<dyn st_statusbar::StatusModule>> = vec![
                Box::new(st_statusbar::AppNameModule),
                Box::new(st_statusbar::SessionIdModule),
                Box::new(st_statusbar::CwdModule),
                Box::new(st_statusbar::TierModule),
                Box::new(st_statusbar::ModelModule),
                Box::new(st_statusbar::ContextPctModule),
                Box::new(st_statusbar::TaskModule),
                Box::new(st_statusbar::TokensModule),
                Box::new(st_statusbar::EfficiencyModule),
                Box::new(st_statusbar::LatencyModule),
                Box::new(st_statusbar::TraceModule),
                Box::new(st_statusbar::ExitCodeModule),
                Box::new(st_statusbar::TimeModule),
            ];
            if !git_branch_disabled {
                sb_modules.push(Box::new(st_statusbar::GitBranchModule::with_symbol(
                    git_branch_symbol,
                )));
            }
            let mut segments = st_statusbar::render_status_bar_parallel(&sb_modules, &sb_ctx, 8);
            // Resolve the tier badge to a registered PUA glyph (or plain
            // fallback text) using the shared registry.
            if let Some(tier) = sb_ctx.tier.as_deref() {
                let term = std::env::var("TERM").unwrap_or_default();
                let badge = {
                    let reg = pty.glyph_registry.lock();
                    tier_badge_text(&reg, tier, &term)
                };
                if let Some(seg) = segments.iter_mut().find(|s| s.name == "tier") {
                    seg.text = badge;
                }
            }
            renderer.set_status_bar_segments(&segments);

            // No top bar: keep it empty so top_bar_height_px() == 0 and the
            // first grid row is given back to the foreground app.
            renderer.set_top_bar_segments(&[]);

            // Update window title.
            let title = build_window_title(
                sb_ctx.tier.as_deref(),
                sb_ctx.active_task.as_deref(),
                sb_ctx.session_id.as_deref(),
                sb_ctx.cwd.as_deref(),
            );
            for w in self.windows.values() {
                w.set_title(&title);
            }

            // Snapshot agent session content and push to renderer.
            //
            // The agent-block overlay paints over the top grid rows (no
            // top-bar offset). That is fine for the base shell, but when a
            // full-screen app owns the alt screen (smedja-tui, vim, less),
            // it owns every cell — the overlay would corrupt its top rows.
            // Suppress it there.
            let (alt_screen, grid_cols) = {
                let g = pty.grid.lock();
                (g.alt_screen, usize::from(g.cols))
            };
            if let Ok(mgr) = self.agent_manager.0.try_lock() {
                let blocks: Vec<st_render::AgentBlockView> = if alt_screen {
                    Vec::new()
                } else {
                    mgr.sessions()
                        .enumerate()
                        .map(|(i, session)| {
                            // Hanging-indent margin: the author label occupies it,
                            // body lines shift right by it (render geometry only —
                            // the content strings are never padded, so copy stays
                            // clean). Cap at half the grid width so a long model
                            // name can't squeeze the body away.
                            let header = st_render::agent_header(&session.model);
                            let max_margin = (grid_cols / 2).max(1);
                            st_render::AgentBlockView {
                                start_row: u16::try_from(i * 4).unwrap_or(u16::MAX),
                                model: session.model.clone(),
                                content_lines: session.content_lines(),
                                approval_pending: session.approval
                                    == st_agent::ApprovalState::Pending,
                                left_margin_cols: st_render::hanging_margin_cols(
                                    header.chars().count(),
                                    max_margin,
                                ),
                            }
                        })
                        .collect()
                };
                renderer.set_agent_blocks(&blocks);
            }

            if let Err(e) = renderer.render() {
                match e.downcast_ref::<st_render::RenderError>() {
                    Some(st_render::RenderError::Frame(
                        wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated,
                    )) => {
                        info!("render: surface Lost/Outdated — reconfiguring");
                        renderer.resize(renderer.size);
                        pty.dirty.store(true, Ordering::Release);
                    }
                    Some(st_render::RenderError::Frame(wgpu::SurfaceError::Timeout)) => {
                        debug!("render: surface Timeout (vsync skip)");
                    }
                    _ => info!("render error: {}", e),
                }
            }
        }

        // A frame was presented (or attempted) above. Record it so about_to_wait
        // can pace idle repaints, and spend one forced-redraw credit — the small
        // bounded budget that bridges the compositor's grey fallback (Hyprland)
        // after a resize/map/unocclusion. Re-arming the *next* redraw is now
        // handled in about_to_wait, gated on dirty/occluded, so the app no longer
        // spins at vsync when idle.
        self.last_present = Some(std::time::Instant::now());
        self.forced_redraws = self.forced_redraws.saturating_sub(1);
    }
}
