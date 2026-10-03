//! Schema migrations extracted from the TypeScript daemon's `migrate()`
//! (`apps/daemon/src/db.ts`).
//!
//! * `0001_core.sql` — the verbatim `CREATE TABLE IF NOT EXISTS` / `CREATE
//!   INDEX IF NOT EXISTS` block (17 tables, 10 indexes).
//! * `ensure_columns.tsv` — the guarded `ALTER TABLE … ADD COLUMN` entries.
//!   SQLite has no `ALTER TABLE … IF NOT EXISTS`, so each entry is applied
//!   only when its table exists and the column is missing — exactly the
//!   `pragma_table_info` guard the TypeScript code uses.
//!
//! The legacy table-rebuild helpers (`migrateWorkspaceProjectsSingleHome`,
//! preview-comment rebuilds) are not ported yet; they only matter for
//! pre-2026-07-21 databases and are tracked in beads.

use rusqlite::Connection;

use crate::storage::StoreError;

const CORE_SQL: &str = include_str!("../migrations/0001_core.sql");
const ENSURE_COLUMNS: &str = include_str!("../migrations/ensure_columns.tsv");

/// Run all migrations. Safe to call on every startup (idempotent).
pub fn run(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(CORE_SQL)?;
    for entry in ENSURE_COLUMNS.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let mut parts = entry.splitn(3, '\t');
        let (Some(table), Some(column), Some(sql)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(StoreError::Migration(entry.to_string()));
        };
        if table_exists(conn, table)? && !column_exists(conn, table, column)? {
            conn.execute_batch(sql)?;
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
        rusqlite::params![table, column],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}
