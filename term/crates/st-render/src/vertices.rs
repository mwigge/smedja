//! Vertex building: turns the cell grid, block decorations, and status/top bars
//! into the vertex arrays consumed by the render pipelines.

use crate::atlas::ATLAS_SIZE;
use crate::{BgVertex, GlyphVertex, Renderer};

impl Renderer {
    // ── Private helpers ───────────────────────────────────────────────────────

    /// Estimates cell size in physical pixels.
    ///
    /// Font size is multiplied by `scale_factor` so each cell occupies the
    /// correct number of physical pixels on `HiDPI` displays.
    fn cell_size(&self) -> (f32, f32) {
        let eff = self.config.font.size * self.scale_factor as f32;
        (eff * 0.6, eff * 1.2)
    }

    fn cell_to_ndc(&self, col: u16, row: u16, cell_w: f32, cell_h: f32) -> (f32, f32, f32, f32) {
        let pw = self.size.width as f32;
        let ph = self.size.height as f32;
        let top_off = self.top_bar_height_px() as f32;
        let x0 = (f32::from(col) * cell_w) / pw * 2.0 - 1.0;
        let y0 = 1.0 - (f32::from(row) * cell_h + top_off) / ph * 2.0;
        let x1 = x0 + cell_w / pw * 2.0;
        let y1 = y0 - cell_h / ph * 2.0;
        (x0, y0, x1, y1)
    }

    /// Converts a pixel-space rectangle `(px0, py0, px1, py1)` to NDC.
    ///
    /// `py0` is the top edge (smaller y in pixel space, larger y in NDC).
    pub(crate) fn px_to_ndc(&self, px0: f32, py0: f32, px1: f32, py1: f32) -> (f32, f32, f32, f32) {
        let pw = self.size.width as f32;
        let ph = self.size.height as f32;
        let x0 = px0 / pw * 2.0 - 1.0;
        let y0 = 1.0 - py0 / ph * 2.0;
        let x1 = px1 / pw * 2.0 - 1.0;
        let y1 = 1.0 - py1 / ph * 2.0;
        (x0, y0, x1, y1)
    }

    pub(crate) fn build_bg_vertices(&self) -> Vec<BgVertex> {
        let (cw, ch) = self.cell_size();
        let mut verts = Vec::with_capacity(self.cells.len() * 6);
        // When a background image is active, multiply cell-background alpha by
        // opacity so the image shows through.  Without an image the existing
        // solid-color behaviour is preserved (alpha unchanged).
        let cell_alpha_mult = if self.bg_image_bind_group.is_some() {
            self.background.opacity
        } else {
            1.0
        };

        for cell in &self.cells {
            let (x0, y0, x1, y1) = self.cell_to_ndc(cell.col, cell.row, cw, ch);
            let c = [
                cell.bg[0],
                cell.bg[1],
                cell.bg[2],
                cell.bg[3] * cell_alpha_mult,
            ];
            // Two triangles forming a quad.
            verts.extend_from_slice(&[
                BgVertex {
                    position: [x0, y0],
                    color: c,
                },
                BgVertex {
                    position: [x1, y0],
                    color: c,
                },
                BgVertex {
                    position: [x0, y1],
                    color: c,
                },
                BgVertex {
                    position: [x1, y0],
                    color: c,
                },
                BgVertex {
                    position: [x1, y1],
                    color: c,
                },
                BgVertex {
                    position: [x0, y1],
                    color: c,
                },
            ]);

            // Underline / strikethrough rules drawn in the cell's foreground
            // colour. NDC y0 is the top edge, y1 the bottom edge.
            if cell.underline || cell.strikethrough {
                let fg = cell.fg;
                let t = 2.0 / self.size.height as f32; // ~1px thick in NDC
                let mut rule = |ytop: f32, ybot: f32| {
                    verts.extend_from_slice(&[
                        BgVertex {
                            position: [x0, ytop],
                            color: fg,
                        },
                        BgVertex {
                            position: [x1, ytop],
                            color: fg,
                        },
                        BgVertex {
                            position: [x0, ybot],
                            color: fg,
                        },
                        BgVertex {
                            position: [x1, ytop],
                            color: fg,
                        },
                        BgVertex {
                            position: [x1, ybot],
                            color: fg,
                        },
                        BgVertex {
                            position: [x0, ybot],
                            color: fg,
                        },
                    ]);
                };
                if cell.underline {
                    rule(y1 + t * 2.0, y1);
                }
                if cell.strikethrough {
                    let ymid = f32::midpoint(y0, y1);
                    rule(ymid + t, ymid - t);
                }
            }
        }

        // Block decoration borders (left 1px bar in #a9652f).
        let border_color: [f32; 4] = [0.663, 0.396, 0.184, 1.0];
        let bar_w = 2.0 / self.size.width as f32; // 1 pixel in NDC
        for dec in &self.block_decorations {
            let (x0, y0, _, _) = self.cell_to_ndc(0, dec.start_row, cw, ch);
            let (_, _, _, y1) = self.cell_to_ndc(0, dec.end_row, cw, ch);
            let x1 = x0 + bar_w;
            let c = border_color;
            verts.extend_from_slice(&[
                BgVertex {
                    position: [x0, y0],
                    color: c,
                },
                BgVertex {
                    position: [x1, y0],
                    color: c,
                },
                BgVertex {
                    position: [x0, y1],
                    color: c,
                },
                BgVertex {
                    position: [x1, y0],
                    color: c,
                },
                BgVertex {
                    position: [x1, y1],
                    color: c,
                },
                BgVertex {
                    position: [x0, y1],
                    color: c,
                },
            ]);
        }

        // ── Agent block backgrounds ───────────────────────────────────────────
        // Each block gets a semi-transparent dark background panel.
        {
            let agent_bg: [f32; 4] = [0.05, 0.05, 0.08, 0.85];
            let pw = self.size.width as f32;
            for block in &self.agent_blocks {
                let row_offset = f32::from(block.start_row);
                // The author label shares the first body row (hanging indent), so
                // the panel spans one row per body line, or a single row when the
                // block has no body yet (label only).
                let row_count = block.content_lines.len().max(1) as f32;
                let py0 = row_offset * ch;
                let py1 = py0 + row_count * ch;
                let (x0, y0, x1, y1) = self.px_to_ndc(0.0, py0, pw, py1);
                verts.extend_from_slice(&[
                    BgVertex {
                        position: [x0, y0],
                        color: agent_bg,
                    },
                    BgVertex {
                        position: [x1, y0],
                        color: agent_bg,
                    },
                    BgVertex {
                        position: [x0, y1],
                        color: agent_bg,
                    },
                    BgVertex {
                        position: [x1, y0],
                        color: agent_bg,
                    },
                    BgVertex {
                        position: [x1, y1],
                        color: agent_bg,
                    },
                    BgVertex {
                        position: [x0, y1],
                        color: agent_bg,
                    },
                ]);
            }
        }

        // ── Status bar background strip ───────────────────────────────────────
        {
            // Always draw the status bar background so the strip is visible
            // even when no modules produce output.
            let sb_h = self.status_bar_height_px() as f32;
            let ph = self.size.height as f32;
            let pw = self.size.width as f32;
            let py0 = ph - sb_h;
            let py1 = ph;
            // Dark background slightly different from terminal bg.
            let sb_bg: [f32; 4] = [0.07, 0.07, 0.09, 1.0];
            let (x0, y0, x1, y1) = self.px_to_ndc(0.0, py0, pw, py1);
            verts.extend_from_slice(&[
                BgVertex {
                    position: [x0, y0],
                    color: sb_bg,
                },
                BgVertex {
                    position: [x1, y0],
                    color: sb_bg,
                },
                BgVertex {
                    position: [x0, y1],
                    color: sb_bg,
                },
                BgVertex {
                    position: [x1, y0],
                    color: sb_bg,
                },
                BgVertex {
                    position: [x1, y1],
                    color: sb_bg,
                },
                BgVertex {
                    position: [x0, y1],
                    color: sb_bg,
                },
            ]);
        }

        // ── Top bar background strip ──────────────────────────────────────────
        {
            let tb_h = self.top_bar_height_px() as f32;
            if tb_h > 0.0 {
                let pw = self.size.width as f32;
                let tb_bg: [f32; 4] = [0.05, 0.05, 0.08, 1.0];
                let (x0, y0, x1, y1) = self.px_to_ndc(0.0, 0.0, pw, tb_h);
                verts.extend_from_slice(&[
                    BgVertex {
                        position: [x0, y0],
                        color: tb_bg,
                    },
                    BgVertex {
                        position: [x1, y0],
                        color: tb_bg,
                    },
                    BgVertex {
                        position: [x0, y1],
                        color: tb_bg,
                    },
                    BgVertex {
                        position: [x1, y0],
                        color: tb_bg,
                    },
                    BgVertex {
                        position: [x1, y1],
                        color: tb_bg,
                    },
                    BgVertex {
                        position: [x0, y1],
                        color: tb_bg,
                    },
                ]);
            }
        }

        verts
    }

    pub(crate) fn build_glyph_vertices(&self) -> Vec<GlyphVertex> {
        let (cw, ch) = self.cell_size();
        let eff_font = self.config.font.size * self.scale_factor as f32;
        let eff_font_key = eff_font.to_bits();
        let sb_font_size = self.status_bar_height_px() as f32 * 0.65;
        let sb_font_key = sb_font_size.to_bits();
        let atlas_size_f = ATLAS_SIZE as f32;
        // Reserve extra capacity for status bar glyphs.
        let extra: usize = self
            .status_bar_segments
            .iter()
            .map(|s| s.text.len())
            .sum::<usize>()
            + self.status_bar_segments.len().saturating_sub(1); // separators
        let mut verts = Vec::with_capacity(self.cells.len() * 6 + extra * 6);

        for cell in &self.cells {
            if cell.ch == ' ' {
                continue;
            }
            // Registered PUA glyphs are drawn by the colour pass — skip them
            // here so they are not also (incorrectly) sampled from the alpha
            // atlas.
            if self.atlas.colour_glyphs.contains_key(&cell.ch) {
                continue;
            }
            // Look up glyph entry from atlas (read-only view — we cannot call
            // get_or_insert here because we'd need &mut self; use cached value).
            let Some(&entry) =
                self.atlas
                    .glyphs
                    .get(&(cell.ch, cell.bold, cell.italic, eff_font_key))
            else {
                tracing::warn!(ch = %cell.ch, "glyph atlas miss — cell skipped");
                continue;
            };
            let u0 = entry.x as f32 / atlas_size_f;
            let v0 = entry.y as f32 / atlas_size_f;
            let u1 = (entry.x + entry.w) as f32 / atlas_size_f;
            let v1 = (entry.y + entry.h) as f32 / atlas_size_f;

            // A double-width glyph is centred over two columns, not one.
            let advance = if cell.wide { cw * 2.0 } else { cw };
            let top_off = self.top_bar_height_px() as f32;
            let baseline_y = f32::from(cell.row) * ch + ch * (2.0 / 3.0) + top_off;
            let glyph_top = baseline_y - entry.bearing_y as f32;
            let glyph_left = f32::from(cell.col) * cw + (advance - entry.w as f32) / 2.0;
            let (x0, y0, x1, y1) = self.px_to_ndc(
                glyph_left,
                glyph_top,
                glyph_left + entry.w as f32,
                glyph_top + entry.h as f32,
            );
            let c = cell.fg;
            verts.extend_from_slice(&[
                GlyphVertex {
                    position: [x0, y0],
                    tex_coords: [u0, v0],
                    color: c,
                },
                GlyphVertex {
                    position: [x1, y0],
                    tex_coords: [u1, v0],
                    color: c,
                },
                GlyphVertex {
                    position: [x0, y1],
                    tex_coords: [u0, v1],
                    color: c,
                },
                GlyphVertex {
                    position: [x1, y0],
                    tex_coords: [u1, v0],
                    color: c,
                },
                GlyphVertex {
                    position: [x1, y1],
                    tex_coords: [u1, v1],
                    color: c,
                },
                GlyphVertex {
                    position: [x0, y1],
                    tex_coords: [u0, v1],
                    color: c,
                },
            ]);
        }

        // ── Status bar glyphs ─────────────────────────────────────────────────
        //
        // Text is rendered at a fixed 12×18 cell size (independent of the
        // terminal grid font) so it fits within the status_bar_height_px() strip.
        let sb_h = self.status_bar_height_px() as f32;
        let ph = self.size.height as f32;
        let pw = self.size.width as f32;
        // Status bar font metrics: fixed 12 px wide, sb_h tall.
        let sb_cw = 7.2_f32; // ~60 % of 12 px
        let mut col_px = 4.0_f32; // 4 px left padding

        for (seg_idx, seg) in self.status_bar_segments.iter().enumerate() {
            // Separator between segments.
            if seg_idx > 0 {
                col_px += sb_cw; // one character-width gap
            }
            let fg_color: [f32; 4] =
                seg.style
                    .fg
                    .as_ref()
                    .map_or([0.957, 0.843, 0.631, 1.0], |c| {
                        [
                            f32::from(c.r) / 255.0,
                            f32::from(c.g) / 255.0,
                            f32::from(c.b) / 255.0,
                            1.0,
                        ]
                    }); // forged_terminal fg

            for ch in seg.text.chars() {
                if ch == ' ' {
                    col_px += sb_cw;
                    continue;
                }
                let Some(&entry) = self.atlas.glyphs.get(&(ch, false, false, sb_font_key)) else {
                    tracing::warn!(ch = %ch, "glyph atlas miss — status-bar cell skipped");
                    col_px += sb_cw;
                    continue;
                };
                let u0 = entry.x as f32 / atlas_size_f;
                let v0 = entry.y as f32 / atlas_size_f;
                let u1 = (entry.x + entry.w) as f32 / atlas_size_f;
                let v1 = (entry.y + entry.h) as f32 / atlas_size_f;

                // Place glyph at natural size — center horizontally in the
                // allocated cell, use bearing_y for vertical baseline placement.
                let strip_top = ph - sb_h;
                let glyph_w = entry.w as f32;
                let glyph_h = entry.h as f32;
                let glyph_left = col_px + (sb_cw - glyph_w) / 2.0;
                let baseline = strip_top + sb_h * (2.0 / 3.0);
                let glyph_top = baseline - entry.bearing_y as f32;
                let (x0, y0, x1, y1) = self.px_to_ndc(
                    glyph_left,
                    glyph_top,
                    glyph_left + glyph_w,
                    glyph_top + glyph_h,
                );
                let c = fg_color;
                verts.extend_from_slice(&[
                    GlyphVertex {
                        position: [x0, y0],
                        tex_coords: [u0, v0],
                        color: c,
                    },
                    GlyphVertex {
                        position: [x1, y0],
                        tex_coords: [u1, v0],
                        color: c,
                    },
                    GlyphVertex {
                        position: [x0, y1],
                        tex_coords: [u0, v1],
                        color: c,
                    },
                    GlyphVertex {
                        position: [x1, y0],
                        tex_coords: [u1, v0],
                        color: c,
                    },
                    GlyphVertex {
                        position: [x1, y1],
                        tex_coords: [u1, v1],
                        color: c,
                    },
                    GlyphVertex {
                        position: [x0, y1],
                        tex_coords: [u0, v1],
                        color: c,
                    },
                ]);
                col_px += sb_cw;
                // Stop if we run off the right edge.
                if col_px >= pw {
                    break;
                }
            }
            if col_px >= pw {
                break;
            }
        }

        // ── Top bar glyphs ────────────────────────────────────────────────────
        {
            let tb_h = self.top_bar_height_px() as f32;
            if tb_h > 0.0 {
                let pw = self.size.width as f32;
                let tb_cw = 7.2_f32;
                let mut tb_col_px = 4.0_f32;

                for (seg_idx, seg) in self.top_bar_segments.iter().enumerate() {
                    if seg_idx > 0 {
                        tb_col_px += tb_cw;
                    }
                    let fg_color: [f32; 4] =
                        seg.style
                            .fg
                            .as_ref()
                            .map_or([0.957, 0.843, 0.631, 1.0], |c| {
                                [
                                    f32::from(c.r) / 255.0,
                                    f32::from(c.g) / 255.0,
                                    f32::from(c.b) / 255.0,
                                    1.0,
                                ]
                            });

                    for ch in seg.text.chars() {
                        if ch == ' ' {
                            tb_col_px += tb_cw;
                            continue;
                        }
                        let Some(&entry) = self.atlas.glyphs.get(&(ch, false, false, sb_font_key))
                        else {
                            tracing::warn!(ch = %ch, "glyph atlas miss — top-bar cell skipped");
                            tb_col_px += tb_cw;
                            continue;
                        };
                        let u0 = entry.x as f32 / atlas_size_f;
                        let v0 = entry.y as f32 / atlas_size_f;
                        let u1 = (entry.x + entry.w) as f32 / atlas_size_f;
                        let v1 = (entry.y + entry.h) as f32 / atlas_size_f;

                        let glyph_w = entry.w as f32;
                        let glyph_h = entry.h as f32;
                        let glyph_left = tb_col_px + (tb_cw - glyph_w) / 2.0;
                        let baseline = tb_h * (2.0 / 3.0);
                        let glyph_top = baseline - entry.bearing_y as f32;
                        let (x0, y0, x1, y1) = self.px_to_ndc(
                            glyph_left,
                            glyph_top,
                            glyph_left + glyph_w,
                            glyph_top + glyph_h,
                        );
                        let c = fg_color;
                        verts.extend_from_slice(&[
                            GlyphVertex {
                                position: [x0, y0],
                                tex_coords: [u0, v0],
                                color: c,
                            },
                            GlyphVertex {
                                position: [x1, y0],
                                tex_coords: [u1, v0],
                                color: c,
                            },
                            GlyphVertex {
                                position: [x0, y1],
                                tex_coords: [u0, v1],
                                color: c,
                            },
                            GlyphVertex {
                                position: [x1, y0],
                                tex_coords: [u1, v0],
                                color: c,
                            },
                            GlyphVertex {
                                position: [x1, y1],
                                tex_coords: [u1, v1],
                                color: c,
                            },
                            GlyphVertex {
                                position: [x0, y1],
                                tex_coords: [u0, v1],
                                color: c,
                            },
                        ]);
                        tb_col_px += tb_cw;
                        if tb_col_px >= pw {
                            break;
                        }
                    }
                    if tb_col_px >= pw {
                        break;
                    }
                }
            }
        }

        // ── Agent block glyphs ────────────────────────────────────────────────
        //
        // Render each agent block's header (model name) and content lines at
        // the block's start_row, using the terminal cell metrics.
        if !self.agent_blocks.is_empty() {
            let agent_header_color: [f32; 4] = [0.4, 0.8, 1.0, 1.0]; // light-blue header
            let agent_text_color: [f32; 4] = [0.9, 0.9, 0.9, 1.0]; // near-white body

            // Helper closure: emit glyph quads for one line of text, starting at
            // `start_col`. The starting column is a pure render-geometry offset —
            // the text string itself is never padded, so selection/copy of the
            // underlying content stays unindented.
            let emit_line = |verts: &mut Vec<GlyphVertex>,
                             text: &str,
                             line_row: u16,
                             start_col: u16,
                             color: [f32; 4]| {
                let mut col = start_col;
                for glyph_ch in text.chars() {
                    if glyph_ch == ' ' {
                        col += 1;
                        continue;
                    }
                    let Some(&entry) =
                        self.atlas
                            .glyphs
                            .get(&(glyph_ch, false, false, eff_font_key))
                    else {
                        col += 1;
                        continue;
                    };
                    let u0 = entry.x as f32 / atlas_size_f;
                    let v0 = entry.y as f32 / atlas_size_f;
                    let u1 = (entry.x + entry.w) as f32 / atlas_size_f;
                    let v1 = (entry.y + entry.h) as f32 / atlas_size_f;
                    let baseline_y = f32::from(line_row) * ch + ch * (2.0 / 3.0);
                    let glyph_top = baseline_y - entry.bearing_y as f32;
                    let glyph_left = f32::from(col) * cw + (cw - entry.w as f32) / 2.0;
                    let (x0, y0, x1, y1) = self.px_to_ndc(
                        glyph_left,
                        glyph_top,
                        glyph_left + entry.w as f32,
                        glyph_top + entry.h as f32,
                    );
                    verts.extend_from_slice(&[
                        GlyphVertex {
                            position: [x0, y0],
                            tex_coords: [u0, v0],
                            color,
                        },
                        GlyphVertex {
                            position: [x1, y0],
                            tex_coords: [u1, v0],
                            color,
                        },
                        GlyphVertex {
                            position: [x0, y1],
                            tex_coords: [u0, v1],
                            color,
                        },
                        GlyphVertex {
                            position: [x1, y0],
                            tex_coords: [u1, v0],
                            color,
                        },
                        GlyphVertex {
                            position: [x1, y1],
                            tex_coords: [u1, v1],
                            color,
                        },
                        GlyphVertex {
                            position: [x0, y1],
                            tex_coords: [u0, v1],
                            color,
                        },
                    ]);
                    col += 1;
                }
            };

            for block in self.agent_blocks.clone() {
                let start_row = block.start_row;
                let margin = block.left_margin_cols;
                // Author label: rendered in the left margin on the block's first
                // row. It stays put at column 0 while the body hangs to its right.
                let header = crate::agent_header(&block.model);
                emit_line(&mut verts, &header, start_row, 0, agent_header_color);
                // Body lines are shifted right by the hanging-indent margin so the
                // first body line sits beside the label and wrapped continuations
                // align under the content, not under the gutter glyph. The shift is
                // geometry only — content strings carry no indent.
                for (i, line_text) in block.content_lines.iter().enumerate() {
                    let row = start_row.saturating_add(u16::try_from(i).unwrap_or(u16::MAX));
                    emit_line(&mut verts, line_text, row, margin, agent_text_color);
                }
            }
        }

        verts
    }

    /// Builds the vertex quads for registered PUA-codepoint cells, sampling the
    /// colour atlas.
    ///
    /// Returns an empty vector when no visible cell resolves to a registered
    /// colour glyph (the common case), so the colour draw is skipped entirely.
    pub(crate) fn build_colour_glyph_vertices(&self) -> Vec<GlyphVertex> {
        let (cw, ch) = self.cell_size();
        let atlas_size_f = ATLAS_SIZE as f32;
        let top_off = self.top_bar_height_px() as f32;
        let mut verts: Vec<GlyphVertex> = Vec::new();

        for cell in &self.cells {
            if cell.ch == ' ' {
                continue;
            }
            let Some(&entry) = self.atlas.colour_glyphs.get(&cell.ch) else {
                continue;
            };
            let u0 = entry.x as f32 / atlas_size_f;
            let v0 = entry.y as f32 / atlas_size_f;
            let u1 = (entry.x + entry.w) as f32 / atlas_size_f;
            let v1 = (entry.y + entry.h) as f32 / atlas_size_f;

            let baseline_y = f32::from(cell.row) * ch + ch * (2.0 / 3.0) + top_off;
            let glyph_top = baseline_y - entry.bearing_y as f32;
            let glyph_left = f32::from(cell.col) * cw + (cw - entry.w as f32) / 2.0;
            let (x0, y0, x1, y1) = self.px_to_ndc(
                glyph_left,
                glyph_top,
                glyph_left + entry.w as f32,
                glyph_top + entry.h as f32,
            );
            // Carry the cell foreground alpha so transparency still applies; the
            // colour shader keeps the glyph's own RGB.
            let c = cell.fg;
            verts.extend_from_slice(&[
                GlyphVertex {
                    position: [x0, y0],
                    tex_coords: [u0, v0],
                    color: c,
                },
                GlyphVertex {
                    position: [x1, y0],
                    tex_coords: [u1, v0],
                    color: c,
                },
                GlyphVertex {
                    position: [x0, y1],
                    tex_coords: [u0, v1],
                    color: c,
                },
                GlyphVertex {
                    position: [x1, y0],
                    tex_coords: [u1, v0],
                    color: c,
                },
                GlyphVertex {
                    position: [x1, y1],
                    tex_coords: [u1, v1],
                    color: c,
                },
                GlyphVertex {
                    position: [x0, y1],
                    tex_coords: [u0, v1],
                    color: c,
                },
            ]);
        }

        verts
    }
}
