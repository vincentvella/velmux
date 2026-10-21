//! Worktree pooling: each repo's base checkout owns a fixed set of warm worktree
//! slots that are claimed for a workstream and released back clean.

pub mod claude;
pub mod config;
pub mod deps;
pub mod git;
mod pool;
pub mod state;

pub use config::{tui_settings, Config, Env, NotifySettings, TuiSettings};
pub use pool::{ClaimOpts, ParkedView, Pool, Released, SlotView};
pub use state::Status;
