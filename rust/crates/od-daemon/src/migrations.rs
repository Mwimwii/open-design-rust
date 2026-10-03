//! Schema migrations extracted from the TypeScript daemon's `migrate()`
//! (`apps/daemon/src/db.ts`).
//!
//! Three layers, run in the order TypeScript runs them:
//!
//! * `0001_core.sql` — the verbatim `CREATE TABLE IF NOT EXISTS` / `CREATE
//!   INDEX IF NOT EXISTS` block (17 tables, 10 indexes).
//! * `ensure_columns.tsv` — the guarded `ALTER TABLE … ADD COLUMN` entries.
//!   SQLite has no `ALTER TABLE … IF NOT EXISTS`, so each entry is applied
//!   only when its table exists and the column is missing — exactly the
//!   `pragma_table_info` guard the TypeScript code uses.
//! * the legacy table-rebuild / backfill helpers, interleaved *between*
//!   ensure-column phases rather than after them all.
//!
//! The interleaving is load-bearing. TypeScript adds `anchor_state` … after
//! `migratePreviewCommentsSlideKey` rebuilt `preview_comments`, and `pin_seq`
//! … after `migratePreviewCommentsAllowMultiplePerElement` rebuilt it again;
//! both rebuilds copy an explicit column list into a fresh table. Applying
//! every ensure-column entry before the helpers would drop those columns (and
//! their data) the moment a legacy database hit a rebuild, so the phase
//! markers below are the exact points TypeScript splits on.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};

use crate::storage::StoreError;

const CORE_SQL: &str = include_str!("../migrations/0001_core.sql");
const ENSURE_COLUMNS: &str = include_str!("../migrations/ensure_columns.tsv");

/// Run all migrations. Safe to call on every startup (idempotent).
///
/// Phase order (mirrors `migrate()` in `apps/daemon/src/db.ts`):
///
/// 1. core schema;
/// 2. ensure columns up to `workspace_projects.metadata_refresh_pending`;
/// 3. [`migrate_workspace_projects_single_home`];
/// 4. ensure columns up to `preview_comments.anchor_state`;
/// 5. [`migrate_preview_comments_slide_key`];
/// 6. ensure columns up to `preview_comments.pin_seq`;
/// 7. [`migrate_preview_comments_allow_multiple_per_element`];
/// 8. ensure columns up to `deployments.status`;
/// 9. [`backfill_preview_comment_pin_seq_and_sort_key`];
/// 10. the remaining ensure columns;
/// 11. the subsystem helpers, in TypeScript's order.
pub fn run(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(CORE_SQL)?;
    let entries = parse_ensure_entries(ENSURE_COLUMNS)?;

    let (phase, rest) =
        split_at_entry(&entries, "workspace_projects", "metadata_refresh_pending")?;
    ensure_columns(conn, phase)?;
    migrate_workspace_projects_single_home(conn)?;

    let (phase, rest) = split_at_entry(rest, "preview_comments", "anchor_state")?;
    ensure_columns(conn, phase)?;
    migrate_preview_comments_slide_key(conn)?;

    let (phase, rest) = split_at_entry(rest, "preview_comments", "pin_seq")?;
    ensure_columns(conn, phase)?;
    migrate_preview_comments_allow_multiple_per_element(conn)?;

    let (phase, rest) = split_at_entry(rest, "deployments", "status")?;
    ensure_columns(conn, phase)?;
    backfill_preview_comment_pin_seq_and_sort_key(conn)?;

    ensure_columns(conn, rest)?;

    migrate_critique(conn)?;
    migrate_media_tasks(conn)?;
    migrate_library(conn)?;
    migrate_plugins(conn)?;
    migrate_project_scenario_bindings(conn)?;
    migrate_strategy_task_store(conn)?;
    migrate_chat_artifacts(conn)?;
    migrate_collab_sync_snapshots(conn)?;
    migrate_comment_relay_outbox(conn)?;
    migrate_amr_terminal_report_outbox(conn)?;
    migrate_public_file_publications(conn)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ensure-columns machinery
// ---------------------------------------------------------------------------

struct EnsureEntry {
    table: String,
    column: String,
    sql: String,
}

fn parse_ensure_entries(raw: &str) -> Result<Vec<EnsureEntry>, StoreError> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut parts = line.splitn(3, '\t');
            let (Some(table), Some(column), Some(sql)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return Err(StoreError::Migration(line.to_string()));
            };
            Ok(EnsureEntry {
                table: table.to_string(),
                column: column.to_string(),
                sql: sql.to_string(),
            })
        })
        .collect()
}

/// Split `entries` so the head ends just before the `(table, column)` marker
/// and the tail starts at it. A missing or reordered marker is a hard error:
/// mis-sequencing a rebuild would silently drop columns.
fn split_at_entry<'a>(
    entries: &'a [EnsureEntry],
    table: &str,
    column: &str,
) -> Result<(&'a [EnsureEntry], &'a [EnsureEntry]), StoreError> {
    let idx = entries
        .iter()
        .position(|entry| entry.table == table && entry.column == column)
        .ok_or_else(|| StoreError::Migration(format!("missing phase marker {table}.{column}")))?;
    Ok((&entries[..idx], &entries[idx..]))
}

fn ensure_columns(conn: &Connection, entries: &[EnsureEntry]) -> Result<(), StoreError> {
    for entry in entries {
        if table_exists(conn, &entry.table)? && !column_exists(conn, &entry.table, &entry.column)? {
            conn.execute_batch(&entry.sql)?;
        }
    }
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, StoreError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        params![table, column],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn table_columns(conn: &Connection, table: &str) -> Result<HashSet<String>, StoreError> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let names = stmt
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names.into_iter().collect())
}

/// `PRAGMA table_info(...).pk` for one column: 1 for the first column of a
/// composite key, 0 for ordinary or absent columns (parity: `?.pk ?? 0`).
fn column_pk(conn: &Connection, table: &str, column: &str) -> Result<i64, StoreError> {
    let pk = conn
        .query_row(
            "SELECT pk FROM pragma_table_info(?1) WHERE name = ?2",
            params![table, column],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(pk.unwrap_or(0))
}

// ---------------------------------------------------------------------------
// migrateWorkspaceProjectsSingleHome
// ---------------------------------------------------------------------------

/// One `workspace_projects` row, as far as the single-home invariant is
/// concerned (parity: `WorkspaceProjectHomeRow`).
struct HomeRow {
    project_id: String,
    workspace_id: String,
    visibility: Option<String>,
    created_by_workspace_member_id: Option<String>,
    created_at: Option<i64>,
}

impl HomeRow {
    /// A row that records no act: `visibility = 'personal'` with no creator —
    /// exactly the shape the old blanket back-fill wrote (parity:
    /// `isBackfilledWorkspaceProjectRow`).
    fn is_backfilled(&self) -> bool {
        self.visibility.as_deref() == Some("personal")
            && self.created_by_workspace_member_id.is_none()
    }

    /// How strongly a row asserts the project lives in its workspace:
    /// 2 = team share, 1 = a recorded act, 0 = an ownerless guess.
    fn evidence(&self) -> i64 {
        if self.visibility.as_deref() == Some("team") {
            2
        } else if self.is_backfilled() {
            0
        } else {
            1
        }
    }
}

/// Collapse every project's rows to the one workspace it belongs to, then
/// narrow the primary key back to `project_id` — parity for
/// `migrateWorkspaceProjectsSingleHome` + `collapseWorkspaceProjectHomes`.
fn migrate_workspace_projects_single_home(conn: &Connection) -> Result<(), StoreError> {
    let dropped = collapse_workspace_project_homes(conn)?;
    if dropped > 0 {
        tracing::warn!(
            "[od] bound {dropped} duplicated workspace project row(s) to a single workspace \
             each. A project belongs to one workspace; the extras came from an older blanket \
             back-fill."
        );
    }

    let project_pk = column_pk(conn, "workspace_projects", "project_id")?;
    let workspace_pk = column_pk(conn, "workspace_projects", "workspace_id")?;
    if project_pk == 1 && workspace_pk == 0 {
        return Ok(());
    }
    conn.execute_batch(WORKSPACE_PROJECTS_REBUILD)?;
    Ok(())
}

fn collapse_workspace_project_homes(conn: &Connection) -> Result<u64, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT project_id, workspace_id, visibility, \
                created_by_workspace_member_id, created_at \
           FROM workspace_projects",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(HomeRow {
                project_id: row.get(0)?,
                workspace_id: row.get(1)?,
                visibility: row.get(2)?,
                created_by_workspace_member_id: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let doomed = workspace_home_rows_to_drop(&rows);
    if doomed.is_empty() {
        return Ok(0);
    }
    let tx = conn.unchecked_transaction()?;
    for idx in &doomed {
        let row = &rows[*idx];
        tx.execute(
            "DELETE FROM workspace_projects WHERE workspace_id = ?1 AND project_id = ?2",
            params![row.workspace_id, row.project_id],
        )?;
    }
    tx.commit()?;
    Ok(doomed.len() as u64)
}

/// Row indexes to delete: per project, the rows that lost the evidence
/// contest — or every row when the winner would render nowhere (an ownerless
/// personal row inside a team workspace).
fn workspace_home_rows_to_drop(rows: &[HomeRow]) -> Vec<usize> {
    let team_workspaces: HashSet<&str> = rows
        .iter()
        .filter(|row| row.visibility.as_deref() == Some("team"))
        .map(|row| row.workspace_id.as_str())
        .collect();

    let mut activity: HashMap<&str, i64> = HashMap::new();
    for row in rows {
        if row.evidence() != 0 {
            *activity.entry(row.workspace_id.as_str()).or_default() += 1;
        }
    }

    let mut project_order: Vec<&str> = Vec::new();
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for (idx, row) in rows.iter().enumerate() {
        let project_id = row.project_id.as_str();
        let group = groups.entry(project_id).or_insert_with(|| {
            project_order.push(project_id);
            Vec::new()
        });
        group.push(idx);
    }

    let compare = |a: &HomeRow, b: &HomeRow| -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let by_evidence = b.evidence().cmp(&a.evidence());
        if by_evidence != Ordering::Equal {
            return by_evidence;
        }
        let a_suppressed = a.evidence() == 0 && team_workspaces.contains(a.workspace_id.as_str());
        let b_suppressed = b.evidence() == 0 && team_workspaces.contains(b.workspace_id.as_str());
        if a_suppressed != b_suppressed {
            return if a_suppressed {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        let by_activity = activity
            .get(b.workspace_id.as_str())
            .copied()
            .unwrap_or(0)
            .cmp(&activity.get(a.workspace_id.as_str()).copied().unwrap_or(0));
        if by_activity != Ordering::Equal {
            return by_activity;
        }
        let by_created_at = a.created_at.unwrap_or(0).cmp(&b.created_at.unwrap_or(0));
        if by_created_at != Ordering::Equal {
            return by_created_at;
        }
        a.workspace_id.cmp(&b.workspace_id)
    };

    let mut doomed = Vec::new();
    for project_id in &project_order {
        let group = &groups[project_id];
        let mut ranked = group.clone();
        ranked.sort_by(|&a, &b| compare(&rows[a], &rows[b]));
        let winner = ranked[0];
        let winner_renders_nowhere = rows[winner].evidence() == 0
            && team_workspaces.contains(rows[winner].workspace_id.as_str());
        if winner_renders_nowhere {
            doomed.extend(group.iter().copied());
        } else {
            doomed.extend(group.iter().copied().filter(|&idx| idx != winner));
        }
    }
    doomed
}

// ---------------------------------------------------------------------------
// migratePreviewCommentsSlideKey
// ---------------------------------------------------------------------------

/// Rebuild `preview_comments` with `slide_key`, dropping the legacy unique key
/// on (project, conversation, file, element). TypeScript runs when
/// `!(hasSlideKey && !hasLegacyUnique)`, i.e. when `slide_key` is missing or
/// the legacy four-column unique is still present.
fn migrate_preview_comments_slide_key(conn: &Connection) -> Result<(), StoreError> {
    let table_sql = preview_comments_table_sql(conn)?;
    if contains_word(&table_sql, "slide_key") && !matches_legacy_slide_unique(&table_sql) {
        return Ok(());
    }
    conn.execute_batch(PREVIEW_COMMENTS_SLIDE_REBUILD)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// migratePreviewCommentsAllowMultiplePerElement
// ---------------------------------------------------------------------------

/// Rebuild `preview_comments` so comments are keyed by `id` only: a UNIQUE
/// clause spanning `project_id` … `element_id` has to go, otherwise a second
/// note on the same element is rejected.
fn migrate_preview_comments_allow_multiple_per_element(conn: &Connection) -> Result<(), StoreError> {
    let table_sql = preview_comments_table_sql(conn)?;
    if !matches_natural_unique(&table_sql) {
        return Ok(());
    }
    conn.execute_batch(PREVIEW_COMMENTS_MULTI_REBUILD)?;
    Ok(())
}

fn preview_comments_table_sql(conn: &Connection) -> Result<String, StoreError> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'preview_comments'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(sql.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// backfillPreviewCommentPinSeqAndSortKey
// ---------------------------------------------------------------------------

/// Assign `pin_seq` / `sort_key` to rows written before those columns existed,
/// ordered exactly like the pre-existing canvas numbering (`created_at ASC,
/// rowid ASC`). Each (project, file) scope's counter is seeded from the
/// highest `pin_seq` already assigned there, so a partial prior run cannot
/// renumber rows that already have a real pin_seq.
fn backfill_preview_comment_pin_seq_and_sort_key(conn: &Connection) -> Result<(), StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, file_path, created_at \
           FROM preview_comments \
          WHERE pin_seq IS NULL \
          ORDER BY project_id ASC, file_path ASC, created_at ASC, rowid ASC",
    )?;
    let pending = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if pending.is_empty() {
        return Ok(());
    }

    let mut stmt = conn.prepare(
        "SELECT project_id, file_path, MAX(pin_seq) \
           FROM preview_comments \
          WHERE pin_seq IS NOT NULL \
          GROUP BY project_id, file_path",
    )?;
    let mut next_pin_seq_by_scope: HashMap<(String, String), i64> = HashMap::new();
    let assigned = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (project_id, file_path, max_seq) in assigned {
        next_pin_seq_by_scope.insert((project_id, file_path), max_seq);
    }

    let tx = conn.unchecked_transaction()?;
    for (id, project_id, file_path, created_at) in pending {
        let scope = (project_id, file_path);
        let next_seq = next_pin_seq_by_scope.get(&scope).copied().unwrap_or(0) + 1;
        next_pin_seq_by_scope.insert(scope, next_seq);
        tx.execute(
            "UPDATE preview_comments SET pin_seq = ?1 WHERE id = ?2",
            params![next_seq, id],
        )?;
        tx.execute(
            "UPDATE preview_comments SET sort_key = ?1 WHERE id = ?2 AND sort_key IS NULL",
            params![created_at, id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// subsystem helpers (parity: the migrate*() calls inside db.ts `migrate()`)
// ---------------------------------------------------------------------------

fn migrate_critique(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(CRITIQUE_DDL)?;
    Ok(())
}

fn migrate_media_tasks(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(MEDIA_TASKS_DDL)?;
    if !column_exists(conn, "media_tasks", "run_id")? {
        conn.execute_batch("ALTER TABLE media_tasks ADD COLUMN run_id TEXT")?;
    }
    Ok(())
}

fn migrate_library(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(LIBRARY_DDL)?;
    Ok(())
}

fn migrate_plugins(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(PLUGINS_DDL)?;

    let marketplace_cols = table_columns(conn, "plugin_marketplaces")?;
    if !marketplace_cols.contains("spec_version") {
        conn.execute_batch(
            "ALTER TABLE plugin_marketplaces ADD COLUMN spec_version TEXT NOT NULL DEFAULT '1.0.0'",
        )?;
    }
    if !marketplace_cols.contains("version") {
        conn.execute_batch(
            "ALTER TABLE plugin_marketplaces ADD COLUMN version TEXT NOT NULL DEFAULT '0.0.0'",
        )?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_marketplaces_version ON plugin_marketplaces(version)",
    )?;

    let installed_cols = table_columns(conn, "installed_plugins")?;
    for (name, ddl) in INSTALLED_PLUGIN_COLUMN_ALTERS {
        if !installed_cols.contains(name) {
            conn.execute_batch(ddl)?;
        }
    }

    let snapshot_cols = table_columns(conn, "applied_plugin_snapshots")?;
    if !snapshot_cols.contains("plugin_spec_version") {
        conn.execute_batch(
            "ALTER TABLE applied_plugin_snapshots ADD COLUMN plugin_spec_version TEXT NOT NULL DEFAULT '1.0.0'",
        )?;
    }
    for (name, ddl) in SNAPSHOT_COLUMN_ALTERS {
        if !snapshot_cols.contains(name) {
            conn.execute_batch(ddl)?;
        }
    }

    if !column_exists(conn, "projects", "applied_plugin_snapshot_id")? {
        conn.execute_batch("ALTER TABLE projects ADD COLUMN applied_plugin_snapshot_id TEXT")?;
    }
    if !column_exists(conn, "conversations", "applied_plugin_snapshot_id")? {
        conn.execute_batch("ALTER TABLE conversations ADD COLUMN applied_plugin_snapshot_id TEXT")?;
    }
    Ok(())
}

/// One-way migration for projects created before exact provenance existed:
/// fold the retired `automaticDefaultScenario` marker into an explicit
/// `scenarioBinding` labelled `legacy_unknown`, or drop the binding when the
/// project no longer points at a snapshot.
fn migrate_project_scenario_bindings(conn: &Connection) -> Result<(), StoreError> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.metadata_json, p.applied_plugin_snapshot_id, s.plugin_id, s.applied_at \
           FROM projects p \
           LEFT JOIN applied_plugin_snapshots s ON s.id = p.applied_plugin_snapshot_id",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let tx = conn.unchecked_transaction()?;
    for (project_id, metadata_json, snapshot_id, plugin_id, applied_at) in rows {
        let metadata = parse_metadata(metadata_json.as_deref());
        let current = metadata
            .as_ref()
            .and_then(|object| object.get("scenarioBinding"));
        let has_exact_current = is_project_scenario_binding(current)
            && binding_field_matches(current, "snapshotId", snapshot_id.as_deref())
            && binding_field_matches(current, "pluginId", plugin_id.as_deref());
        let had_retired_marker = metadata
            .as_ref()
            .is_some_and(|object| object.contains_key("automaticDefaultScenario"));
        if has_exact_current && !had_retired_marker {
            continue;
        }

        let mut next = metadata.clone().unwrap_or_default();
        next.remove("automaticDefaultScenario");
        if snapshot_id.is_some() && plugin_id.is_some() {
            let mut binding = Map::new();
            binding.insert("schemaVersion".to_string(), Value::from(1));
            binding.insert("provenance".to_string(), Value::from("legacy_unknown"));
            binding.insert(
                "pluginId".to_string(),
                Value::from(plugin_id.clone().unwrap_or_default()),
            );
            binding.insert(
                "snapshotId".to_string(),
                Value::from(snapshot_id.clone().unwrap_or_default()),
            );
            binding.insert("boundAt".to_string(), Value::from(applied_at.unwrap_or(0)));
            next.insert("scenarioBinding".to_string(), Value::Object(binding));
        } else {
            next.remove("scenarioBinding");
        }
        let metadata_out = if next.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&Value::Object(next)).map_err(|err| {
                StoreError::Migration(format!("project {project_id} metadata_json: {err}"))
            })?)
        };
        tx.execute(
            "UPDATE projects SET metadata_json = ?1 WHERE id = ?2",
            params![metadata_out, project_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn parse_metadata(raw: Option<&str>) -> Option<Map<String, Value>> {
    let raw = raw?;
    match serde_json::from_str::<Value>(raw).ok()? {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

/// Parity for `isProjectScenarioBinding`.
fn is_project_scenario_binding(value: Option<&Value>) -> bool {
    let Some(Value::Object(binding)) = value else {
        return false;
    };
    let schema_version_ok = binding
        .get("schemaVersion")
        .and_then(Value::as_f64)
        .is_some_and(|version| version == 1.0);
    let provenance_ok = binding
        .get("provenance")
        .and_then(Value::as_str)
        .is_some_and(|provenance| {
            matches!(
                provenance,
                "automatic_default" | "explicit_user" | "legacy_unknown"
            )
        });
    let plugin_id_ok = binding
        .get("pluginId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty());
    let snapshot_id_ok = binding
        .get("snapshotId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty());
    let bound_at_ok = binding
        .get("boundAt")
        .and_then(Value::as_f64)
        .is_some_and(f64::is_finite);
    let task_profile_ok = match binding.get("taskProfile") {
        None => true,
        Some(Value::String(profile)) => matches!(
            profile.as_str(),
            "prototype" | "ppt" | "marketing" | "hyperframes"
        ),
        Some(_) => false,
    };
    schema_version_ok
        && provenance_ok
        && plugin_id_ok
        && snapshot_id_ok
        && bound_at_ok
        && task_profile_ok
}

/// `binding.<field> === row.<field>` where the row side is a SQL NULL read as
/// JS `null`: `string === null` is false, so a NULL row column never matches.
fn binding_field_matches(binding: Option<&Value>, field: &str, row: Option<&str>) -> bool {
    let Some(row) = row else {
        return false;
    };
    binding
        .and_then(|value| value.get(field))
        .and_then(Value::as_str)
        == Some(row)
}

fn migrate_strategy_task_store(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(STRATEGY_TASKS_DDL)?;
    add_column_if_missing(
        conn,
        "strategy_task_executions",
        "execution_intent TEXT NOT NULL DEFAULT 'produce'",
    )?;
    add_column_if_missing(
        conn,
        "strategy_task_executions",
        "intent_resolution_version INTEGER",
    )?;
    migrate_intent_resolution_store(conn)?;
    for definition in STRATEGY_EXECUTION_COLUMN_ALTERS {
        add_column_if_missing(conn, "strategy_task_executions", definition)?;
    }
    for definition in STRATEGY_RUN_COLUMN_ALTERS {
        add_column_if_missing(conn, "strategy_task_runs", definition)?;
    }
    migrate_frozen_skill_package_store(conn)?;
    Ok(())
}

/// Parity for `addColumnIfMissing`: the column name is the first
/// whitespace-token of the ALTER definition.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    definition: &str,
) -> Result<(), StoreError> {
    let Some(column) = definition.split_whitespace().next() else {
        return Ok(());
    };
    if column_exists(conn, table, column)? {
        return Ok(());
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {definition}"))?;
    Ok(())
}

fn migrate_intent_resolution_store(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(STRATEGY_INTENT_RESOLUTION_DDL)?;
    Ok(())
}

fn migrate_frozen_skill_package_store(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(FROZEN_SKILL_PACKAGE_DDL)?;
    Ok(())
}

fn migrate_chat_artifacts(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(CHAT_ARTIFACTS_DDL)?;
    drop_legacy_open_policy_column(conn)?;
    Ok(())
}

/// Rebuild `message_artifacts` without `open_policy`: the column was `NOT
/// NULL` with a CHECK constraint and SQLite refuses `DROP COLUMN` on a column
/// a CHECK mentions, so the rows are copied across instead.
fn drop_legacy_open_policy_column(conn: &Connection) -> Result<(), StoreError> {
    if !column_exists(conn, "message_artifacts", "open_policy")? {
        return Ok(());
    }
    conn.execute_batch(MESSAGE_ARTIFACTS_REBUILD)?;
    Ok(())
}

fn migrate_collab_sync_snapshots(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(COLLAB_SYNC_SNAPSHOTS_DDL)?;
    Ok(())
}

fn migrate_comment_relay_outbox(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(COMMENT_RELAY_OUTBOX_DDL)?;
    Ok(())
}

/// Upgrade both fresh databases and the three-column table shipped by #7392:
/// add the outbox columns, then backfill `terminal_at_iso` plus the zeroed
/// timestamps for rows written before them.
fn migrate_amr_terminal_report_outbox(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(AMR_TERMINAL_REPORT_OUTBOX_DDL)?;
    for (name, ddl) in AMR_COLUMN_ALTERS {
        if !column_exists(conn, "amr_terminal_report_outbox", name)? {
            conn.execute_batch(ddl)?;
        }
    }
    let mut stmt = conn.prepare(
        "SELECT run_id, terminal_at \
           FROM amr_terminal_report_outbox \
          WHERE terminal_at_iso IS NULL OR terminal_at_iso = ''",
    )?;
    let pending = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if !pending.is_empty() {
        let tx = conn.unchecked_transaction()?;
        for (run_id, terminal_at) in pending {
            tx.execute(
                "UPDATE amr_terminal_report_outbox \
                    SET terminal_at_iso = ?1, \
                        next_attempt_at = CASE WHEN next_attempt_at = 0 THEN terminal_at ELSE next_attempt_at END, \
                        created_at = CASE WHEN created_at = 0 THEN terminal_at ELSE created_at END, \
                        updated_at = CASE WHEN updated_at = 0 THEN terminal_at ELSE updated_at END \
                  WHERE run_id = ?2",
                params![to_iso8601_utc(terminal_at), run_id],
            )?;
        }
        tx.commit()?;
    }
    conn.execute_batch(AMR_TERMINAL_REPORT_OUTBOX_INDEXES)?;
    Ok(())
}

fn migrate_public_file_publications(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(PUBLIC_FILE_PUBLICATIONS_DDL)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// legacy-table-shape predicates (parity for the TypeScript regexes)
// ---------------------------------------------------------------------------

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Case-insensitive `\bword\b` search over ASCII (parity for `/…/i` around
/// ASCII SQL identifiers; JavaScript `\b` is ASCII-defined too).
fn find_word(haystack: &str, word: &str, from: usize) -> Option<usize> {
    let hay = haystack.as_bytes();
    let needle = word.as_bytes();
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    let mut idx = from;
    while idx + needle.len() <= hay.len() {
        if hay[idx..idx + needle.len()].eq_ignore_ascii_case(needle) {
            let boundary_before = idx == 0 || !is_word_byte(hay[idx - 1]);
            let end = idx + needle.len();
            let boundary_after = end == hay.len() || !is_word_byte(hay[end]);
            if boundary_before && boundary_after {
                return Some(idx);
            }
        }
        idx += 1;
    }
    None
}

fn contains_word(haystack: &str, word: &str) -> bool {
    find_word(haystack, word, 0).is_some()
}

fn find_ascii_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() || needle.len() > hay.len() - from {
        return None;
    }
    (from..=hay.len() - needle.len())
        .find(|&idx| hay[idx..idx + needle.len()].eq_ignore_ascii_case(needle))
}

fn is_sql_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn skip_space(bytes: &[u8], mut idx: usize) -> usize {
    while idx < bytes.len() && is_sql_space(bytes[idx]) {
        idx += 1;
    }
    idx
}

/// `/UNIQUE\s*\(\s*project_id\s*,\s*conversation_id\s*,\s*file_path\s*,\s*element_id\s*\)/i`
fn matches_legacy_slide_unique(sql: &str) -> bool {
    const COLUMNS: [&[u8]; 4] = [
        b"project_id",
        b"conversation_id",
        b"file_path",
        b"element_id",
    ];
    let bytes = sql.as_bytes();
    let mut search = 0;
    while let Some(start) = find_ascii_ci(bytes, b"UNIQUE", search) {
        search = start + 1;
        let mut idx = skip_space(bytes, start + b"UNIQUE".len());
        if bytes.get(idx) != Some(&b'(') {
            continue;
        }
        idx += 1;
        for (position, column) in COLUMNS.iter().enumerate() {
            idx = skip_space(bytes, idx);
            let matches_column = bytes
                .get(idx..)
                .and_then(|rest| rest.get(..column.len()))
                .is_some_and(|window| window.eq_ignore_ascii_case(column));
            if !matches_column {
                break;
            }
            idx += column.len();
            idx = skip_space(bytes, idx);
            if position + 1 < COLUMNS.len() {
                if bytes.get(idx) != Some(&b',') {
                    break;
                }
                idx += 1;
            } else if bytes.get(idx) == Some(&b')') {
                return true;
            }
        }
    }
    false
}

/// `/UNIQUE\s*\([^)]*\bproject_id\b[^)]*\belement_id\b[^)]*\)/i`
fn matches_natural_unique(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut search = 0;
    while let Some(start) = find_ascii_ci(bytes, b"UNIQUE", search) {
        search = start + 1;
        let open = skip_space(bytes, start + b"UNIQUE".len());
        if bytes.get(open) != Some(&b'(') {
            continue;
        }
        let content_start = open + 1;
        let mut end = content_start;
        while end < bytes.len() && bytes[end] != b')' {
            end += 1;
        }
        if end >= bytes.len() {
            continue;
        }
        // Safe: `content_start` and `end` index the ASCII '(' / ')' bytes.
        let content = &sql[content_start..end];
        if let Some(project) = find_word(content, "project_id", 0) {
            if find_word(content, "element_id", project + "project_id".len()).is_some() {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// ISO-8601 UTC timestamps (parity for `new Date(ms).toISOString()`)
// ---------------------------------------------------------------------------

/// `new Date(milliseconds).toISOString()`, including the extended `±YYYYYY`
/// form JavaScript uses for years outside 0000–9999.
fn to_iso8601_utc(ms: i64) -> String {
    const MS_PER_DAY: i64 = 86_400_000;
    let days = ms.div_euclid(MS_PER_DAY);
    let ms_of_day = ms.rem_euclid(MS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = ms_of_day / 3_600_000;
    let minute = (ms_of_day % 3_600_000) / 60_000;
    let second = (ms_of_day % 60_000) / 1_000;
    let millisecond = ms_of_day % 1_000;
    let date = if (0..=9999).contains(&year) {
        format!("{year:04}-{month:02}-{day:02}")
    } else if year >= 0 {
        format!("+{year:06}")
    } else {
        format!("-{:06}", -year)
    };
    format!("{date}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}Z")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → (y, m, d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let year = i64::try_from(year_of_era).unwrap_or(0) + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    let adjust = i64::from(u32::from(month <= 2));
    (year + adjust, month, day)
}

// ---------------------------------------------------------------------------
// verbatim SQL from the TypeScript helpers
// ---------------------------------------------------------------------------

/// `migrateWorkspaceProjectsSingleHome` rebuild (verbatim from db.ts): rename
/// the composite-key table aside, copy the rows into the narrowed shape, drop
/// it, and restore the index.
const WORKSPACE_PROJECTS_REBUILD: &str = r#"
    DROP INDEX IF EXISTS idx_workspace_projects_workspace_visibility;
    ALTER TABLE workspace_projects RENAME TO workspace_projects_legacy_multi_workspace;
    CREATE TABLE workspace_projects (
      project_id TEXT PRIMARY KEY,
      workspace_id TEXT NOT NULL,
      visibility TEXT NOT NULL CHECK (visibility IN ('personal', 'team')),
      resource_state TEXT NOT NULL CHECK (resource_state IN ('active', 'frozen', 'deleted')),
      created_by_workspace_member_id TEXT,
      updated_by_workspace_member_id TEXT,
      resource_hub_resource_id TEXT,
      cloud_tombstoned_at INTEGER,
      sync_state TEXT,
      metadata_refresh_pending INTEGER NOT NULL DEFAULT 0,
      version INTEGER NOT NULL DEFAULT 1,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
    );
    INSERT INTO workspace_projects
      (project_id, workspace_id, visibility, resource_state,
       created_by_workspace_member_id, updated_by_workspace_member_id,
       resource_hub_resource_id, cloud_tombstoned_at,
       sync_state, version, created_at, updated_at)
    SELECT project_id, workspace_id, visibility, resource_state,
           created_by_workspace_member_id, updated_by_workspace_member_id,
           resource_hub_resource_id, cloud_tombstoned_at,
           sync_state, version, created_at, updated_at
      FROM workspace_projects_legacy_multi_workspace;
    DROP TABLE workspace_projects_legacy_multi_workspace;
    CREATE INDEX IF NOT EXISTS idx_workspace_projects_workspace_visibility
      ON workspace_projects(workspace_id, visibility, updated_at DESC);
"#;

/// `migratePreviewCommentsSlideKey` rebuild (verbatim from db.ts).
const PREVIEW_COMMENTS_SLIDE_REBUILD: &str = r#"
    CREATE TABLE preview_comments_next (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      conversation_id TEXT NOT NULL,
      file_path TEXT NOT NULL,
      element_id TEXT NOT NULL,
      selector TEXT NOT NULL,
      label TEXT NOT NULL,
      text TEXT NOT NULL,
      position_json TEXT NOT NULL,
      html_hint TEXT NOT NULL,
      selection_kind TEXT,
      member_count INTEGER,
      pod_members_json TEXT,
      style_json TEXT,
      attachments_json TEXT,
      slide_index INTEGER,
      slide_key INTEGER NOT NULL DEFAULT -1,
      note TEXT NOT NULL,
      status TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE,
      FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
    );

    INSERT INTO preview_comments_next
      (id, project_id, conversation_id, file_path, element_id, selector, label,
       text, position_json, html_hint, selection_kind, member_count, pod_members_json,
       style_json, attachments_json, slide_index, slide_key, note, status, created_at, updated_at)
    SELECT id, project_id, conversation_id, file_path, element_id, selector, label,
       text, position_json, html_hint, selection_kind, member_count, pod_members_json,
       style_json, attachments_json, slide_index, COALESCE(slide_index, -1), note, status, created_at, updated_at
      FROM preview_comments;

    DROP TABLE preview_comments;
    ALTER TABLE preview_comments_next RENAME TO preview_comments;
    CREATE INDEX IF NOT EXISTS idx_preview_comments_conversation
      ON preview_comments(project_id, conversation_id, updated_at DESC);
"#;

/// `migratePreviewCommentsAllowMultiplePerElement` rebuild (verbatim from db.ts).
const PREVIEW_COMMENTS_MULTI_REBUILD: &str = r#"
    CREATE TABLE preview_comments_multi_next (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      conversation_id TEXT NOT NULL,
      file_path TEXT NOT NULL,
      element_id TEXT NOT NULL,
      selector TEXT NOT NULL,
      label TEXT NOT NULL,
      text TEXT NOT NULL,
      position_json TEXT NOT NULL,
      html_hint TEXT NOT NULL,
      selection_kind TEXT,
      member_count INTEGER,
      pod_members_json TEXT,
      style_json TEXT,
      attachments_json TEXT,
      slide_index INTEGER,
      slide_key INTEGER NOT NULL DEFAULT -1,
      note TEXT NOT NULL,
      status TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      anchor_state TEXT,
      anchored_version INTEGER,
      author_member_id TEXT,
      last_good_position_json TEXT,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE,
      FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
    );

    INSERT INTO preview_comments_multi_next
      (id, project_id, conversation_id, file_path, element_id, selector, label,
       text, position_json, html_hint, selection_kind, member_count, pod_members_json,
       style_json, attachments_json, slide_index, slide_key, note, status, created_at, updated_at,
       anchor_state, anchored_version, author_member_id, last_good_position_json)
    SELECT id, project_id, conversation_id, file_path, element_id, selector, label,
       text, position_json, html_hint, selection_kind, member_count, pod_members_json,
       style_json, attachments_json, slide_index, slide_key, note, status, created_at, updated_at,
       anchor_state, anchored_version, author_member_id, last_good_position_json
      FROM preview_comments;

    DROP TABLE preview_comments;
    ALTER TABLE preview_comments_multi_next RENAME TO preview_comments;
    CREATE INDEX IF NOT EXISTS idx_preview_comments_conversation
      ON preview_comments(project_id, conversation_id, updated_at DESC);
    CREATE INDEX IF NOT EXISTS idx_preview_comments_conversation_created
      ON preview_comments(project_id, conversation_id, created_at ASC);
"#;

/// `dropLegacyOpenPolicyColumn` rebuild (verbatim from chat-artifacts/store.ts).
const MESSAGE_ARTIFACTS_REBUILD: &str = r#"
    PRAGMA foreign_keys = OFF;
    BEGIN;
    CREATE TABLE message_artifacts__rebuild (
      message_id TEXT NOT NULL,
      ordinal INTEGER NOT NULL,
      id TEXT NOT NULL UNIQUE,
      snapshot_id TEXT,
      workspace_artifact_id TEXT,
      display_policy TEXT NOT NULL CHECK (display_policy IN
        ('latest_with_static_preview','immutable_snapshot')),
      label_at_capture TEXT NOT NULL,
      kind TEXT NOT NULL,
      html_version_id TEXT,
      created_at INTEGER NOT NULL,
      PRIMARY KEY (message_id, ordinal),
      FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE,
      FOREIGN KEY(snapshot_id)
        REFERENCES chat_artifact_snapshots(id) ON DELETE SET NULL,
      FOREIGN KEY(workspace_artifact_id)
        REFERENCES workspace_artifacts(id) ON DELETE SET NULL
    );
    INSERT INTO message_artifacts__rebuild
      (message_id, ordinal, id, snapshot_id, workspace_artifact_id,
       display_policy, label_at_capture, kind, html_version_id, created_at)
      SELECT message_id, ordinal, id, snapshot_id, workspace_artifact_id,
             display_policy, label_at_capture, kind, html_version_id, created_at
        FROM message_artifacts;
    DROP TABLE message_artifacts;
    ALTER TABLE message_artifacts__rebuild RENAME TO message_artifacts;
    CREATE INDEX IF NOT EXISTS idx_message_artifacts_snapshot
      ON message_artifacts(snapshot_id);
    COMMIT;
    PRAGMA foreign_keys = ON;
"#;

/// `migrateCritique` (verbatim from critique/persistence.ts).
const CRITIQUE_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS critique_runs (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      conversation_id TEXT,
      artifact_path TEXT,
      status TEXT NOT NULL CHECK (status IN
        ('shipped','below_threshold','timed_out','interrupted','degraded','failed','legacy','running')),
      score REAL,
      rounds_json TEXT NOT NULL DEFAULT '[]',
      transcript_path TEXT,
      protocol_version INTEGER NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE,
      FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE SET NULL
    );

    CREATE INDEX IF NOT EXISTS idx_critique_runs_project
      ON critique_runs(project_id, updated_at DESC);

    CREATE INDEX IF NOT EXISTS idx_critique_runs_status
      ON critique_runs(status);
"#;

/// `migrateMediaTasks` (verbatim from media/tasks.ts).
const MEDIA_TASKS_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS media_tasks (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      run_id TEXT,
      status TEXT NOT NULL CHECK (status IN
        ('queued','running','done','failed','interrupted')),
      surface TEXT,
      model TEXT,
      progress_json TEXT NOT NULL DEFAULT '[]',
      file_json TEXT,
      error_json TEXT,
      started_at INTEGER NOT NULL,
      ended_at INTEGER,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_media_tasks_project
      ON media_tasks(project_id, updated_at DESC);

    CREATE INDEX IF NOT EXISTS idx_media_tasks_status
      ON media_tasks(status, updated_at DESC);
"#;

/// `migrateLibrary` (verbatim from library-store.ts).
const LIBRARY_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS library_assets (
      id TEXT PRIMARY KEY,
      kind TEXT NOT NULL,
      storage TEXT NOT NULL DEFAULT 'owned',
      source_url TEXT,
      source_title TEXT,
      source_domain TEXT,
      captured_at INTEGER NOT NULL,
      archived_date TEXT NOT NULL,
      file_path TEXT,
      origin_project_id TEXT,
      rel_path TEXT,
      mime TEXT,
      width INTEGER,
      height INTEGER,
      size INTEGER,
      content_hash TEXT NOT NULL,
      caption TEXT,
      ocr_text TEXT,
      palette_json TEXT,
      tags_json TEXT,
      metadata_json TEXT,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      UNIQUE(content_hash)
    );
    CREATE INDEX IF NOT EXISTS idx_library_assets_archived
      ON library_assets(archived_date DESC, created_at DESC);
    CREATE INDEX IF NOT EXISTS idx_library_assets_kind
      ON library_assets(kind, created_at DESC);
    CREATE INDEX IF NOT EXISTS idx_library_assets_domain
      ON library_assets(source_domain);
    CREATE INDEX IF NOT EXISTS idx_library_assets_origin
      ON library_assets(origin_project_id);

    CREATE TABLE IF NOT EXISTS library_asset_sources (
      id TEXT PRIMARY KEY,
      asset_id TEXT NOT NULL,
      source_kind TEXT NOT NULL,
      project_id TEXT,
      conversation_id TEXT,
      run_id TEXT,
      design_system_id TEXT,
      rel_path TEXT,
      created_at INTEGER NOT NULL,
      FOREIGN KEY(asset_id) REFERENCES library_assets(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_library_sources_asset
      ON library_asset_sources(asset_id);
    CREATE INDEX IF NOT EXISTS idx_library_sources_project
      ON library_asset_sources(project_id);
    CREATE INDEX IF NOT EXISTS idx_library_sources_ds
      ON library_asset_sources(design_system_id);

    CREATE TABLE IF NOT EXISTS library_embeddings (
      asset_id TEXT PRIMARY KEY,
      model TEXT NOT NULL,
      dim INTEGER NOT NULL,
      vector BLOB NOT NULL,
      indexed_text TEXT,
      created_at INTEGER NOT NULL,
      FOREIGN KEY(asset_id) REFERENCES library_assets(id) ON DELETE CASCADE
    );

    CREATE TABLE IF NOT EXISTS library_tasks (
      id TEXT PRIMARY KEY,
      asset_id TEXT NOT NULL,
      status TEXT NOT NULL DEFAULT 'queued',
      progress_json TEXT NOT NULL DEFAULT '[]',
      error_json TEXT,
      started_at INTEGER NOT NULL,
      ended_at INTEGER,
      FOREIGN KEY(asset_id) REFERENCES library_assets(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_library_tasks_asset
      ON library_tasks(asset_id);

    CREATE TABLE IF NOT EXISTS library_tokens (
      token_hash TEXT PRIMARY KEY,
      label TEXT NOT NULL,
      extension_origin TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      last_used_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS library_digests (
      date TEXT PRIMARY KEY,
      project_id TEXT,
      artifact_path TEXT,
      summary TEXT,
      created_at INTEGER NOT NULL
    );
"#;

/// `migratePlugins` DDL (verbatim from plugins/persistence.ts).
const PLUGINS_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS installed_plugins (
      id                   TEXT PRIMARY KEY,
      title                TEXT NOT NULL,
      version              TEXT NOT NULL,
      source_kind          TEXT NOT NULL,
      source               TEXT NOT NULL,
      pinned_ref           TEXT,
      source_digest        TEXT,
      source_marketplace_id TEXT,
      source_marketplace_entry_name TEXT,
      source_marketplace_entry_version TEXT,
      marketplace_trust    TEXT,
      resolved_source      TEXT,
      resolved_ref         TEXT,
      manifest_digest      TEXT,
      archive_integrity    TEXT,
      trust                TEXT NOT NULL,
      capabilities_granted TEXT NOT NULL,
      manifest_json        TEXT NOT NULL,
      fs_path              TEXT NOT NULL,
      installed_at         INTEGER NOT NULL,
      updated_at           INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_installed_plugins_source_kind
      ON installed_plugins(source_kind);

    CREATE TABLE IF NOT EXISTS plugin_marketplaces (
      id            TEXT PRIMARY KEY,
      url           TEXT NOT NULL,
      spec_version  TEXT NOT NULL DEFAULT '1.0.0',
      version       TEXT NOT NULL DEFAULT '0.0.0',
      trust         TEXT NOT NULL,
      manifest_json TEXT NOT NULL,
      added_at      INTEGER NOT NULL,
      refreshed_at  INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS applied_plugin_snapshots (
      id                       TEXT PRIMARY KEY,
      project_id               TEXT NOT NULL,
      conversation_id          TEXT,
      run_id                   TEXT,
      plugin_id                TEXT NOT NULL,
      plugin_spec_version      TEXT NOT NULL DEFAULT '1.0.0',
      plugin_version           TEXT NOT NULL,
      manifest_source_digest   TEXT NOT NULL,
      strategy_json            TEXT,
      source_marketplace_id    TEXT,
      source_marketplace_entry_name TEXT,
      source_marketplace_entry_version TEXT,
      marketplace_trust        TEXT,
      resolved_source          TEXT,
      resolved_ref             TEXT,
      archive_integrity        TEXT,
      pinned_ref               TEXT,
      task_kind                TEXT NOT NULL,
      inputs_json              TEXT NOT NULL,
      resolved_context_json    TEXT NOT NULL,
      craft_requires_json      TEXT NOT NULL DEFAULT '[]',
      pipeline_json            TEXT,
      genui_surfaces_json      TEXT NOT NULL DEFAULT '[]',
      capabilities_granted     TEXT NOT NULL,
      capabilities_required    TEXT NOT NULL DEFAULT '[]',
      assets_staged_json       TEXT NOT NULL,
      connectors_required_json TEXT NOT NULL DEFAULT '[]',
      connectors_resolved_json TEXT NOT NULL DEFAULT '[]',
      mcp_servers_json         TEXT NOT NULL DEFAULT '[]',
      plugin_title             TEXT,
      plugin_description       TEXT,
      query_text               TEXT,
      status                   TEXT NOT NULL DEFAULT 'fresh',
      applied_at               INTEGER NOT NULL,
      expires_at               INTEGER,
      FOREIGN KEY (project_id)      REFERENCES projects(id)      ON DELETE CASCADE,
      FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL
    );

    CREATE INDEX IF NOT EXISTS idx_snapshots_project ON applied_plugin_snapshots(project_id);
    CREATE INDEX IF NOT EXISTS idx_snapshots_run     ON applied_plugin_snapshots(run_id);
    CREATE INDEX IF NOT EXISTS idx_snapshots_plugin  ON applied_plugin_snapshots(plugin_id, plugin_version);

    -- §10.2 devloop audit + per-iteration billing surface.
    -- run_id is a free string today (in-memory runs, no FK target).
    CREATE TABLE IF NOT EXISTS run_devloop_iterations (
      id                    TEXT PRIMARY KEY,
      run_id                TEXT NOT NULL,
      stage_id              TEXT NOT NULL,
      iteration             INTEGER NOT NULL,
      artifact_diff_summary TEXT,
      critique_summary      TEXT,
      tokens_used           INTEGER,
      ended_at              INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_devloop_run        ON run_devloop_iterations(run_id);
    CREATE INDEX IF NOT EXISTS idx_devloop_run_stage  ON run_devloop_iterations(run_id, stage_id);

    -- §10.3 GenUI surface persisted state. The lookup rules in §10.3.3
    -- read this table at run / conversation / project tier; F8 enforces
    -- the cross-conversation cache hit on a second oauth-prompt.
    -- conversation_id / run_id are stored as plain TEXT (no FK) because
    -- runs are in-memory; conversation FK is set up by the daemon's
    -- existing migrations and we don't want to fail on legacy DBs that
    -- predate it. plugin_snapshot_id is a FK to applied_plugin_snapshots.
    CREATE TABLE IF NOT EXISTS genui_surfaces (
      id                    TEXT PRIMARY KEY,
      project_id            TEXT NOT NULL,
      conversation_id       TEXT,
      run_id                TEXT,
      plugin_snapshot_id    TEXT NOT NULL,
      surface_id            TEXT NOT NULL,
      kind                  TEXT NOT NULL,
      persist               TEXT NOT NULL,
      schema_digest         TEXT,
      value_json            TEXT,
      status                TEXT NOT NULL,
      responded_by          TEXT,
      requested_at          INTEGER NOT NULL,
      responded_at          INTEGER,
      expires_at            INTEGER,
      FOREIGN KEY (project_id)         REFERENCES projects(id)                  ON DELETE CASCADE,
      FOREIGN KEY (plugin_snapshot_id) REFERENCES applied_plugin_snapshots(id)  ON DELETE SET NULL
    );

    CREATE INDEX IF NOT EXISTS idx_genui_proj_surface ON genui_surfaces(project_id, surface_id);
    CREATE INDEX IF NOT EXISTS idx_genui_conv_surface ON genui_surfaces(conversation_id, surface_id);
    CREATE INDEX IF NOT EXISTS idx_genui_run          ON genui_surfaces(run_id);

    CREATE TABLE IF NOT EXISTS skill_plugin_candidates (
      id                   TEXT PRIMARY KEY,
      project_id           TEXT NOT NULL,
      run_id               TEXT,
      conversation_id      TEXT,
      assistant_message_id TEXT,
      fingerprint          TEXT NOT NULL,
      status               TEXT NOT NULL DEFAULT 'active',
      title                TEXT NOT NULL,
      description          TEXT NOT NULL,
      confidence           REAL NOT NULL,
      source_refs_json     TEXT NOT NULL,
      provenance_json      TEXT NOT NULL,
      draft_path           TEXT,
      created_at           INTEGER NOT NULL,
      updated_at           INTEGER NOT NULL,
      dismissed_at         INTEGER,
      UNIQUE(project_id, fingerprint),
      FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_skill_plugin_candidates_project
      ON skill_plugin_candidates(project_id, status, created_at DESC);
"#;

/// `installed_plugins` guarded ALTERs from `migratePlugins`.
const INSTALLED_PLUGIN_COLUMN_ALTERS: [(&str, &str); 8] = [
    (
        "source_marketplace_entry_name",
        "ALTER TABLE installed_plugins ADD COLUMN source_marketplace_entry_name TEXT",
    ),
    (
        "source_marketplace_entry_version",
        "ALTER TABLE installed_plugins ADD COLUMN source_marketplace_entry_version TEXT",
    ),
    (
        "marketplace_trust",
        "ALTER TABLE installed_plugins ADD COLUMN marketplace_trust TEXT",
    ),
    (
        "resolved_source",
        "ALTER TABLE installed_plugins ADD COLUMN resolved_source TEXT",
    ),
    (
        "resolved_ref",
        "ALTER TABLE installed_plugins ADD COLUMN resolved_ref TEXT",
    ),
    (
        "manifest_digest",
        "ALTER TABLE installed_plugins ADD COLUMN manifest_digest TEXT",
    ),
    (
        "archive_integrity",
        "ALTER TABLE installed_plugins ADD COLUMN archive_integrity TEXT",
    ),
    (
        "bundled_content_digest",
        "ALTER TABLE installed_plugins ADD COLUMN bundled_content_digest TEXT",
    ),
];

/// `applied_plugin_snapshots` guarded ALTERs from `migratePlugins`.
const SNAPSHOT_COLUMN_ALTERS: [(&str, &str); 8] = [
    (
        "strategy_json",
        "ALTER TABLE applied_plugin_snapshots ADD COLUMN strategy_json TEXT",
    ),
    (
        "source_marketplace_entry_name",
        "ALTER TABLE applied_plugin_snapshots ADD COLUMN source_marketplace_entry_name TEXT",
    ),
    (
        "source_marketplace_entry_version",
        "ALTER TABLE applied_plugin_snapshots ADD COLUMN source_marketplace_entry_version TEXT",
    ),
    (
        "marketplace_trust",
        "ALTER TABLE applied_plugin_snapshots ADD COLUMN marketplace_trust TEXT",
    ),
    (
        "resolved_source",
        "ALTER TABLE applied_plugin_snapshots ADD COLUMN resolved_source TEXT",
    ),
    (
        "resolved_ref",
        "ALTER TABLE applied_plugin_snapshots ADD COLUMN resolved_ref TEXT",
    ),
    (
        "archive_integrity",
        "ALTER TABLE applied_plugin_snapshots ADD COLUMN archive_integrity TEXT",
    ),
    (
        "craft_requires_json",
        "ALTER TABLE applied_plugin_snapshots ADD COLUMN craft_requires_json TEXT NOT NULL DEFAULT '[]'",
    ),
];

/// `migrateStrategyTaskStore` DDL (verbatim from strategies/task-store.ts).
const STRATEGY_TASKS_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS strategy_task_executions (
      task_execution_id TEXT PRIMARY KEY,
      schema_version INTEGER NOT NULL DEFAULT 1,
      revision INTEGER NOT NULL DEFAULT 0,
      project_id TEXT NOT NULL,
      conversation_id TEXT NOT NULL,
      snapshot_id TEXT NOT NULL,
      strategy_id TEXT NOT NULL,
      strategy_version TEXT NOT NULL,
      strategy_package_hash TEXT NOT NULL,
      selected_agent_id TEXT NOT NULL,
      route TEXT CHECK (route IN ('direct_edit', 'full_plan')),
      input_stage TEXT NOT NULL CHECK (
        input_stage IN ('request', 'clarification', 'contract_repair', 'production')
      ),
      outcome TEXT NOT NULL CHECK (
        outcome IN (
          'running', 'clarification_required', 'plan_ready',
          'completed', 'blocked', 'canceled'
        )
      ),
      execution_mode TEXT CHECK (execution_mode IN ('simple', 'complex')),
      execution_intent TEXT NOT NULL DEFAULT 'produce',
      plan_contract_json TEXT,
      plan_contract_hash TEXT,
      clarification_count INTEGER NOT NULL DEFAULT 0 CHECK (clarification_count BETWEEN 0 AND 1),
      plan_contract_repair_attempts INTEGER NOT NULL DEFAULT 0 CHECK (
        plan_contract_repair_attempts BETWEEN 0 AND 1
      ),
      initial_run_id TEXT NOT NULL,
      latest_run_id TEXT NOT NULL,
      prompt_bundle_schema TEXT,
      prompt_bundle_text TEXT,
      prompt_bundle_utf8_bytes INTEGER,
      prompt_bundle_sha256 TEXT,
      frozen_input_identity_json TEXT,
      blocked_reason_codes_json TEXT,
      blocked_visible_text TEXT,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE,
      FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
      FOREIGN KEY(snapshot_id) REFERENCES applied_plugin_snapshots(id)
    );

    CREATE INDEX IF NOT EXISTS idx_strategy_task_executions_project_conversation
      ON strategy_task_executions(project_id, conversation_id, updated_at DESC);

    CREATE TABLE IF NOT EXISTS strategy_task_runs (
      task_execution_id TEXT NOT NULL,
      run_id TEXT NOT NULL UNIQUE,
      input_stage TEXT NOT NULL CHECK (
        input_stage IN ('request', 'clarification', 'contract_repair', 'production')
      ),
      task_run_index INTEGER NOT NULL CHECK (task_run_index >= 0),
      source_run_id TEXT,
      final_text_kind TEXT,
      final_text_schema TEXT,
      final_text TEXT,
      final_text_utf8_bytes INTEGER,
      final_text_sha256 TEXT,
      created_at INTEGER NOT NULL,
      PRIMARY KEY(task_execution_id, task_run_index),
      FOREIGN KEY(task_execution_id) REFERENCES strategy_task_executions(task_execution_id)
        ON DELETE CASCADE
    );
"#;

/// `addColumnIfMissing` definitions for `strategy_task_executions` after the
/// intent-resolution store is created, in TypeScript order (`execution_intent`
/// and `intent_resolution_version` run before it and are applied inline).
const STRATEGY_EXECUTION_COLUMN_ALTERS: [&str; 7] = [
    "prompt_bundle_schema TEXT",
    "prompt_bundle_text TEXT",
    "prompt_bundle_utf8_bytes INTEGER",
    "prompt_bundle_sha256 TEXT",
    "frozen_input_identity_json TEXT",
    "blocked_reason_codes_json TEXT",
    "blocked_visible_text TEXT",
];

/// `addColumnIfMissing` definitions for `strategy_task_runs`.
const STRATEGY_RUN_COLUMN_ALTERS: [&str; 5] = [
    "final_text_kind TEXT",
    "final_text_schema TEXT",
    "final_text TEXT",
    "final_text_utf8_bytes INTEGER",
    "final_text_sha256 TEXT",
];

/// `migrateIntentResolutionStore` (verbatim from
/// strategies/od-next/intent-resolution-store.ts).
const STRATEGY_INTENT_RESOLUTION_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS strategy_task_intent_resolution (
      task_execution_id TEXT PRIMARY KEY REFERENCES strategy_task_executions(task_execution_id) ON DELETE CASCADE,
      version INTEGER NOT NULL CHECK(version = 1),
      state TEXT NOT NULL CHECK(state IN ('unresolved','claimed','started','resolved','failed')),
      attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts IN (0,1)),
      run_id TEXT UNIQUE,
      source_run_id TEXT,
      source_result_json TEXT,
      source_result_sha256 TEXT,
      reply_json TEXT,
      reply_sha256 TEXT
    );
    CREATE TABLE IF NOT EXISTS strategy_task_run_write_evidence (
      task_execution_id TEXT NOT NULL REFERENCES strategy_task_executions(task_execution_id) ON DELETE CASCADE,
      run_id TEXT NOT NULL,
      files_written INTEGER NOT NULL CHECK(files_written >= 0),
      unknown INTEGER NOT NULL CHECK(unknown IN (0,1)),
      source_mask INTEGER NOT NULL CHECK(source_mask BETWEEN 1 AND 7),
      PRIMARY KEY(task_execution_id, run_id),
      FOREIGN KEY(run_id) REFERENCES strategy_task_runs(run_id)
    );
"#;

/// `migrateFrozenSkillPackageStore` (verbatim from
/// strategies/od-next/frozen-skill-package.ts).
const FROZEN_SKILL_PACKAGE_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS strategy_task_frozen_skill_packages (
      task_execution_id TEXT PRIMARY KEY,
      schema TEXT NOT NULL,
      identity TEXT NOT NULL,
      payload_json TEXT NOT NULL,
      FOREIGN KEY(task_execution_id) REFERENCES strategy_task_executions(task_execution_id)
        ON DELETE CASCADE
    );
"#;

/// `migrateChatArtifacts` DDL (verbatim from chat-artifacts/store.ts).
const CHAT_ARTIFACTS_DDL: &str = r#"
    -- Mutable "what is in Design Files right now" identity. Path is NOT the
    -- identity: overwrite only moves the digest, rename only moves the path,
    -- and delete leaves a tombstone rather than freeing the id for reuse.
    CREATE TABLE IF NOT EXISTS workspace_artifacts (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      current_path TEXT,
      kind TEXT NOT NULL,
      mime TEXT,
      current_digest TEXT,
      current_size INTEGER,
      current_mtime INTEGER,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      deleted_at INTEGER,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
    );

    -- At most one LIVE identity per (project, path). Tombstoned rows drop out
    -- of the index, so a new file written at a deleted path gets a fresh
    -- identity instead of silently resurrecting the deleted one.
    CREATE UNIQUE INDEX IF NOT EXISTS idx_workspace_artifacts_live_path
      ON workspace_artifacts(project_id, current_path)
      WHERE deleted_at IS NULL;

    CREATE INDEX IF NOT EXISTS idx_workspace_artifacts_project
      ON workspace_artifacts(project_id, updated_at DESC);

    -- Content-addressed blob index. storage_key is a daemon-internal
    -- RELATIVE key under the snapshot root; it is never returned over HTTP and
    -- never accepted from a caller.
    CREATE TABLE IF NOT EXISTS chat_artifact_blobs (
      digest TEXT PRIMARY KEY,
      storage_key TEXT NOT NULL,
      byte_size INTEGER NOT NULL,
      mime TEXT,
      created_at INTEGER NOT NULL,
      last_verified_at INTEGER
    );

    -- Immutable message evidence. One row per capture attempt, including the
    -- ones that failed: a refusal is data, not an absence.
    CREATE TABLE IF NOT EXISTS chat_artifact_snapshots (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      workspace_artifact_id TEXT,
      source_path_at_capture TEXT NOT NULL,
      kind TEXT NOT NULL,
      mime TEXT,
      content_digest TEXT,
      thumbnail_digest TEXT,
      source_size INTEGER,
      source_mtime INTEGER,
      expected_size INTEGER,
      expected_mtime INTEGER,
      expected_digest TEXT,
      temp_key TEXT,
      run_id TEXT,
      media_task_id TEXT,
      capture_state TEXT NOT NULL CHECK (capture_state IN
        ('pending','ready','failed','orphaned')),
      failure_code TEXT,
      created_at INTEGER NOT NULL,
      ready_at INTEGER,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE,
      FOREIGN KEY(workspace_artifact_id)
        REFERENCES workspace_artifacts(id) ON DELETE SET NULL
    );

    CREATE INDEX IF NOT EXISTS idx_chat_artifact_snapshots_project
      ON chat_artifact_snapshots(project_id, created_at DESC);
    CREATE INDEX IF NOT EXISTS idx_chat_artifact_snapshots_state
      ON chat_artifact_snapshots(capture_state, created_at);
    CREATE INDEX IF NOT EXISTS idx_chat_artifact_snapshots_content
      ON chat_artifact_snapshots(content_digest);
    CREATE INDEX IF NOT EXISTS idx_chat_artifact_snapshots_thumbnail
      ON chat_artifact_snapshots(thumbnail_digest);

    -- The join that gives a chat message its cards. Cascades with the message,
    -- which is also what makes conversation delete / project delete / fork work
    -- without a bespoke cleanup pass.
    CREATE TABLE IF NOT EXISTS message_artifacts (
      message_id TEXT NOT NULL,
      ordinal INTEGER NOT NULL,
      id TEXT NOT NULL UNIQUE,
      snapshot_id TEXT,
      workspace_artifact_id TEXT,
      display_policy TEXT NOT NULL CHECK (display_policy IN
        ('latest_with_static_preview','immutable_snapshot')),
      -- No open_policy column, on purpose: every card opens the workspace's
      -- latest file, so workspace_artifact_id above IS the click target.
      -- See policy.ts for the ruling.
      label_at_capture TEXT NOT NULL,
      kind TEXT NOT NULL,
      html_version_id TEXT,
      created_at INTEGER NOT NULL,
      PRIMARY KEY (message_id, ordinal),
      FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE,
      FOREIGN KEY(snapshot_id)
        REFERENCES chat_artifact_snapshots(id) ON DELETE SET NULL,
      FOREIGN KEY(workspace_artifact_id)
        REFERENCES workspace_artifacts(id) ON DELETE SET NULL
    );

    CREATE INDEX IF NOT EXISTS idx_message_artifacts_snapshot
      ON message_artifacts(snapshot_id);
"#;

/// `migrateCollabSyncSnapshots` (verbatim from collab/sync-snapshot-store.ts).
const COLLAB_SYNC_SNAPSHOTS_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS collab_sync_snapshots (
      face TEXT NOT NULL,
      account_id TEXT NOT NULL,
      workspace_id TEXT NOT NULL,
      digest_token TEXT NOT NULL,
      snapshot_json TEXT NOT NULL,
      updated_at INTEGER NOT NULL,
      PRIMARY KEY (face, account_id, workspace_id)
    );
"#;

/// `migrateCommentRelayOutbox` (verbatim from collab/comment-relay-outbox.ts).
const COMMENT_RELAY_OUTBOX_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS comment_relay_outbox (
      workspace_id TEXT NOT NULL,
      workspace_member_id TEXT NOT NULL,
      team_id TEXT NOT NULL,
      project_id TEXT NOT NULL,
      comment_id TEXT NOT NULL,
      expected_owner_member_id TEXT,
      payload_json TEXT NOT NULL,
      revision INTEGER NOT NULL DEFAULT 1,
      attempt_count INTEGER NOT NULL DEFAULT 0,
      next_attempt_at INTEGER NOT NULL,
      last_error TEXT,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      PRIMARY KEY (workspace_id, workspace_member_id, project_id, comment_id)
    );

    CREATE INDEX IF NOT EXISTS idx_comment_relay_outbox_due
      ON comment_relay_outbox(next_attempt_at, updated_at);
"#;

/// `migrateAmrTerminalReportOutbox` base table (verbatim from
/// storage/amr-terminal-report-outbox.ts).
const AMR_TERMINAL_REPORT_OUTBOX_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS amr_terminal_report_outbox (
      run_id TEXT PRIMARY KEY,
      outcome TEXT NOT NULL CHECK (outcome IN ('failed', 'canceled')),
      terminal_at INTEGER NOT NULL
    );
"#;

/// The `additions` list of `migrateAmrTerminalReportOutbox`.
const AMR_COLUMN_ALTERS: [(&str, &str); 11] = [
    (
        "terminal_at_iso",
        "ALTER TABLE amr_terminal_report_outbox ADD COLUMN terminal_at_iso TEXT",
    ),
    (
        "state",
        "ALTER TABLE amr_terminal_report_outbox ADD COLUMN state TEXT NOT NULL DEFAULT 'pending'",
    ),
    (
        "attempt_count",
        "ALTER TABLE amr_terminal_report_outbox ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "next_attempt_at",
        "ALTER TABLE amr_terminal_report_outbox ADD COLUMN next_attempt_at INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "version",
        "ALTER TABLE amr_terminal_report_outbox ADD COLUMN version INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "lease_until",
        "ALTER TABLE amr_terminal_report_outbox ADD COLUMN lease_until INTEGER",
    ),
    (
        "last_error_code",
        "ALTER TABLE amr_terminal_report_outbox ADD COLUMN last_error_code TEXT",
    ),
    (
        "last_error",
        "ALTER TABLE amr_terminal_report_outbox ADD COLUMN last_error TEXT",
    ),
    (
        "receipt",
        "ALTER TABLE amr_terminal_report_outbox ADD COLUMN receipt TEXT",
    ),
    (
        "created_at",
        "ALTER TABLE amr_terminal_report_outbox ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "updated_at",
        "ALTER TABLE amr_terminal_report_outbox ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0",
    ),
];

const AMR_TERMINAL_REPORT_OUTBOX_INDEXES: &str = r#"
    CREATE INDEX IF NOT EXISTS idx_amr_terminal_report_outbox_due
      ON amr_terminal_report_outbox(state, next_attempt_at, lease_until, run_id);
    CREATE INDEX IF NOT EXISTS idx_amr_terminal_report_outbox_terminal_at
      ON amr_terminal_report_outbox(terminal_at, run_id);
"#;

/// `migratePublicFilePublications` (verbatim from
/// collab/public-file-publication-store.ts).
const PUBLIC_FILE_PUBLICATIONS_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS public_file_publications (
      resource_team_id TEXT NOT NULL,
      owner_member_id TEXT NOT NULL,
      project_id TEXT NOT NULL,
      file_path TEXT NOT NULL,
      url TEXT NOT NULL,
      slug TEXT NOT NULL,
      file_name TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      PRIMARY KEY (resource_team_id, owner_member_id, project_id, file_path)
    );
"#;
