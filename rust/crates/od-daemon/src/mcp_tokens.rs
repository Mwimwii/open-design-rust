//! OAuth token storage for external HTTP / SSE MCP servers — parity port of
//! `apps/daemon/src/mcp-tokens.ts` (the entire file).
//!
//! The daemon owns the OAuth flow end-to-end so the user never needs a
//! transient `localhost:<port>` listener and a token survives across agent
//! turns. Records live in `<dataDir>/mcp-tokens.json`, keyed by
//! `McpServerConfig.id`, written atomically (mkdir recursive →
//! `<file>.<8 hex>.tmp` → rename → best-effort `chmod 0600`) behind a
//! per-dataDir write lock — the same pattern the config store in
//! [`crate::mcp`] uses.
//!
//! Parity sources in `mcp-tokens.ts`:
//!
//! * `StoredMcpToken` / `McpTokensFile` interfaces (line 26 / 54)
//! * `sanitizeTokensFile` / `sanitizeToken` (line 72 / 86)
//! * `readTokensFile` (line 148) and `writeTokensFile` (line 176)
//! * `getToken` / `setToken` / `clearToken` / `readAllTokens` (line 205 /
//!   214 / 227 / 241)
//! * `isTokenExpired` (line 251)
//!
//! Consumed by step 4's routes in [`crate::mcp`]:
//! `GET /api/mcp/oauth/status` (mcp-routes.ts:360) and
//! `POST /api/mcp/oauth/disconnect` (mcp-routes.ts:381). The OAuth start /
//! callback half (`mcp-oauth.ts`, `getPublicBaseUrl`,
//! `renderOAuthResultPage`) is the next step and is not ported yet.
//!
//! DOCUMENTED DEVIATIONS
//!
//! * **Key order.** TypeScript has two different key orders for one stored
//!   token: the `stored` literal the OAuth callback writes
//!   (`mcp-routes.ts:323`: `accessToken, refreshToken, tokenType, scope,
//!   expiresAt, savedAt, …`) and the rebuild order `sanitizeToken` produces
//!   on every read (`mcp-tokens.ts:135`: `accessToken, tokenType, savedAt,
//!   refreshToken, scope, expiresAt, …`). [`StoredMcpToken`] is declared
//!   once, in the **sanitized/rebuild order**, and every write path here
//!   serializes that struct. A file freshly written by the Rust callback
//!   therefore differs from a file freshly written by the TS callback in
//!   key order only — content is identical, and TypeScript rewrites each
//!   record in sanitized order on its next read-modify-write anyway.
//! * **`McpTokensFile.servers` is an insertion-ordered
//!   `Vec<(String, StoredMcpToken)>`,** not a map type: serde only
//!   implements `Serialize`/`Deserialize` for `serde_json::Map<String,
//!   Value>` (untyped values), so the typed + ordered `Record` shape is
//!   spelled as pairs here and a custom [`Serialize`] impl emits the JSON
//!   object. Upsert keeps an existing id's position (parity with JS
//!   `file.servers[id] = token`, which never reorders object keys) and
//!   removal shifts later entries left (parity with JS `delete`).
//! * **Numbers.** `expiresAt` / `savedAt` are [`serde_json::Number`], not
//!   `f64`: a JavaScript number keeps its integer spelling through
//!   `JSON.stringify`, while Rust's f64 formatter prints `1.0` for integral
//!   values — byte parity with `JSON.stringify(next, null, 2)` requires the
//!   integer form. `Number.isFinite` maps to
//!   `as_f64().is_some_and(f64::is_finite)`; serde_json cannot hold a
//!   non-finite number at all (`Number::from_f64(NaN)` is `None`), so that
//!   guard is defensive. The one observable consequence: a hand-edited file
//!   containing an out-of-range literal such as `1e999` parses to `Infinity`
//!   in V8 (TS drops just that field and keeps the record) but is rejected
//!   by serde_json, so this port logs `[mcp-tokens] Corrupted JSON …` and
//!   returns empty — the same class of parse-error divergence documented
//!   for the config store in step 2.
//! * **Write lock.** TypeScript chains a `writeLocks` promise per file path
//!   *inside* this module (`mcp-tokens.ts:163`). This port keeps the same
//!   lock state in [`crate::routes::AppState::mcp_tokens_lock`] (one lock
//!   per daemon = one per data dir) and the *route* holds it across the
//!   whole read → modify → write cycle, mirroring how `mcp_write_lock`
//!   guards `write_mcp_config`. Reads take no lock, exactly like TS.
//! * **`chmod 0600`** runs on POSIX only. `std::os::unix` permissions have
//!   no Windows equivalent, and Node's `fs.chmod` on Windows ignores the
//!   mode bits and succeeds, so there is nothing to do there. Failures log
//!   `tracing::warn!` carrying the TS text
//!   `[mcp-tokens] could not chmod 0600` unless the error kind is
//!   `PermissionDenied` (EPERM) or `Unsupported` (ENOTSUP/EOPNOTSUPP) —
//!   the two cases where TypeScript stays quiet. The kinds are matched
//!   through `io::ErrorKind` rather than raw errno constants, which are
//!   platform-specific.
//! * **`readAllTokens`** returns ordered `Vec<(String, StoredMcpToken)>`
//!   pairs instead of a JS `Record<string, StoredMcpToken>`.
//! * **Prototype-named ids.** TypeScript looks a record up with
//!   `file.servers[serverId] ?? null` (mcp-tokens.ts:210) and tests
//!   membership with `serverId in file.servers` (mcp-tokens.ts:233)
//!   against a plain object, so an id that names an *inherited*
//!   `Object.prototype` member while being absent from the file —
//!   `toString`, `valueOf`, `hasOwnProperty`, `__proto__`,
//!   `constructor`, … — answers truthy: `getToken` hands the status
//!   route a `Function` / `Object.prototype`, which `res.json` reduces
//!   to `{"connected":true,"expiresAt":null,"scope":null}` (the
//!   `savedAt` key drops out), and `clearToken` takes the membership
//!   branch and rewrites an otherwise unchanged file. The ordered pair
//!   list here has no prototype, so both report such an id as absent:
//!   the status route answers `{"connected":false}` and a disconnect
//!   skips the write. On the write path, `setToken('__proto__', …)`
//!   assigns the object's prototype in JS — the record never becomes an
//!   own property, so the rewrite emits `{"servers":{}}` — where
//!   [`McpTokensFile::insert`] stores it as an entry (the sanitizer
//!   skips that id on the next read on both sides, so only the bytes on
//!   disk differ). A *stored* record under any other id round-trips
//!   identically; the `__proto__` / `constructor` sanitizer skip list is
//!   TypeScript's own.
//! * **I/O wording.** The corrupted-JSON log keeps TS's
//!   `[mcp-tokens] Corrupted JSON, returning empty:` prefix but carries
//!   serde_json's parse description instead of V8's, and a non-ENOENT
//!   read error surfaces Rust's `io::Error` Display instead of Node's
//!   `err.message` (`EACCES: permission denied, open …`) — the same
//!   wording gap the config store documents in step 2. Invalid UTF-8 in
//!   the file is replaced lossily before parsing, matching Node's
//!   `readFile(path, 'utf8')` replacement rather than failing the read.
//! * **Blocking.** Every helper here is synchronous and the routes call it
//!   from `tokio::task::spawn_blocking` (parity: the async
//!   `node:fs/promises` calls the TS module awaits). The blocking boundary
//!   covers the full mkdir → write → rename → chmod sequence.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::{Map, Number, Value};

use crate::mcp::random_tmp_suffix;

/// Stored OAuth token for a single MCP server (parity: the
/// `StoredMcpToken` interface, mcp-tokens.ts:26) — the relevant subset of
/// an OAuth 2.0 token-endpoint response (RFC 6749 §5.1) plus the OAuth
/// client context the original authorization-code exchange used. Refresh
/// tokens are bound (RFC 6749 §6) to the client that received them, so
/// we have to refresh against the same `client_id` / `redirect_uri` pair —
/// persisting the context here is what lets us do that without re-running
/// authorization.
///
/// Field order is load-bearing: it reproduces `sanitizeToken`'s rebuild
/// order (mcp-tokens.ts:135) and both write paths serialize this struct
/// (see the module DOCUMENTED DEVIATIONS).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredMcpToken {
    /// The bearer token to send as `Authorization: Bearer …`.
    pub access_token: String,
    /// RFC 6749 §5.1 `token_type`. Almost always `Bearer`.
    pub token_type: String,
    /// Wall-clock epoch ms when this record was first persisted.
    pub saved_at: Number,
    /// Refresh token (RFC 6749 §6) if the auth server issued one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Space-separated scopes granted (verbatim from the token response).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Absolute epoch ms at which `accessToken` expires. Absent when the
    /// provider never expires the token (`is_token_expired` then says
    /// "not expired").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Number>,
    /// Token endpoint that issued this token; reused verbatim for refresh.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// Client id that obtained the refresh token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Confidential-client secret, if the upstream issued one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Authorization-server issuer, used to look the cached client back up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_server_issuer: Option<String>,
    /// Redirect URI registered with the client at authorization time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    /// RFC 8707 resource indicator the original token was scoped to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_url: Option<String>,
}

/// The persisted file shape (parity: `McpTokensFile`, mcp-tokens.ts:54) —
/// `{ "servers": { [McpServerConfig.id]: StoredMcpToken } }`, insertion
/// ordered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpTokensFile {
    /// Ordered `McpServerConfig.id` → token pairs; serialized as a JSON
    /// object (see the module DOCUMENTED DEVIATIONS for why this is not a
    /// map type).
    pub servers: Vec<(String, StoredMcpToken)>,
}

impl McpTokensFile {
    /// Parity: `EMPTY` (mcp-tokens.ts:59).
    pub fn empty() -> Self {
        Self {
            servers: Vec::new(),
        }
    }

    /// Parity: `file.servers[serverId]` — the stored record, if any.
    pub fn get(&self, server_id: &str) -> Option<&StoredMcpToken> {
        self.servers
            .iter()
            .find(|(id, _)| id == server_id)
            .map(|(_, token)| token)
    }

    /// Parity: `file.servers[serverId] = token` — upsert that keeps an
    /// existing id's position (JS object property assignment never
    /// reorders keys).
    pub fn insert(&mut self, server_id: String, token: StoredMcpToken) {
        match self.servers.iter_mut().find(|(id, _)| *id == server_id) {
            Some(slot) => slot.1 = token,
            None => self.servers.push((server_id, token)),
        }
    }

    /// Parity: `delete file.servers[serverId]` — removes the record and
    /// shifts later entries left (JS `delete` preserves the remaining
    /// order). `None` when the id was absent, which is what lets
    /// [`clear_token`] skip the write entirely.
    pub fn remove(&mut self, server_id: &str) -> Option<StoredMcpToken> {
        let index = self.servers.iter().position(|(id, _)| id == server_id)?;
        Some(self.servers.remove(index).1)
    }
}

impl Serialize for McpTokensFile {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut root = serializer.serialize_map(Some(1))?;
        root.serialize_entry("servers", &ServerEntries(&self.servers))?;
        root.end()
    }
}

/// Serializes the pair list as the JSON object TypeScript's `Record` shape
/// writes (`JSON.stringify` sees a plain object either way).
struct ServerEntries<'a>(&'a [(String, StoredMcpToken)]);

impl Serialize for ServerEntries<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (id, token) in self.0 {
            map.serialize_entry(id, token)?;
        }
        map.end()
    }
}

// ---- sanitizer (parity: sanitizeTokensFile / sanitizeToken) --------------

/// Parity: `isPlainObject` (mcp-tokens.ts:65) — a JSON object, never an
/// array (`Value::as_object` already folds out `Array.isArray`).
fn as_plain_object(raw: &Value) -> Option<&Map<String, Value>> {
    raw.as_object()
}

/// Parity: `sanitizeTokensFile` (mcp-tokens.ts:72) — coerce a freeform JSON
/// blob into the typed shape, dropping anything that doesn't deserialize
/// cleanly. Used at read time and as a defensive pass when third-party
/// tooling has hand-edited the file: a non-object root or a non-object
/// `servers` yields an empty file, the prototype-polluting ids are skipped,
/// and the input's insertion order is preserved.
pub fn sanitize_tokens_file(raw: &Value) -> McpTokensFile {
    let Some(root) = as_plain_object(raw) else {
        return McpTokensFile::empty();
    };
    let Some(servers) = root.get("servers").and_then(as_plain_object) else {
        return McpTokensFile::empty();
    };
    let mut out = McpTokensFile::empty();
    for (id, value) in servers {
        if id == "__proto__" || id == "constructor" {
            continue;
        }
        if let Some(token) = sanitize_token(value) {
            out.servers.push((id.clone(), token));
        }
    }
    out
}

/// Parity: `sanitizeToken` (mcp-tokens.ts:86) — `None` when the record has
/// no usable `accessToken`; otherwise the trimmed record with `tokenType`
/// defaulting to `Bearer`, every blank optional string dropped, a finite
/// `expiresAt`, and a finite `savedAt` defaulting to now.
fn sanitize_token(raw: &Value) -> Option<StoredMcpToken> {
    let object = as_plain_object(raw)?;
    // `typeof raw.accessToken === 'string' ? raw.accessToken.trim() : ''`
    // followed by the `if (!accessToken)` drop.
    let access_token = trimmed_string(object, "accessToken")?;
    let token_type =
        trimmed_string(object, "tokenType").unwrap_or_else(|| "Bearer".to_string());
    let saved_at = match object.get("savedAt") {
        Some(Value::Number(number)) if is_finite_number(number) => number.clone(),
        // `Date.now()` — current epoch milliseconds.
        _ => Number::from(now_epoch_millis()),
    };
    let expires_at = match object.get("expiresAt") {
        Some(Value::Number(number)) if is_finite_number(number) => Some(number.clone()),
        _ => None,
    };
    Some(StoredMcpToken {
        access_token,
        token_type,
        saved_at,
        refresh_token: trimmed_string(object, "refreshToken"),
        scope: trimmed_string(object, "scope"),
        expires_at,
        token_endpoint: trimmed_string(object, "tokenEndpoint"),
        client_id: trimmed_string(object, "clientId"),
        client_secret: trimmed_string(object, "clientSecret"),
        auth_server_issuer: trimmed_string(object, "authServerIssuer"),
        redirect_uri: trimmed_string(object, "redirectUri"),
        resource_url: trimmed_string(object, "resourceUrl"),
    })
}

/// Parity: the repeated `typeof raw[key] === 'string' && raw[key].trim() ?
/// raw[key].trim() : undefined` rule (mcp-tokens.ts:95-134) — a non-string
/// or blank-after-trim value is omitted.
fn trimmed_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    let value = object.get(key)?.as_str()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Parity: `Number.isFinite(number)`. Every number serde_json can hold is
/// finite already (see the module DOCUMENTED DEVIATIONS), so this guard is
/// defensive parity with the TypeScript check.
fn is_finite_number(number: &Number) -> bool {
    number.as_f64().is_some_and(f64::is_finite)
}

/// Parity: `Date.now()` — current epoch milliseconds.
fn now_epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

// ---- storage (parity: readTokensFile / writeTokensFile) -----------------

/// Parity: `tokensFile` (mcp-tokens.ts:61) — `<dataDir>/mcp-tokens.json`;
/// `<dataDir>` is the resolved daemon data root (`RuntimePaths::data_dir`).
fn tokens_file(data_dir: &Path) -> PathBuf {
    data_dir.join("mcp-tokens.json")
}

/// Parity: `readTokensFile` (mcp-tokens.ts:148). Missing file → empty,
/// corrupted JSON → logged empty with the TS message prefix, any other I/O
/// error → `Err` (the route turns it into the 500). Blocking; the routes
/// call it from `spawn_blocking`.
pub fn read_tokens_file(data_dir: &Path) -> std::io::Result<McpTokensFile> {
    let bytes = match std::fs::read(tokens_file(data_dir)) {
        Ok(bytes) => bytes,
        // Parity: `err.code === 'ENOENT'` → `{ servers: {} }`.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(McpTokensFile::empty());
        }
        Err(err) => return Err(err),
    };
    // Node's `readFile(path, 'utf8')` substitutes U+FFFD for invalid bytes
    // instead of failing; parse the same replacement text.
    let raw = String::from_utf8_lossy(&bytes);
    match serde_json::from_str::<Value>(&raw) {
        Ok(parsed) => Ok(sanitize_tokens_file(&parsed)),
        Err(err) => {
            tracing::error!("[mcp-tokens] Corrupted JSON, returning empty: {err}");
            Ok(McpTokensFile::empty())
        }
    }
}

/// Parity: `writeTokensFile` (mcp-tokens.ts:176) — mkdir recursive, write
/// `<file>.<8 hex>.tmp`, rename onto `<dataDir>/mcp-tokens.json`, then
/// best-effort `chmod 0600`. The caller (the route) holds
/// [`crate::routes::AppState::mcp_tokens_lock`] across this call the way
/// TS's `withLock` wraps read-modify-write, so concurrent writers are
/// mutually exclusive per process. A failed rename leaves the temp file
/// behind, exactly like TypeScript. Blocking; call from `spawn_blocking`.
pub fn write_tokens_file(data_dir: &Path, next: &McpTokensFile) -> std::io::Result<()> {
    let file = tokens_file(data_dir);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = PathBuf::from(format!("{}.{}.tmp", file.display(), random_tmp_suffix()));
    let text =
        serde_json::to_string_pretty(next).map_err(|err| std::io::Error::other(err.to_string()))?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &file)?;
    lockdown_owner_only(&file);
    Ok(())
}

/// Best-effort lockdown of the file mode (parity: the `chmod(file, 0o600)`
/// try/catch in `writeTokensFile`). Bearer tokens can hand someone
/// posting-as-you against the upstream MCP, so the file becomes
/// owner-only read/write where the OS supports it; the `ENOTSUP` / `EPERM`
/// cases TypeScript swallows map to `ErrorKind::Unsupported` /
/// `ErrorKind::PermissionDenied`, everything else warns and the write
/// still succeeds.
#[cfg(unix)]
fn lockdown_owner_only(file: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let result = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
    if let Err(err) = result {
        let quiet = matches!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
        );
        if !quiet {
            tracing::warn!("[mcp-tokens] could not chmod 0600 {} {err}", file.display());
        }
    }
}

/// Node's `fs.chmod` on Windows understands only the read-only bit and
/// succeeds as a no-op for `0o600`; `std::os::unix` permissions do not
/// exist there, so this port has nothing to do (see DOCUMENTED DEVIATIONS).
#[cfg(not(unix))]
fn lockdown_owner_only(_file: &Path) {}

// ---- accessors (parity: getToken / setToken / clearToken / readAllTokens)-

/// Parity: `getToken` (mcp-tokens.ts:205) — the current token for
/// `server_id`, or `None` when none is stored (or the persisted entry is
/// malformed and sanitized away). Reads take no lock, exactly like TS.
pub fn get_token(data_dir: &Path, server_id: &str) -> std::io::Result<Option<StoredMcpToken>> {
    let file = read_tokens_file(data_dir)?;
    Ok(file.get(server_id).cloned())
}

/// Parity: `setToken` (mcp-tokens.ts:214) — read → insert → write as one
/// cycle. The caller holds [`crate::routes::AppState::mcp_tokens_lock`]
/// for the whole cycle (TS does it inside `withLock`); see the module
/// DOCUMENTED DEVIATIONS.
pub fn set_token(data_dir: &Path, server_id: &str, token: StoredMcpToken) -> std::io::Result<()> {
    let mut file = read_tokens_file(data_dir)?;
    file.insert(server_id.to_string(), token);
    write_tokens_file(data_dir, &file)
}

/// Parity: `clearToken` (mcp-tokens.ts:227) — no-op when the id is absent,
/// so an unchanged file is never rewritten (the same mtime on disk).
pub fn clear_token(data_dir: &Path, server_id: &str) -> std::io::Result<()> {
    let mut file = read_tokens_file(data_dir)?;
    if file.remove(server_id).is_none() {
        return Ok(());
    }
    write_tokens_file(data_dir, &file)
}

/// Parity: `readAllTokens` (mcp-tokens.ts:241) — one disk hit per spawn,
/// not one per server. The TS return type is
/// `Record<string, StoredMcpToken>`; ordered pairs carry the same
/// information (see the module DOCUMENTED DEVIATIONS).
pub fn read_all_tokens(data_dir: &Path) -> std::io::Result<Vec<(String, StoredMcpToken)>> {
    Ok(read_tokens_file(data_dir)?.servers)
}

// ---- expiry (parity: isTokenExpired) ------------------------------------

/// Parity: the `skew = 30_000` default of `isTokenExpired`
/// (mcp-tokens.ts:251) — a token within 30s of expiring counts as expired.
pub const DEFAULT_EXPIRY_SKEW_MS: u64 = 30_000;

/// Parity: `isTokenExpired` (mcp-tokens.ts:251) — `false` when the record
/// carries no `expiresAt` (many providers issue non-expiring tokens), else
/// `expiresAt - skew <= now`, evaluated in `f64` exactly like JavaScript's
/// numbers.
pub fn is_token_expired(token: &StoredMcpToken, now_ms: u64, skew_ms: u64) -> bool {
    let Some(expires_at) = token.expires_at.as_ref().and_then(Number::as_f64) else {
        return false;
    };
    // `u64 as f64` (not `f64::from`): std has no `From<u64> for f64`, and
    // the rounding this performs is exactly what JavaScript's `number`
    // arithmetic does.
    expires_at - (skew_ms as f64) <= now_ms as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanitize one JSON literal — the same text `JSON.parse` would hand
    /// the TypeScript sanitizer.
    fn file(raw: &str) -> McpTokensFile {
        sanitize_tokens_file(&serde_json::from_str::<Value>(raw).expect("literal is valid JSON"))
    }

    /// The single record `id` of `raw`, if the sanitizer kept it.
    fn token(raw: &str) -> Option<StoredMcpToken> {
        token_for(raw, "a")
    }

    /// Same lookup for an arbitrary id (records under test live at
    /// different keys).
    fn token_for(raw: &str, id: &str) -> Option<StoredMcpToken> {
        file(raw).get(id).cloned()
    }

    fn temp_data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("od-mcp-tokens-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    // ---- sanitizer (parity: sanitizeTokensFile / sanitizeToken) ----

    #[test]
    fn a_record_without_an_access_token_is_dropped() {
        for raw in [
            r#"{"servers":{"a":{}}}"#,
            r#"{"servers":{"a":{"tokenType":"Bearer","savedAt":1}}}"#,
            r#"{"servers":{"a":{"accessToken":null}}}"#,
            r#"{"servers":{"a":{"accessToken":42}}}"#,
            r#"{"servers":{"a":{"accessToken":"   "}}}"#,
            r#"{"servers":{"a":"not an object"}}"#,
            r#"{"servers":{"a":["array"]}}"#,
        ] {
            assert!(token(raw).is_none(), "should drop: {raw}");
        }
        // A usable record survives the drop check.
        assert_eq!(
            token(r#"{"servers":{"a":{"accessToken":"t"}}}"#)
                .expect("kept")
                .access_token,
            "t"
        );
    }

    #[test]
    fn proto_polluting_ids_are_skipped_but_order_is_kept() {
        let parsed = file(
            r#"{"servers":{
                "z":{"accessToken":"z-tok"},
                "__proto__":{"accessToken":"evil"},
                "constructor":{"accessToken":"evil"},
                "a":{"accessToken":"a-tok"},
                "m":{"accessToken":"   "}
            }}"#,
        );
        let ids: Vec<&str> = parsed.servers.iter().map(|(id, _)| id.as_str()).collect();
        // Insertion order preserved, dangerous ids skipped, blank record
        // dropped.
        assert_eq!(ids, ["z", "a"]);
    }

    #[test]
    fn token_type_defaults_to_bearer_and_string_fields_are_trimmed() {
        // Absent / blank / non-string tokenType → `Bearer`.
        for raw in [
            r#"{"servers":{"a":{"accessToken":"t"}}}"#,
            r#"{"servers":{"a":{"accessToken":"t","tokenType":"   "}}}"#,
            r#"{"servers":{"a":{"accessToken":"t","tokenType":42}}}"#,
        ] {
            assert_eq!(token(raw).expect("kept").token_type, "Bearer", "raw: {raw}");
        }
        // A present tokenType is stored trimmed (not normalized).
        assert_eq!(
            token(r#"{"servers":{"a":{"accessToken":" t ","tokenType":" bearer "}}}"#)
                .expect("kept")
                .token_type,
            "bearer"
        );

        let kept = token(
            r#"{"servers":{"a":{
                "accessToken":"  tok  ",
                "refreshToken":"  rt  ",
                "scope":"  read write  ",
                "tokenEndpoint":"  https://auth.example/token  ",
                "clientId":"  cid  ",
                "clientSecret":"  sec  ",
                "authServerIssuer":"  https://auth.example  ",
                "redirectUri":"  http://127.0.0.1:7456/cb  ",
                "resourceUrl":"  https://api.example  "
            }}}"#,
        )
        .expect("kept");
        assert_eq!(kept.access_token, "tok");
        assert_eq!(kept.refresh_token.as_deref(), Some("rt"));
        assert_eq!(kept.scope.as_deref(), Some("read write"));
        assert_eq!(
            kept.token_endpoint.as_deref(),
            Some("https://auth.example/token")
        );
        assert_eq!(kept.client_id.as_deref(), Some("cid"));
        assert_eq!(kept.client_secret.as_deref(), Some("sec"));
        assert_eq!(
            kept.auth_server_issuer.as_deref(),
            Some("https://auth.example")
        );
        assert_eq!(
            kept.redirect_uri.as_deref(),
            Some("http://127.0.0.1:7456/cb")
        );
        assert_eq!(kept.resource_url.as_deref(), Some("https://api.example"));
    }

    #[test]
    fn blank_or_non_string_optionals_are_omitted() {
        let kept = token(
            r#"{"servers":{"a":{
                "accessToken":"t",
                "refreshToken":"  ",
                "scope":"",
                "tokenEndpoint":null,
                "clientId":7,
                "clientSecret":"   ",
                "authServerIssuer":"",
                "redirectUri":"  ",
                "resourceUrl":true
            }}}"#,
        )
        .expect("kept");
        assert!(kept.refresh_token.is_none());
        assert!(kept.scope.is_none());
        assert!(kept.token_endpoint.is_none());
        assert!(kept.client_id.is_none());
        assert!(kept.client_secret.is_none());
        assert!(kept.auth_server_issuer.is_none());
        assert!(kept.redirect_uri.is_none());
        assert!(kept.resource_url.is_none());

        // And they serialize as absent keys, not `null`s.
        let json = serde_json::to_value(&kept).expect("serialize");
        assert!(json.get("refreshToken").is_none());
        assert!(json.get("scope").is_none());
    }

    #[test]
    fn non_number_or_unrepresentable_expires_at_is_omitted() {
        // `Number.isFinite` parity: every non-number shape TypeScript drops
        // is dropped here too (the record itself survives — only the field
        // is omitted).
        for raw in [
            r#"{"servers":{"a":{"accessToken":"t","expiresAt":null}}}"#,
            r#"{"servers":{"a":{"accessToken":"t","expiresAt":"1759812345678"}}}"#,
            r#"{"servers":{"a":{"accessToken":"t","expiresAt":true}}}"#,
            r#"{"servers":{"a":{"accessToken":"t","expiresAt":{}}}}"#,
        ] {
            let fresh = token(raw).expect("kept");
            assert!(fresh.expires_at.is_none(), "omitted: {raw}");
        }
        // A finite number is kept verbatim.
        assert_eq!(
            token(r#"{"servers":{"a":{"accessToken":"t","expiresAt":1759812345678}}}"#)
                .expect("kept")
                .expires_at
                .as_ref()
                .and_then(Number::as_u64),
            Some(1759812345678)
        );
        // Non-finite numbers are unreachable inputs (module DOCUMENTED
        // DEVIATIONS): `NaN` is not JSON at all — V8's `JSON.parse`
        // rejects it too — and `1e999`, which V8 parses as `Infinity` and
        // TypeScript would then drop as one non-finite field, overflows
        // f64 and is refused outright by serde_json.
        assert!(serde_json::from_str::<Value>(r#"{"expiresAt":NaN}"#).is_err());
        assert!(
            serde_json::from_str::<Value>(
                r#"{"servers":{"a":{"accessToken":"t","expiresAt":1e999}}}"#
            )
            .is_err(),
            "an out-of-range number is a whole-file parse error here"
        );
    }

    #[test]
    fn saved_at_defaults_to_now_when_missing_or_unusable() {
        let before = now_epoch_millis();
        for raw in [
            r#"{"servers":{"a":{"accessToken":"t"}}}"#,
            r#"{"servers":{"a":{"accessToken":"t","savedAt":null}}}"#,
            r#"{"servers":{"a":{"accessToken":"t","savedAt":"yesterday"}}}"#,
        ] {
            let saved_at = token(raw).expect("kept").saved_at.as_u64();
            let saved_at = saved_at.expect("integer epoch ms");
            assert!(
                (before..=now_epoch_millis()).contains(&saved_at),
                "savedAt should default to now: {saved_at} (before {before}), raw: {raw}"
            );
        }
        // A finite savedAt is kept verbatim.
        assert_eq!(
            token(r#"{"servers":{"a":{"accessToken":"t","savedAt":1700000000000}}}"#)
                .expect("kept")
                .saved_at
                .as_u64(),
            Some(1700000000000)
        );
    }

    #[test]
    fn non_object_roots_and_non_object_servers_yield_an_empty_file() {
        for raw in ["null", "[]", "\"x\"", "42", "{}", "{\"servers\":\"nope\"}", "{\"servers\":[1,2]}"] {
            assert_eq!(
                sanitize_tokens_file(&serde_json::from_str::<Value>(raw).expect("JSON")),
                McpTokensFile::empty(),
                "empty file for: {raw}"
            );
        }
    }

    // ---- serialization byte-parity (JSON.stringify(next, null, 2)) ----

    /// The exact text `JSON.stringify(sanitizeTokensFile(input), null, 2)`
    /// produces in Node: sanitized rebuild key order (module DOCUMENTED
    /// DEVIATIONS), 2-space indent, no trailing newline.
    const FULL_PRETTY: &str = r#"{
  "servers": {
    "abc": {
      "accessToken": "tok-123",
      "tokenType": "bearer",
      "savedAt": 1700000000000,
      "refreshToken": "rt",
      "scope": "a b",
      "expiresAt": 1759812345678,
      "tokenEndpoint": "https://auth.example/token",
      "clientId": "client-id",
      "clientSecret": "secret",
      "authServerIssuer": "https://auth.example",
      "redirectUri": "http://127.0.0.1:7456/cb",
      "resourceUrl": "https://api.example"
    }
  }
}"#;

    #[test]
    fn a_full_record_serializes_byte_identically_to_json_stringify() {
        // Input deliberately in the TS *callback* key order and padded with
        // whitespace: sanitization must rebuild both shape and order.
        let input = r#"{
            "servers": {
                "abc": {
                    "accessToken": "  tok-123  ",
                    "refreshToken": " rt ",
                    "expiresAt": 1759812345678,
                    "tokenType": " bearer ",
                    "scope": " a b ",
                    "savedAt": 1700000000000,
                    "tokenEndpoint": " https://auth.example/token ",
                    "clientId": " client-id ",
                    "clientSecret": " secret ",
                    "authServerIssuer": " https://auth.example ",
                    "redirectUri": " http://127.0.0.1:7456/cb ",
                    "resourceUrl": " https://api.example "
                }
            }
        }"#;
        let parsed = file(input);
        assert_eq!(
            serde_json::to_string_pretty(&parsed).expect("pretty"),
            FULL_PRETTY
        );
        // …and the same text round-trips through the reader.
        let dir = temp_data_dir("pretty");
        write_tokens_file(&dir, &parsed).expect("write");
        let text = std::fs::read_to_string(dir.join("mcp-tokens.json")).expect("read back");
        assert_eq!(text, FULL_PRETTY, "on-disk text is 2-space pretty JSON");
        assert!(!text.ends_with('\n'), "no trailing newline, like JSON.stringify");
        assert_eq!(read_tokens_file(&dir).expect("re-read"), parsed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- storage (parity: readTokensFile / setToken / clearToken) ----

    #[test]
    fn read_returns_empty_for_missing_and_corrupted_files() {
        let dir = temp_data_dir("read");
        assert_eq!(
            read_tokens_file(&dir).expect("missing file"),
            McpTokensFile::empty()
        );

        let file_path = dir.join("mcp-tokens.json");
        std::fs::write(&file_path, "{not valid").expect("write corrupt");
        assert_eq!(
            read_tokens_file(&dir).expect("corrupted file"),
            McpTokensFile::empty()
        );

        // Valid JSON that sanitizes away also reads back empty.
        std::fs::write(&file_path, "[1, 2]").expect("write array");
        assert_eq!(
            read_tokens_file(&dir).expect("array file"),
            McpTokensFile::empty()
        );

        // A real record round-trips, and the write is chmod'ed 0600.
        let parsed = file(r#"{"servers":{"a":{"accessToken":"t","scope":"s"}}}"#);
        write_tokens_file(&dir, &parsed).expect("write");
        assert_eq!(read_tokens_file(&dir).expect("valid file"), parsed);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file_path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "bearer tokens are owner-only");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_token_round_trips_through_get_token() {
        let dir = temp_data_dir("set-get");
        assert_eq!(
            get_token(&dir, "a").expect("read missing file"),
            None,
            "nothing stored yet"
        );

        let stored = token(
            r#"{"servers":{"a":{"accessToken":"t-1","expiresAt":1759812345678,"scope":"read"}}}"#,
        )
        .expect("record");
        set_token(&dir, "a", stored.clone()).expect("set");
        assert_eq!(get_token(&dir, "a").expect("get"), Some(stored));

        // A second id merges into the same file; the first keeps its slot.
        let other = token_for(r#"{"servers":{"b":{"accessToken":"t-2"}}}"#, "b").expect("record");
        set_token(&dir, "b", other.clone()).expect("set second");
        let all = read_all_tokens(&dir).expect("read all");
        let ids: Vec<&str> = all.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(get_token(&dir, "b").expect("get second"), Some(other));
        // Re-setting an existing id replaces the record in place.
        let replaced = token(r#"{"servers":{"a":{"accessToken":"t-3"}}}"#).expect("record");
        set_token(&dir, "a", replaced.clone()).expect("replace");
        let all = read_all_tokens(&dir).expect("read all again");
        let ids: Vec<&str> = all.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["a", "b"], "position kept on upsert");
        assert_eq!(get_token(&dir, "a").expect("get replaced"), Some(replaced));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_token_removes_the_record_and_skips_the_write_when_absent() {
        let dir = temp_data_dir("clear");
        let parsed = file(r#"{"servers":{"a":{"accessToken":"t-1"},"b":{"accessToken":"t-2"}}}"#);
        write_tokens_file(&dir, &parsed).expect("seed");
        let file_path = dir.join("mcp-tokens.json");

        // Present → the record (and only the record) disappears.
        clear_token(&dir, "a").expect("clear");
        let after = read_tokens_file(&dir).expect("re-read");
        let ids: Vec<&str> = after.servers.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["b"], "later entries shift left, like JS delete");

        // Absent → no rewrite at all: content and mtime stay put. The
        // sleep makes the mtime assertion meaningful against the coarse
        // (several-ms) filesystem timestamp granularity.
        let before_text = std::fs::read_to_string(&file_path).expect("text");
        let before_mtime = std::fs::metadata(&file_path).expect("metadata").modified().expect("mtime");
        std::thread::sleep(std::time::Duration::from_millis(25));
        clear_token(&dir, "missing").expect("no-op");
        let after_text = std::fs::read_to_string(&file_path).expect("text again");
        let after_mtime = std::fs::metadata(&file_path).expect("metadata").modified().expect("mtime");
        assert_eq!(before_text, after_text, "an absent id must not rewrite");
        assert_eq!(before_mtime, after_mtime, "the file was replaced on disk");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- expiry (parity: isTokenExpired) ----

    fn expiry_token(expires_at: Option<u64>) -> StoredMcpToken {
        let raw = match expires_at {
            Some(ms) => format!(r#"{{"servers":{{"a":{{"accessToken":"t","expiresAt":{ms}}}}}}}"#),
            None => r#"{"servers":{"a":{"accessToken":"t"}}}"#.to_string(),
        };
        token(&raw).expect("record")
    }

    #[test]
    fn expired_tokens_account_for_the_default_skew() {
        // No expiry recorded → never expired (non-expiring providers).
        assert!(!is_token_expired(
            &expiry_token(None),
            1_700_000_000_000,
            DEFAULT_EXPIRY_SKEW_MS
        ));

        let expires_at = 1_700_000_030_000_u64;
        let token = expiry_token(Some(expires_at));

        // Far-future expiry → not expired.
        assert!(!is_token_expired(&token, 1_600_000_000_000, DEFAULT_EXPIRY_SKEW_MS));
        // Inside the 30s skew window → expired.
        assert!(is_token_expired(&token, expires_at - 1, DEFAULT_EXPIRY_SKEW_MS));
        assert!(is_token_expired(&token, expires_at, DEFAULT_EXPIRY_SKEW_MS));
        // Boundary: `expiresAt - skew <= now` — one ms before the window is
        // still fresh, exactly on it is expired.
        assert!(!is_token_expired(&token, expires_at - 30_001, DEFAULT_EXPIRY_SKEW_MS));
        assert!(is_token_expired(&token, expires_at - 30_000, DEFAULT_EXPIRY_SKEW_MS));
        // A wider skew widens the window the same way.
        assert!(!is_token_expired(&token, expires_at - 60_001, 60_000));
        assert!(is_token_expired(&token, expires_at - 60_000, 60_000));
        // Zero skew: expired only once `now` reaches `expiresAt`.
        assert!(!is_token_expired(&token, expires_at - 1, 0));
        assert!(is_token_expired(&token, expires_at, 0));
    }
}
