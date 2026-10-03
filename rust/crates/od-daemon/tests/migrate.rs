//! Integration tests for the ported TypeScript `migrate()` helpers
//! (`apps/daemon/src/db.ts` + its subsystem imports).
//!
//! Each helper gets an OLD-shape fixture built with raw SQL, one
//! [`od_daemon::migrations::run`], assertions on the transformed shape and
//! data, and a second run to prove idempotency. The last test opens a fresh
//! [`od_daemon::Store`] and asserts every legacy marker is in the modern state.

use od_daemon::migrations::run;
use od_daemon::Store;
use rusqlite::Connection;

fn open_mem() -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory database");
    // Parity with `Store::open` / TS `openDatabase`: migrations run with
    // foreign keys enforced, so fixtures have to satisfy them too.
    conn.pragma_update(None, "foreign_keys", "ON")
        .expect("foreign_keys = ON");
    conn
}

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info(?1)")
        .expect("prepare pragma");
    let names = stmt
        .query_map([table], |row| row.get::<_, String>(0))
        .expect("pragma_table_info")
        .collect::<Result<Vec<_>, _>>();
    names.expect("pragma rows")
}

fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    columns(conn, table).iter().any(|name| name == column)
}

fn table_sql(conn: &Connection, table: &str) -> String {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get::<_, String>(0),
    )
    .unwrap_or_default()
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .expect("sqlite_master")
        > 0
}

fn index_exists(conn: &Connection, index: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [index],
        |row| row.get::<_, i64>(0),
    )
    .expect("sqlite_master")
        > 0
}

/// Whole-schema fingerprint: (type, name, sql) for every object.
fn schema_dump(conn: &Connection) -> Vec<(String, String, Option<String>)> {
    let mut stmt = conn
        .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
        .expect("prepare dump");
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("dump")
        .collect::<Result<Vec<_>, _>>();
    rows.expect("dump rows")
}

/// `SELECT`s every column of `table` (in table order), rendered as text, so a
/// second migration run can be compared byte-for-byte.
fn row_dump(conn: &Connection, table: &str, order_by: &str) -> Vec<Vec<String>> {
    let cols = columns(conn, table);
    let list = cols.join(", ");
    let sql = format!("SELECT {list} FROM {table} ORDER BY {order_by}");
    let mut stmt = conn.prepare(&sql).expect("prepare row dump");
    let mut cursor = stmt.query([]).expect("query row dump");
    let mut rows = Vec::new();
    while let Some(row) = cursor.next().expect("next") {
        let mut values = Vec::new();
        for (idx, _) in cols.iter().enumerate() {
            let value: Option<rusqlite::types::Value> = row.get(idx).expect("get");
            values.push(match value {
                None | Some(rusqlite::types::Value::Null) => "NULL".to_string(),
                Some(rusqlite::types::Value::Integer(int)) => int.to_string(),
                Some(rusqlite::types::Value::Real(real)) => real.to_string(),
                Some(rusqlite::types::Value::Text(text)) => text,
                Some(rusqlite::types::Value::Blob(blob)) => format!("{blob:?}"),
            });
        }
        rows.push(values);
    }
    rows
}

fn scalar_i64(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).expect("scalar")
}

// ---------------------------------------------------------------------------
// migrateWorkspaceProjectsSingleHome
// ---------------------------------------------------------------------------

/// The pre-2026-07-21 shape: composite `PRIMARY KEY (workspace_id,
/// project_id)` plus the blanket back-fill's duplicate rows.
const LEGACY_WORKSPACE_PROJECTS: &str = r#"
    CREATE TABLE projects (
      id TEXT PRIMARY KEY,
      name TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );
    INSERT INTO projects (id, name, created_at, updated_at) VALUES
      ('p1', 'P1', 1, 1), ('p2', 'P2', 1, 1), ('p3', 'P3', 1, 1), ('p4', 'P4', 1, 1);
    CREATE TABLE workspace_projects (
      project_id TEXT NOT NULL,
      workspace_id TEXT NOT NULL,
      visibility TEXT NOT NULL CHECK (visibility IN ('personal', 'team')),
      resource_state TEXT NOT NULL CHECK (resource_state IN ('active', 'frozen', 'deleted')),
      created_by_workspace_member_id TEXT,
      updated_by_workspace_member_id TEXT,
      resource_hub_resource_id TEXT,
      cloud_tombstoned_at INTEGER,
      sync_state TEXT,
      version INTEGER NOT NULL DEFAULT 1,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      PRIMARY KEY (workspace_id, project_id)
    );
    INSERT INTO workspace_projects
      (project_id, workspace_id, visibility, resource_state,
       created_by_workspace_member_id, created_at, updated_at)
    VALUES
      -- p1: an ownerless guess loses to the row with a recorded act.
      ('p1', 'ws-a', 'personal', 'active', NULL, 100, 100),
      ('p1', 'ws-b', 'personal', 'active', 'm1', 100, 100),
      -- p2: two guesses tie down to created_at; workspace id decides.
      ('p2', 'ws-x', 'personal', 'active', NULL, 100, 100),
      ('p2', 'ws-y', 'personal', 'active', NULL, 100, 100),
      -- p3: a team share outranks everything.
      ('p3', 'ws-t', 'team', 'active', 'm1', 100, 100),
      -- p4: every candidate is an ownerless guess in a team workspace.
      ('p4', 'ws-t', 'personal', 'active', NULL, 100, 100);
"#;

fn workspace_home_snapshot(conn: &Connection) -> (Vec<(String, String)>, String) {
    let rows = {
        let mut stmt = conn
            .prepare("SELECT project_id, workspace_id FROM workspace_projects ORDER BY project_id")
            .expect("prepare");
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .collect::<Result<Vec<(String, String)>, _>>()
            .expect("rows")
    };
    (rows, table_sql(conn, "workspace_projects"))
}

#[test]
fn workspace_projects_rebuild_narrows_key_and_collapses_duplicates() {
    let conn = open_mem();
    conn.execute_batch(LEGACY_WORKSPACE_PROJECTS)
        .expect("legacy fixture");

    run(&conn).expect("first run");
    let (rows, sql) = workspace_home_snapshot(&conn);
    assert_eq!(
        rows,
        vec![
            ("p1".to_string(), "ws-b".to_string()),
            ("p2".to_string(), "ws-x".to_string()),
            ("p3".to_string(), "ws-t".to_string()),
        ],
        "p4 must collapse to zero rows; p1 keeps the recorded act"
    );
    assert!(
        sql.contains("project_id TEXT PRIMARY KEY"),
        "primary key not narrowed: {sql}"
    );
    assert!(
        !sql.contains("PRIMARY KEY (workspace_id, project_id)"),
        "composite key survived: {sql}"
    );
    assert!(index_exists(&conn, "idx_workspace_projects_workspace_visibility"));
    assert!(!table_exists(&conn, "workspace_projects_legacy_multi_workspace"));
    assert!(has_column(&conn, "workspace_projects", "metadata_refresh_pending"));

    // The rebuild's INSERT list omits metadata_refresh_pending; TypeScript
    // adds the column after the rebuild, so every copied row reads 0.
    let pending = scalar_i64(
        &conn,
        "SELECT COUNT(*) FROM workspace_projects WHERE metadata_refresh_pending = 0",
    );
    assert_eq!(pending, 3);

    run(&conn).expect("second run");
    assert_eq!(workspace_home_snapshot(&conn), (rows, sql));
}

#[test]
fn workspace_projects_modern_shape_is_a_no_op() {
    let conn = open_mem();
    run(&conn).expect("first run");
    conn.execute_batch(
        "INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p1', 'P', 1, 1);
         INSERT INTO workspace_projects
           (project_id, workspace_id, visibility, resource_state, created_at, updated_at)
         VALUES ('p1', 'ws-a', 'personal', 'active', 1, 1);",
    )
    .expect("seed modern row");

    let before_schema = schema_dump(&conn);
    let before_rows = row_dump(&conn, "workspace_projects", "project_id");
    run(&conn).expect("second run");
    assert_eq!(schema_dump(&conn), before_schema);
    assert_eq!(row_dump(&conn, "workspace_projects", "project_id"), before_rows);
}

// ---------------------------------------------------------------------------
// migratePreviewCommentsSlideKey
// ---------------------------------------------------------------------------

/// Oldest preview-comment shape: no `slide_key`, natural unique on
/// (project, conversation, file, element).
const LEGACY_SLIDE_PREVIEW_COMMENTS: &str = r#"
    CREATE TABLE projects (
      id TEXT PRIMARY KEY,
      name TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );
    CREATE TABLE conversations (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );
    INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p1', 'P1', 1, 1);
    INSERT INTO conversations (id, project_id, created_at, updated_at) VALUES ('conv1', 'p1', 1, 1);
    CREATE TABLE preview_comments (
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
      slide_index INTEGER,
      note TEXT NOT NULL,
      status TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      UNIQUE(project_id, conversation_id, file_path, element_id)
    );
    INSERT INTO preview_comments
      (id, project_id, conversation_id, file_path, element_id, selector, label,
       text, position_json, html_hint, slide_index, note, status, created_at, updated_at)
    VALUES
      ('c1', 'p1', 'conv1', 'f.html', 'e1', 'sel', 'L', 'first', '{}', 'hint', NULL, '', 'open', 200, 200),
      ('c2', 'p1', 'conv1', 'f.html', 'e2', 'sel', 'L', 'second', '{}', 'hint', 2, '', 'open', 100, 100),
      ('c3', 'p1', 'conv1', 'f.html', 'e3', 'sel', 'L', 'third', '{}', 'hint', NULL, '', 'open', 300, 300);
"#;

#[test]
fn preview_comments_legacy_unique_gets_slide_key_rebuild() {
    let conn = open_mem();
    conn.execute_batch(LEGACY_SLIDE_PREVIEW_COMMENTS)
        .expect("legacy fixture");

    run(&conn).expect("first run");

    let sql = table_sql(&conn, "preview_comments");
    assert!(!sql.to_uppercase().contains("UNIQUE"), "unique survived: {sql}");
    for column in [
        "slide_key",
        "selection_kind",
        "member_count",
        "pod_members_json",
        "style_json",
        "attachments_json",
        "slide_index",
        "anchor_state",
        "anchored_version",
        "author_member_id",
        "last_good_position_json",
        "pin_seq",
        "pin_seq_confirmed",
        "sort_key",
    ] {
        assert!(
            has_column(&conn, "preview_comments", column),
            "missing column {column}"
        );
    }

    // slide_key backfills from slide_index, NULL becoming -1.
    let keys = {
        let mut stmt = conn
            .prepare("SELECT id, slide_key FROM preview_comments ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
    };
    assert_eq!(
        keys,
        vec![
            ("c1".to_string(), -1),
            ("c2".to_string(), 2),
            ("c3".to_string(), -1)
        ]
    );
    assert!(index_exists(&conn, "idx_preview_comments_conversation"));
    assert!(!table_exists(&conn, "preview_comments_next"));

    let rows = row_dump(&conn, "preview_comments", "id");
    run(&conn).expect("second run");
    assert_eq!(row_dump(&conn, "preview_comments", "id"), rows);
    assert_eq!(table_sql(&conn, "preview_comments"), sql);
}

// ---------------------------------------------------------------------------
// migratePreviewCommentsAllowMultiplePerElement
// ---------------------------------------------------------------------------

/// Intermediate shape: `slide_key` exists but a natural unique still spans
/// project…element (with slide_key in the key), so the multi-per-element
/// rebuild — not the slide-key one — has to run.
const MULTI_PER_ELEMENT_PREVIEW_COMMENTS: &str = r#"
    CREATE TABLE projects (
      id TEXT PRIMARY KEY,
      name TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );
    CREATE TABLE conversations (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );
    INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p1', 'P1', 1, 1);
    INSERT INTO conversations (id, project_id, created_at, updated_at) VALUES ('conv1', 'p1', 1, 1);
    CREATE TABLE preview_comments (
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
      slide_index INTEGER,
      slide_key INTEGER NOT NULL DEFAULT -1,
      note TEXT NOT NULL,
      status TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      UNIQUE(project_id, conversation_id, file_path, element_id, slide_key)
    );
    INSERT INTO preview_comments
      (id, project_id, conversation_id, file_path, element_id, selector, label,
       text, position_json, html_hint, slide_index, slide_key, note, status, created_at, updated_at)
    VALUES
      ('c1', 'p1', 'conv1', 'f.html', 'e1', 'sel', 'L', 'note one', '{}', 'hint', 0, 0, '', 'open', 100, 100),
      ('c2', 'p1', 'conv1', 'f.html', 'e1', 'sel', 'L', 'note two', '{}', 'hint', 1, 1, '', 'open', 200, 200),
      ('c3', 'p1', 'conv1', 'f.html', 'e2', 'sel', 'L', 'note three', '{}', 'hint', 0, 0, '', 'open', 300, 300);
"#;

#[test]
fn preview_comments_natural_unique_gets_multi_per_element_rebuild() {
    let conn = open_mem();
    conn.execute_batch(MULTI_PER_ELEMENT_PREVIEW_COMMENTS)
        .expect("legacy fixture");
    let slide_sql = table_sql(&conn, "preview_comments");

    run(&conn).expect("first run");

    let sql = table_sql(&conn, "preview_comments");
    assert!(!sql.to_uppercase().contains("UNIQUE"), "unique survived: {sql}");
    assert_ne!(sql, slide_sql, "table was never rebuilt");
    assert!(!table_exists(&conn, "preview_comments_multi_next"));
    // Only the multi rebuild (not the slide one) creates this index.
    assert!(index_exists(&conn, "idx_preview_comments_conversation_created"));
    for column in ["anchor_state", "author_member_id", "pin_seq", "sort_key"] {
        assert!(
            has_column(&conn, "preview_comments", column),
            "missing column {column}"
        );
    }

    let rows = row_dump(&conn, "preview_comments", "id");
    assert_eq!(rows.len(), 3, "both notes on e1 must survive");
    let notes = {
        let mut stmt = conn
            .prepare("SELECT id, text FROM preview_comments ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .collect::<Result<Vec<(String, String)>, _>>()
            .expect("rows")
    };
    assert_eq!(
        notes,
        vec![
            ("c1".to_string(), "note one".to_string()),
            ("c2".to_string(), "note two".to_string()),
            ("c3".to_string(), "note three".to_string()),
        ]
    );
    run(&conn).expect("second run");
    assert_eq!(row_dump(&conn, "preview_comments", "id"), rows);
    assert_eq!(table_sql(&conn, "preview_comments"), sql);
}

// ---------------------------------------------------------------------------
// backfillPreviewCommentPinSeqAndSortKey
// ---------------------------------------------------------------------------

/// Modern shape minus the pin columns (they post-date this table).
const PRE_PIN_PREVIEW_COMMENTS: &str = r#"
    CREATE TABLE projects (
      id TEXT PRIMARY KEY,
      name TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );
    CREATE TABLE conversations (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );
    INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p1', 'P1', 1, 1);
    INSERT INTO conversations (id, project_id, created_at, updated_at) VALUES ('conv1', 'p1', 1, 1);
    CREATE TABLE preview_comments (
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
      note TEXT NOT NULL,
      status TEXT NOT NULL,
      slide_index INTEGER,
      slide_key INTEGER NOT NULL DEFAULT -1,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );
    INSERT INTO preview_comments
      (id, project_id, conversation_id, file_path, element_id, selector, label,
       text, position_json, html_hint, note, status, slide_key, created_at, updated_at)
    VALUES
      ('a1', 'p1', 'conv1', 'a.html', 'e1', 'sel', 'L', 'oldest', '{}', 'hint', '', 'open', 0, 30, 30),
      ('a2', 'p1', 'conv1', 'a.html', 'e1', 'sel', 'L', 'first', '{}', 'hint', '', 'open', 0, 10, 10),
      ('a3', 'p1', 'conv1', 'a.html', 'e2', 'sel', 'L', 'middle', '{}', 'hint', '', 'open', 0, 20, 20),
      ('b1', 'p1', 'conv1', 'b.html', 'e1', 'sel', 'L', 'other scope', '{}', 'hint', '', 'open', 0, 5, 5);
"#;

/// `sort_key` is a `REAL` column (parity with the TS `ALTER ... ADD COLUMN
/// sort_key REAL`), so it reads back as `f64` even though the backfill writes
/// integer `created_at` values.
fn pin_snapshot(conn: &Connection) -> Vec<(String, Option<i64>, Option<f64>)> {
    let mut stmt = conn
        .prepare("SELECT id, pin_seq, sort_key FROM preview_comments ORDER BY id")
        .expect("prepare");
    stmt.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .expect("query")
    .collect::<Result<Vec<_>, _>>()
    .expect("rows")
}

#[test]
fn preview_comments_pin_seq_and_sort_key_backfill() {
    let conn = open_mem();
    conn.execute_batch(PRE_PIN_PREVIEW_COMMENTS)
        .expect("legacy fixture");

    run(&conn).expect("first run");

    // Pin numbers order by created_at inside each (project, file) scope, and
    // sort_key reproduces "newest first" from created_at.
    assert_eq!(
        pin_snapshot(&conn),
        vec![
            ("a1".to_string(), Some(3), Some(30.0)),
            ("a2".to_string(), Some(1), Some(10.0)),
            ("a3".to_string(), Some(2), Some(20.0)),
            ("b1".to_string(), Some(1), Some(5.0)),
        ]
    );
    let confirmed = scalar_i64(
        &conn,
        "SELECT COUNT(*) FROM preview_comments WHERE pin_seq_confirmed = 1",
    );
    assert_eq!(confirmed, 4);

    run(&conn).expect("second run");
    assert_eq!(
        pin_snapshot(&conn),
        vec![
            ("a1".to_string(), Some(3), Some(30.0)),
            ("a2".to_string(), Some(1), Some(10.0)),
            ("a3".to_string(), Some(2), Some(20.0)),
            ("b1".to_string(), Some(1), Some(5.0)),
        ],
        "second run must not renumber"
    );

    // A partial prior run must seed from the highest pin_seq in the scope
    // instead of restarting at 1 and colliding.
    conn.execute_batch(
        "UPDATE preview_comments SET pin_seq = 7 WHERE id = 'a1';
         INSERT INTO preview_comments
           (id, project_id, conversation_id, file_path, element_id, selector, label,
            text, position_json, html_hint, note, status, slide_key, created_at, updated_at)
         VALUES
           ('a4', 'p1', 'conv1', 'a.html', 'e3', 'sel', 'L', 'late', '{}', 'hint', '', 'open', 0, 40, 40);",
    )
    .expect("partial run seed");
    run(&conn).expect("third run");
    let snapshot = pin_snapshot(&conn);
    assert!(
        snapshot.contains(&("a4".to_string(), Some(8), Some(40.0))),
        "a4 must continue from MAX(pin_seq) = 7, got {snapshot:?}"
    );

    run(&conn).expect("fourth run");
    assert_eq!(pin_snapshot(&conn), snapshot);
}

// ---------------------------------------------------------------------------
// migrateCritique
// ---------------------------------------------------------------------------

#[test]
fn critique_runs_table_created_on_legacy_database() {
    let conn = open_mem();
    assert!(!table_exists(&conn, "critique_runs"));

    run(&conn).expect("first run");
    assert!(table_exists(&conn, "critique_runs"));
    for column in [
        "id",
        "project_id",
        "conversation_id",
        "artifact_path",
        "status",
        "score",
        "rounds_json",
        "transcript_path",
        "protocol_version",
        "created_at",
        "updated_at",
    ] {
        assert!(has_column(&conn, "critique_runs", column), "missing {column}");
    }
    assert!(index_exists(&conn, "idx_critique_runs_project"));
    assert!(index_exists(&conn, "idx_critique_runs_status"));

    let before = schema_dump(&conn);
    run(&conn).expect("second run");
    assert_eq!(schema_dump(&conn), before);
}

// ---------------------------------------------------------------------------
// migrateMediaTasks
// ---------------------------------------------------------------------------

#[test]
fn media_tasks_legacy_table_gains_run_id() {
    let conn = open_mem();
    conn.execute_batch(
        "CREATE TABLE media_tasks (
           id TEXT PRIMARY KEY,
           project_id TEXT NOT NULL,
           status TEXT NOT NULL,
           started_at INTEGER NOT NULL,
           created_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL
         );
         INSERT INTO media_tasks (id, project_id, status, started_at, created_at, updated_at)
         VALUES ('m1', 'p1', 'queued', 10, 10, 10);",
    )
    .expect("legacy fixture");

    run(&conn).expect("first run");
    assert!(has_column(&conn, "media_tasks", "run_id"));
    assert!(index_exists(&conn, "idx_media_tasks_project"));
    assert!(index_exists(&conn, "idx_media_tasks_status"));
    let (status, run_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, run_id FROM media_tasks WHERE id = 'm1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("row");
    assert_eq!(status, "queued");
    assert_eq!(run_id, None, "the new column starts NULL");

    let before = row_dump(&conn, "media_tasks", "id");
    run(&conn).expect("second run");
    assert_eq!(row_dump(&conn, "media_tasks", "id"), before);
}

// ---------------------------------------------------------------------------
// migrateLibrary
// ---------------------------------------------------------------------------

#[test]
fn library_tables_created_on_legacy_database() {
    let conn = open_mem();

    run(&conn).expect("first run");
    for table in [
        "library_assets",
        "library_asset_sources",
        "library_embeddings",
        "library_tasks",
        "library_tokens",
        "library_digests",
    ] {
        assert!(table_exists(&conn, table), "missing table {table}");
    }
    assert!(has_column(&conn, "library_assets", "content_hash"));
    assert!(index_exists(&conn, "idx_library_assets_archived"));
    assert!(index_exists(&conn, "idx_library_sources_ds"));

    let before = schema_dump(&conn);
    run(&conn).expect("second run");
    assert_eq!(schema_dump(&conn), before);
}

// ---------------------------------------------------------------------------
// migratePlugins
// ---------------------------------------------------------------------------

#[test]
fn plugins_legacy_shapes_gain_marketplace_columns() {
    let conn = open_mem();
    conn.execute_batch(
        "CREATE TABLE installed_plugins (
           id TEXT PRIMARY KEY,
           title TEXT NOT NULL,
           version TEXT NOT NULL,
           source_kind TEXT NOT NULL,
           source TEXT NOT NULL,
           trust TEXT NOT NULL,
           capabilities_granted TEXT NOT NULL,
           manifest_json TEXT NOT NULL,
           fs_path TEXT NOT NULL,
           installed_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL
         );
         INSERT INTO installed_plugins
           (id, title, version, source_kind, source, trust, capabilities_granted,
            manifest_json, fs_path, installed_at, updated_at)
         VALUES ('pl1', 'Plugin', '1.0.0', 'marketplace', 'src', 'trusted', '[]', '{}', '/p', 1, 1);

         CREATE TABLE plugin_marketplaces (
           id TEXT PRIMARY KEY,
           url TEXT NOT NULL,
           trust TEXT NOT NULL,
           manifest_json TEXT NOT NULL,
           added_at INTEGER NOT NULL,
           refreshed_at INTEGER NOT NULL
         );

         CREATE TABLE applied_plugin_snapshots (
           id TEXT PRIMARY KEY,
           project_id TEXT NOT NULL,
           run_id TEXT,
           plugin_id TEXT NOT NULL,
           plugin_version TEXT NOT NULL,
           manifest_source_digest TEXT NOT NULL,
           task_kind TEXT NOT NULL,
           inputs_json TEXT NOT NULL,
           resolved_context_json TEXT NOT NULL,
           capabilities_granted TEXT NOT NULL,
           assets_staged_json TEXT NOT NULL,
           applied_at INTEGER NOT NULL
         );",
    )
    .expect("legacy fixture");

    run(&conn).expect("first run");

    for column in [
        "source_marketplace_entry_name",
        "source_marketplace_entry_version",
        "marketplace_trust",
        "resolved_source",
        "resolved_ref",
        "manifest_digest",
        "archive_integrity",
        "bundled_content_digest",
    ] {
        assert!(
            has_column(&conn, "installed_plugins", column),
            "installed_plugins missing {column}"
        );
    }
    assert!(has_column(&conn, "plugin_marketplaces", "spec_version"));
    assert!(has_column(&conn, "plugin_marketplaces", "version"));
    assert!(index_exists(&conn, "idx_marketplaces_version"));
    for column in [
        "plugin_spec_version",
        "strategy_json",
        "source_marketplace_entry_name",
        "marketplace_trust",
        "resolved_source",
        "resolved_ref",
        "archive_integrity",
        "craft_requires_json",
    ] {
        assert!(
            has_column(&conn, "applied_plugin_snapshots", column),
            "applied_plugin_snapshots missing {column}"
        );
    }
    assert!(has_column(&conn, "projects", "applied_plugin_snapshot_id"));
    assert!(has_column(&conn, "conversations", "applied_plugin_snapshot_id"));
    for table in ["run_devloop_iterations", "genui_surfaces", "skill_plugin_candidates"] {
        assert!(table_exists(&conn, table), "missing table {table}");
    }

    let before = schema_dump(&conn);
    run(&conn).expect("second run");
    assert_eq!(schema_dump(&conn), before);
}

// ---------------------------------------------------------------------------
// migrateProjectScenarioBindings
// ---------------------------------------------------------------------------

fn metadata_of(conn: &Connection, project: &str) -> Option<String> {
    conn.query_row(
        "SELECT metadata_json FROM projects WHERE id = ?1",
        [project],
        |row| row.get::<_, Option<String>>(0),
    )
    .expect("project row")
}

#[test]
fn scenario_bindings_backfilled_from_legacy_marker() {
    let conn = open_mem();
    conn.execute_batch(
        "CREATE TABLE projects (
           id TEXT PRIMARY KEY,
           name TEXT NOT NULL,
           metadata_json TEXT,
           applied_plugin_snapshot_id TEXT,
           created_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL
         );
         CREATE TABLE applied_plugin_snapshots (
           id TEXT PRIMARY KEY,
           project_id TEXT NOT NULL,
           run_id TEXT,
           plugin_id TEXT NOT NULL,
           plugin_version TEXT NOT NULL,
           applied_at INTEGER NOT NULL
         );
         INSERT INTO projects (id, name, metadata_json, created_at, updated_at) VALUES
           ('p-legacy', 'Legacy',
            '{\"automaticDefaultScenario\": true, \"title\": \"T\"}', 1, 1),
           ('p-exact', 'Exact',
            '{\"scenarioBinding\": {\"schemaVersion\": 1, \"provenance\": \"explicit_user\", \"pluginId\": \"plugin-b\", \"snapshotId\": \"snap-2\", \"boundAt\": 5}}', 1, 1),
           ('p-drop', 'Drop',
            '{\"automaticDefaultScenario\": true, \"scenarioBinding\": {\"schemaVersion\": 1, \"provenance\": \"automatic_default\", \"pluginId\": \"plugin-c\", \"snapshotId\": \"snap-3\", \"boundAt\": 9}}', 1, 1),
           ('p-broken', 'Broken', 'not json', 1, 1);
         INSERT INTO applied_plugin_snapshots (id, project_id, plugin_id, plugin_version, applied_at) VALUES
           ('snap-1', 'p-legacy', 'plugin-a', '1.0.0', 1700000000000),
           ('snap-2', 'p-exact', 'plugin-b', '1.0.0', 7),
           ('snap-3', 'p-drop', 'plugin-c', '1.0.0', 9),
           ('snap-4', 'p-broken', 'plugin-x', '1.0.0', 42);",
    )
    .expect("legacy fixture");
    conn.execute_batch(
        "UPDATE projects SET applied_plugin_snapshot_id = 'snap-1' WHERE id = 'p-legacy';
         UPDATE projects SET applied_plugin_snapshot_id = 'snap-2' WHERE id = 'p-exact';
         UPDATE projects SET applied_plugin_snapshot_id = 'snap-4' WHERE id = 'p-broken';
         UPDATE projects SET applied_plugin_snapshot_id = NULL WHERE id = 'p-drop';",
    )
    .expect("snapshot links");

    run(&conn).expect("first run");

    let legacy: serde_json::Value =
        serde_json::from_str(&metadata_of(&conn, "p-legacy").expect("legacy metadata"))
            .expect("legacy metadata json");
    assert!(
        legacy.get("automaticDefaultScenario").is_none(),
        "retired marker must be deleted: {legacy}"
    );
    assert_eq!(
        legacy["scenarioBinding"],
        serde_json::json!({
            "schemaVersion": 1,
            "provenance": "legacy_unknown",
            "pluginId": "plugin-a",
            "snapshotId": "snap-1",
            "boundAt": 1700000000000_i64,
        }),
        "binding must be folded in: {legacy}"
    );
    assert_eq!(legacy["title"], "T");

    // An exact, marker-free binding is left byte-identical.
    let exact_raw = metadata_of(&conn, "p-exact").expect("exact metadata");
    assert_eq!(
        exact_raw,
        "{\"scenarioBinding\": {\"schemaVersion\": 1, \"provenance\": \"explicit_user\", \"pluginId\": \"plugin-b\", \"snapshotId\": \"snap-2\", \"boundAt\": 5}}"
    );

    // No snapshot: the retired marker and the stale binding both go.
    assert_eq!(metadata_of(&conn, "p-drop"), None);

    // Unparseable metadata is replaced by the binding alone.
    let broken: serde_json::Value =
        serde_json::from_str(&metadata_of(&conn, "p-broken").expect("broken metadata"))
            .expect("broken metadata json");
    assert_eq!(
        broken,
        serde_json::json!({
            "scenarioBinding": {
                "schemaVersion": 1,
                "provenance": "legacy_unknown",
                "pluginId": "plugin-x",
                "snapshotId": "snap-4",
                "boundAt": 42,
            }
        })
    );

    let before = row_dump(&conn, "projects", "id");
    run(&conn).expect("second run");
    assert_eq!(row_dump(&conn, "projects", "id"), before);
    assert_eq!(metadata_of(&conn, "p-exact"), Some(exact_raw));
}

// ---------------------------------------------------------------------------
// migrateStrategyTaskStore (plus its intent / frozen-skill sub-helpers)
// ---------------------------------------------------------------------------

#[test]
fn strategy_task_store_legacy_tables_gain_columns() {
    let conn = open_mem();
    conn.execute_batch(
        "CREATE TABLE strategy_task_executions (
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
           input_stage TEXT NOT NULL,
           outcome TEXT NOT NULL,
           initial_run_id TEXT NOT NULL,
           latest_run_id TEXT NOT NULL,
           created_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL
         );
         CREATE TABLE strategy_task_runs (
           task_execution_id TEXT NOT NULL,
           run_id TEXT NOT NULL UNIQUE,
           input_stage TEXT NOT NULL,
           task_run_index INTEGER NOT NULL,
           source_run_id TEXT,
           created_at INTEGER NOT NULL,
           PRIMARY KEY(task_execution_id, task_run_index)
         );",
    )
    .expect("legacy fixture");

    run(&conn).expect("first run");

    for column in [
        "execution_intent",
        "intent_resolution_version",
        "prompt_bundle_schema",
        "prompt_bundle_text",
        "prompt_bundle_utf8_bytes",
        "prompt_bundle_sha256",
        "frozen_input_identity_json",
        "blocked_reason_codes_json",
        "blocked_visible_text",
    ] {
        assert!(
            has_column(&conn, "strategy_task_executions", column),
            "strategy_task_executions missing {column}"
        );
    }
    for column in [
        "final_text_kind",
        "final_text_schema",
        "final_text",
        "final_text_utf8_bytes",
        "final_text_sha256",
    ] {
        assert!(
            has_column(&conn, "strategy_task_runs", column),
            "strategy_task_runs missing {column}"
        );
    }
    assert!(table_exists(&conn, "strategy_task_intent_resolution"));
    assert!(table_exists(&conn, "strategy_task_run_write_evidence"));
    assert!(table_exists(&conn, "strategy_task_frozen_skill_packages"));

    let before = schema_dump(&conn);
    run(&conn).expect("second run");
    assert_eq!(schema_dump(&conn), before);
}

// ---------------------------------------------------------------------------
// migrateChatArtifacts
// ---------------------------------------------------------------------------

#[test]
fn chat_artifacts_rebuild_drops_open_policy() {
    let conn = open_mem();
    conn.execute_batch(
        "CREATE TABLE message_artifacts (
           message_id TEXT NOT NULL,
           ordinal INTEGER NOT NULL,
           id TEXT NOT NULL UNIQUE,
           snapshot_id TEXT,
           workspace_artifact_id TEXT,
           display_policy TEXT NOT NULL CHECK (display_policy IN
             ('latest_with_static_preview','immutable_snapshot')),
           open_policy TEXT NOT NULL CHECK (open_policy IN ('preview', 'direct')),
           label_at_capture TEXT NOT NULL,
           kind TEXT NOT NULL,
           html_version_id TEXT,
           created_at INTEGER NOT NULL,
           PRIMARY KEY (message_id, ordinal)
         );
         INSERT INTO message_artifacts
           (message_id, ordinal, id, display_policy, open_policy, label_at_capture,
            kind, created_at)
         VALUES
           ('msg1', 0, 'art1', 'immutable_snapshot', 'direct', 'L', 'html', 10),
           ('msg1', 1, 'art2', 'latest_with_static_preview', 'preview', 'L', 'png', 20);",
    )
    .expect("legacy fixture");

    run(&conn).expect("first run");

    assert!(!has_column(&conn, "message_artifacts", "open_policy"));
    assert!(table_exists(&conn, "workspace_artifacts"));
    assert!(table_exists(&conn, "chat_artifact_blobs"));
    assert!(table_exists(&conn, "chat_artifact_snapshots"));
    assert!(index_exists(&conn, "idx_workspace_artifacts_live_path"));
    let rows = row_dump(&conn, "message_artifacts", "id");
    assert_eq!(rows.len(), 2, "rows must survive the rebuild");
    assert!(rows.iter().any(|row| row.iter().any(|v| v == "art1")));
    assert!(rows.iter().any(|row| row.iter().any(|v| v == "art2")));

    let before = schema_dump(&conn);
    run(&conn).expect("second run");
    assert_eq!(schema_dump(&conn), before);
    assert_eq!(row_dump(&conn, "message_artifacts", "id"), rows);
}

// ---------------------------------------------------------------------------
// migrateCollabSyncSnapshots / migrateCommentRelayOutbox /
// migratePublicFilePublications
// ---------------------------------------------------------------------------

#[test]
fn collab_sync_snapshots_table_created() {
    let conn = open_mem();
    run(&conn).expect("first run");
    assert!(table_exists(&conn, "collab_sync_snapshots"));
    let before = schema_dump(&conn);
    run(&conn).expect("second run");
    assert_eq!(schema_dump(&conn), before);
}

#[test]
fn comment_relay_outbox_table_created() {
    let conn = open_mem();
    run(&conn).expect("first run");
    assert!(table_exists(&conn, "comment_relay_outbox"));
    assert!(index_exists(&conn, "idx_comment_relay_outbox_due"));
    let before = schema_dump(&conn);
    run(&conn).expect("second run");
    assert_eq!(schema_dump(&conn), before);
}

#[test]
fn public_file_publications_table_created() {
    let conn = open_mem();
    run(&conn).expect("first run");
    assert!(table_exists(&conn, "public_file_publications"));
    for column in [
        "resource_team_id",
        "owner_member_id",
        "project_id",
        "file_path",
        "url",
        "slug",
        "file_name",
        "created_at",
        "updated_at",
    ] {
        assert!(
            has_column(&conn, "public_file_publications", column),
            "missing {column}"
        );
    }
    let before = schema_dump(&conn);
    run(&conn).expect("second run");
    assert_eq!(schema_dump(&conn), before);
}

// ---------------------------------------------------------------------------
// migrateAmrTerminalReportOutbox
// ---------------------------------------------------------------------------

#[test]
fn amr_outbox_three_column_table_upgrades_and_backfills_iso() {
    let conn = open_mem();
    conn.execute_batch(
        "CREATE TABLE amr_terminal_report_outbox (
           run_id TEXT PRIMARY KEY,
           outcome TEXT NOT NULL CHECK (outcome IN ('failed', 'canceled')),
           terminal_at INTEGER NOT NULL
         );
         INSERT INTO amr_terminal_report_outbox (run_id, outcome, terminal_at) VALUES
           ('r1', 'failed', 1700000000000),
           ('r2', 'canceled', 86400000),
           ('r3', 'failed', -86400000);",
    )
    .expect("legacy fixture");

    run(&conn).expect("first run");

    for column in [
        "terminal_at_iso",
        "state",
        "attempt_count",
        "next_attempt_at",
        "version",
        "lease_until",
        "last_error_code",
        "last_error",
        "receipt",
        "created_at",
        "updated_at",
    ] {
        assert!(
            has_column(&conn, "amr_terminal_report_outbox", column),
            "missing {column}"
        );
    }
    assert!(index_exists(&conn, "idx_amr_terminal_report_outbox_due"));
    assert!(index_exists(&conn, "idx_amr_terminal_report_outbox_terminal_at"));

    let backfilled: Vec<(String, String, i64, i64, i64, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT run_id, terminal_at_iso, next_attempt_at, created_at, updated_at, state
                   FROM amr_terminal_report_outbox ORDER BY run_id",
            )
            .expect("prepare");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
    };
    assert_eq!(
        backfilled,
        vec![
            (
                "r1".to_string(),
                "2023-11-14T22:13:20.000Z".to_string(),
                1_700_000_000_000,
                1_700_000_000_000,
                1_700_000_000_000,
                "pending".to_string()
            ),
            (
                "r2".to_string(),
                "1970-01-02T00:00:00.000Z".to_string(),
                86_400_000,
                86_400_000,
                86_400_000,
                "pending".to_string()
            ),
            (
                "r3".to_string(),
                "1969-12-31T00:00:00.000Z".to_string(),
                -86_400_000,
                -86_400_000,
                -86_400_000,
                "pending".to_string()
            ),
        ],
        "`new Date(ms).toISOString()` parity plus zeroed-timestamp backfill"
    );

    let before = row_dump(&conn, "amr_terminal_report_outbox", "run_id");
    run(&conn).expect("second run");
    assert_eq!(row_dump(&conn, "amr_terminal_report_outbox", "run_id"), before);
}

// ---------------------------------------------------------------------------
// fresh Store::open
// ---------------------------------------------------------------------------

#[test]
fn fresh_store_open_applies_everything_and_is_idempotent() {
    let dir = std::env::temp_dir().join(format!(
        "od-daemon-migrate-fresh-{}-{:?}",
        std::process::id(),
        std::thread::current().id(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let db_file = dir.join("app.sqlite");

    {
        let store = Store::open(&db_file).expect("first open");
        drop(store);
    }
    let first = {
        let conn = Connection::open(&db_file).expect("reopen");
        schema_dump(&conn)
    };

    {
        let store = Store::open(&db_file).expect("second open");
        drop(store);
    }
    let conn = Connection::open(&db_file).expect("reopen again");
    assert_eq!(
        schema_dump(&conn),
        first,
        "a second open must change nothing"
    );

    // Every helper's modern marker on a fresh database.
    assert!(table_exists(&conn, "critique_runs"));
    assert!(table_exists(&conn, "media_tasks"));
    assert!(table_exists(&conn, "library_assets"));
    assert!(table_exists(&conn, "installed_plugins"));
    assert!(table_exists(&conn, "applied_plugin_snapshots"));
    assert!(table_exists(&conn, "strategy_task_executions"));
    assert!(table_exists(&conn, "strategy_task_intent_resolution"));
    assert!(table_exists(&conn, "strategy_task_frozen_skill_packages"));
    assert!(table_exists(&conn, "workspace_artifacts"));
    assert!(table_exists(&conn, "collab_sync_snapshots"));
    assert!(table_exists(&conn, "comment_relay_outbox"));
    assert!(table_exists(&conn, "public_file_publications"));
    assert!(has_column(&conn, "amr_terminal_report_outbox", "terminal_at_iso"));

    let preview_sql = table_sql(&conn, "preview_comments");
    assert!(preview_sql.contains("slide_key"), "missing slide_key");
    assert!(
        !preview_sql.to_uppercase().contains("UNIQUE"),
        "legacy unique must not exist: {preview_sql}"
    );
    for column in ["anchor_state", "pin_seq", "pin_seq_confirmed", "sort_key"] {
        assert!(
            has_column(&conn, "preview_comments", column),
            "missing {column}"
        );
    }
    let workspace_sql = table_sql(&conn, "workspace_projects");
    assert!(
        workspace_sql.contains("project_id TEXT PRIMARY KEY"),
        "not narrowed: {workspace_sql}"
    );
    assert!(!table_exists(&conn, "workspace_projects_legacy_multi_workspace"));
    assert!(!table_exists(&conn, "preview_comments_next"));
    assert!(!table_exists(&conn, "preview_comments_multi_next"));
    assert!(!table_exists(&conn, "message_artifacts__rebuild"));
    assert!(!has_column(&conn, "message_artifacts", "open_policy"));

    let _ = std::fs::remove_dir_all(&dir);
}

