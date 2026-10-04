# Rust rewrite

Rust port of the OpenDesign desktop stack: a **Tauri v2** shell plus an
**axum + SQLite** daemon, targeting behavioral parity with the TypeScript
implementation in `apps/daemon/` and `apps/web/`.

## Layout

| Path | Role |
| --- | --- |
| `crates/od-core` | Shared foundation: runtime path resolution (`paths.rs`), error types. |
| `crates/od-daemon` | The daemon: axum router, auth, SQLite storage, migrations, and route modules ported from TS. |
| `apps/od-tauri` | Tauri v2 desktop shell embedding `od-daemon`; React (Vite) UI in `ui/`, Rust glue in `src-tauri/`. |

Workspace members are `crates/*` and `apps/*/src-tauri` (see `rust/Cargo.toml`).

## Verify

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cd rust
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

UI (from `rust/apps/od-tauri`, Node on PATH):

```sh
npm run typecheck
npm run build:ui
```

The Tauri shell serves the built UI from the embedded daemon (`web_dist`);
the daemon itself binds per its config (default port `7456`) and honors the
data-directory contract below.

## Data directory

`OD_DATA_DIR` is the single data-root truth source. This README deliberately
does not restate path rules — read **"Daemon data directory contract"** in the
repository-root `AGENTS.md` and follow it.

## Parity rules

- Route modules carry doc comments referencing their TS source
  (`file:line`); response field names match TS (camelCase).
- Errors use the JSON envelope `{"error": {"code", "message"}}`.
- Every route registers in `routes.rs` **before** the `/api/{*rest}`
  catch-all.
- Filesystem access goes through the containment helpers in
  `project_dir.rs`; IDs validate with `isSafeId` (`[A-Za-z0-9._-]`,
  length 1–128, not all dots).
- Known/accepted deviations (e.g. the workspace-authority lane
  `authorizeProjectRequest`) are documented in each module's header comment —
  extend those docs when skipping behavior, never silently.

## Port status

Tracked with beads (`bd list`); the epic is `open-design-rust-59s`.

| Area | State |
| --- | --- |
| Workspace scaffold, daemon core, auth, storage, SPA serving | done |
| Tauri v2 shell + React UI, Xvfb smoke test | done |
| Project file routes, project conversations | done |
| Sandbox external-root allowlist, legacy migrate helpers | done |
| Static resources + chat artifacts routes | done |
| Runs engine (create/list/events/SSE/cancel/steer/result-package) | done |
| MCP routes (`apps/daemon/src/mcp.ts`) | open — `open-design-rust-r2r` |
| Chat + BYOK routes | open — `open-design-rust-us2` |

To port a surface: read the TS source, mirror routes with parity, add tests
under `crates/od-daemon/tests/`, verify the three cargo commands above, then
commit (atomic, verified states only) and close the bead with the commit hash.
