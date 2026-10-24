//! Worktree pooling: each repo's base checkout owns a fixed set of warm worktree
//! slots that are claimed for a workstream and released back clean.

pub mod claude;
pub mod config;
pub mod deps;
pub mod gh;
pub mod git;
mod pool;
pub mod state;
pub mod sys;

pub use config::{tui_settings, Config, Env, NotifySettings, Ports, SyncMode, TuiSettings};
pub use pool::{BaseUpdate, ClaimOpts, ParkedView, Pool, Released, SlotView, OLD_DAYS};
pub use state::Status;
