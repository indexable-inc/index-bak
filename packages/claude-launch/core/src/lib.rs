//! A typed way to start headless Claude Code (#ENG-11979).
//!
//! Four places in this tree spawn `claude -p` today and each one builds its
//! own argv from string literals: the kernel's subagent launcher
//! (`packages/mcp-ex/lib/ix_mcp/agents/backend.ex`), loom
//! (`packages/loom/lib/loom/claude.ex`), and the Python TUI harness
//! (`packages/tui/tui-py/python/tui/harness.py`). Two of them also carry
//! their own line-JSON dispatcher, each with a catch-all arm that drops any
//! event kind the CLI grows. This crate is the one definition all of them
//! can share: [`Config`] renders the argv, and the child's
//! `--output-format stream-json` output comes back as [`Event`] values.
//!
//! ```no_run
//! # async fn example() -> Result<(), claude_launch_core::Error> {
//! use claude_launch_core::{Config, PermissionMode};
//!
//! let config = Config::print("say hi")
//!     .permission_mode(PermissionMode::Plan)
//!     .allowed_tools(["Read"]);
//! let outcome = claude_launch_core::run(&config).await?;
//! println!("{}", outcome.text);
//! # Ok(())
//! # }
//! ```
//!
//! # The flag surface is not ours
//!
//! Every flag name and every stream-json field this crate knows about was
//! read off `claude --help` and a real `--output-format stream-json` run of
//! Claude Code **2.1.220 on 2026-08-02**. The CLI ships no machine-readable
//! schema for either: `--output-format schema` is rejected
//! (`Allowed choices are text, json, stream-json`) and the installed package
//! carries no `.d.ts`, so this is a hand-written mirror and it can fall
//! behind. [`Event::Unrecognized`] is how it says so out loud rather than
//! dropping the line; see [`event`] for the policy.

mod argv;
mod config;
mod event;
mod run;

pub use config::{
    Config, Effort, Error, Features, InputFormat, McpPolicy, McpServer, McpTransport, OutputFormat,
    PermissionMode, SessionMode, SettingSource,
};
pub use event::{Block, Event, Init, Message, Outcome, SCHEMA_CHECKED};
pub use run::{event_stream, run};
