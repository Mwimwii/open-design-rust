//! SQLite storage — parity port of `apps/daemon/src/db.ts` (core subset).
//!
//! Schema DDL and guarded column additions are extracted verbatim from the
//! TypeScript `migrate()` so an existing daemon `app.sqlite` opens cleanly in
//! Rust and a fresh Rust-created database stays drop-in compatible with the
//! TypeScript daemon.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::OptionalExtension;
use serde::Serialize;

use crate::migrations;

/// Shared SQLite handle. `rusqlite::Connection` is `Send` but not `Sync`, so
/// it sits behind a mutex; queries are short-lived (parity: better-sqlite3
/// runs synchronously on one process too).
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

/// A `projects` row, snake_case exactly as the TypeScript daemon returns it.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub skill_id: Option<String>,
    pub design_system_id: Option<String>,
    pub pending_prompt: Option<String>,
    pub metadata_json: Option<String>,
    pub custom_instructions: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

const PROJECT_COLUMNS: &str = "id, name, skill_id, design_system_id, pending_prompt, \
     metadata_json, custom_instructions, created_at, updated_at";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("store lock poisoned")]
    Poisoned,

    #[error("bad migration entry: {0}")]
    Migration(String),
}

impl Store {
    /// Open (creating if needed) `<data root>/app.sqlite`, apply WAL /
    /// foreign-key pragmas, and run migrations — the same sequence as the
    /// TypeScript `openDatabase()`.
    pub fn open(db_file: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = db_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(db_file)?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        migrations::run(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, rusqlite::Connection>, StoreError> {
        self.conn.lock().map_err(|_| StoreError::Poisoned)
    }

    /// The no-scope project catalog: every project no workspace has claimed
    /// (parity: `listUnboundProjects`). Workspace-bound projects must not leak
    /// to a caller with no workspace identity — they are served by the
    /// workspace-scoped route, which is not ported yet.
    pub fn list_unbound_projects(&self) -> Result<Vec<ProjectRow>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {PROJECT_COLUMNS} FROM projects p \
             WHERE NOT EXISTS (SELECT 1 FROM workspace_projects w WHERE w.project_id = p.id) \
             ORDER BY p.updated_at DESC"
        ))?;
        let rows = stmt.query_map([], map_project_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn get_project(&self, id: &str) -> Result<Option<ProjectRow>, StoreError> {
        let conn = self.lock()?;
        let sql = format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE id = ?1");
        let row = conn.query_row(&sql, [id], map_project_row).optional()?;
        Ok(row)
    }

    /// Workspace claiming a project, if any (parity:
    /// `getWorkspaceProjectByProjectId`).
    pub fn workspace_id_for_project(&self, id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT workspace_id FROM workspace_projects WHERE project_id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// Test/ops helper: run a statement outside the route layer.
    pub fn execute<P: rusqlite::Params>(&self, sql: &str, params: P) -> Result<usize, StoreError> {
        let conn = self.lock()?;
        Ok(conn.execute(sql, params)?)
    }
}

fn map_project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectRow> {
    Ok(ProjectRow {
        id: row.get("id")?,
        name: row.get("name")?,
        skill_id: row.get("skill_id")?,
        design_system_id: row.get("design_system_id")?,
        pending_prompt: row.get("pending_prompt")?,
        metadata_json: row.get("metadata_json")?,
        custom_instructions: row.get("custom_instructions")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// Port of `isSafeId` from `apps/daemon/src/projects.ts` — guards every
/// project-id-to-path join.
pub fn is_safe_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 128 {
        return false;
    }
    if id.chars().all(|c| c == '.') {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("od-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("app.sqlite")
    }

    #[test]
    fn open_migrates_and_is_idempotent() {
        let db = temp_db("migrate");
        Store::open(&db).expect("first open");
        Store::open(&db).expect("second open (idempotent)");

        // Guarded ALTERs from the TypeScript migrate() must have landed.
        let conn = rusqlite::Connection::open(&db).unwrap();
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('messages')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for expected in ["run_id", "session_mode", "cancel_origin", "forked_into_json"] {
            assert!(cols.contains(&expected.to_string()), "missing column {expected}");
        }
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn unbound_listing_hides_workspace_claimed_projects() {
        let db = temp_db("unbound");
        let store = Store::open(&db).unwrap();
        let now = 1_700_000_000_000_i64;
        store
            .execute(
                "INSERT INTO projects (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["p-free", "Free project", now, now],
            )
            .unwrap();
        store
            .execute(
                "INSERT INTO projects (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["p-claimed", "Claimed project", now, now],
            )
            .unwrap();
        store
            .execute(
                "INSERT INTO workspace_projects (project_id, workspace_id, visibility, resource_state, created_at, updated_at) \
                 VALUES (?1, 'ws-1', 'personal', 'active', ?2, ?2)",
                rusqlite::params!["p-claimed", now],
            )
            .unwrap();

        let listed = store.list_unbound_projects().unwrap();
        let ids: Vec<&str> = listed.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["p-free"]);

        let claimed = store.get_project("p-claimed").unwrap().expect("row");
        assert_eq!(
            store.workspace_id_for_project("p-claimed").unwrap().as_deref(),
            Some("ws-1")
        );
        assert_eq!(store.workspace_id_for_project("p-free").unwrap(), None);
        assert_eq!(claimed.name, "Claimed project");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn safe_id_rules_match_typescript() {
        assert!(is_safe_id("abc-123_x.y"));
        assert!(!is_safe_id(""));
        assert!(!is_safe_id(".."));
        assert!(!is_safe_id("a/b"));
        assert!(!is_safe_id(&"x".repeat(129)));
    }
}
