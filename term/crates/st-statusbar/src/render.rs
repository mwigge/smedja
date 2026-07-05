//! Parallel render pipeline and `format_bar` token substitution.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use crate::{ModuleContext, Segment, SegmentStyle, StatusModule};

// ── Parallel render ───────────────────────────────────────────────────────────

/// Renders all modules in parallel using rayon + per-module thread timeout.
///
/// Each module is evaluated in a scoped `std::thread` so that `recv_timeout` can
/// emit a `"?"` placeholder when a module does not answer within
/// [`StatusModule::timeout_ms`]. Modules that return `None` are omitted.
///
/// The `budget_ms` parameter is accepted for API compatibility; per-module
/// timeouts are the primary enforcement mechanism.
#[must_use]
pub fn render_status_bar_parallel(
    modules: &[Box<dyn StatusModule>],
    ctx: &ModuleContext,
    _budget_ms: u64,
) -> Vec<Segment> {
    use rayon::prelude::*;

    // Arc so every rayon task can share ctx with its spawned thread.
    let ctx = Arc::new(ctx.clone());

    modules
        .par_iter()
        .filter_map(|module| {
            let timeout = Duration::from_millis(module.timeout_ms());
            let (tx, rx) = mpsc::channel::<Option<Segment>>();
            let ctx_clone = Arc::clone(&ctx);
            let module_ref: &dyn StatusModule = module.as_ref();

            // A scoped thread borrows `module_ref` safely: the scope does not
            // exit until the spawned thread is joined, so the borrowed module can
            // never be observed after it is dropped (no use-after-free) and the
            // thread cannot outlive this call (no per-timeout thread leak). On a
            // slow module, `recv_timeout` yields the `"?"` placeholder while the
            // scope still joins the worker before returning.
            std::thread::scope(|scope| {
                scope.spawn(move || {
                    let _ = tx.send(module_ref.evaluate(&ctx_clone));
                });

                match rx.recv_timeout(timeout) {
                    Ok(seg) => seg,
                    Err(_) => Some(Segment {
                        name: "?".to_owned(),
                        text: "?".to_owned(),
                        style: SegmentStyle::default(),
                    }),
                }
            })
        })
        .collect()
}

// ── format_bar ────────────────────────────────────────────────────────────────

/// Substitutes `$module_name` tokens in `format` with the matching segment text.
///
/// Unresolved tokens (modules not present in `segments`) are removed. Pipe
/// characters `|` are replaced with the box-drawing vertical `│` (U+2502).
#[must_use]
pub fn format_bar(segments: &[Segment], format: &str) -> String {
    let mut result = format.to_owned();

    // Replace matched tokens.
    for seg in segments {
        let token = format!("${}", seg.name);
        result = result.replace(&token, &seg.text);
    }

    // Remove leftover $tokens.
    let mut out = String::with_capacity(result.len());
    let mut chars = result.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            while chars
                .peek()
                .is_some_and(|ch| ch.is_alphanumeric() || *ch == '_')
            {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }

    // Replace ASCII pipe with box-drawing vertical bar.
    out.replace('|', "\u{2502}")
}
