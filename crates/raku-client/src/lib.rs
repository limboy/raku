//! Rust transport and lifecycle support for clients of `raku-daemon`.
//!
//! This crate intentionally depends only on [`raku_protocol`], so GUI and CLI
//! clients cannot accidentally reach daemon-owned filesystem, Git, database,
//! or provider implementations.

mod client;
pub mod command_env;
pub mod composer_complete;
pub mod driver;
pub mod legacy_data;
pub mod persistence;
mod process;
mod workspace_client;

pub use client::{DaemonClient, PendingResponse};
pub use process::{DaemonProcess, DaemonSupervisor};
pub use raku_protocol::*;
pub use workspace_client::WorkspaceClient;

pub mod git_branch {
    pub use raku_protocol::git::{BranchEntry, BranchSnapshot};
}

pub mod git_commit {
    pub use raku_protocol::git::AgentInvocation;
    pub use raku_protocol::git::CommitSnapshot as Snapshot;
}

pub mod worktree {
    pub use raku_protocol::git::CreatedWorktree;
}
