//! Tool-loop guard — parity port of `apps/daemon/src/tool-loop-guard.ts`.
//!
//! Observes the normalized `tool_use` / `tool_result` events every agent lane
//! emits and returns a verdict when a run looks stuck repeating failing calls.
//! Two independent triggers:
//!
//! * **consecutive errors** — N errored results in a row with no successful
//!   tool call between them;
//! * **repeated failure** — the same `(tool, action)` signature errors K times.
//!
//! WARN fires once, HALT fires once (and supersedes WARN), and a halted guard
//! is inert. `halt` is opt-in through `OD_TOOL_LOOP_GUARD=halt`; the default is
//! `warn`.
//!
//! DOCUMENTED DEVIATIONS
//!
//! * `displayToolSignature` truncates by `char` count, not UTF-16 code units.
//!   The cap exists to keep the emitted event readable, so a 160-char display
//!   form is the same product behavior.
//! * `collapseWhitespace` uses Rust's `char::is_whitespace` rather than a
//!   literal JS `\s` class; the two disagree only on `\u{0085}`/`\u{FEFF}`,
//!   which do not occur in tool names or commands.
//! * `JSON.stringify` key order for integer-like object keys differs from
//!   `serde_json`'s insertion order. Tool inputs do not use integer keys.

use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};

/// Why the guard tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLoopReason {
    ConsecutiveErrors,
    RepeatedFailure,
}

impl ToolLoopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolLoopReason::ConsecutiveErrors => "consecutive-errors",
            ToolLoopReason::RepeatedFailure => "repeated-failure",
        }
    }
}

/// What the run loop should do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLoopAction {
    Warn,
    Halt,
}

impl ToolLoopAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolLoopAction::Warn => "warn",
            ToolLoopAction::Halt => "halt",
        }
    }
}

/// Operating mode (`OD_TOOL_LOOP_GUARD`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLoopMode {
    Off,
    Warn,
    Halt,
}

/// The verdict returned the instant a threshold is crossed (parity:
/// `ToolLoopVerdict`, shaped like the `tool_loop` SSE payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLoopVerdict {
    pub reason: ToolLoopReason,
    pub action: ToolLoopAction,
    pub tool_name: String,
    pub signature: String,
    pub count: usize,
}

impl ToolLoopVerdict {
    /// The `tool_loop` agent-event payload, with the run id attached the same
    /// way [`crate::runs`] attaches it to every lane event.
    pub fn to_json(&self, run_id: &str) -> Value {
        json!({
            "runId": run_id,
            "type": "tool_loop",
            "reason": self.reason.as_str(),
            "action": self.action.as_str(),
            "toolName": self.tool_name,
            "signature": self.signature,
            "count": self.count,
        })
    }
}

/// Threshold overrides (parity: `ToolLoopGuardOptions`).
#[derive(Debug, Clone, Copy)]
pub struct ToolLoopGuardOptions {
    pub warn_consecutive: usize,
    pub halt_consecutive: usize,
    pub warn_repeat: usize,
    pub halt_repeat: usize,
    pub mode: ToolLoopMode,
}

impl Default for ToolLoopGuardOptions {
    fn default() -> Self {
        Self {
            warn_consecutive: 5,
            halt_consecutive: 10,
            warn_repeat: 4,
            halt_repeat: 8,
            mode: ToolLoopMode::Warn,
        }
    }
}

/// Cap on the tool_use bookkeeping map.
const MAX_TRACKED_USES: usize = 512;
/// Cap on the per-signature failure tally.
const MAX_TRACKED_SIGNATURES: usize = 512;
/// Display-form cap for the emitted signature.
const SIGNATURE_MAX_LEN: usize = 160;

const READ_ONLY_TOOL_NAMES: &[&str] = &[
    "read",
    "glob",
    "ls",
    "grep",
    "notebookread",
    "webfetch",
    "websearch",
    "todowrite",
    "askuserquestion",
];

const SHELL_TOOL_NAMES: &[&str] = &[
    "bash",
    "shell",
    "sh",
    "run_command",
    "run_terminal_cmd",
    "execute_command",
    "terminal",
];

const READ_ONLY_SHELL_BINARIES: &[&str] = &[
    "cat",
    "ls",
    "grep",
    "rg",
    "egrep",
    "fgrep",
    "head",
    "tail",
    "pwd",
    "echo",
    "printf",
    "which",
    "type",
    "wc",
    "stat",
    "file",
    "tree",
    "diff",
    "jq",
    "awk",
    "date",
    "test",
    "true",
    "false",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "cut",
    "sort",
    "uniq",
    "column",
    "cmp",
];

fn mutating_find_action() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\s-(?:delete|exec|execdir|ok|okdir|fprint\w*|fls)\b").expect("find regex")
    })
}

fn in_place_edit() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\s-[a-z]*i|\s--in-place").expect("in-place regex"))
}

fn read_only_git_subcommand() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\bgit\s+(status|diff|log|show|branch|ls-files|rev-parse|describe|blame|cat-file|remote)\b",
        )
        .expect("git regex")
    })
}

fn shell_head_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_./-]+").expect("shell head regex"))
}

fn env_prefix_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?:[A-Za-z0-9_./-]*/)?env\b").expect("env prefix regex")
    })
}

fn env_assignment_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=").expect("env assignment regex"))
}

fn dev_null_redirect() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\d*>\s*/dev/null").expect("dev null regex"))
}

fn fd_redirect() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\d*>&\d+").expect("fd redirect regex"))
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn shell_head(segment: &str) -> String {
    let trimmed = segment.trim();
    shell_head_pattern()
        .find(trimmed)
        .map(|m| m.as_str())
        .unwrap_or("")
        .to_lowercase()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Strip a leading `env [flags] KEY=VALUE...` wrapper so the real subcommand is
/// what gets classified.
fn unwrap_env_prefix(segment: &str) -> String {
    let mut rest = segment.trim().to_string();
    while shell_head(&rest) == "env" {
        let Some(m) = env_prefix_pattern().find(&rest) else {
            break;
        };
        let after = rest[m.end()..].trim().to_string();
        let tokens: Vec<&str> = if after.is_empty() {
            Vec::new()
        } else {
            after.split_whitespace().collect()
        };
        let mut index = 0usize;
        while index < tokens.len() {
            let token = tokens[index];
            if token == "-u" || token == "--unset" {
                index += 2;
                continue;
            }
            if token.starts_with('-') {
                index += 1;
                continue;
            }
            if env_assignment_pattern().is_match(token) {
                index += 1;
                continue;
            }
            break;
        }
        let next = tokens
            .get(index..)
            .map(|slice| slice.join(" "))
            .unwrap_or_default();
        if next == rest {
            break;
        }
        rest = next;
    }
    rest
}

/// Parity: `isReadOnlyShellCommand`. Anything not recognised as a pure
/// inspection is treated as a state change (progress), so the guard never
/// halts a run that is genuinely fixing things through the shell.
pub fn is_read_only_shell_command(command: &str) -> bool {
    let cmd = command.trim();
    if cmd.is_empty() {
        return false;
    }
    let redirs = dev_null_redirect().replace_all(cmd, "");
    let redirs = fd_redirect().replace_all(&redirs, "");
    if redirs.contains('>') {
        return false;
    }
    for raw_seg in cmd.split("||").flat_map(|s| s.split("&&")).flat_map(|s| {
        // `[;|]` in the TS split — split on either character.
        s.split([';', '|'])
    }) {
        let seg = unwrap_env_prefix(raw_seg);
        if seg.trim().is_empty() {
            continue;
        }
        let head = shell_head(&seg);
        if head.is_empty() {
            return false;
        }
        if head == "find" {
            if mutating_find_action().is_match(&seg) {
                return false;
            }
            continue;
        }
        if head == "sed" || head == "perl" {
            if in_place_edit().is_match(&seg) {
                return false;
            }
            continue;
        }
        if head == "git" {
            if read_only_git_subcommand().is_match(&seg) {
                continue;
            }
            return false;
        }
        if !READ_ONLY_SHELL_BINARIES.contains(&head.as_str()) {
            return false;
        }
    }
    true
}

/// Parity: `isProgressSuccess` — a successful call that changed state clears
/// the repeated-failure tally; a read-only inspection does not.
pub fn is_progress_success(name: &str, signature: &str) -> bool {
    let lower = name.trim().to_lowercase();
    if READ_ONLY_TOOL_NAMES.contains(&lower.as_str()) {
        return false;
    }
    if SHELL_TOOL_NAMES.contains(&lower.as_str()) {
        let prefix = format!("{name} ");
        let command = signature.strip_prefix(&prefix).unwrap_or(signature);
        return !is_read_only_shell_command(command);
    }
    true
}

/// Parity: `computeToolSignature` — the stable dedup key for a tool call.
pub fn compute_tool_signature(name: &str, input: &Value) -> String {
    let tool_name = if name.is_empty() { "tool" } else { name };
    let mut detail = String::new();
    if let Some(record) = input.as_object() {
        let command = record.get("command").and_then(Value::as_str);
        let file_path = record
            .get("file_path")
            .or_else(|| record.get("filePath"))
            .or_else(|| record.get("path"))
            .and_then(Value::as_str);
        let old_string = record
            .get("old_string")
            .or_else(|| record.get("oldString"))
            .and_then(Value::as_str);
        if let Some(command) = command {
            detail = command.to_string();
        } else if let Some(file_path) = file_path {
            detail = match old_string {
                Some(old_string) => format!("{file_path} :: {old_string}"),
                None => file_path.to_string(),
            };
        } else {
            let serialized = serde_json::to_string(input).unwrap_or_default();
            if serialized != "{}" {
                detail = serialized;
            }
        }
    } else if let Some(text) = input.as_str() {
        detail = text.to_string();
    }
    if detail.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name} {}", collapse_whitespace(&detail))
    }
}

/// Parity: `displayToolSignature` — the bounded form surfaced in the event.
pub fn display_tool_signature(signature: &str) -> String {
    if signature.chars().count() > SIGNATURE_MAX_LEN {
        let mut display: String = signature.chars().take(SIGNATURE_MAX_LEN - 1).collect();
        display.push('…');
        display
    } else {
        signature.to_string()
    }
}

/// Resolve the guard mode from the environment (parity:
/// `resolveToolLoopMode`). Unrecognised values fall back to `warn`.
pub fn resolve_tool_loop_mode(env: Option<&str>) -> ToolLoopMode {
    match env.unwrap_or("").trim().to_lowercase().as_str() {
        "off" => ToolLoopMode::Off,
        "halt" => ToolLoopMode::Halt,
        _ => ToolLoopMode::Warn,
    }
}

#[derive(Debug, Clone)]
struct TrackedUse {
    name: String,
    signature: String,
}

/// Run-scoped tool-loop guard (parity: `ToolLoopGuard`).
#[derive(Debug)]
pub struct ToolLoopGuard {
    options: ToolLoopGuardOptions,
    uses: HashMap<String, TrackedUse>,
    use_order: VecDeque<String>,
    fail_counts: HashMap<String, usize>,
    fail_order: VecDeque<String>,
    consecutive_errors: usize,
    warned: bool,
    halted: bool,
}

impl ToolLoopGuard {
    pub fn new(options: ToolLoopGuardOptions) -> Self {
        Self {
            options,
            uses: HashMap::new(),
            use_order: VecDeque::new(),
            fail_counts: HashMap::new(),
            fail_order: VecDeque::new(),
            consecutive_errors: 0,
            warned: false,
            halted: false,
        }
    }

    pub fn warned(&self) -> bool {
        self.warned
    }

    pub fn halted(&self) -> bool {
        self.halted
    }

    /// Record a tool call so a later errored result can be attributed to its
    /// action signature. Safe to call with unknown/missing input.
    pub fn observe_tool_use(&mut self, id: &str, name: &str, input: &Value) {
        if self.options.mode == ToolLoopMode::Off || self.halted || id.is_empty() {
            return;
        }
        let name = if name.is_empty() { "tool" } else { name };
        let signature = compute_tool_signature(name, input);
        if !self.uses.contains_key(id) {
            self.use_order.push_back(id.to_string());
        }
        self.uses.insert(
            id.to_string(),
            TrackedUse {
                name: name.to_string(),
                signature,
            },
        );
        while self.uses.len() > MAX_TRACKED_USES {
            let Some(oldest) = self.use_order.pop_front() else {
                break;
            };
            self.uses.remove(&oldest);
        }
    }

    /// Record a tool result. Returns a verdict the first time WARN is crossed
    /// and again the first time HALT is crossed; otherwise `None`.
    pub fn observe_tool_result(
        &mut self,
        tool_use_id: &str,
        is_error: bool,
        content: &str,
    ) -> Option<ToolLoopVerdict> {
        if self.options.mode == ToolLoopMode::Off || self.halted {
            return None;
        }
        let use_record = if tool_use_id.is_empty() {
            None
        } else {
            self.uses.remove(tool_use_id)
        };

        if !is_error {
            self.consecutive_errors = 0;
            if let Some(use_record) = &use_record {
                if is_progress_success(&use_record.name, &use_record.signature) {
                    self.fail_counts.clear();
                    self.fail_order.clear();
                } else if self.fail_counts.contains_key(&use_record.signature) {
                    self.fail_counts.remove(&use_record.signature);
                }
            }
            return None;
        }

        let tool_name = use_record
            .as_ref()
            .map(|record| record.name.clone())
            .unwrap_or_else(|| "tool".to_string());
        let signature = use_record
            .as_ref()
            .map(|record| record.signature.clone())
            .unwrap_or_else(|| compute_tool_signature(&tool_name, &json!(content)));

        self.consecutive_errors += 1;
        let repeat_count = self.fail_counts.get(&signature).copied().unwrap_or(0) + 1;
        if !self.fail_counts.contains_key(&signature) {
            self.fail_order.push_back(signature.clone());
        }
        self.fail_counts.insert(signature.clone(), repeat_count);
        while self.fail_counts.len() > MAX_TRACKED_SIGNATURES {
            let Some(oldest) = self.fail_order.pop_front() else {
                break;
            };
            self.fail_counts.remove(&oldest);
        }
        let display_signature = display_tool_signature(&signature);

        if self.options.mode == ToolLoopMode::Halt {
            let repeat_halt = repeat_count >= self.options.halt_repeat;
            let consecutive_halt = self.consecutive_errors >= self.options.halt_consecutive;
            if repeat_halt || consecutive_halt {
                self.halted = true;
                self.warned = true;
                return Some(if repeat_halt {
                    ToolLoopVerdict {
                        reason: ToolLoopReason::RepeatedFailure,
                        action: ToolLoopAction::Halt,
                        tool_name,
                        signature: display_signature,
                        count: repeat_count,
                    }
                } else {
                    ToolLoopVerdict {
                        reason: ToolLoopReason::ConsecutiveErrors,
                        action: ToolLoopAction::Halt,
                        tool_name,
                        signature: display_signature,
                        count: self.consecutive_errors,
                    }
                });
            }
        }

        if !self.warned {
            let repeat_warn = repeat_count >= self.options.warn_repeat;
            let consecutive_warn = self.consecutive_errors >= self.options.warn_consecutive;
            if repeat_warn || consecutive_warn {
                self.warned = true;
                return Some(if repeat_warn {
                    ToolLoopVerdict {
                        reason: ToolLoopReason::RepeatedFailure,
                        action: ToolLoopAction::Warn,
                        tool_name,
                        signature: display_signature,
                        count: repeat_count,
                    }
                } else {
                    ToolLoopVerdict {
                        reason: ToolLoopReason::ConsecutiveErrors,
                        action: ToolLoopAction::Warn,
                        tool_name,
                        signature: display_signature,
                        count: self.consecutive_errors,
                    }
                });
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> ToolLoopGuard {
        ToolLoopGuard::new(ToolLoopGuardOptions::default())
    }

    fn observe_failure(guard: &mut ToolLoopGuard, index: usize, name: &str, input: Value) -> Option<ToolLoopVerdict> {
        let id = format!("t{index}");
        guard.observe_tool_use(&id, name, &input);
        guard.observe_tool_result(&id, true, "boom")
    }

    #[test]
    fn signature_prefers_the_action_identifying_field() {
        assert_eq!(
            compute_tool_signature("Bash", &json!({ "command": "  pnpm   test  " })),
            "Bash pnpm test"
        );
        assert_eq!(
            compute_tool_signature(
                "Edit",
                &json!({ "file_path": "a.ts", "old_string": "x", "new_string": "y" })
            ),
            "Edit a.ts :: x"
        );
        assert_eq!(
            compute_tool_signature("Read", &json!({ "filePath": "b.ts" })),
            "Read b.ts"
        );
        assert_eq!(
            compute_tool_signature("ExitPlanMode", &json!({})),
            "ExitPlanMode"
        );
        assert_eq!(compute_tool_signature("", &json!("hi")), "tool hi");
        assert_eq!(compute_tool_signature("Glob", &Value::Null), "Glob");
    }

    #[test]
    fn display_signature_is_bounded_but_the_key_is_not() {
        let long = "Bash ".to_string() + &"x".repeat(400);
        let display = display_tool_signature(&long);
        assert_eq!(display.chars().count(), SIGNATURE_MAX_LEN);
        assert!(display.ends_with('…'));
        assert!(
            compute_tool_signature("Bash", &json!({ "command": "x".repeat(400) }))
                .chars()
                .count()
                > 160
        );
    }

    #[test]
    fn read_only_shell_classification_matches_ts_examples() {
        for command in [
            "ls -la",
            "cat a.txt | grep x",
            "git status",
            "git diff HEAD -- a.ts",
            "find . -name '*.ts'",
            "sed -n '1,5p' a.txt",
            "env CI=1 grep -r foo .",
            "echo hi >/dev/null 2>&1",
        ] {
            assert!(is_read_only_shell_command(command), "{command}");
        }
        for command in [
            "sed -i 's/a/b/' a.ts",
            "perl -pi -e 's/a/b/' a.ts",
            "git commit -m x",
            "find . -delete",
            "find . -exec rm {} \\;",
            "pnpm install",
            "echo hi > out.txt",
            "python3 -c 'print(1)'",
            "env NODE_ENV=production pnpm install",
            "(sed -i 's/a/b/' a.ts)",
            "",
        ] {
            assert!(!is_read_only_shell_command(command), "{command}");
        }
    }

    #[test]
    fn progress_success_clears_tallies_only_for_real_changes() {
        assert!(!is_progress_success("Read", "Read a.ts"));
        assert!(!is_progress_success("Bash", "Bash ls -la"));
        assert!(is_progress_success("Bash", "Bash sed -i s/a/b/ a.ts"));
        assert!(is_progress_success("Edit", "Edit a.ts :: x"));
        assert!(!is_progress_success("TodoWrite", "TodoWrite"));
        assert!(!is_progress_success("todowrite", "todowrite"));
    }

    #[test]
    fn consecutive_errors_warn_once_then_keep_counting() {
        let mut guard = guard();
        for index in 0..4 {
            assert!(observe_failure(
                &mut guard,
                index,
                "Bash",
                json!({ "command": format!("false {index}") })
            )
            .is_none());
        }
        let verdict = observe_failure(&mut guard, 4, "Bash", json!({ "command": "false 4" }))
            .expect("warn at five");
        assert_eq!(verdict.reason, ToolLoopReason::ConsecutiveErrors);
        assert_eq!(verdict.action, ToolLoopAction::Warn);
        assert_eq!(verdict.count, 5);
        assert!(guard.warned());
        assert!(!guard.halted());
        // Latching: no second warning.
        assert!(observe_failure(&mut guard, 5, "Bash", json!({ "command": "false 5" })).is_none());
    }

    #[test]
    fn repeated_identical_failures_warn_sooner_than_a_mixed_streak() {
        let mut guard = guard();
        for index in 0..3 {
            assert!(observe_failure(
                &mut guard,
                index,
                "Edit",
                json!({ "file_path": "a.ts", "old_string": "x" })
            )
            .is_none());
        }
        let verdict = observe_failure(
            &mut guard,
            3,
            "Edit",
            json!({ "file_path": "a.ts", "old_string": "x" }),
        )
        .expect("warn at four repeats");
        assert_eq!(verdict.reason, ToolLoopReason::RepeatedFailure);
        assert_eq!(verdict.count, 4);
        assert_eq!(verdict.tool_name, "Edit");
        assert!(verdict.signature.starts_with("Edit a.ts :: x"));
    }

    #[test]
    fn read_only_success_does_not_clear_a_fixation_loop() {
        let mut guard = guard();
        let failing = json!({ "file_path": "a.ts", "old_string": "x" });
        for index in 0..3 {
            assert!(observe_failure(&mut guard, index, "Edit", failing.clone()).is_none());
            guard.observe_tool_use(&format!("r{index}"), "Read", &json!({ "path": "a.ts" }));
            guard.observe_tool_result(&format!("r{index}"), false, "contents");
        }
        let verdict = observe_failure(&mut guard, 3, "Edit", failing).expect("still warns");
        assert_eq!(verdict.reason, ToolLoopReason::RepeatedFailure);
        assert_eq!(verdict.count, 4);
    }

    #[test]
    fn a_real_progress_success_clears_the_repeat_tally() {
        let mut guard = guard();
        let failing = json!({ "command": "pnpm test" });
        for index in 0..3 {
            assert!(observe_failure(&mut guard, index, "Bash", failing.clone()).is_none());
        }
        guard.observe_tool_use("fix", "Edit", &json!({ "file_path": "a.ts", "old_string": "x" }));
        guard.observe_tool_result("fix", false, "ok");
        for index in 10..13 {
            assert!(observe_failure(&mut guard, index, "Bash", failing.clone()).is_none());
        }
        assert!(!guard.warned());
    }

    #[test]
    fn halt_mode_terminates_at_the_ceiling_and_supersedes_warn() {
        let mut guard = ToolLoopGuard::new(ToolLoopGuardOptions {
            mode: ToolLoopMode::Halt,
            ..Default::default()
        });
        let mut verdicts = Vec::new();
        for index in 0..8 {
            if let Some(verdict) = observe_failure(
                &mut guard,
                index,
                "Edit",
                json!({ "file_path": "a.ts", "old_string": "x" }),
            ) {
                verdicts.push(verdict);
            }
        }
        assert_eq!(verdicts.len(), 2);
        assert_eq!(verdicts[0].action, ToolLoopAction::Warn);
        assert_eq!(verdicts[1].action, ToolLoopAction::Halt);
        assert_eq!(verdicts[1].reason, ToolLoopReason::RepeatedFailure);
        assert_eq!(verdicts[1].count, 8);
        assert!(guard.halted());
        // Inert once halted.
        assert!(observe_failure(&mut guard, 9, "Bash", json!({ "command": "false" })).is_none());
    }

    #[test]
    fn off_mode_never_reports() {
        let mut guard = ToolLoopGuard::new(ToolLoopGuardOptions {
            mode: ToolLoopMode::Off,
            ..Default::default()
        });
        for index in 0..20 {
            assert!(observe_failure(
                &mut guard,
                index,
                "Bash",
                json!({ "command": "false" })
            )
            .is_none());
        }
        assert!(!guard.warned());
    }

    #[test]
    fn mode_resolution_falls_back_to_warn() {
        assert_eq!(resolve_tool_loop_mode(None), ToolLoopMode::Warn);
        assert_eq!(resolve_tool_loop_mode(Some(" HALT ")), ToolLoopMode::Halt);
        assert_eq!(resolve_tool_loop_mode(Some("off")), ToolLoopMode::Off);
        assert_eq!(resolve_tool_loop_mode(Some("nonsense")), ToolLoopMode::Warn);
    }

    #[test]
    fn verdict_json_matches_the_contract_shape() {
        let verdict = ToolLoopVerdict {
            reason: ToolLoopReason::RepeatedFailure,
            action: ToolLoopAction::Halt,
            tool_name: "Edit".to_string(),
            signature: "Edit a.ts :: x".to_string(),
            count: 8,
        };
        let payload = verdict.to_json("run-1");
        assert_eq!(payload["runId"], "run-1");
        assert_eq!(payload["type"], "tool_loop");
        assert_eq!(payload["reason"], "repeated-failure");
        assert_eq!(payload["action"], "halt");
        assert_eq!(payload["count"], 8);
    }
}
