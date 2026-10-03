//! OpenDesign daemon core in Rust.
//!
//! Parity reference: `apps/daemon` (TypeScript/Express). This crate owns the
//! HTTP surface (`/api/*`), SQLite storage under `RUNTIME_DATA_DIR/app.sqlite`,
//! and (coming next) the MCP server and agent-runtime adapters.
//!
//! Path rules come from `AGENTS.md` → "Daemon data directory contract": all
//! daemon-owned data derives from the resolved `OD_DATA_DIR`
//! ([`od_core::RuntimePaths`]).

pub mod auth;
pub mod config;
pub mod migrations;
pub mod routes;
pub mod server;
pub mod storage;

pub use config::DaemonConfig;
pub use routes::AppState;
pub use server::{RunningDaemon, StartError};
pub use storage::{ProjectRow, Store, StoreError};
