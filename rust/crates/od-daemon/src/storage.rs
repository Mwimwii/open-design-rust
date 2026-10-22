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

impl ProjectRow {
    /// The camelCase project shape `apps/web` reads.
    ///
    /// The Rust row mirrors the SQLite columns (snake_case); the TypeScript
    /// daemon's `getProject` mapper hands the web app camelCase keys with
    /// `metadata_json` parsed into `metadata`. Emitting the raw row makes the
    /// UI drop the project's metadata (baseDir, importedFrom, entryFile) and
    /// render every id field as `undefined`.
    pub fn to_api_json(&self) -> serde_json::Value {
        let metadata = self
            .metadata_json
            .as_deref()
            .filter(|raw| !raw.trim().is_empty())
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .unwrap_or(serde_json::Value::Null);
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "skillId": self.skill_id,
            "designSystemId": self.design_system_id,
            "pendingPrompt": self.pending_prompt,
            "metadata": metadata,
            "customInstructions": self.custom_instructions,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        })
    }
}

/// Input for [`Store::insert_project`] (parity: `insertProject`).
pub struct NewProject<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub skill_id: Option<&'a str>,
    pub design_system_id: Option<&'a str>,
    pub pending_prompt: Option<&'a str>,
    pub metadata_json: Option<&'a str>,
    pub custom_instructions: Option<&'a str>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The merged row a project patch writes (parity: `updateProject`'s `merged`).
///
/// The patch is resolved before it reaches the store: the route composes the
/// whole document (metadata included) so the write stays one statement.
#[derive(Debug, Clone)]
pub struct ProjectUpdate {
    pub name: String,
    pub skill_id: Option<String>,
    pub design_system_id: Option<String>,
    pub pending_prompt: Option<String>,
    pub metadata_json: Option<String>,
    pub custom_instructions: Option<String>,
    pub updated_at: i64,
}

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

    /// Insert a project row (parity: `insertProject`).
    pub fn insert_project(&self, project: NewProject<'_>) -> Result<ProjectRow, StoreError> {
        self.execute(
            "INSERT INTO projects
               (id, name, skill_id, design_system_id, pending_prompt,
                metadata_json, custom_instructions, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                project.id,
                project.name,
                project.skill_id,
                project.design_system_id,
                project.pending_prompt,
                project.metadata_json,
                project.custom_instructions,
                project.created_at,
                project.updated_at,
            ],
        )?;
        self.get_project(project.id)?.ok_or(StoreError::Poisoned)
    }

    /// Replace a project's saved tab state (parity: `setTabs`).
    pub fn set_tabs(
        &self,
        project_id: &str,
        tabs: &[String],
        active: Option<&str>,
        now: i64,
    ) -> Result<(), StoreError> {
        let state = serde_json::json!({ "tabs": tabs, "active": active });
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO tabs_state (project_id, updated_at, state_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(project_id) DO UPDATE SET
               updated_at = excluded.updated_at,
               state_json = excluded.state_json",
            rusqlite::params![project_id, now, state.to_string()],
        )?;
        tx.execute("DELETE FROM tabs WHERE project_id = ?1", [project_id])?;
        for (position, name) in tabs.iter().enumerate() {
            tx.execute(
                "INSERT INTO tabs (project_id, name, position, is_active)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    project_id,
                    name,
                    position as i64,
                    i64::from(active == Some(name.as_str())),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Port of `listTabs` (db.ts:5435): the saved `tabs_state` document wins
    /// over the per-row `tabs` table, because a client that saved an active
    /// tab may have written a state the rows cannot represent.
    pub fn list_tabs(&self, project_id: &str) -> Result<serde_json::Value, StoreError> {
        let conn = self.lock()?;
        let mut state_stmt = conn.prepare(
            "SELECT updated_at, state_json FROM tabs_state WHERE project_id = ?1 LIMIT 1",
        )?;
        let state = state_stmt
            .query_row([project_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .optional()?;
        if let Some((updated_at, Some(state_json))) = state.as_ref() {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(state_json) {
                if parsed.get("tabs").and_then(|tabs| tabs.as_array()).is_some() {
                    let mut body = parsed;
                    if let Some(object) = body.as_object_mut() {
                        object.insert("hasSavedState".to_string(), serde_json::json!(true));
                        object.insert("updatedAt".to_string(), serde_json::json!(updated_at));
                    }
                    return Ok(body);
                }
            }
        }
        let mut rows_stmt = conn
            .prepare("SELECT name, is_active FROM tabs WHERE project_id = ?1 ORDER BY position ASC")?;
        let rows = rows_stmt
            .query_map([project_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let tabs: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, _)| serde_json::json!(name))
            .collect();
        let active = rows
            .iter()
            .find(|(_, is_active)| *is_active != 0)
            .map(|(name, _)| serde_json::json!(name));
        let mut body = serde_json::json!({
            "tabs": tabs,
            "active": active,
            "hasSavedState": !rows.is_empty() || state.is_some(),
        });
        if let Some((updated_at, _)) = state.as_ref() {
            body.as_object_mut()
                .expect("object literal")
                .insert("updatedAt".to_string(), serde_json::json!(updated_at));
        }
        Ok(body)
    }

    /// Replace a project's stored metadata (parity: `updateProject` with a
    /// metadata patch).
    ///
    /// `updateProject` merges the patch over the existing row and stamps
    /// `updatedAt` with the current clock; the metadata patch here is
    /// authoritative (the caller composes the whole document), exactly like
    /// the TypeScript route does before it calls the update.
    pub fn update_project_metadata(
        &self,
        id: &str,
        metadata_json: &str,
        now: i64,
    ) -> Result<Option<ProjectRow>, StoreError> {
        {
            let conn = self.lock()?;
            let updated = conn.execute(
                "UPDATE projects SET metadata_json = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![metadata_json, now, id],
            )?;
            if updated == 0 {
                return Ok(None);
            }
        }
        self.get_project(id)
    }

    /// Replace a project's row with the merged values (parity: `updateProject`).
    ///
    /// The patch is merged by the caller (the route composes the whole
    /// document), so this writes the resolved row in one statement.
    pub fn update_project(
        &self,
        id: &str,
        update: &ProjectUpdate,
    ) -> Result<Option<ProjectRow>, StoreError> {
        {
            let conn = self.lock()?;
            let updated = conn.execute(
                "UPDATE projects SET name = ?1, skill_id = ?2, design_system_id = ?3,
                    pending_prompt = ?4, metadata_json = ?5, custom_instructions = ?6,
                    updated_at = ?7
                  WHERE id = ?8",
                rusqlite::params![
                    update.name,
                    update.skill_id,
                    update.design_system_id,
                    update.pending_prompt,
                    update.metadata_json,
                    update.custom_instructions,
                    update.updated_at,
                    id,
                ],
            )?;
            if updated == 0 {
                return Ok(None);
            }
        }
        self.get_project(id)
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

    /// Generic read helper for route layers: map every row of `sql` through
    /// `f` under the same connection lock as [`Self::execute`].
    pub fn query<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<Vec<T>, StoreError>
    where
        P: rusqlite::Params,
        F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params, f)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Single-row variant of [`Self::query`]; `None` when no row matches.
    pub fn query_one<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<Option<T>, StoreError>
    where
        P: rusqlite::Params,
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        let conn = self.lock()?;
        Ok(conn.query_row(sql, params, f).optional()?)
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
