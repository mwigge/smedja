//! `st-statusbar` — modular status bar with parallel module execution via rayon + threads.
//!
//! Each [`StatusModule`] is evaluated in a dedicated `std::thread` so that slow
//! modules (e.g. git probes) cannot block the rendering pipeline beyond their
//! individual [`StatusModule::timeout_ms`] budget.

mod basic;
mod format;
mod git;
mod perf;
mod render;
mod starship;
mod system;
mod types;

pub use basic::{
    AppNameModule, ContextPctModule, CwdModule, ExitCodeModule, InterfaceModule, ModelModule,
    SessionIdModule, TaskModule, TierModule,
};
pub use format::format_bar;
pub use git::{GitBranchModule, GitStatusModule};
pub use perf::{EfficiencyModule, LatencyModule, TokensModule, TraceModule};
pub use render::render_status_bar_parallel;
pub use starship::{load_starship_fallback, StarshipConfig};
pub use system::{BatteryModule, LanguageModule, TimeModule};
pub use types::{Color, ModuleContext, Segment, SegmentStyle, StatusModule};
