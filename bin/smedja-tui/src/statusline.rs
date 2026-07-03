use crate::statusbar::ModuleCtx;
use crate::theme::{palette, runner_color, runner_label};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Builds the starship-style segmented status line: a mode pip, a runner chip
/// (brand-coloured), tier, mode, and a dim session id, separated by thin dots.
/// Colour-segmented rather than powerline-glyph based, so it needs no Nerd Font.
pub(crate) fn status_bar_line(ctx: &ModuleCtx<'_>, no_color: bool) -> Line<'static> {
    let p = palette();
    let plain = no_color;
    let dim = if plain {
        Style::default()
    } else {
        Style::default().fg(p.text_dim).bg(p.panel)
    };
    let sep = || Span::styled(" · ", dim);
    let chip = |text: String, color: Color, bold: bool| {
        let mut s = if plain {
            Style::default()
        } else {
            Style::default().fg(color).bg(p.panel)
        };
        if bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        Span::styled(text, s)
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    // Mode pip — input (insert/normal) vs scroll.
    let (pip, pip_label) = if ctx.input_mode {
        if ctx.vim_normal_mode {
            ("◇", "NORMAL")
        } else {
            ("●", "INSERT")
        }
    } else {
        ("◆", "SCROLL")
    };
    spans.push(chip(format!("{pip} {pip_label}"), p.accent, true));

    if let Some(runner) = ctx.runner {
        spans.push(sep());
        spans.push(chip(
            format!("◆ {}", runner_label(runner)),
            runner_color(runner),
            true,
        ));
    }
    if let Some(tier) = ctx.tier {
        spans.push(sep());
        let c = match tier {
            "local" => p.local,
            "deep" => p.deep,
            _ => p.fast,
        };
        spans.push(chip(tier.to_owned(), c, false));
    }
    if let Some(mode) = ctx.mode {
        spans.push(sep());
        let mc = crate::theme::agent_color(mode);
        spans.push(chip(mode.to_owned(), mc, false));
    }
    spans.push(sep());
    spans.push(chip(
        ctx.session_id.chars().take(8).collect::<String>(),
        p.text_dim,
        false,
    ));
    if let Some(pct) = ctx.ctx_pct {
        spans.push(sep());
        let color = if pct >= 95 {
            p.error
        } else if pct >= 80 {
            p.warn
        } else {
            p.text_dim
        };
        spans.push(chip(format!("▓ {pct}%"), color, false));
    }
    if ctx.pending {
        spans.push(chip("  ⟳".to_owned(), p.accent, true));
    }
    Line::from(spans)
}

/// A dim, right-aligned discoverability hint for the status row — surfaces the
/// few entry points (slash commands + the rail toggles) so they are not
/// keybind-only knowledge.
pub(crate) fn status_hint_line(no_color: bool) -> Line<'static> {
    let p = palette();
    let style = if no_color {
        Style::default()
    } else {
        Style::default().fg(p.text_dim).bg(p.panel)
    };
    Line::from(Span::styled(
        "/help · ^W/^⇧W sessions · ^O obs · ^L lsp ".to_owned(),
        style,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

    #[test]
    fn status_bar_line_segments_runner_tier_session() {
        let ctx = ModuleCtx {
            session_id: "abcd1234ef",
            mode: Some("impl"),
            tier: Some("deep"),
            runner: Some("claude-cli"),
            pending: false,
            input_mode: true,
            vim_normal_mode: false,
            ctx_pct: None,
        };
        let text: String = status_bar_line(&ctx, true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("INSERT"), "{text}");
        assert!(text.contains("CLAUDE"), "{text}"); // runner_label uppercases
        assert!(text.contains("deep"), "{text}");
        assert!(text.contains("abcd1234"), "{text}"); // 8-char session id
    }

    #[test]
    fn status_bar_shows_ctx_pct_when_nonzero() {
        let ctx = ModuleCtx {
            session_id: "abc",
            mode: None,
            tier: None,
            runner: None,
            pending: false,
            input_mode: true,
            vim_normal_mode: false,
            ctx_pct: Some(61),
        };
        let text: String = status_bar_line(&ctx, true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("61%"), "ctx gauge must appear: {text}");
    }

    #[test]
    fn status_bar_omits_ctx_gauge_when_none() {
        let ctx = ModuleCtx {
            session_id: "abc",
            mode: None,
            tier: None,
            runner: None,
            pending: false,
            input_mode: true,
            vim_normal_mode: false,
            ctx_pct: None,
        };
        let text: String = status_bar_line(&ctx, true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!text.contains('%'), "no gauge when ctx_pct is None: {text}");
    }

    #[test]
    fn status_hint_advertises_real_entry_points() {
        let text: String = status_hint_line(true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("/help"), "{text}");
        assert!(text.contains("^W"), "{text}");
    }

    #[test]
    fn status_bar_shows_tier_when_set() {
        let mut state = make_state("sess-xyz");
        state.tier = Some("fast".into());
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(content.contains("fast"), "status bar must render the tier");
    }

    #[test]
    fn status_bar_shows_unknown_when_no_tier() {
        let mut state = make_state("sess-xyz");
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(!content.trim().is_empty());
    }

    #[test]
    fn status_bar_shows_runner_when_set() {
        let mut state = make_state("sess-runner");
        state.runner = "anthropic".to_owned();
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("ANTHROPIC"),
            "status bar must render the runner label; got: {content}"
        );
    }

    #[test]
    fn status_bar_shows_input_mode_badge_when_not_scroll() {
        let mut state = make_state("sess-mode");
        state.scroll_focus = false;
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("INSERT"),
            "status bar must show INSERT when scroll_focus=false; got: {content}"
        );
    }

    #[test]
    fn status_bar_shows_normal_mode_badge_when_scroll() {
        let mut state = make_state("sess-mode");
        state.scroll_focus = true;
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("SCROLL"),
            "status bar must show SCROLL when scroll_focus=true; got: {content}"
        );
    }
}
