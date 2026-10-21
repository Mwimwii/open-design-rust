//! Media task read model — parity port of the read half of
//! `apps/daemon/src/media/tasks.ts` plus `media/task-batches.ts`.
//!
//! The media executors themselves are not ported yet (see the module note at
//! the bottom of `routes.rs`'s media section); this module owns the durable
//! row shape the daemon already stores (`media_tasks`, created by
//! `migrations::migrate_media_tasks`) and the `N/M` batch coordinates the
//! project task list renders.
//!
//! DOCUMENTED DEVIATIONS
//!
//! * `normalizeError` validates `subject`/`nextStep` against the contract
//!   allowlists exactly like TS. `retryable` survives only when the column
//!   holds an explicit boolean.
//! * The task-list route's moved-file recovery (re-resolving a `done` task's
//!   file by size/mtime across the current project inventory) is not ported;
//!   a task whose recorded name no longer resolves simply omits `file`, which
//!   is the same shape TS produces when the project root is unavailable.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::storage::{Store, StoreError};

pub const TERMINAL_STATUSES: &[&str] = &["done", "failed", "interrupted"];

const MEDIA_TASK_ERROR_SUBJECTS: &[&str] = &["prompt", "input_image", "output_image"];
const MEDIA_FAILURE_NEXT_STEPS: &[&str] = &[
    "revise-request",
    "switch-model",
    "open-settings",
    "sign-in",
    "add-credit",
    "retry-later",
];

/// One normalized `media_tasks` row (parity: `MediaTaskRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct MediaTaskRow {
    pub id: String,
    pub project_id: String,
    pub sequence: i64,
    pub run_id: Option<String>,
    pub status: String,
    pub surface: Option<String>,
    pub model: Option<String>,
    pub progress: Vec<String>,
    pub file: Value,
    pub error: Value,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl MediaTaskRow {
    pub fn is_terminal(&self) -> bool {
        TERMINAL_STATUSES.contains(&self.status.as_str())
    }
}

const COLS: &str = "id, rowid AS sequence, project_id, run_id, status, surface, model, \
progress_json, file_json, error_json, started_at, ended_at, created_at, updated_at";

fn parse_json(raw: Option<String>) -> Value {
    match raw {
        Some(raw) if !raw.is_empty() => serde_json::from_str(&raw).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn parse_progress(raw: Option<String>) -> Vec<String> {
    match parse_json(raw) {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Parity: `normalizeError`. Every stored field is reconstructed so a restart
/// does not silently drop attribution.
pub fn normalize_media_task_error(value: &Value) -> Value {
    let Some(obj) = value.as_object() else {
        return Value::Null;
    };
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if message.is_empty() {
        return Value::Null;
    }
    let mut out = Map::new();
    out.insert("message".to_string(), json!(message));
    if let Some(status) = obj.get("status").and_then(Value::as_i64) {
        out.insert("status".to_string(), json!(status));
    }
    if let Some(code) = obj.get("code").and_then(Value::as_str) {
        out.insert("code".to_string(), json!(code));
    }
    if let Some(subject) = obj
        .get("subject")
        .and_then(Value::as_str)
        .filter(|subject| MEDIA_TASK_ERROR_SUBJECTS.contains(subject))
    {
        out.insert("subject".to_string(), json!(subject));
    }
    if let Some(retryable) = obj.get("retryable").and_then(Value::as_bool) {
        out.insert("retryable".to_string(), json!(retryable));
    }
    if let Some(next_step) = obj
        .get("nextStep")
        .and_then(Value::as_str)
        .filter(|step| MEDIA_FAILURE_NEXT_STEPS.contains(step))
    {
        out.insert("nextStep".to_string(), json!(next_step));
    }
    Value::Object(out)
}

fn normalize_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MediaTaskRow> {
    let progress_json: Option<String> = row.get("progress_json")?;
    let file_json: Option<String> = row.get("file_json")?;
    let error_json: Option<String> = row.get("error_json")?;
    Ok(MediaTaskRow {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        sequence: row.get("sequence")?,
        run_id: row.get("run_id")?,
        status: row.get("status")?,
        surface: row.get("surface")?,
        model: row.get("model")?,
        progress: parse_progress(progress_json),
        file: parse_json(file_json),
        error: normalize_media_task_error(&parse_json(error_json)),
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// Parity: `listMediaTasksByProject` — newest first, terminal rows included
/// only on request.
pub fn list_media_tasks_by_project(
    store: &Store,
    project_id: &str,
    include_terminal: bool,
) -> Result<Vec<MediaTaskRow>, StoreError> {
    let sql = format!(
        "SELECT {COLS} FROM media_tasks WHERE project_id = ?1 ORDER BY started_at DESC"
    );
    let rows = store.query(&sql, [project_id], normalize_row)?;
    Ok(rows
        .into_iter()
        .filter(|row| include_terminal || !row.is_terminal())
        .collect())
}

/// Parity: `MediaTaskBatchMembership`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaTaskBatchMembership {
    pub batch_id: String,
    pub batch_index: usize,
    pub batch_size: usize,
}

/// Parity: `assignMediaTaskBatches` — a maximal run of same-run, same-surface
/// tasks whose lifetimes overlap. A task with no run id can never join
/// another task's batch.
pub fn assign_media_task_batches(
    tasks: &[MediaTaskRow],
) -> HashMap<String, MediaTaskBatchMembership> {
    let mut ordered: Vec<&MediaTaskRow> = tasks.iter().collect();
    ordered.sort_by_key(|task| task.sequence);

    // Shared member lists, exactly like the TS implementation: a joining task
    // mutates the batch it joined, so `batchSize` is only known at the end.
    let mut batches: Vec<Vec<String>> = Vec::new();
    // key -> (index into `batches`, the latest end observed in that batch).
    let mut open: HashMap<String, (usize, i64)> = HashMap::new();

    for task in ordered {
        let owner = match task.run_id.as_deref() {
            Some(run_id) if !run_id.is_empty() => format!("run:{run_id}"),
            _ => format!("task:{}", task.id),
        };
        let key = format!("{owner} {}", task.surface.as_deref().unwrap_or(""));
        // A task that has not ended yet overlaps everything created after it.
        let ends_at = task.ended_at.unwrap_or(i64::MAX);
        if let Some(&(index, current_ends)) = open.get(&key) {
            if task.started_at < current_ends {
                batches[index].push(task.id.clone());
                open.insert(key, (index, current_ends.max(ends_at)));
                continue;
            }
        }
        batches.push(vec![task.id.clone()]);
        open.insert(key, (batches.len() - 1, ends_at));
    }

    let mut membership = HashMap::new();
    for members in &batches {
        let Some(first) = members.first() else {
            continue;
        };
        for (index, id) in members.iter().enumerate() {
            membership.insert(
                id.clone(),
                MediaTaskBatchMembership {
                    batch_id: first.clone(),
                    batch_index: index + 1,
                    batch_size: members.len(),
                },
            );
        }
    }
    membership
}

/// The `file` metadata for a confirmed project file (parity:
/// `reconciledMediaTaskFile`).
pub fn reconciled_media_task_file(task_file: &Value, project_file: &Value) -> Value {
    let mut out = task_file.as_object().cloned().unwrap_or_default();
    for key in ["name", "size", "mtime", "kind", "mime"] {
        if let Some(value) = project_file.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(out)
}

/// Build the `GET /api/projects/:id/media/tasks` payload.
///
/// `confirmed_files` maps a task id to the project-file metadata the caller
/// resolved from disk. Tasks without an entry omit `file`, matching TS when
/// the recorded name no longer resolves.
pub fn media_task_list_payload(
    rows: &[MediaTaskRow],
    confirmed_files: &HashMap<String, Value>,
    now_ms: i64,
) -> Value {
    let batches = assign_media_task_batches(rows);
    let mut tasks: Vec<Value> = rows
        .iter()
        .map(|task| {
            let mut out = Map::new();
            out.insert("taskId".to_string(), json!(task.id));
            out.insert("sequence".to_string(), json!(task.sequence));
            if let Some(batch) = batches.get(&task.id) {
                out.insert("batchId".to_string(), json!(batch.batch_id));
                out.insert("batchIndex".to_string(), json!(batch.batch_index));
                out.insert("batchSize".to_string(), json!(batch.batch_size));
            }
            if let Some(run_id) = task.run_id.as_deref().filter(|id| !id.is_empty()) {
                out.insert("runId".to_string(), json!(run_id));
            }
            out.insert("status".to_string(), json!(task.status));
            out.insert("startedAt".to_string(), json!(task.started_at));
            out.insert(
                "endedAt".to_string(),
                task.ended_at.map(|value| json!(value)).unwrap_or(Value::Null),
            );
            let end = task.ended_at.unwrap_or(now_ms);
            out.insert("elapsed".to_string(), json!(((end - task.started_at) / 1000).max(0)));
            out.insert(
                "surface".to_string(),
                task.surface.as_ref().map(|value| json!(value)).unwrap_or(Value::Null),
            );
            out.insert(
                "model".to_string(),
                task.model.as_ref().map(|value| json!(value)).unwrap_or(Value::Null),
            );
            let recent: Vec<Value> = task
                .progress
                .iter()
                .rev()
                .take(3)
                .rev()
                .map(|line| json!(line))
                .collect();
            out.insert("progress".to_string(), Value::Array(recent));
            out.insert("progressCount".to_string(), json!(task.progress.len()));
            if let Some(project_file) = confirmed_files.get(&task.id) {
                out.insert(
                    "file".to_string(),
                    reconciled_media_task_file(&task.file, project_file),
                );
            }
            if task.status == "failed" || task.status == "interrupted" {
                out.insert("error".to_string(), task.error.clone());
            }
            Value::Object(out)
        })
        .collect();
    // Newest first, with creation order breaking parallel fan-out ties.
    tasks.sort_by(|left, right| {
        let left_started = left["startedAt"].as_i64().unwrap_or(0);
        let right_started = right["startedAt"].as_i64().unwrap_or(0);
        right_started
            .cmp(&left_started)
            .then_with(|| {
                let left_seq = left["sequence"].as_i64().unwrap_or(0);
                let right_seq = right["sequence"].as_i64().unwrap_or(0);
                right_seq.cmp(&left_seq)
            })
    });
    json!({ "tasks": tasks })
}

/// Names a task still claims, used by callers that need to avoid a moved-file
/// collision (parity: the `claimedNames` set in the TS route).
pub fn claimed_task_file_names(rows: &[MediaTaskRow]) -> HashSet<String> {
    rows.iter()
        .filter_map(|row| {
            row.file
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, run: Option<&str>, surface: &str, started: i64, ended: Option<i64>, seq: i64) -> MediaTaskRow {
        MediaTaskRow {
            id: id.to_string(),
            project_id: "p1".to_string(),
            sequence: seq,
            run_id: run.map(str::to_string),
            status: if ended.is_some() { "done".to_string() } else { "running".to_string() },
            surface: Some(surface.to_string()),
            model: Some("m".to_string()),
            progress: vec!["a".to_string(), "b".to_string(), "c".to_string(), "d".to_string()],
            file: json!({ "name": format!("{id}.png"), "size": 1 }),
            error: Value::Null,
            started_at: started,
            ended_at: ended,
            created_at: started,
            updated_at: started,
        }
    }

    #[test]
    fn batches_group_overlapping_same_run_tasks() {
        // Two overlapping tasks in run r1 share a batch; the third starts
        // after both ended, so it opens its own.
        let rows = vec![
            row("a", Some("r1"), "image", 100, Some(300), 1),
            row("b", Some("r1"), "image", 200, Some(400), 2),
            row("c", Some("r1"), "image", 500, Some(600), 3),
        ];
        let batches = assign_media_task_batches(&rows);
        assert_eq!(batches["a"].batch_id, "a");
        assert_eq!(batches["a"].batch_size, 2);
        assert_eq!(batches["b"].batch_index, 2);
        assert_eq!(batches["c"].batch_size, 1);
        assert_eq!(batches["c"].batch_index, 1);
    }

    #[test]
    fn a_task_without_a_run_never_joins_another_batch() {
        let rows = vec![
            row("a", None, "image", 100, None, 1),
            row("b", None, "image", 150, None, 2),
        ];
        let batches = assign_media_task_batches(&rows);
        assert_eq!(batches["a"].batch_size, 1);
        assert_eq!(batches["b"].batch_size, 1);
    }

    #[test]
    fn payload_keeps_the_last_three_progress_lines_and_elapsed_seconds() {
        let rows = vec![row("a", Some("r1"), "image", 1_000, Some(4_500), 1)];
        let payload = media_task_list_payload(&rows, &HashMap::new(), 9_999);
        let task = &payload["tasks"][0];
        assert_eq!(task["taskId"], "a");
        assert_eq!(task["elapsed"], 3);
        assert_eq!(task["progress"], json!(["b", "c", "d"]));
        assert_eq!(task["progressCount"], 4);
        assert_eq!(task["batchIndex"], 1);
        assert!(task["file"].is_null());
        assert!(task.get("error").is_none());
    }

    #[test]
    fn running_tasks_use_now_for_elapsed_and_failed_ones_carry_error() {
        let mut running = row("a", Some("r1"), "video", 1_000, None, 1);
        running.status = "running".to_string();
        let mut failed = row("b", Some("r1"), "video", 2_000, Some(2_500), 2);
        failed.status = "failed".to_string();
        failed.error = normalize_media_task_error(&json!({
            "message": "nope",
            "code": "BAD_REQUEST",
            "subject": "prompt",
            "retryable": false,
            "nextStep": "revise-request",
            "ignored": true,
        }));
        let payload = media_task_list_payload(&[running, failed], &HashMap::new(), 5_000);
        let tasks = payload["tasks"].as_array().unwrap();
        assert_eq!(tasks[0]["taskId"], "b");
        assert_eq!(tasks[0]["error"]["nextStep"], "revise-request");
        assert_eq!(tasks[0]["error"]["retryable"], false);
        assert!(tasks[0]["error"].get("ignored").is_none());
        assert_eq!(tasks[1]["elapsed"], 4);
        assert_eq!(tasks[1]["endedAt"], Value::Null);
    }

    #[test]
    fn confirmed_files_override_the_recorded_name_and_metadata() {
        let rows = vec![row("a", Some("r1"), "image", 1, Some(2), 1)];
        let mut confirmed = HashMap::new();
        confirmed.insert(
            "a".to_string(),
            json!({ "name": "moved.png", "size": 9, "mtime": 7, "kind": "image", "mime": "image/png" }),
        );
        let payload = media_task_list_payload(&rows, &confirmed, 3);
        assert_eq!(payload["tasks"][0]["file"]["name"], "moved.png");
        assert_eq!(payload["tasks"][0]["file"]["size"], 9);
    }

    #[test]
    fn error_normalization_drops_unknown_subjects_and_steps() {
        assert_eq!(normalize_media_task_error(&Value::Null), Value::Null);
        assert_eq!(
            normalize_media_task_error(&json!({ "message": "" })),
            Value::Null
        );
        let error = normalize_media_task_error(&json!({
            "message": "bad",
            "subject": "made-up",
            "nextStep": "made-up",
        }));
        assert!(error.get("subject").is_none());
        assert!(error.get("nextStep").is_none());
    }
}
