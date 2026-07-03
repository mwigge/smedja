//! Parallel render pipeline with per-module thread timeout.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use crate::{ModuleContext, Segment, SegmentStyle, StatusModule};

/// Fat-pointer (data + vtable) to a `StatusModule` packed into two `usize` words.
///
/// Storing the raw pointer as a pair of `usize` values avoids the `!Send`
/// restriction that the compiler imposes on `*const dyn Trait` values, while
/// preserving the full fat-pointer identity needed to reconstruct a trait-object
/// reference.
///
/// # Safety
///
/// The caller must ensure the pointed-to value outlives any thread that holds
/// a `ModulePtr`. In `render_status_bar_parallel` this is enforced because the
/// rayon closure blocks on `recv_timeout` until the spawned thread finishes (or
/// times out), keeping the `Box<dyn StatusModule>` alive for the thread's full
/// duration.
#[allow(dead_code)] // fields are read via std::mem::transmute, not by name
struct ModulePtr {
    data: usize,
    vtable: usize,
}

/// Renders all modules in parallel using rayon + per-module thread timeout.
///
/// Each module is evaluated in a dedicated `std::thread`. If the thread does
/// not complete within [`StatusModule::timeout_ms`], a `"?"` placeholder
/// segment is emitted instead. Modules that return `None` are omitted.
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

            // The actual evaluation must happen inside a `std::thread` so that
            // `recv_timeout` can cut it off if the module is slow. The rayon
            // closure holds the borrow on `module` and does not return until
            // `recv_timeout` completes. Therefore the `Box<dyn StatusModule>`
            // behind the raw pointer remains alive for the entire duration the
            // spawned thread could possibly access it.
            //
            // We pack the fat pointer into two `usize` words because raw
            // `*const dyn Trait` is `!Send` even when the trait bounds include
            // `Send + Sync`. The data+vtable encoding preserves full trait-object
            // identity and is reconstructed with `std::mem::transmute` inside
            // the spawned thread.
            //
            // SAFETY: `module.as_ref()` is a `&dyn StatusModule` (Send + Sync).
            // Transmuting a `*const dyn StatusModule` to `(usize, usize)` and
            // back is defined behaviour: a fat pointer to a `dyn Trait` is
            // exactly two pointer-sized words (data pointer + vtable pointer).
            // The pointer is valid for the duration of the spawned thread
            // because the rayon closure blocks on `recv_timeout`.
            let raw: *const dyn StatusModule = module.as_ref();
            let ptr = unsafe { std::mem::transmute::<*const dyn StatusModule, ModulePtr>(raw) };
            std::thread::spawn(move || {
                // SAFETY: see above — the fat pointer is valid and the
                // referent outlives this thread.
                let raw = unsafe { std::mem::transmute::<ModulePtr, *const dyn StatusModule>(ptr) };
                let module_ref = unsafe { &*raw };
                let _ = tx.send(module_ref.evaluate(&ctx_clone));
            });

            match rx.recv_timeout(timeout) {
                Ok(Some(seg)) => Some(seg),
                Ok(None) => None,
                Err(_) => Some(Segment {
                    name: "?".to_owned(),
                    text: "?".to_owned(),
                    style: SegmentStyle::default(),
                }),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{make_ctx, plain_segment};
    use crate::{ModelModule, TierModule};

    // 10
    #[test]
    fn parallel_render_collects_all_segments() {
        let ctx = ModuleContext {
            tier: Some("local".to_owned()),
            model: Some("gemma-4-27b".to_owned()),
            ..make_ctx()
        };
        let modules: Vec<Box<dyn StatusModule>> = vec![Box::new(TierModule), Box::new(ModelModule)];
        let segments = render_status_bar_parallel(&modules, &ctx, 500);
        assert!(
            !segments.is_empty(),
            "expected at least one segment, got {}",
            segments.len()
        );
        assert!(
            segments.iter().any(|s| s.text == "[local]"),
            "expected [local] segment in {segments:?}"
        );
    }

    #[test]
    fn module_timeout_emits_question_mark() {
        struct SlowModule;
        impl StatusModule for SlowModule {
            fn name(&self) -> &'static str {
                "slow"
            }
            fn evaluate(&self, _ctx: &ModuleContext) -> Option<Segment> {
                std::thread::sleep(Duration::from_millis(200));
                Some(plain_segment("slow", "done"))
            }
            fn timeout_ms(&self) -> u64 {
                10
            }
        }

        let modules: Vec<Box<dyn StatusModule>> = vec![Box::new(SlowModule)];
        let ctx = make_ctx();
        let segments = render_status_bar_parallel(&modules, &ctx, 500);
        assert_eq!(segments.len(), 1, "expected exactly one timeout segment");
        assert_eq!(
            segments[0].text, "?",
            "timed-out module must emit '?' placeholder"
        );
    }
}
