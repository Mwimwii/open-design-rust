//! OpenDesign daemon core in Rust.
//!
//! Parity reference: `apps/daemon` (TypeScript/Express). This crate owns the
//! HTTP surface (`/api/*`), SQLite storage under `RUNTIME_DATA_DIR/app.sqlite`,
//! and (coming next) the MCP server and agent-runtime adapters.
//!
//! Path rules come from `AGENTS.md` → "Daemon data directory contract": all
//! daemon-owned data derives from the resolved `OD_DATA_DIR`
//! ([`od_core::RuntimePaths`]).

pub mod agents;
pub mod app_config;
pub mod agent_auth;
pub mod agent_connection;
pub mod auth;
pub mod chat;
pub mod connection_test;
pub mod detect;
pub mod launch;
pub mod critique;
pub mod chat_artifacts;
pub mod config;
pub mod conversations;
pub mod diagnostics;
pub mod egress;
pub mod frontmatter;
pub mod mcp;
pub mod mcp_inject;
pub mod mcp_oauth;
pub mod mcp_tokens;
pub mod media_tasks;
pub mod migrations;
pub mod plain_stream;
pub mod project_create;
pub mod project_dir;
pub mod proxy;
pub mod project_files;
pub mod prompt_telemetry;
pub mod protocol;
pub mod provider_models;
pub mod redact;
pub mod role_marker_guard;
pub mod route_guard;
pub mod routes;
pub mod runs;
pub mod server;
pub mod skill_catalog;
pub mod smoke_sessions;
pub mod spawn;
pub mod static_resources;
pub mod static_spa;
pub mod tool_loop_guard;
pub mod storage;
pub mod telemetry_environment;

pub use config::DaemonConfig;
pub use routes::AppState;
pub use server::{RunningDaemon, StartError};
pub use storage::{ProjectRow, Store, StoreError};
