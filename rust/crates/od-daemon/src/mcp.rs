//! External MCP route support — parity port of the same-origin guard in
//! `apps/daemon/src/origin-validation.ts` that the external MCP endpoints
//! consult (`isLocalSameOrigin(req, resolvedPortRef.current)` in
//! `apps/daemon/src/mcp-routes.ts:97` and its siblings).
//!
//! Step 1 of the MCP routes port (bead `open-design-rust-r2r`): the guard
//! lands here first; the routes themselves move into this module in the
//! next step. They will build [`OriginGuardInputs`] from the request
//! headers plus [`crate::routes::AppState::resolved_port`] — the
//! actually-bound port, mirroring `resolvedPortRef` in
//! `apps/daemon/src/server.ts` (`config.port` stays `0` until the listener
//! binds, and tests bind ephemerally).
//!
//! Parity sources in `origin-validation.ts`:
//!
//! * `configuredAllowedOrigins` / `configuredAllowedHosts` (line 15 / 31)
//! * `allowedBrowserPorts` (line 92)
//! * `parseHostHeader` (line 104)
//! * `isPrivateIpv4` / `isIpLiteralHostname` / `isLoopbackOrPrivateLanHost`
//!   (line 115 / 130 / 140)
//! * `isAllowedBrowserHost` / `isAllowedBrowserOrigin` (line 153 / 176)
//! * `isLocalSameOrigin` (line 212)
//!
//! DOCUMENTED DEVIATIONS
//!
//! * TypeScript's `configuredAllowedOrigins` **throws** when an
//!   `OD_ALLOWED_ORIGINS` entry is not a parseable `http(s)` URL, which
//!   makes every guarded request fail. This port skips such entries
//!   instead — fail-closed, the allow-list can only ever shrink — and emits
//!   a `tracing::warn!` per occurrence so a typo stays visible.
//! * Hostnames follow the WHATWG URL parser (ASCII lowercasing,
//!   percent-decoding, IPv4/IPv6 canonicalization, forbidden-host-code-point
//!   rejection) except for UTS-46/IDNA: punycode conversion of non-ASCII
//!   hostnames is not ported. Hostnames whose bytes fall outside visible
//!   ASCII (raw or percent-decoded) are rejected outright — fail-closed
//!   where TypeScript would punycode them.
//! * `OD_WEB_PORT` is read as a plain decimal float. JavaScript's
//!   `Number()` additionally accepts hex (`0x10`) and `Infinity` forms and
//!   would keep a non-finite result in the port list; those inputs are
//!   ignored here (no real port string can match them anyway).
//! * IPv6 literals canonicalize through `std`'s `Ipv6Addr` formatter, which
//!   prints IPv4-mapped addresses dotted (`[::ffff:127.0.0.1]`) where the
//!   WHATWG serializer prints hex (`[::ffff:7f00:1]`). Only exact-string
//!   allow-list membership can tell the two apart (mismatch fails closed).
//! * Header extraction takes the first value of a repeated `Origin` /
//!   `Sec-Fetch-Site` header; Node joins duplicates with `", "` before the
//!   guard ever sees them. A joined value never matches the allow-list, so
//!   the only observable difference is a duplicated header whose first value
//!   is itself acceptable. Header bytes that are not valid UTF-8 are treated
//!   as absent here (Node decodes header values as latin-1).
//!
//! Step 2 of the MCP routes port (bead `open-design-rust-r2r`): this module
//! now also owns the external MCP server configuration surface — parity for
//! `GET`/`PUT /api/mcp/servers` (`mcp-routes.ts:191` / `:205`) plus the
//! storage and sanitization layer behind them (`mcp-config.ts:30-275`) and
//! the built-in template list (`MCP_TEMPLATES`, mcp-config.ts:567).
//!
//! DOCUMENTED DEVIATIONS (step 2 — configuration routes)
//!
//! * Error bodies: TypeScript answers the guard with the flat
//!   `{ error: "cross-origin request rejected" }` and handler failures with
//!   `{ error: String(err.message) }`. This port keeps the message text
//!   (`cross-origin request rejected`, the I/O error text) but wraps it in
//!   the crate-wide envelope `{ error: { code, message } }` mandated by
//!   `rust/README.md` ("Errors use the JSON envelope"), with `code` =
//!   `FORBIDDEN` / `INTERNAL_ERROR` / `BAD_REQUEST`. Clients that accept
//!   `CompatibleErrorResponse` parse both shapes (see
//!   `specs/current/daemon-http-adapter.md` → wire-format note).
//! * URL normalization runs through the `url` crate — the WHATWG URL
//!   reference implementation — instead of V8's parser. Acceptance (parse,
//!   then http/https only) and `toString()` output match `new URL(...)` for
//!   the shapes configs carry: origin trailing slash, default-port removal,
//!   host lowercasing, dot-segment cleanup (including `%2e` forms), space →
//!   `%20`, IDNA. Exotic-host edge cases (IDNA/UTS-46 table drift between
//!   crate and V8 versions, IPv6 canonical spelling) may differ — the same
//!   class of approximation already listed for the guard above.
//! * The template list ships as embedded JSON (`mcp-templates.json` at the
//!   crate root), parsed once into `serde_json::Value`s and served verbatim.
//!   It is generated mechanically from the TS literal — slice from
//!   `export const MCP_TEMPLATES` to its closing `];`, `eval`, then
//!   `JSON.stringify(arr, null, 2)`. The `McpTemplate` / `McpTemplateField`
//!   TS types are deliberately not ported: no daemon-side code reads
//!   individual template fields (the web UI does, over the wire).
//! * Request-body parsing: axum's `Json<Value>` extractor rejects malformed
//!   JSON with 400 and a plain-text body where Express' `express.json()`
//!   surfaces Body-Parser's own error payload (same 400 status, different
//!   text), rejects a missing `application/json` Content-Type with 415 (same
//!   as Body-Parser), and rejects a zero-length body with 400 where
//!   Body-Parser would hand the route `{}` — sanitized to an empty config
//!   and answered 200. Extraction runs before the handler here exactly as
//!   `express.json()` middleware runs before the route, so a malformed
//!   cross-origin body fails 400 before the guard in both stacks.
//! * `POST`/`DELETE`/`OPTIONS` on `/api/mcp/servers` answer 405 here (the
//!   static path is registered for GET/PUT only) where Express falls
//!   through to its JSON 404 (and auto-answers `OPTIONS` with `Allow`) —
//!   the same divergence as every other merged router in this crate.
//! * Read failures mirror `readMcpConfig`'s three-way split (`ENOENT` →
//!   empty, parse error → logged empty, anything else → error): the log
//!   line keeps the TS text `[mcp-config] Corrupted JSON, returning empty:`
//!   but carries serde_json's parse description instead of V8's, and a
//!   non-ENOENT I/O error surfaces Rust's `io::Error` Display instead of
//!   Node's `CODE: syscall, op 'path'` wording. Invalid UTF-8 in the file
//!   is replaced lossily before parsing, matching Node's `utf8` `readFile`
//!   replacement rather than failing the read.
//! * Write serialization uses a `tokio::sync::Mutex` held in `AppState`
//!   (`mcp_write_lock`) across the whole sanitize→mkdir→write→rename, one
//!   lock per daemon (= one per data dir), mirroring the per-dataDir
//!   `writeLocks` promise chain in `mcp-config.ts`. The temp-file suffix is
//!   8 lowercase hex chars from a randomly-seeded std hasher — format parity
//!   with `randomBytes(4).toString('hex')`, not cryptographic; it only
//!   separates concurrent temp files and writes are already serialized.
//! * serde_json is compiled with `preserve_order` for this crate's
//!   workspace so object key order (env/headers maps, template objects,
//!   `json!` payloads) follows insertion order like `JSON.stringify` —
//!   required for the byte-for-byte on-disk parity asserted in the tests.
//! * The sanitizer's id rule is byte-exact against
//!   `/^[a-z0-9][a-z0-9_-]{0,63}$/i`; JavaScript's case canonicalization
//!   turns up no non-ASCII acceptors for that pattern, so ASCII checks
//!   reproduce it (non-ASCII bytes reject).
//!
//! Step 3 of the MCP routes port (bead `open-design-rust-r2r`): the install
//! surface — the pure snippet builder (`mcp-install-info.ts`), `GET
//! /api/mcp/install-info` with its 5s cache (mcp-routes.ts:13-112), the
//! `codex mcp add|remove|get` runner (`codex-cli.ts`), and the three
//! one-click Codex routes (mcp-routes.ts:118-185).
//!
//! DOCUMENTED DEVIATIONS (step 3 — install info + Codex install)
//!
//! * `cli_path` (TS `OD_BIN`, resolved by `resolveDaemonCliPath`, is
//!   daemon-paths.ts:19): the Rust rewrite has no `od` CLI port yet. This
//!   port keeps the env half of that resolver (`OD_DAEMON_CLI_PATH`, then
//!   `OD_BIN`) and, when neither is set, guesses the sibling `od`
//!   executable next to `std::env::current_exe()` instead of resolving the
//!   TypeScript package's `dist/cli.js`. `cli_exists` stays a real `fs`
//!   probe, so the TS buildHint fires (correctly) when that sibling is
//!   missing — the normal case here.
//! * `exec_path` (TS `process.execPath`, the Node runtime) =
//!   `std::env::current_exe()`, and `node_exists` probes it. Consequence,
//!   stated honestly: the snippet shape (`command` + `args`) still mirrors
//!   TypeScript byte-for-byte, but with neither the `od` CLI nor a Node
//!   launcher in the rewrite it is not runnable yet — `cli_exists` is
//!   false, so the payload carries the buildHint that says so instead of
//!   pretending to be ready.
//! * `platform` maps Rust `std::env::consts::OS` to the TypeScript
//!   `NodeJS.Platform` strings: `macos → "darwin"`, `windows → "win32"`,
//!   `linux → "linux"`, else the raw Rust value.
//! * `electron_as_node` is always false (no Electron host), and
//!   `is_sidecar_mode` is always false with no inherited sidecar client
//!   entries in `sidecar_env` (this daemon has no sidecar client), so the
//!   `--daemon-url` args form is always baked. The
//!   `OD_MCP_BOOTSTRAP_COMMAND` / `OD_MCP_BOOTSTRAP_ARGS` env merge from
//!   `computeInstallPayload` (mcp-routes.ts:53-63) IS ported at the call
//!   site; `managedMcpRegistrationEnv()` and the 5-second
//!   `isManagedMcpBootstrapEnv` registration-refresh timer
//!   (mcp-routes.ts:126-140, `mcp-managed-registration.ts`,
//!   `mcp-bootstrap.ts`) are NOT ported — an explicit out-of-scope
//!   follow-up. (`parseCodexMcpRegistration` is ported and unit-tested as
//!   part of `codex-cli.ts`'s surface even though its refresh caller is
//!   that follow-up.)
//! * `sidecar_env` is a `BTreeMap`, so merged keys iterate in sorted order
//!   where a JS object keeps insertion order. With no inherited sidecar
//!   entries the only observable difference is the relative order of
//!   `OD_MCP_BOOTSTRAP_ARGS` and `OD_MCP_BOOTSTRAP_COMMAND` when both are
//!   set: payload content is identical, key order in `env` (and therefore
//!   in the `--env` argv) differs.
//! * Error envelopes need no deviation for the three Codex handlers: they
//!   already answer through TS `sendApiError`, whose body is exactly the
//!   `{ error: { code, message } }` envelope this crate mandates, so
//!   status, code, and message match TypeScript. The install-info handler
//!   has no failure body in TS at all (it cannot fail), and the guard's
//!   403 keeps the step-2 wrap noted above.
//! * Codex executable resolution: TS `resolveAgentBin` (through
//!   `createCodexCliInvocation`) also finds npm's Windows `codex.cmd` shim
//!   and honors configured per-runtime bin overrides; this port spawns a
//!   bare `codex` from `PATH` — Windows behavior is a documented deviation
//!   and there is no configured-bin override. The TS test seam
//!   `setCodexRunner` is likewise not ported: the pure argv / failure /
//!   parse helpers are what the unit tests exercise.
//! * Spawn failures surface Rust's `io::Error` Display (`codex CLI not
//!   found: No such file or directory (os error 2)`) where Node would
//!   report `spawn codex ENOENT`; `CodexRunError::NotFound` plays the role
//!   of `err.code === 'ENOENT'` in `probeCodexInstall`. Non-UTF8 child
//!   output is decoded lossily, matching Node's `String(buffer)` UTF-8
//!   substitution.
//! * The install-info cache lives in `AppState` (daemon-scoped, the same
//!   lifetime as the TS closure variable) behind a `std::sync::Mutex` that
//!   is never held across an `.await`, and its TTL is measured on a
//!   monotonic `Instant` instead of `Date.now()`.
//! * `webBaseUrl` reuses the guard's `js_number` parse of `OD_WEB_PORT`
//!   (step 1 covers hex/`Infinity` differences) and formats the number
//!   with Rust's `Display`, so a JS-exponential value such as `1e21` would
//!   print in decimal here.
//!
//! Step 4 of the MCP routes port (bead `open-design-rust-r2r`): the OAuth
//! status surface — `GET /api/mcp/oauth/status` (mcp-routes.ts:360) and
//! `POST /api/mcp/oauth/disconnect` (mcp-routes.ts:381) — plus the token
//! store behind them, ported in full as [`crate::mcp_tokens`]
//! (`mcp-tokens.ts:1-258`: sanitizer, atomic write + `chmod`, expiry
//! skew).
//!
//! DOCUMENTED DEVIATIONS (step 4 — OAuth status + disconnect)
//!
//! * Error bodies: the step-2 envelope wrap applies again. TypeScript's
//!   flat `{ error: "cross-origin request rejected" }`,
//!   `{ error: "serverId is required" }`, and
//!   `{ error: String(err.message) }` become the crate-wide
//!   `{ error: { code, message } }` with the message text kept verbatim
//!   (`FORBIDDEN` / `BAD_REQUEST` / `INTERNAL_ERROR`). The 500s carry
//!   Rust's `io::Error` Display where Node reports `err.message` — the
//!   same wording gap step 2 documents for config-store I/O.
//! * Disconnect body parsing: in Express, `express.json({ limit: '4mb' })`
//!   (server.ts:3221) runs before this route, so a *malformed* JSON body
//!   answers 400 **before** `isLocalSameOrigin` ever runs, while a
//!   missing or non-JSON `Content-Type` is skipped outright — `req.body`
//!   stays undefined and the route's own guard runs next. This port
//!   extracts `Result<Json<Value>, JsonRejection>`, which never fails the
//!   request, and reproduces that split from inside the handler: a parse
//!   error returns axum's 400 **before** the guard (same status as
//!   body-parser's `entity.parse.failed`, plain-text body — the step-2
//!   wording deviation), `MissingJsonContentType` — how axum 0.8.9 names
//!   both the absent and the non-JSON `Content-Type` case — becomes the
//!   "no body" path that does run the guard, and every parsed body feeds
//!   the TS `req.body?.serverId` check. Two shapes take the other side of
//!   that split here, and both only move *which* rejection answers;
//!   neither turns a request TypeScript rejects into a 200: an **empty**
//!   `application/json` body is special-cased by body-parser (`parse`
//!   returns `{}` for a zero-length body, so the guard runs) but is an
//!   EOF parse error for serde_json, so it 400s before the guard; and
//!   body-parser's `strict` mode refuses a **scalar** top-level value
//!   (`"x"` / `42` / `null`) with a 400 before the guard, while
//!   serde_json accepts it as a [`Value`] and this handler runs the
//!   guard. Same-origin status is 400 in every one of those cases either
//!   way — only cross-origin traffic flips (403 where the guard ran,
//!   instead of TypeScript's 400; 400 where the body was refused first,
//!   instead of its 403) and the response wording moves. Axum's default
//!   2 MB body limit answers 413 where body-parser allows 4 MB (both
//!   reject before the guard; the boundary differs).
//! * Status query: `RawQuery` plus `routes::single_query_value` reproduce
//!   Express query decoding (percent-decoding, `+` → space), and a
//!   repeated `serverId` key collapses to "missing" the way TypeScript's
//!   `typeof req.query.serverId === 'string'` refuses an array — the same
//!   400 follows. The value is trimmed exactly like
//!   `req.query.serverId.trim()`.
//! * Registering each path for its one method makes the other method
//!   answer 405 here where Express falls through to its JSON 404 (and
//!   auto-answers `OPTIONS`) — the same divergence as every other merged
//!   router in this crate.
//! * Not ported yet (next step): `POST /api/mcp/oauth/start`,
//!   `GET /api/mcp/oauth/callback`, and their support layer —
//!   `mcp-oauth.ts`, `getPublicBaseUrl`, `renderOAuthResultPage`
//!   (mcp-routes.ts:230-358). Until they land, the status route can only
//!   report tokens an earlier build wrote.

use std::collections::{BTreeMap, HashSet};
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::mcp_tokens::{clear_token, get_token};
use crate::routes::{api_error, internal_error, single_query_value, AppState};

/// The request facts the same-origin guard needs (parity: the `req` + `env`
/// arguments of `isLocalSameOrigin`, origin-validation.ts:212).
#[derive(Debug, Clone, Default)]
pub struct OriginGuardInputs {
    /// Raw `Host` header value.
    pub host: String,
    /// `Origin` header; `None` when absent (curl, CLI, and same-origin
    /// browser GETs that omit it per the Fetch spec).
    pub origin: Option<String>,
    /// `Sec-Fetch-Site` header; consulted only when `origin` is absent.
    pub sec_fetch_site: Option<String>,
    /// The port the listener actually bound. `0` reproduces TypeScript's
    /// "port not resolved yet" fail-closed state (`config.port` in tests).
    pub resolved_port: u16,
    /// `OD_BIND_HOST`; an empty value falls back to `127.0.0.1`.
    pub bind_host: String,
    /// Raw `OD_ALLOWED_ORIGINS` value (comma-separated deployment origins).
    pub allowed_origins_raw: String,
    /// Raw `OD_WEB_PORT` value (the split-port web proxy listener).
    pub web_port_raw: String,
}

// ---- guard (parity: isLocalSameOrigin, origin-validation.ts:212) ----------

/// Decide whether a request is same-origin for the local daemon.
///
/// The control flow mirrors `isLocalSameOrigin` exactly: compute the
/// allowed port list, the bind host, and the configured allow-list once
/// (including the IP-literal subset used by the no-Origin host check), then
/// branch on whether `Origin` is present.
pub fn is_local_same_origin(inputs: &OriginGuardInputs) -> bool {
    let ports = allowed_browser_ports(inputs.resolved_port, &inputs.web_port_raw);
    let bind_host = if inputs.bind_host.is_empty() {
        "127.0.0.1"
    } else {
        inputs.bind_host.as_str()
    };
    let extra_allowed_origins = configured_allowed_origins(&inputs.allowed_origins_raw);
    let ip_only_extra_origins: Vec<ParsedUrl> = extra_allowed_origins
        .iter()
        .filter(|origin| is_ip_literal_hostname(&origin.hostname))
        .cloned()
        .collect();

    let local_host_allowed =
        is_allowed_browser_host(&inputs.host, &ports, bind_host, &ip_only_extra_origins);
    let Some(origin) = inputs.origin.as_deref().filter(|value| !value.is_empty()) else {
        if local_host_allowed {
            return true;
        }
        // Browsers omit Origin on same-origin GET subresources (Fetch spec),
        // which made hostname entries in OD_ALLOWED_ORIGINS unreachable
        // behind a reverse proxy. Sec-Fetch-Site is set by the user agent
        // and cannot be modified by page script, so "same-origin" attests
        // to the target origin — only then consult the full allow-list.
        if inputs.sec_fetch_site.as_deref() == Some("same-origin") {
            return is_allowed_browser_host(&inputs.host, &ports, bind_host, &extra_allowed_origins);
        }
        return false;
    };
    // Reverse-proxy escape hatch: the daemon sees the proxy upstream's Host,
    // so an Origin that exactly matches an allow-listed deployment origin
    // is trusted before any host check.
    if extra_allowed_origins
        .iter()
        .any(|allowed| allowed.origin() == origin)
    {
        return true;
    }
    if !is_allowed_browser_host(&inputs.host, &ports, bind_host, &extra_allowed_origins) {
        return false;
    }
    is_allowed_browser_origin(origin, &inputs.host, &ports, bind_host, &extra_allowed_origins)
}

/// [`is_local_same_origin`] with `OD_ALLOWED_ORIGINS`, `OD_WEB_PORT`, and
/// `OD_BIND_HOST` read from the process environment at call time — parity
/// with TypeScript's live `process.env` reads (the `env` argument of
/// `isLocalSameOrigin`).
pub fn is_local_same_origin_from_env(
    host: &str,
    origin: Option<&str>,
    sec_fetch_site: Option<&str>,
    resolved_port: u16,
) -> bool {
    is_local_same_origin(&OriginGuardInputs {
        host: host.to_string(),
        origin: origin.map(str::to_string),
        sec_fetch_site: sec_fetch_site.map(str::to_string),
        resolved_port,
        bind_host: std::env::var("OD_BIND_HOST").unwrap_or_default(),
        allowed_origins_raw: std::env::var("OD_ALLOWED_ORIGINS").unwrap_or_default(),
        web_port_raw: std::env::var("OD_WEB_PORT").unwrap_or_default(),
    })
}

// ---- guard helpers (allowedBrowserPorts / configuredAllowedOrigins / …) ---

/// Parity: `allowedBrowserPorts` (origin-validation.ts:92). Returns port
/// *strings* because every consumer compares against `String(port)` — the
/// parsed Host/Origin port and the `"host:port"` explicit-set entries.
fn allowed_browser_ports(primary: u16, web_port_raw: &str) -> Vec<String> {
    let mut ports = Vec::new();
    if primary != 0 {
        ports.push(primary.to_string());
    }
    if let Some(web_port) =
        js_number(web_port_raw).filter(|web_port| *web_port != f64::from(primary))
    {
        ports.push(web_port.to_string());
    }
    ports
}

/// `Number(raw)` for `OD_WEB_PORT` in guard-relevant form: trimmed, and
/// `None` for empty / `NaN` / `±0` (all falsy in JavaScript) plus
/// non-finite values (see DOCUMENTED DEVIATIONS).
fn js_number(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: f64 = trimmed.parse().ok()?;
    (value.is_finite() && value != 0.0).then_some(value)
}

/// Parity: `configuredAllowedOrigins` + `configuredAllowedHosts`
/// (origin-validation.ts:15 / 31) — one parse yields both the normalized
/// `origin` (scheme://host[:port]) and the `host` those map to.
fn configured_allowed_origins(raw: &str) -> Vec<ParsedUrl> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let mut origins = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match parse_url(entry) {
            // Also covers entries with a non-http(s) scheme: TypeScript
            // throws for both shapes (see DOCUMENTED DEVIATIONS).
            Some(url) => origins.push(url),
            None => tracing::warn!(entry = %entry, "skipping malformed OD_ALLOWED_ORIGINS entry"),
        }
    }
    origins
}

/// Parity: `isAllowedBrowserHost` (origin-validation.ts:153). The explicit
/// set holds every `loopback-or-bind-host:port` plus the host of each
/// extra allowed origin; anything outside it must land on an allowed port
/// AND a loopback/private-LAN hostname.
fn is_allowed_browser_host(
    host_header: &str,
    ports: &[String],
    bind_host: &str,
    extra_allowed_origins: &[ParsedUrl],
) -> bool {
    let Some(request_host) = parse_host_header(host_header) else {
        return false;
    };

    let mut explicit_hosts = HashSet::new();
    for port in ports {
        for loopback_host in ["127.0.0.1", "localhost", "[::1]"] {
            explicit_hosts.insert(format!("{loopback_host}:{port}"));
        }
        explicit_hosts.insert(format!("{bind_host}:{port}"));
    }
    for allowed in extra_allowed_origins {
        explicit_hosts.insert(allowed.host.clone());
    }
    if explicit_hosts.contains(&request_host.host) {
        return true;
    }

    // Parity: `ports.map(String).includes(requestHost.port)` where
    // `requestHost.port` is `parsed.port || '80'`.
    let request_port = if request_host.port.is_empty() {
        "80"
    } else {
        request_host.port.as_str()
    };
    if !ports.iter().any(|port| port == request_port) {
        return false;
    }
    is_loopback_or_private_lan_host(&request_host.hostname)
}

/// Parity: `isAllowedBrowserOrigin` (origin-validation.ts:176). Exact
/// allow-list membership first, then the explicit
/// `scheme://loopback-or-bind-host:port` set, then the same
/// allowed-port + hostname-equality + loopback/private-LAN fall-through.
fn is_allowed_browser_origin(
    origin: &str,
    host_header: &str,
    ports: &[String],
    bind_host: &str,
    extra_allowed_origins: &[ParsedUrl],
) -> bool {
    if extra_allowed_origins
        .iter()
        .any(|allowed| allowed.origin() == origin)
    {
        return true;
    }
    let Some(parsed_origin) = parse_url(origin) else {
        return false;
    };
    let Some(request_host) = parse_host_header(host_header) else {
        return false;
    };

    let mut explicit_origins = HashSet::new();
    for port in ports {
        for scheme in ["http", "https"] {
            for loopback_host in ["127.0.0.1", "localhost", "[::1]"] {
                explicit_origins.insert(format!("{scheme}://{loopback_host}:{port}"));
            }
            explicit_origins.insert(format!("{scheme}://{bind_host}:{port}"));
        }
    }
    if explicit_origins.contains(origin) {
        return true;
    }

    // Parity: `parsedOrigin.port || (https ? '443' : '80')`.
    let origin_port = if parsed_origin.port.is_empty() {
        if parsed_origin.scheme == "https" {
            "443"
        } else {
            "80"
        }
    } else {
        parsed_origin.port.as_str()
    };
    if !ports.iter().any(|port| port == origin_port) {
        return false;
    }
    if parsed_origin.hostname != request_host.hostname {
        return false;
    }
    is_loopback_or_private_lan_host(&parsed_origin.hostname)
}

/// Parity: `isLoopbackOrPrivateLanHost` (origin-validation.ts:140).
fn is_loopback_or_private_lan_host(hostname: &str) -> bool {
    let host = hostname.to_ascii_lowercase();
    matches!(
        host.as_str(),
        "localhost" | "127.0.0.1" | "::1" | "[::1]" | "0.0.0.0" | "::"
    ) || is_private_ipv4(&host)
}

/// Parity: `isPrivateIpv4` (origin-validation.ts:115) — four ASCII-numeric
/// octets (`Number()` semantics, so `010` is 10) in the RFC1918 /
/// link-local ranges.
fn is_private_ipv4(hostname: &str) -> bool {
    let Some([a, b, ..]) = dotted_octets(hostname) else {
        return false;
    };
    match [a, b] {
        [10, _] => true,
        [172, b] if (16..=31).contains(&b) => true,
        [192, 168] => true,
        [169, 254] => true,
        _ => false,
    }
}

/// Parity: `isIpLiteralHostname` (origin-validation.ts:130) — a bracketed
/// IPv6 literal or four decimal octets within 0–255.
fn is_ip_literal_hostname(hostname: &str) -> bool {
    let host = hostname.trim();
    if host.is_empty() {
        return false;
    }
    if host.starts_with('[') && host.ends_with(']') {
        return true;
    }
    dotted_octets(host).is_some()
}

/// Four ASCII-numeric octets within 0–255, else `None` — the numeric
/// shape shared by `isPrivateIpv4` / `isIpLiteralHostname` (`/^\d+$/` per
/// part, then `Number.isInteger(n) && n >= 0 && n <= 255`).
fn dotted_octets(hostname: &str) -> Option<[u16; 4]> {
    let mut parts = hostname.split('.');
    let mut octets = [0u16; 4];
    for octet in &mut octets {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *octet = part.parse::<u16>().ok()?;
        if *octet > 255 {
            return None;
        }
    }
    parts.next().is_none().then_some(octets)
}

// ---- WHATWG-flavored URL parsing (parity: `new URL` / `parseHostHeader`) --

/// Normalized pieces of an `http(s)` authority. `hostname` / `host` /
/// `port` mirror the URL standard's fields (default port omitted from
/// `host`, IPv6 bracketed, IPv4 canonicalized); `origin()` serializes the
/// `origin` field TypeScript reads.
#[derive(Debug, Clone)]
struct ParsedUrl {
    scheme: String,
    hostname: String,
    /// `hostname[:port]` with the scheme's default port omitted (URL `host`).
    host: String,
    /// URL `port`: normalized digits, empty when absent or default.
    port: String,
}

impl ParsedUrl {
    /// `scheme://host` — the URL `origin` serialization, default port
    /// already omitted by `host` (parity: `parsed.origin`).
    fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.host)
    }
}

/// Parity: `parseHostHeader` (origin-validation.ts:104) —
/// `new URL('http://' + String(value).trim())`; unparsable hosts yield
/// `None` instead of `null`. The `port || '80'` fallback is applied where
/// the port is compared, so `host` can keep its default-port omission.
fn parse_host_header(value: &str) -> Option<ParsedUrl> {
    parse_url(&format!("http://{value}"))
}

/// Parity: the `new URL(...)` parses behind `configuredAllowedOrigins` and
/// `isAllowedBrowserOrigin`. `None` covers every case where the URL parser
/// throws or yields a non-http(s) scheme — each guard branch treats those
/// as a rejection.
fn parse_url(raw: &str) -> Option<ParsedUrl> {
    let cleaned = preprocess(raw);
    let (scheme, rest) = split_scheme(&cleaned)?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    // Special-scheme "authority ignore slashes": `http:////host` ≡ `http://host`.
    let rest = rest.trim_start_matches(['/', '\\']);
    let authority = rest
        .split(['/', '\\', '?', '#'])
        .next()
        .unwrap_or_default();
    // Userinfo is discarded; the WHATWG host parser restarts after the
    // last `@` (`http://user:pass@host` → host `host`).
    let authority = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    if authority.is_empty() {
        return None;
    }

    let (raw_host, raw_port) = if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6 literal: `[::1]` / `[::1]:7456`.
        let close = rest.find(']')?;
        let address: Ipv6Addr = rest[..close].parse().ok()?;
        let after = &rest[close + 1..];
        if !after.is_empty() && !after.starts_with(':') {
            return None;
        }
        (
            format!("[{address}]"),
            after.strip_prefix(':').unwrap_or_default(),
        )
    } else {
        match authority.find(':') {
            Some(colon) => (authority[..colon].to_string(), &authority[colon + 1..]),
            None => (authority.to_string(), ""),
        }
    };
    if raw_host.is_empty() {
        return None;
    }
    let hostname = if raw_host.starts_with('[') {
        raw_host // IPv6 already canonicalized through `Ipv6Addr`
    } else {
        normalize_hostname(&raw_host)?
    };
    let port = normalize_port(raw_port, &scheme)?;
    let host = if port.is_empty() {
        hostname.clone()
    } else {
        format!("{hostname}:{port}")
    };
    Some(ParsedUrl {
        scheme,
        hostname,
        host,
        port,
    })
}

/// WHATWG input preprocessing plus `parseHostHeader`'s `.trim()`: strip
/// tab / LF / CR everywhere, then trim control and whitespace at both ends.
fn preprocess(raw: &str) -> String {
    raw.chars()
        .filter(|character| !matches!(character, '\t' | '\n' | '\r'))
        .collect::<String>()
        .trim_matches(|character: char| character.is_whitespace() || character.is_control())
        .to_string()
}

/// RFC 3986 scheme (`alpha *( alpha / digit / "+" / "-" / "." ) ":"`),
/// matching the URL parser's scheme state; `None` when absent or invalid.
fn split_scheme(raw: &str) -> Option<(&str, &str)> {
    let mut indices = raw.char_indices();
    match indices.next() {
        Some((_, first)) if first.is_ascii_alphabetic() => {}
        _ => return None,
    }
    for (index, character) in indices {
        if character == ':' {
            return Some((&raw[..index], &raw[index + 1..]));
        }
        if !(character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')) {
            return None;
        }
    }
    None
}

/// Percent-decode + ASCII-lowercase + canonicalize the way the URL host
/// parser does; `None` for anything it would fail on — forbidden host code
/// points, controls/space/non-ASCII (no IDNA, see DOCUMENTED DEVIATIONS),
/// or an IPv4-shaped name that does not parse.
fn normalize_hostname(raw: &str) -> Option<String> {
    let decoded = percent_decode(raw)?;
    for byte in decoded.bytes() {
        // Visible ASCII only (0x21..=0x7E). Controls, space, and non-ASCII
        // would fail — or be punycode-converted by — IDNA.
        if !(0x21..=0x7e).contains(&byte) {
            return None;
        }
        // Forbidden host code points, plus any `%` left behind by an
        // invalid escape or `%25` (IDNA rejects it in TypeScript).
        if matches!(
            byte,
            b'#' | b'/' | b':' | b'<' | b'>' | b'?' | b'@' | b'[' | b'\\' | b']' | b'^' | b'|' | b'%'
        ) {
            return None;
        }
    }
    let hostname = decoded.to_ascii_lowercase();
    if ends_in_number(&hostname) {
        parse_ipv4(&hostname)
    } else {
        Some(hostname)
    }
}

/// WHATWG `percentDecode`: valid `%XX` triplets become their byte; invalid
/// escapes stay literal and are rejected by the caller's `%` check (IDNA
/// refuses `%`). Decoded bytes must still form UTF-8 — see DOCUMENTED
/// DEVIATIONS.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (
                hex_digit(bytes[index + 1]),
                hex_digit(bytes[index + 2]),
            ) {
                decoded.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).ok()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// URL `port` field: ASCII digits normalized to their numeric form, empty
/// when absent or equal to the scheme's default (the URL serializer omits
/// default ports from `host`), `None` when out of range or junk (parity:
/// the parser throws on `:bad` / `:99999`).
fn normalize_port(raw: &str, scheme: &str) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }
    if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value: u64 = raw.parse().ok()?;
    if value > u64::from(u16::MAX) {
        return None;
    }
    let default = if scheme == "https" { 443 } else { 80 };
    if value == default {
        return Some(String::new());
    }
    Some(value.to_string())
}

/// WHATWG "ends in a number": the final label (ignoring one trailing empty
/// label) is all digits or a `0x`-prefixed hex number — only then does the
/// IPv4 parser apply (`1.2.3.0xag` stays a domain, `1.2.3.0X7F` becomes
/// `1.2.3.127`).
fn ends_in_number(hostname: &str) -> bool {
    let mut labels: Vec<&str> = hostname.split('.').collect();
    if labels.last().is_some_and(|label| label.is_empty()) {
        labels.pop();
    }
    match labels.last() {
        Some(label) if !label.is_empty() => {
            label.bytes().all(|byte| byte.is_ascii_digit())
                || label
                    .strip_prefix("0x")
                    .or_else(|| label.strip_prefix("0X"))
                    .is_some_and(|digits| digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
        }
        _ => false,
    }
}

/// WHATWG IPv4 parser: at most four dot-separated parts (one trailing dot
/// allowed), radix-aware part numbers (hex `0x…`, octal leading-`0`,
/// decimal), non-final parts capped at 255, final part below
/// `256^(5 − parts)`. Returns the canonical dotted quad.
fn parse_ipv4(hostname: &str) -> Option<String> {
    let mut parts: Vec<&str> = hostname.split('.').collect();
    if parts.last().is_some_and(|part| part.is_empty()) {
        parts.pop();
    }
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut numbers = Vec::with_capacity(parts.len());
    for part in &parts {
        numbers.push(parse_ipv4_number(part)?);
    }
    let (last, non_final) = numbers.split_last()?;
    if non_final.iter().any(|number| *number > 255) {
        return None;
    }
    if *last >= 256u64.checked_pow((5 - parts.len()) as u32)? {
        return None;
    }
    // Non-final parts land at 256³, 256², …; the final part always lands
    // at 256⁰ (the URL spec's combine order: `127.1` → 127.0.0.1).
    let mut value: u64 = 0;
    for (index, number) in numbers.iter().enumerate() {
        if index + 1 == numbers.len() {
            value += number;
        } else {
            value += number * 256u64.pow((3 - index) as u32);
        }
    }
    if value > u64::from(u32::MAX) {
        return None; // unreachable behind the caps above; kept explicit
    }
    Some(format!(
        "{}.{}.{}.{}",
        value >> 24,
        (value >> 16) & 0xff,
        (value >> 8) & 0xff,
        value & 0xff
    ))
}

/// One IPv4 part: `0x…` hex, leading-`0` octal, else decimal (parity with
/// the WHATWG "IPv4 number parser", including a bare `0x` → 0).
fn parse_ipv4_number(part: &str) -> Option<u64> {
    if part.is_empty() {
        return None;
    }
    let (digits, radix) = if let Some(hex) =
        part.strip_prefix("0x").or_else(|| part.strip_prefix("0X"))
    {
        (hex, 16u32)
    } else if part.len() > 1 && part.starts_with('0') {
        (part, 8)
    } else {
        (part, 10)
    };
    if digits.is_empty() {
        return Some(0); // only reachable for a bare `0x` / `0X`
    }
    let mut value: u64 = 0;
    for digit in digits.bytes() {
        let digit = char::from(digit).to_digit(radix)? as u64;
        value = value.checked_mul(u64::from(radix))?.checked_add(digit)?;
    }
    Some(value)
}

// ───────────────────────────────────────────────────────────────────────
// External MCP server configuration — parity port of `mcp-config.ts:30-275`
// and the GET/PUT `/api/mcp/servers` routes (`mcp-routes.ts:191` / `:205`).
// ───────────────────────────────────────────────────────────────────────

// ---- wire types (parity: McpTransport / McpAuthMode / McpServerConfig) ----

/// Parity: `McpTransport` (mcp-config.ts:30) — the `stdio | sse | http`
/// string union, serialized exactly as those lowercase literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Sse,
    Http,
}

/// Parity: `McpAuthMode` (mcp-config.ts:31) — `none | oauth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpAuthMode {
    None,
    Oauth,
}

/// Parity: `McpServerConfig` (mcp-config.ts:33).
///
/// Field declaration order is load-bearing: it reproduces the insertion
/// order of the object `sanitizeMcpServer` builds, so
/// `serde_json::to_string_pretty` writes the same bytes as TypeScript's
/// `JSON.stringify(next, null, 2)` (stdio entries never carry `url` /
/// `authMode` / `headers`; http/sse entries never carry `command` / `args`
/// / `env`; `label` / `templateId` appear only when non-empty — see the
/// `skip_serializing_if`s below).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct McpServerConfig {
    pub id: String,
    pub transport: McpTransport,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(rename = "templateId", skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// String→string map; values are always `Value::String` (enforced by
    /// [`sanitize_string_map`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "authMode", skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<McpAuthMode>,
    /// Same shape and rules as [`McpServerConfig::env`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<Map<String, Value>>,
}

/// Parity: `McpConfig` (mcp-config.ts:47) — `{ servers: [...] }`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
}

impl McpConfig {
    fn empty() -> Self {
        Self {
            servers: Vec::new(),
        }
    }
}

// ---- sanitizer (parity: mcp-config.ts helpers + sanitizeMcpServer) ------

/// Parity: `SERVER_ID_PATTERN = /^[a-z0-9][a-z0-9_-]{0,63}$/i`
/// (mcp-config.ts:100) — 1–64 ASCII characters, first alphanumeric.
fn is_valid_server_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let Some((first, rest)) = bytes.split_first() else {
        return false;
    };
    rest.len() <= 63
        && first.is_ascii_alphanumeric()
        && rest
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Parity: `isPlainObject` (mcp-config.ts:106) — `serde_json::Value`'s
/// object variant (`null` and arrays are not objects).
fn is_plain_object(raw: &Value) -> Option<&Map<String, Value>> {
    raw.as_object()
}

/// Parity: `sanitizeStringMap` (mcp-config.ts:110). Key order follows the
/// input object (serde_json is built with `preserve_order`), matching
/// JavaScript's `Object.entries` insertion order. Returns `None` when
/// nothing survives — an absent field, not an empty map.
fn sanitize_string_map(raw: &Value) -> Option<Map<String, Value>> {
    let object = is_plain_object(raw)?;
    let mut out = Map::new();
    for (key, value) in object {
        // Prototype-pollution sentinels: never persist these keys.
        if key == "__proto__" || key == "constructor" {
            continue;
        }
        if key.trim().is_empty() {
            continue;
        }
        let Value::String(value) = value else {
            continue;
        };
        // Drop empty / whitespace-only values. Persisting them is worse
        // than omitting them: the spawn-time merge treats a present header
        // as "user pinned this", which would block our daemon-issued OAuth
        // Bearer from being injected (parity comment at mcp-config.ts:117).
        if value.trim().is_empty() {
            continue;
        }
        out.insert(key.clone(), Value::String(value.clone()));
    }
    (!out.is_empty()).then_some(out)
}

/// Parity: `sanitizeStringArray` (mcp-config.ts:129) — strings only,
/// non-arrays and all-filtered-out arrays collapse to `None`.
fn sanitize_string_array(raw: &Value) -> Option<Vec<String>> {
    let array = raw.as_array()?;
    let out: Vec<String> = array
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    (!out.is_empty()).then_some(out)
}

/// Parity: `normalizeHost` (mcp-config.ts:135) — strip one leading `[`,
/// one trailing `]`, lowercase, strip trailing dots.
fn normalize_host(hostname: &str) -> String {
    let mut host = hostname;
    if let Some(rest) = host.strip_prefix('[') {
        host = rest;
    }
    if let Some(rest) = host.strip_suffix(']') {
        host = rest;
    }
    host.to_ascii_lowercase()
        .trim_end_matches('.')
        .to_string()
}

/// Parity: `isLoopbackHost` (mcp-config.ts:142) — `localhost`, `::1`,
/// `127.x.x.x`, or `::ffff:127.x.x.x`, after [`normalize_host`]. The
/// `127.` patterns mirror the JS regexes (`\.\d{1,3}` without a 0–255
/// range check).
fn is_loopback_host(hostname: &str) -> bool {
    let host = normalize_host(hostname);
    if host == "localhost" || host == "::1" {
        return true;
    }
    if is_127_quad(&host) {
        return true;
    }
    host.strip_prefix("::ffff:")
        .is_some_and(is_127_quad)
}

/// `^127(?:\.\d{1,3}){3}$` (ASCII digits, 1–3 per part, first part exactly
/// `127`) — shared by [`is_loopback_host`]'s plain and IPv4-mapped forms.
fn is_127_quad(host: &str) -> bool {
    let mut parts = host.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    if first != "127" {
        return false;
    }
    let mut count = 1;
    for part in parts {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        count += 1;
    }
    count == 4
}

/// Parity: `inferMcpAuthModeForUrl` (mcp-config.ts:150) — unparsable or
/// missing URLs default to `oauth` for backward compatibility; loopback
/// endpoints need no managed OAuth.
pub fn infer_mcp_auth_mode_for_url(raw_url: Option<&str>) -> McpAuthMode {
    let Some(raw_url) = raw_url.filter(|url| !url.is_empty()) else {
        return McpAuthMode::Oauth;
    };
    match url::Url::parse(raw_url) {
        Ok(parsed) => {
            if is_loopback_host(parsed.host_str().unwrap_or_default()) {
                McpAuthMode::None
            } else {
                McpAuthMode::Oauth
            }
        }
        Err(_) => McpAuthMode::Oauth,
    }
}

/// Parity: `sanitizeMcpAuthMode` (mcp-config.ts:159) — exact
/// case-sensitive `none` / `oauth` membership, else `None` (→ inferred).
fn sanitize_mcp_auth_mode(raw: &Value) -> Option<McpAuthMode> {
    match raw.as_str()? {
        "none" => Some(McpAuthMode::None),
        "oauth" => Some(McpAuthMode::Oauth),
        _ => None,
    }
}

/// Parity: `effectiveMcpAuthMode` (mcp-config.ts:165) — stdio/sse-less
/// servers never carry managed OAuth; http/sse use the pinned `authMode`
/// or infer from the stored (normalized) URL.
pub fn effective_mcp_auth_mode(server: &McpServerConfig) -> McpAuthMode {
    if server.transport != McpTransport::Http && server.transport != McpTransport::Sse {
        return McpAuthMode::None;
    }
    server
        .auth_mode
        .unwrap_or_else(|| infer_mcp_auth_mode_for_url(server.url.as_deref()))
}

/// Parity: `sanitizeMcpServer` (mcp-config.ts:175). Validates one
/// user-supplied entry, dropping invalid *fields* so a typo in one server
/// doesn't tank the whole config, and the whole entry when it is
/// unsalvageable (bad id, unknown transport, missing transport-required
/// fields) — `None` is TypeScript's `null`.
pub fn sanitize_mcp_server(raw: &Value) -> Option<McpServerConfig> {
    let object = is_plain_object(raw)?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if !is_valid_server_id(id) {
        return None;
    }
    // Non-string / absent transport defaults to `stdio`; an unrecognized
    // *string* transport rejects the entry (mcp-config.ts:179-180).
    let transport = match object.get("transport") {
        Some(Value::String(transport)) => match transport.as_str() {
            "stdio" => McpTransport::Stdio,
            "sse" => McpTransport::Sse,
            "http" => McpTransport::Http,
            _ => return None,
        },
        _ => McpTransport::Stdio,
    };

    let mut next = McpServerConfig {
        id: id.to_string(),
        transport,
        // `enabled !== false`: only a literal JSON `false` disables; a
        // string `"false"` or a number keeps the default `true`.
        enabled: object.get("enabled") != Some(&Value::Bool(false)),
        label: None,
        template_id: None,
        command: None,
        args: None,
        env: None,
        url: None,
        auth_mode: None,
        headers: None,
    };
    if let Some(label) = object.get("label").and_then(Value::as_str) {
        if !label.trim().is_empty() {
            next.label = Some(label.trim().to_string());
        }
    }
    if let Some(template_id) = object.get("templateId").and_then(Value::as_str) {
        if !template_id.trim().is_empty() {
            next.template_id = Some(template_id.trim().to_string());
        }
    }

    match transport {
        McpTransport::Stdio => {
            // A stdio server without a usable command cannot spawn.
            let command = object
                .get("command")
                .and_then(Value::as_str)?
                .trim()
                .to_string();
            if command.is_empty() {
                return None;
            }
            next.command = Some(command);
            next.args = object.get("args").and_then(sanitize_string_array);
            next.env = object.get("env").and_then(sanitize_string_map);
        }
        McpTransport::Sse | McpTransport::Http => {
            let raw_url = object.get("url").and_then(Value::as_str)?;
            let trimmed = raw_url.trim();
            if trimmed.is_empty() {
                return None;
            }
            // Reject anything that isn't an http(s) URL — protects against
            // accidental `file://` / `javascript:` slipping into a config
            // file (parity comment at mcp-config.ts:204).
            let parsed = url::Url::parse(trimmed).ok()?;
            if parsed.scheme() != "http" && parsed.scheme() != "https" {
                return None;
            }
            let href = parsed.to_string();
            next.url = Some(href.clone());
            next.auth_mode = Some(
                sanitize_mcp_auth_mode(object.get("authMode").unwrap_or(&Value::Null))
                    .unwrap_or_else(|| infer_mcp_auth_mode_for_url(Some(&href))),
            );
            next.headers = object.get("headers").and_then(sanitize_string_map);
        }
    }
    Some(next)
}

/// Parity: `sanitizeMcpConfig` (mcp-config.ts:220) — non-objects become an
/// empty config; entries keep list order, first occurrence per id wins,
/// invalid entries are dropped silently.
pub fn sanitize_mcp_config(raw: &Value) -> McpConfig {
    let Some(object) = is_plain_object(raw) else {
        return McpConfig::empty();
    };
    let fallback = Vec::new();
    let list = object
        .get("servers")
        .and_then(Value::as_array)
        .unwrap_or(&fallback);
    let mut seen = HashSet::new();
    let mut servers = Vec::new();
    for entry in list {
        let Some(server) = sanitize_mcp_server(entry) else {
            continue;
        };
        // De-dupe by id (first wins) — `insert` reports the duplicate.
        if !seen.insert(server.id.clone()) {
            continue;
        }
        servers.push(server);
    }
    McpConfig { servers }
}

// ---- storage (parity: config / read / write in mcp-config.ts) -----------

/// Parity: `configFile` (mcp-config.ts:102) — `<dataDir>/mcp-config.json`;
/// `<dataDir>` is the resolved daemon data root (`RuntimePaths::data_dir`).
fn config_file(data_dir: &Path) -> PathBuf {
    data_dir.join("mcp-config.json")
}

/// Parity: `readMcpConfig` (mcp-config.ts:235). Missing file → empty,
/// corrupted JSON → logged empty with the TS message prefix, any other I/O
/// error → `Err` (the route turns it into the 500). Blocking; call from
/// `spawn_blocking`.
pub fn read_mcp_config(data_dir: &Path) -> std::io::Result<McpConfig> {
    let bytes = match std::fs::read(config_file(data_dir)) {
        Ok(bytes) => bytes,
        // Parity: `err.code === 'ENOENT'` → `{ servers: [] }`.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(McpConfig::empty());
        }
        Err(err) => return Err(err),
    };
    // Node's `readFile(path, 'utf8')` substitutes U+FFFD for invalid
    // bytes instead of failing; parse the same replacement text.
    let raw = String::from_utf8_lossy(&bytes);
    match serde_json::from_str::<Value>(&raw) {
        Ok(parsed) => Ok(sanitize_mcp_config(&parsed)),
        Err(err) => {
            tracing::error!("[mcp-config] Corrupted JSON, returning empty: {err}");
            Ok(McpConfig::empty())
        }
    }
}

/// 8 lowercase hex chars for the atomic-write temp file — format parity
/// with `randomBytes(4).toString('hex')` in `doWrite`. The bytes come from
/// a `RandomState`-seeded hasher (OS randomness) mixed with pid + time;
/// not cryptographic, they only need to keep concurrent temp files apart
/// and writes are serialized per process anyway. Shared with the token
/// store in [`crate::mcp_tokens`], which writes the same shape.
pub(crate) fn random_tmp_suffix() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hash, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    std::process::id().hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_nanos())
        .hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

/// Parity: `writeMcpConfig` + `doWrite` (mcp-config.ts:253 / 267) —
/// sanitize, mkdir recursive, write `<file>.<8 hex>.tmp`, rename onto the
/// config path. Serialization (the `AppState::mcp_write_lock` the route
/// holds across this call, mirroring the per-dataDir `writeLocks` promise
/// chain) makes concurrent writers mutually exclusive per process. A
/// failed rename leaves the temp file behind, exactly like TypeScript.
/// Blocking; call from `spawn_blocking`.
pub fn write_mcp_config(data_dir: &Path, body: &Value) -> std::io::Result<McpConfig> {
    let next = sanitize_mcp_config(body);
    let file = config_file(data_dir);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = PathBuf::from(format!("{}.{}.tmp", file.display(), random_tmp_suffix()));
    let text = serde_json::to_string_pretty(&next)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &file)?;
    Ok(next)
}

// ---- embedded template list (parity: MCP_TEMPLATES, mcp-config.ts:567) ---

/// Generated artifact: `JSON.stringify(MCP_TEMPLATES, null, 2)` extracted
/// from `apps/daemon/src/mcp-config.ts` (slice from
/// `export const MCP_TEMPLATES` to its closing `];`, `eval` the literal).
/// Regenerate whenever the TS literal changes — the integration test in
/// `tests/mcp.rs` pins id-sequence parity between the two.
const MCP_TEMPLATES_JSON: &str = include_str!("../mcp-templates.json");

/// Parity: `MCP_TEMPLATES` — parsed once, served verbatim.
fn templates() -> &'static [Value] {
    static TEMPLATES: OnceLock<Vec<Value>> = OnceLock::new();
    TEMPLATES.get_or_init(|| {
        serde_json::from_str(MCP_TEMPLATES_JSON).expect("embedded mcp-templates.json parses")
    })
}

// ---- routes (parity: GET/PUT /api/mcp/servers, mcp-routes.ts:191 / 205) --

/// Register the external MCP route surface. Merged in
/// `routes::build_router` before the `/api/{*rest}` catch-all: the
/// configuration routes (`GET`/`PUT /api/mcp/servers`, step 2), the
/// install-info route and the three Codex one-click install routes
/// (step 3), and the OAuth status + disconnect routes (step 4).
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/mcp/servers",
            get(get_servers).put(put_servers),
        )
        .route("/api/mcp/install-info", get(get_install_info))
        .route(
            "/api/mcp/install/codex/status",
            get(get_codex_status),
        )
        .route(
            "/api/mcp/install/codex",
            post(post_install_codex).delete(delete_install_codex),
        )
        .route("/api/mcp/oauth/status", get(get_oauth_status))
        .route("/api/mcp/oauth/disconnect", post(post_oauth_disconnect))
}

/// Parity: the `isLocalSameOrigin(req, getResolvedPort())` prologue of both
/// handlers (mcp-routes.ts:192 / 206), fed from this request's headers and
/// the actually-bound port. Returns `None` when the request is allowed, or
/// the 403 rejection response to return as-is.
fn local_same_origin_rejection(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let header_str = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
    };
    let allowed = is_local_same_origin_from_env(
        header_str("host").unwrap_or_default(),
        header_str("origin"),
        header_str("sec-fetch-site"),
        state.resolved_port.load(Ordering::SeqCst),
    );
    if allowed {
        return None;
    }
    Some(api_error(
        StatusCode::FORBIDDEN,
        "FORBIDDEN",
        "cross-origin request rejected",
    ))
}

/// `{"servers": [...], "templates": [...]}` — the shared success body of
/// both routes (mcp-routes.ts:197 / 211); key order included.
fn servers_response(config: McpConfig) -> Response {
    Json(json!({ "servers": config.servers, "templates": templates() })).into_response()
}

/// `GET /api/mcp/servers` (parity: mcp-routes.ts:191) — saved entries plus
/// the built-in template list. Storage failure → 500 whose `message` is
/// `String(err.message)` from the TS route (see DOCUMENTED DEVIATIONS for
/// the envelope and wording differences).
async fn get_servers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = local_same_origin_rejection(&state, &headers) {
        return response;
    }
    let data_dir = state.config.paths.data_dir().to_path_buf();
    match tokio::task::spawn_blocking(move || read_mcp_config(&data_dir)).await {
        Ok(Ok(config)) => servers_response(config),
        Ok(Err(err)) => {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", &err.to_string())
        }
        Err(join) => internal_error(&join.to_string()),
    }
}

/// `PUT /api/mcp/servers` (parity: mcp-routes.ts:205) — sanitize, persist,
/// echo the sanitized config. `writeMcpConfig` only rejects when the write
/// fails (sanitization silently drops bad entries), so storage failures
/// answer 400 with `String(err.message)` — the same status TS answers any
/// rejection with. The write lock spans sanitize → rename, matching the
/// `writeLocks` chain.
async fn put_servers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(response) = local_same_origin_rejection(&state, &headers) {
        return response;
    }
    let data_dir = state.config.paths.data_dir().to_path_buf();
    let _write = state.mcp_write_lock.lock().await;
    match tokio::task::spawn_blocking(move || write_mcp_config(&data_dir, &body)).await {
        Ok(Ok(config)) => servers_response(config),
        Ok(Err(err)) => {
            api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", &err.to_string())
        }
        // TS has no equivalent of a panicking task; the route's `catch`
        // still maps every failure to 400, so the status matches.
        Err(join) => api_error(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            &join.to_string(),
        ),
    }
}

// ───────────────────────────────────────────────────────────────────────
// Install payload + Codex one-click install — parity port of
// `mcp-install-info.ts`, `codex-cli.ts`, and the install routes in
// `mcp-routes.ts:13-185`.
// ───────────────────────────────────────────────────────────────────────

// ---- payload builder (parity: buildMcpInstallPayload) ------------------

/// Parity: `BuildMcpInstallPayloadInputs` (mcp-install-info.ts:14). Like the
/// TS module, this is pure: the fs probes, `process.execPath`, and sidecar
/// detection stay in the caller ([`compute_install_payload`]).
#[derive(Debug, Clone)]
pub struct BuildMcpInstallPayloadInputs {
    /// TS `cliPath` — the `od` CLI entry (see DOCUMENTED DEVIATIONS for how
    /// this port resolves TS's `OD_BIN`).
    pub cli_path: String,
    pub cli_exists: bool,
    /// TS `execPath` — the runtime running the daemon right now
    /// (`process.execPath` in TS, `std::env::current_exe()` here).
    pub exec_path: String,
    pub node_exists: bool,
    pub port: u16,
    /// TS `NodeJS.Platform` spelling — see [`ts_platform`].
    pub platform: String,
    pub data_dir: String,
    pub electron_as_node: bool,
    /// True when the daemon was bootstrapped as a sidecar and the spawned
    /// `od mcp` should discover the live URL through its inherited client
    /// instead of a baked `--daemon-url`. Always false in this port (see
    /// DOCUMENTED DEVIATIONS).
    pub is_sidecar_mode: bool,
    /// TS `sidecarEnv` — opaque entries merged after `OD_DATA_DIR`.
    pub sidecar_env: BTreeMap<String, String>,
    /// TS `webBaseUrl` — `None` when the daemon has no known web port.
    pub web_base_url: Option<String>,
}

/// Parity: `McpInstallPayload` (mcp-install-info.ts:40). Field order is
/// load-bearing: it reproduces the TS return literal, so
/// `serde_json::to_string` writes the same bytes as `JSON.stringify`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInstallPayload {
    pub command: String,
    pub args: Vec<String>,
    /// String→string map whose insertion order matches the TS object literal
    /// (`OD_DATA_DIR` first, then `sidecar_env`, then
    /// `ELECTRON_RUN_AS_NODE`).
    pub env: Map<String, Value>,
    pub daemon_url: String,
    /// Browser-facing studio base URL the daemon is paired with, when known;
    /// `null` otherwise (serialized explicitly, like the TS `null`).
    pub web_base_url: Option<String>,
    pub platform: String,
    pub cli_exists: bool,
    pub node_exists: bool,
    pub build_hint: Option<String>,
}

/// Parity: `buildMcpInstallPayload` (mcp-install-info.ts:55) — the exact
/// `{command, args, env, daemonUrl, webBaseUrl, platform, cliExists,
/// nodeExists, buildHint}` shape.
///
/// `OD_DATA_DIR` is pinned to the daemon's resolved data root so a spawned
/// MCP process writes where the daemon does even when the IDE that launched
/// it (Antigravity, VS Code, …) does not inherit the packaged app's
/// environment — parity comment for issue #848 at mcp-install-info.ts:69.
pub fn build_mcp_install_payload(inputs: BuildMcpInstallPayloadInputs) -> McpInstallPayload {
    let mut hints: Vec<String> = Vec::new();
    if !inputs.cli_exists {
        hints.push(format!(
            "OpenDesign CLI entry is missing at {}. Rebuild the daemon or packaged app and refresh.",
            inputs.cli_path
        ));
    }
    if !inputs.node_exists {
        hints.push(format!(
            "Node-compatible runtime at {} no longer exists. Reinstall OpenDesign or Node and restart the daemon.",
            inputs.exec_path
        ));
    }

    let mut env = Map::new();
    env.insert("OD_DATA_DIR".to_string(), Value::String(inputs.data_dir));
    // A sidecar entry named `OD_DATA_DIR` wins — the TS object spread
    // overwrites the literal key it was spread after.
    for (key, value) in inputs.sidecar_env {
        env.insert(key, Value::String(value));
    }
    if inputs.electron_as_node {
        env.insert("ELECTRON_RUN_AS_NODE".to_string(), Value::String("1".to_string()));
    }

    let daemon_url = format!("http://127.0.0.1:{}", inputs.port);
    // Sidecar mode: omit `--daemon-url` so the spawned `od mcp` discovers
    // the live URL through its inherited client on every spawn, surviving
    // ephemeral-port restarts. Direct launches have no socket and need the
    // URL baked (parity comment at mcp-install-info.ts:82).
    let args = if inputs.is_sidecar_mode {
        vec![inputs.cli_path, "mcp".to_string()]
    } else {
        vec![
            inputs.cli_path,
            "mcp".to_string(),
            "--daemon-url".to_string(),
            daemon_url.clone(),
        ]
    };

    McpInstallPayload {
        command: inputs.exec_path,
        args,
        env,
        daemon_url,
        // `typeof webBaseUrl === 'string' && length > 0` (TS keeps the URL,
        // else null).
        web_base_url: inputs.web_base_url.filter(|url| !url.is_empty()),
        platform: inputs.platform,
        cli_exists: inputs.cli_exists,
        node_exists: inputs.node_exists,
        build_hint: (!hints.is_empty()).then(|| hints.join(" ")),
    }
}

// ---- call-site inputs (parity: computeInstallPayload, mcp-routes.ts:47) --

/// `OD_DAEMON_CLI_PATH` (daemon-paths.ts:10) — the first env override
/// `resolveDaemonCliPath` consults before `OD_BIN`.
const DAEMON_CLI_PATH_ENV: &str = "OD_DAEMON_CLI_PATH";

/// The `od` CLI entry for the payload — parity for the `OD_BIN` value the TS
/// route reads through `resolveDaemonCliPath()` (daemon-paths.ts:19).
///
/// The env half of that resolver is ported verbatim; the package-dist
/// fallback (`require.resolve('@open-design/daemon/package.json')` →
/// `dist/cli.js`) has no Rust equivalent, so this guesses the sibling `od`
/// executable next to the running binary (see DOCUMENTED DEVIATIONS).
fn resolve_od_bin() -> String {
    for key in [DAEMON_CLI_PATH_ENV, "OD_BIN"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                continue;
            }
            // `path.resolve(value)`: a relative override joins the cwd.
            let path = PathBuf::from(trimmed);
            let resolved = if path.is_absolute() {
                path
            } else {
                match std::env::current_dir() {
                    Ok(cwd) => cwd.join(&path),
                    Err(_) => path,
                }
            };
            return resolved.to_string_lossy().into_owned();
        }
    }
    let sibling = if cfg!(windows) { "od.exe" } else { "od" };
    match std::env::current_exe() {
        Ok(exe) => exe.with_file_name(sibling).to_string_lossy().into_owned(),
        Err(_) => sibling.to_string(),
    }
}

/// Parity: `platform: process.platform` (mcp-routes.ts:87) — Rust's
/// `std::env::consts::OS` spelled as the TypeScript `NodeJS.Platform` value.
fn ts_platform(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Parity: the `webBaseUrl` half of `computeInstallPayload`
/// (mcp-routes.ts:70-74) — `http://127.0.0.1:<OD_WEB_PORT>` when the raw
/// value is a finite number `> 0`, else `null`. The parse is the same
/// [`js_number`] the origin guard uses (see DOCUMENTED DEVIATIONS).
fn web_base_url(web_port_raw: Option<&str>) -> Option<String> {
    let value = js_number(web_port_raw?)?;
    (value > 0.0).then(|| format!("http://127.0.0.1:{value}"))
}

/// Parity: `computeInstallPayload` (mcp-routes.ts:47-94) — the caller-side
/// side effects behind the pure builder: `OD_BIN` + fs probes,
/// `process.execPath`, the bound port and resolved data dir, the
/// `OD_MCP_BOOTSTRAP_*` env merge, and `webBaseUrl`.
/// `web_port_raw` is the same raw `OD_WEB_PORT` the route used as its cache
/// key (`None` = unset, matching TS's `?? null`).
fn compute_install_payload(state: &AppState, web_port_raw: Option<&str>) -> McpInstallPayload {
    let cli_path = resolve_od_bin();
    let cli_exists = Path::new(&cli_path).exists();
    let exec_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("node"));
    let node_exists = exec_path.exists();
    let mut sidecar_env = BTreeMap::new();
    for key in ["OD_MCP_BOOTSTRAP_COMMAND", "OD_MCP_BOOTSTRAP_ARGS"] {
        // TS: `env.X != null && env.X.length > 0`.
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                sidecar_env.insert(key.to_string(), value);
            }
        }
    }
    build_mcp_install_payload(BuildMcpInstallPayloadInputs {
        cli_path,
        cli_exists,
        exec_path: exec_path.to_string_lossy().into_owned(),
        node_exists,
        port: state.resolved_port.load(Ordering::SeqCst),
        platform: ts_platform(std::env::consts::OS).to_string(),
        data_dir: state.config.paths.data_dir().to_string_lossy().into_owned(),
        // No Electron host in this port; no sidecar client either — see the
        // step-3 DOCUMENTED DEVIATIONS above.
        electron_as_node: false,
        is_sidecar_mode: false,
        sidecar_env,
        web_base_url: web_base_url(web_port_raw),
    })
}

// ---- install-info route (parity: mcp-routes.ts:96-112) -----------------

/// Parity: `INSTALL_INFO_TTL_MS = 5000` (mcp-routes.ts:32).
const INSTALL_INFO_TTL: Duration = Duration::from_secs(5);

/// Parity: the `installInfoCache` closure variable (mcp-routes.ts:33-37).
/// It lives in [`crate::routes::AppState`] so it is daemon-scoped exactly
/// like the `registerMcpRoutes` closure was.
pub struct InstallInfoCache {
    /// Monotonic stand-in for TS's `Date.now()` stamp.
    t: Instant,
    payload: McpInstallPayload,
    /// Raw `OD_WEB_PORT` value the entry was computed under (`None` = unset);
    /// part of the TS cache key.
    web_port: Option<String>,
}

/// The shared cache slot `AppState` hands to this module.
pub type InstallInfoCacheSlot = Arc<std::sync::Mutex<Option<InstallInfoCache>>>;

/// `GET /api/mcp/install-info` (parity: mcp-routes.ts:96-112) — the payload
/// the Settings → MCP panel renders as copyable snippets, cached for 5s
/// keyed on the raw `OD_WEB_PORT` value. The guard runs before the cache,
/// exactly as in TS.
async fn get_install_info(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = local_same_origin_rejection(&state, &headers) {
        return response;
    }
    let web_port = std::env::var("OD_WEB_PORT").ok();
    // Held only across synchronous work — no `.await` while locked.
    let mut cache = state
        .mcp_install_info_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(entry) = cache.as_ref() {
        if entry.web_port == web_port && entry.t.elapsed() < INSTALL_INFO_TTL {
            return Json(entry.payload.clone()).into_response();
        }
    }
    let payload = compute_install_payload(&state, web_port.as_deref());
    *cache = Some(InstallInfoCache {
        t: Instant::now(),
        payload: payload.clone(),
        web_port,
    });
    drop(cache);
    Json(payload).into_response()
}

// ---- Codex CLI runner (parity: codex-cli.ts) ----------------------------

/// Parity: `CODEX_MCP_NAME = 'open-design'` (mcp-routes.ts:118) — the MCP
/// server name in `~/.codex/config.toml`.
const CODEX_MCP_NAME: &str = "open-design";

/// Parity: `defaultCodexRunner`'s 30s `setTimeout` (codex-cli.ts:58).
const CODEX_TIMEOUT: Duration = Duration::from_secs(30);

/// Parity: `CodexRunnerResult` (codex-cli.ts:15). `exit_code` keeps TS's
/// `code ?? -1` for signal deaths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRunnerResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The runner's failure shapes. `NotFound` is this port's stand-in for
/// Node's `err.code === 'ENOENT'` in `probeCodexInstall`; `Failed` carries
/// the exact messages TypeScript throws from `installCodexMcp` /
/// `uninstallCodexMcp`.
#[derive(Debug, thiserror::Error)]
pub enum CodexRunError {
    /// Parity: `new Error('codex CLI timed out after 30s')`.
    #[error("codex CLI timed out after 30s")]
    Timeout,
    /// Spawn failure with `ErrorKind::NotFound` — no `codex` on `PATH`
    /// (message wording deviates, see DOCUMENTED DEVIATIONS).
    #[error("codex CLI not found: {0}")]
    NotFound(String),
    /// Any other spawn / I/O failure.
    #[error("failed to run codex CLI: {0}")]
    Spawn(String),
    /// Parity: the `new Error(\`codex mcp … failed: ${failureDetail(result)}\`)`
    /// throws for a non-zero exit.
    #[error("{0}")]
    Failed(String),
}

/// Parity: `CodexInstallStatus` (codex-cli.ts:92).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CodexInstallStatus {
    /// True when the `codex` CLI was found and is runnable.
    pub available: bool,
    /// True when a server with this name is already registered.
    pub installed: bool,
}

/// Parity: `CodexInstallSpec` (codex-cli.ts:116).
#[derive(Debug, Clone, PartialEq)]
pub struct CodexInstallSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// The payload `env`, iterated in its own (insertion) order when the
    /// argv is built.
    pub env: Map<String, Value>,
}

/// Parity: `defaultCodexRunner.run` (codex-cli.ts:45-78) — spawn `codex`
/// with stdin ignored and stdout/stderr piped, reject after 30s. The
/// executable is resolved from `PATH` directly (see DOCUMENTED DEVIATIONS);
/// `env_overrides` mirrors the runner's `opts.env` merge onto the inherited
/// environment.
pub async fn run_codex(
    args: &[&str],
    env_overrides: &[(String, String)],
) -> Result<CodexRunnerResult, CodexRunError> {
    let mut command = tokio::process::Command::new("codex");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Parity: `child.kill('SIGKILL')` on timeout — dropping the output
        // future at the deadline kills the child before we reject.
        .kill_on_drop(true);
    for (key, value) in env_overrides {
        command.env(key, value);
    }
    let outcome = tokio::time::timeout(CODEX_TIMEOUT, command.output()).await;
    let output = match outcome {
        Err(_elapsed) => return Err(CodexRunError::Timeout),
        Ok(Err(err)) => {
            return Err(match err.kind() {
                std::io::ErrorKind::NotFound => CodexRunError::NotFound(err.to_string()),
                _ => CodexRunError::Spawn(err.to_string()),
            });
        }
        Ok(Ok(output)) => output,
    };
    Ok(CodexRunnerResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Parity: `probeCodexInstall` (codex-cli.ts:103-114) — a spawn failure with
/// `ENOENT` means "no Codex CLI"; anything else propagates to the route's
/// `CODEX_PROBE_FAILED` 500.
pub async fn probe_codex_install(name: &str) -> Result<CodexInstallStatus, CodexRunError> {
    match run_codex(&["mcp", "get", name], &[]).await {
        Ok(result) => Ok(CodexInstallStatus {
            available: true,
            installed: result.exit_code == 0,
        }),
        Err(CodexRunError::NotFound(_)) => Ok(CodexInstallStatus {
            available: false,
            installed: false,
        }),
        Err(err) => Err(err),
    }
}

/// One `env` entry rendered for the `--env K=V` argv pair.
fn env_entry_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Parity: the argv assembly at the head of `installCodexMcp`
/// (codex-cli.ts:127-131), exposed as a pure function so the ordering
/// (`--env` pairs in payload-env order, then `--` + command + args) is
/// testable without spawning the CLI.
pub fn codex_mcp_add_argv(spec: &CodexInstallSpec) -> Vec<String> {
    let mut argv = vec!["mcp".to_string(), "add".to_string(), spec.name.clone()];
    for (key, value) in &spec.env {
        argv.push("--env".to_string());
        argv.push(format!("{key}={}", env_entry_text(value)));
    }
    argv.push("--".to_string());
    argv.push(spec.command.clone());
    argv.extend(spec.args.iter().cloned());
    argv
}

/// Parity: `installCodexMcp` (codex-cli.ts:126-136).
pub async fn install_codex_mcp(spec: &CodexInstallSpec) -> Result<(), CodexRunError> {
    let argv = codex_mcp_add_argv(spec);
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    let result = run_codex(&args, &[]).await?;
    if result.exit_code != 0 {
        return Err(CodexRunError::Failed(format!(
            "codex mcp add failed: {}",
            failure_detail(&result)
        )));
    }
    Ok(())
}

/// Parity: `uninstallCodexMcp` (codex-cli.ts:192-197).
pub async fn uninstall_codex_mcp(name: &str) -> Result<(), CodexRunError> {
    let result = run_codex(&["mcp", "remove", name], &[]).await?;
    if result.exit_code != 0 {
        return Err(CodexRunError::Failed(format!(
            "codex mcp remove failed: {}",
            failure_detail(&result)
        )));
    }
    Ok(())
}

/// Parity: `failureDetail` (codex-cli.ts:199-201) — trimmed stderr, else
/// trimmed stdout, else `exit <code>`.
fn failure_detail(result: &CodexRunnerResult) -> String {
    let stderr = result.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = result.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_string();
    }
    format!("exit {}", result.exit_code)
}

/// Parity: `CodexMcpRegistration` (codex-cli.ts:138).
#[derive(Debug, Clone, PartialEq)]
pub struct CodexMcpRegistration {
    pub command: String,
    pub args: Vec<String>,
    pub env: Map<String, Value>,
}

/// Parity: `parseCodexMcpRegistration` (codex-cli.ts:146-164) — parses
/// `codex mcp get <name> --json` for a stdio server; `None` for any other
/// transport or an unrecognized shape. The managed-refresh caller is out of
/// scope (see DOCUMENTED DEVIATIONS), but the parser is part of this TS
/// module's surface.
pub fn parse_codex_mcp_registration(stdout: &str) -> Option<CodexMcpRegistration> {
    let parsed: Value = serde_json::from_str(stdout).ok()?;
    // `parsed.transport` on a non-object / missing / null transport is
    // `undefined`/`null` in JS → `null` here.
    let transport = parsed.as_object()?.get("transport")?.as_object()?;
    let command = transport.get("command")?.as_str()?.to_string();
    // Non-arrays and arrays with a non-string element collapse to `[]`
    // (the TS `every` guard), unlike a *missing* `args`, which is also `[]`.
    let args = match transport.get("args") {
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    // `transport.env != null && typeof transport.env === 'object'`, then
    // keep only string values. `Object.entries` also walks arrays with index
    // keys, so array-shaped envs collect `"0"`, `"1"`, … like JS would.
    let mut env = Map::new();
    match transport.get("env") {
        Some(Value::Object(map)) => {
            for (key, value) in map {
                if let Some(text) = value.as_str() {
                    env.insert(key.clone(), Value::String(text.to_string()));
                }
            }
        }
        Some(Value::Array(items)) => {
            for (index, value) in items.iter().enumerate() {
                if let Some(text) = value.as_str() {
                    env.insert(index.to_string(), Value::String(text.to_string()));
                }
            }
        }
        _ => {}
    }
    Some(CodexMcpRegistration {
        command,
        args,
        env,
    })
}

// ---- Codex install routes (parity: mcp-routes.ts:142-185) ---------------

/// `GET /api/mcp/install/codex/status` (parity: mcp-routes.ts:142-152) —
/// `{available, installed}` for the one-click toggle; runner failures (other
/// than "no CLI") answer 500 `CODEX_PROBE_FAILED` with `String(err.message)`.
async fn get_codex_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = local_same_origin_rejection(&state, &headers) {
        return response;
    }
    match probe_codex_install(CODEX_MCP_NAME).await {
        Ok(status) => Json(status).into_response(),
        Err(err) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "CODEX_PROBE_FAILED",
            &err.to_string(),
        ),
    }
}

/// `POST /api/mcp/install/codex` (parity: mcp-routes.ts:154-173) — feed the
/// exact fields the copyable snippet would carry into `codex mcp add`.
/// An incomplete payload (missing CLI or runtime) refuses first with the
/// buildHint as the message, matching `payload.buildHint ?? 'install payload
/// not ready'`.
async fn post_install_codex(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = local_same_origin_rejection(&state, &headers) {
        return response;
    }
    let web_port = std::env::var("OD_WEB_PORT").ok();
    let payload = compute_install_payload(&state, web_port.as_deref());
    if !payload.cli_exists || !payload.node_exists {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INSTALL_INFO_INCOMPLETE",
            payload
                .build_hint
                .as_deref()
                .unwrap_or("install payload not ready"),
        );
    }
    let spec = CodexInstallSpec {
        name: CODEX_MCP_NAME.to_string(),
        command: payload.command.clone(),
        args: payload.args.clone(),
        env: payload.env.clone(),
    };
    match install_codex_mcp(&spec).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(err) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "CODEX_INSTALL_FAILED",
            &err.to_string(),
        ),
    }
}

/// `DELETE /api/mcp/install/codex` (parity: mcp-routes.ts:175-185) —
/// `codex mcp remove open-design`; failures answer 500
/// `CODEX_UNINSTALL_FAILED` with `String(err.message)`.
async fn delete_install_codex(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = local_same_origin_rejection(&state, &headers) {
        return response;
    }
    match uninstall_codex_mcp(CODEX_MCP_NAME).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(err) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "CODEX_UNINSTALL_FAILED",
            &err.to_string(),
        ),
    }
}

// ───────────────────────────────────────────────────────────────────────
// OAuth status surface — parity port of `GET /api/mcp/oauth/status` and
// `POST /api/mcp/oauth/disconnect` (mcp-routes.ts:360 / 381) over the
// token store in `crate::mcp_tokens` (`mcp-tokens.ts`).
// ───────────────────────────────────────────────────────────────────────

/// `GET /api/mcp/oauth/status?serverId=…` (parity: mcp-routes.ts:360) —
/// `{connected:false}` when no token is stored, otherwise
/// `{connected:true, expiresAt, scope, savedAt}` with the two optional
/// fields falling back to `null` (`tok.expiresAt ?? null` /
/// `tok.scope ?? null`). Storage failure → 500 whose `message` is
/// `String(err.message)` from the TS route (see DOCUMENTED DEVIATIONS for
/// the envelope and wording differences).
async fn get_oauth_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    if let Some(response) = local_same_origin_rejection(&state, &headers) {
        return response;
    }
    // `typeof req.query.serverId === 'string' ? req.query.serverId.trim()
    // : ''` — `single_query_value` already percent-decodes (and refuses a
    // repeated key, which Express hands the route as an array), so only
    // the trim and the empty check remain.
    let server_id = single_query_value(query.as_deref(), "serverId")
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    if server_id.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "serverId is required");
    }
    let data_dir = state.config.paths.data_dir().to_path_buf();
    match tokio::task::spawn_blocking(move || get_token(&data_dir, &server_id)).await {
        Ok(Ok(None)) => Json(json!({ "connected": false })).into_response(),
        Ok(Ok(Some(token))) => Json(json!({
            "connected": true,
            "expiresAt": token.expires_at,
            "scope": token.scope,
            "savedAt": token.saved_at,
        }))
        .into_response(),
        Ok(Err(err)) => {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", &err.to_string())
        }
        Err(join) => internal_error(&join.to_string()),
    }
}

/// `POST /api/mcp/oauth/disconnect` (parity: mcp-routes.ts:381) — drop the
/// stored token and answer `{ok:true}`. `clearToken` is a no-op for an
/// absent id, yet the route still answers 200 (TypeScript does too). The
/// body arrives as `Result<Json<Value>, JsonRejection>` so this handler —
/// not the extractor — decides how a parse outcome interacts with the
/// guard, reproducing Express' middleware-before-route ordering (see
/// DOCUMENTED DEVIATIONS).
async fn post_oauth_disconnect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let payload = match body {
        Ok(Json(value)) => Some(value),
        Err(err) => match err {
            // No / non-JSON Content-Type: `express.json()` skips parsing
            // and leaves `req.body` undefined — the "no body" path below,
            // which still runs the guard first.
            JsonRejection::MissingJsonContentType(_) => None,
            // Malformed JSON: body-parser would answer 400 before the
            // guard too — same status here, axum's plain-text body (the
            // step-2 wording deviation).
            other => return other.into_response(),
        },
    };
    if let Some(response) = local_same_origin_rejection(&state, &headers) {
        return response;
    }
    // `typeof req.body?.serverId === 'string' ? req.body.serverId.trim()
    // : ''` → 400 when the result is empty.
    let server_id = payload
        .as_ref()
        .and_then(|value| value.get("serverId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if server_id.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "serverId is required");
    }
    let _write = state.mcp_tokens_lock.lock().await;
    let data_dir = state.config.paths.data_dir().to_path_buf();
    let server_id = server_id.to_string();
    match tokio::task::spawn_blocking(move || clear_token(&data_dir, &server_id)).await {
        Ok(Ok(())) => Json(json!({ "ok": true })).into_response(),
        Ok(Err(err)) => {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", &err.to_string())
        }
        Err(join) => internal_error(&join.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: u16 = 7456;

    fn inputs(host: &str) -> OriginGuardInputs {
        OriginGuardInputs {
            host: host.to_string(),
            resolved_port: PORT,
            bind_host: "127.0.0.1".to_string(),
            ..OriginGuardInputs::default()
        }
    }

    fn with_origin(host: &str, origin: &str) -> OriginGuardInputs {
        OriginGuardInputs {
            origin: Some(origin.to_string()),
            ..inputs(host)
        }
    }

    // ---- isLocalSameOrigin branches ----

    #[test]
    fn allows_no_origin_request_on_the_bound_loopback_host() {
        assert!(is_local_same_origin(&inputs("127.0.0.1:7456")));
        assert!(is_local_same_origin(&inputs("localhost:7456")));
    }

    #[test]
    fn rejects_no_origin_request_from_a_foreign_host() {
        assert!(!is_local_same_origin(&inputs("evil.example:7456")));
        assert!(!is_local_same_origin(&inputs("evil.example")));
    }

    #[test]
    fn rejects_foreign_origin_even_from_a_local_host() {
        assert!(!is_local_same_origin(&with_origin(
            "127.0.0.1:7456",
            "http://evil.example"
        )));
        // …and when the foreign origin carries the daemon port, so the
        // check falls through to hostname equality.
        assert!(!is_local_same_origin(&with_origin(
            "127.0.0.1:7456",
            "http://evil.example:7456"
        )));
    }

    #[test]
    fn allows_local_origin_on_the_bound_port() {
        assert!(is_local_same_origin(&with_origin(
            "127.0.0.1:7456",
            "http://127.0.0.1:7456"
        )));
        // The explicit origin set covers both schemes.
        assert!(is_local_same_origin(&with_origin(
            "127.0.0.1:7456",
            "https://127.0.0.1:7456"
        )));
        assert!(is_local_same_origin(&with_origin(
            "localhost:7456",
            "http://localhost:7456"
        )));
        // `Origin: null` (sandboxed frames) is never accepted.
        assert!(!is_local_same_origin(&with_origin("127.0.0.1:7456", "null")));
    }

    #[test]
    fn sec_fetch_site_same_origin_unlocks_the_hostname_allow_list() {
        let allow_list = "https://nas.example.ts.net";
        let request = OriginGuardInputs {
            allowed_origins_raw: allow_list.to_string(),
            sec_fetch_site: Some("same-origin".to_string()),
            ..inputs("nas.example.ts.net")
        };
        assert!(is_local_same_origin(&request));

        // Without the attestation the hostname entry stays unreachable.
        let request = OriginGuardInputs {
            allowed_origins_raw: allow_list.to_string(),
            ..inputs("nas.example.ts.net")
        };
        assert!(!is_local_same_origin(&request));
        let request = OriginGuardInputs {
            allowed_origins_raw: allow_list.to_string(),
            sec_fetch_site: Some("cross-site".to_string()),
            ..inputs("nas.example.ts.net")
        };
        assert!(!is_local_same_origin(&request));

        // Host alone is forgeable: a foreign host never passes.
        let request = OriginGuardInputs {
            allowed_origins_raw: allow_list.to_string(),
            sec_fetch_site: Some("same-origin".to_string()),
            ..inputs("evil.example.com")
        };
        assert!(!is_local_same_origin(&request));
    }

    #[test]
    fn ip_literal_allowed_origin_hosts_reach_the_guard_without_sec_fetch() {
        let request = OriginGuardInputs {
            allowed_origins_raw: "http://100.86.154.169:7456".to_string(),
            ..inputs("100.86.154.169:7456")
        };
        assert!(is_local_same_origin(&request));

        // Hostname entries need Sec-Fetch-Site or an exact Origin match.
        let request = OriginGuardInputs {
            allowed_origins_raw: "https://od.example.com".to_string(),
            ..inputs("od.example.com")
        };
        assert!(!is_local_same_origin(&request));
    }

    #[test]
    fn configured_origin_bypasses_the_host_check_only_on_exact_match() {
        let request = OriginGuardInputs {
            resolved_port: 7457,
            allowed_origins_raw: "http://192.168.8.168:7457".to_string(),
            ..with_origin("172.18.0.5:7457", "http://192.168.8.168:7457")
        };
        assert!(is_local_same_origin(&request));

        // Same hostname/port plus a trailing slash is not an exact match.
        assert!(!is_local_same_origin(&OriginGuardInputs {
            origin: Some("http://192.168.8.168:7457/".to_string()),
            ..request.clone()
        }));
        // …and a foreign Origin is still refused.
        assert!(!is_local_same_origin(&OriginGuardInputs {
            origin: Some("http://evil.example.com".to_string()),
            ..request.clone()
        }));
        // The no-Origin branch keeps falling back to host validation: the
        // container IP is private (172.16.0.0/12) and the port matches, so it
        // passes — parity: `origin-validation.test.ts` "preserves the
        // no-Origin behavior". An entirely external host still fails.
        assert!(is_local_same_origin(&OriginGuardInputs {
            origin: None,
            ..request.clone()
        }));
        assert!(!is_local_same_origin(&OriginGuardInputs {
            origin: None,
            ..with_origin("evil.example.com:7457", "http://192.168.8.168:7457")
        }));
    }

    #[test]
    fn rejects_private_lan_origin_on_a_non_allowed_port() {
        let request = with_origin("192.168.18.16:7456", "http://192.168.18.16:9999");
        assert!(!is_local_same_origin(&request));
        // The matching port is allowed (LAN dev setups, parity: TS tests).
        assert!(is_local_same_origin(&with_origin(
            "192.168.18.16:7456",
            "http://192.168.18.16:7456"
        )));
        // An Origin/Host hostname mismatch inside the LAN is refused.
        assert!(!is_local_same_origin(&with_origin(
            "192.168.18.17:7456",
            "http://192.168.18.16:7456"
        )));
    }

    #[test]
    fn host_without_a_port_falls_back_to_port_80() {
        // No port → port "80", which the guard only accepts when allowed.
        assert!(!is_local_same_origin(&inputs("127.0.0.1")));
        assert!(!is_local_same_origin(&inputs("localhost")));
        let request = OriginGuardInputs {
            web_port_raw: "80".to_string(),
            ..inputs("127.0.0.1")
        };
        assert!(is_local_same_origin(&request));
    }

    #[test]
    fn allows_ipv6_loopback_host() {
        assert!(is_local_same_origin(&inputs("[::1]:7456")));
        assert!(is_local_same_origin(&with_origin(
            "[::1]:7456",
            "http://[::1]:7456"
        )));
    }

    #[test]
    fn rejects_unparsable_host_headers() {
        for host in [
            "",
            "evil example",
            "[::1",
            "192.168.1.256:7456",
            "1.2.3.4.5:7456",
            "host:notaport",
            "host:99999",
            "example.1:7456",
        ] {
            assert!(!is_local_same_origin(&inputs(host)), "should reject: {host}");
            assert!(parse_host_header(host).is_none(), "should not parse: {host}");
        }
    }

    #[test]
    fn fails_closed_while_the_port_is_unresolved() {
        let request = OriginGuardInputs {
            resolved_port: 0,
            ..with_origin("127.0.0.1:7456", "http://127.0.0.1:7456")
        };
        assert!(!is_local_same_origin(&request));
        let request = OriginGuardInputs {
            resolved_port: 0,
            ..inputs("127.0.0.1:7456")
        };
        assert!(!is_local_same_origin(&request));
    }

    #[test]
    fn od_web_port_extends_the_allowed_port_list() {
        let request = OriginGuardInputs {
            web_port_raw: "8080".to_string(),
            ..with_origin("127.0.0.1:8080", "http://127.0.0.1:8080")
        };
        assert!(is_local_same_origin(&request));
        // Unknown ports stay blocked even with OD_WEB_PORT set.
        assert!(!is_local_same_origin(&OriginGuardInputs {
            web_port_raw: "8080".to_string(),
            ..with_origin("127.0.0.1:7456", "http://127.0.0.1:9090")
        }));
        // Zero / empty / non-numeric OD_WEB_PORT contributes nothing.
        for web_port in ["0", "", "not-a-port"] {
            let request = OriginGuardInputs {
                web_port_raw: web_port.to_string(),
                ..with_origin("127.0.0.1:7456", "http://127.0.0.1:8080")
            };
            assert!(
                !is_local_same_origin(&request),
                "web port {web_port:?} should not open 8080"
            );
        }
    }

    #[test]
    fn non_loopback_bind_host_is_explicitly_allowed() {
        let request = OriginGuardInputs {
            bind_host: "100.64.1.2".to_string(),
            ..with_origin("100.64.1.2:7456", "http://100.64.1.2:7456")
        };
        assert!(is_local_same_origin(&request));
        // Unknown external origins remain blocked alongside it.
        let request = OriginGuardInputs {
            bind_host: "100.64.1.2".to_string(),
            ..with_origin("127.0.0.1:7456", "http://evil.example:7456")
        };
        assert!(!is_local_same_origin(&request));
    }

    // ---- helper parity ----

    #[test]
    fn allowed_origins_parsing_normalizes_and_skips_malformed_entries() {
        let origins = configured_allowed_origins(
            "http://good.example, , garbage, ftp://other.example, https://od.example.com:8443/path",
        );
        let serialized: Vec<String> = origins.iter().map(ParsedUrl::origin).collect();
        assert_eq!(
            serialized,
            ["http://good.example", "https://od.example.com:8443"]
        );
        let hosts: Vec<&str> = origins.iter().map(|origin| origin.host.as_str()).collect();
        assert_eq!(hosts, ["good.example", "od.example.com:8443"]);

        // Blank values keep the strict default.
        assert!(configured_allowed_origins("").is_empty());
        assert!(configured_allowed_origins("   ").is_empty());
    }

    #[test]
    fn private_ipv4_matches_only_rfc1918_and_link_local_quads() {
        for private in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "169.254.10.20",
        ] {
            assert!(is_private_ipv4(private), "should be private: {private}");
        }
        for other in [
            "172.15.255.255",
            "172.32.0.1",
            "192.168.1.256",
            "1.2.3.999",
            "1.2.3.4",
            "a.b.c.d",
            "1.2.3",
            "1.2.3.4.5",
            "10.0.0.x",
            "",
        ] {
            assert!(!is_private_ipv4(other), "should not be private: {other}");
        }
        // JavaScript `Number('010')` parity: leading zeros still parse.
        assert!(is_private_ipv4("010.0.0.1"));
    }

    #[test]
    fn ip_literal_hostname_shape() {
        assert!(is_ip_literal_hostname("10.0.0.5"));
        assert!(is_ip_literal_hostname("[fd00::1]"));
        assert!(!is_ip_literal_hostname("1.2.3.999"));
        assert!(!is_ip_literal_hostname("nas.example.ts.net"));
        assert!(!is_ip_literal_hostname(""));
    }

    #[test]
    fn parse_host_header_matches_the_url_parser() {
        let parsed = parse_host_header("EVIL.EXAMPLE:7456").expect("parses");
        assert_eq!(parsed.hostname, "evil.example");
        assert_eq!(parsed.host, "evil.example:7456");
        assert_eq!(parsed.port, "7456");

        // Default ports are omitted from `host` (the `|| '80'` fallback
        // happens where the port is compared).
        let parsed = parse_host_header("localhost:80").expect("parses");
        assert_eq!(parsed.host, "localhost");
        assert_eq!(parsed.port, "");

        let parsed = parse_host_header("[::1]").expect("parses");
        assert_eq!(parsed.hostname, "[::1]");
        assert_eq!(parsed.host, "[::1]");
        assert_eq!(parsed.port, "");
        let parsed = parse_host_header("[::1]:7456").expect("parses");
        assert_eq!(parsed.host, "[::1]:7456");
        assert_eq!(parsed.port, "7456");

        // WHATWG IPv4 canonicalization feeds the guard canonical names.
        assert_eq!(
            parse_host_header("127.1:7456").expect("parses").hostname,
            "127.0.0.1"
        );
        assert_eq!(
            parse_host_header("010.0.0.1:7456").expect("parses").hostname,
            "8.0.0.1"
        );
        assert_eq!(parse_host_header("127.0.0.1").expect("parses").port, "");
    }

    #[test]
    fn env_wrapper_reads_the_live_environment() {
        // Both assertions hold regardless of the ambient environment: an
        // explicit loopback host on the bound port is always allowed, and
        // an external host with neither Origin nor Sec-Fetch-Site never is.
        assert!(is_local_same_origin_from_env(
            "127.0.0.1:7456",
            None,
            None,
            7456
        ));
        assert!(!is_local_same_origin_from_env(
            "evil.example:7456",
            None,
            None,
            7456
        ));
    }

    // ---- config sanitizer (parity: sanitizeMcpServer / sanitizeMcpConfig) --

    /// Sanitize one entry from a JSON literal (the same text
    /// `JSON.parse(req.body)` would hand the TypeScript sanitizer).
    fn server(raw: &str) -> Option<McpServerConfig> {
        sanitize_mcp_server(&serde_json::from_str::<Value>(raw).expect("literal is valid JSON"))
    }

    /// The normalized `url` an http transport entry stores (parity:
    /// `parsed.toString()` after `new URL(raw.trim())`).
    fn normalized_url(url: &str) -> String {
        let raw = format!(r#"{{"id":"s","transport":"http","url":"{url}"}}"#);
        server(&raw)
            .unwrap_or_else(|| panic!("should accept {url}"))
            .url
            .expect("http entries carry a url")
    }

    fn temp_data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("od-mcp-unit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn server_ids_follow_the_slug_pattern() {
        assert!(server(r#"{"id":"a","command":"x"}"#).is_some());
        // `/i`: case-insensitive, ASCII.
        assert!(server(r#"{"id":"GitHub-CLI_1","command":"x"}"#).is_some());
        // Trimmed before the test.
        assert_eq!(
            server(r#"{"id":"  padded  ","command":"x"}"#)
                .expect("trimmed id")
                .id,
            "padded"
        );
        for bad in [
            r#"{"id":"","command":"x"}"#,
            r#"{"command":"x"}"#,
            r#"{"id":42,"command":"x"}"#,
            r#"{"id":"NOT VALID id","transport":"stdio","command":"x"}"#,
            r#"{"id":"../evil","command":"x"}"#,
            r#"{"id":"a.b","command":"x"}"#,
            r#"{"id":"-leading","command":"x"}"#,
            r#"{"id":"_leading","command":"x"}"#,
            r#"{"id":"has/slash","command":"x"}"#,
            r#"{"id":"has:colon","command":"x"}"#,
        ] {
            assert!(server(bad).is_none(), "should reject: {bad}");
        }
        // Length boundary: 1..=64 ok, 65 rejected.
        assert!(server(&format!(
            r#"{{"id":"{}","command":"x"}}"#,
            "a".repeat(64)
        ))
        .is_some());
        assert!(server(&format!(
            r#"{{"id":"{}","command":"x"}}"#,
            "a".repeat(65)
        ))
        .is_none());
    }

    #[test]
    fn transport_defaults_to_stdio_and_rejects_unknown_strings() {
        assert_eq!(
            server(r#"{"id":"a","command":"x"}"#).expect("absent").transport,
            McpTransport::Stdio
        );
        // Non-string transports fall back to stdio (typeof check at
        // mcp-config.ts:179).
        for raw in [
            r#"{"id":"a","transport":null,"command":"x"}"#,
            r#"{"id":"a","transport":42,"command":"x"}"#,
            r#"{"id":"a","transport":["http"],"command":"x"}"#,
        ] {
            assert_eq!(
                server(raw).expect("non-string transport").transport,
                McpTransport::Stdio,
                "should default stdio: {raw}"
            );
        }
        // Unrecognized *string* transports reject, case-sensitively.
        for raw in [
            r#"{"id":"a","transport":"udp","command":"x"}"#,
            r#"{"id":"a","transport":"HTTP"}"#,
            r#"{"id":"a","transport":"stdio ","command":"x"}"#,
        ] {
            assert!(server(raw).is_none(), "should reject: {raw}");
        }
        assert_eq!(
            server(r#"{"id":"a","transport":"sse","url":"https://x.example/"}"#)
                .expect("sse ok")
                .transport,
            McpTransport::Sse
        );
    }

    #[test]
    fn enabled_only_stays_false_for_literal_json_false() {
        assert!(server(r#"{"id":"a","command":"x"}"#).expect("absent").enabled);
        assert!(
            !server(r#"{"id":"a","command":"x","enabled":false}"#)
                .expect("false")
                .enabled
        );
        // Everything else — including the string "false" — is `!== false`.
        for raw in [
            r#"{"id":"a","command":"x","enabled":"false"}"#,
            r#"{"id":"a","command":"x","enabled":0}"#,
            r#"{"id":"a","command":"x","enabled":null}"#,
        ] {
            assert!(server(raw).expect("entry").enabled, "should stay enabled: {raw}");
        }
    }

    #[test]
    fn label_and_template_id_are_trimmed_or_omitted() {
        let entry = server(r#"{"id":"a","command":"x","label":"  My Label  ","templateId":"  tpl  "}"#)
            .expect("entry");
        assert_eq!(entry.label.as_deref(), Some("My Label"));
        assert_eq!(entry.template_id.as_deref(), Some("tpl"));

        // Blank after trim, or non-string → the key is omitted entirely.
        for raw in [
            r#"{"id":"a","command":"x","label":"   ","templateId":"  "}"#,
            r#"{"id":"a","command":"x","label":123,"templateId":null}"#,
        ] {
            let entry = server(raw).expect("entry");
            assert!(entry.label.is_none(), "label omitted: {raw}");
            assert!(entry.template_id.is_none(), "templateId omitted: {raw}");
        }
    }

    #[test]
    fn stdio_requires_a_non_blank_command() {
        assert!(server(r#"{"id":"a","transport":"stdio"}"#).is_none());
        assert!(server(r#"{"id":"a","transport":"stdio","command":"   "}"#).is_none());
        assert!(server(r#"{"id":"a","transport":"stdio","command":null}"#).is_none());
        assert!(server(r#"{"id":"a","transport":"stdio","command":42}"#).is_none());
        assert_eq!(
            server(r#"{"id":"a","command":"  npx  "}"#)
                .expect("entry")
                .command
                .as_deref(),
            Some("npx")
        );
    }

    #[test]
    fn args_keep_strings_only() {
        let entry = server(r#"{"id":"a","command":"x","args":["-y","pkg",7,null,true]}"#)
            .expect("entry");
        assert_eq!(
            entry.args.as_deref(),
            Some(["-y".to_string(), "pkg".to_string()].as_slice())
        );
        // Empty strings survive (JS keeps any string), unlike blank env
        // values.
        let entry = server(r#"{"id":"a","command":"x","args":["","x"]}"#).expect("entry");
        assert_eq!(
            entry.args.as_deref(),
            Some(["".to_string(), "x".to_string()].as_slice())
        );
        // Non-arrays, empty arrays, and arrays with no strings vanish.
        for raw in [
            r#"{"id":"a","command":"x","args":"-y"}"#,
            r#"{"id":"a","command":"x","args":[]}"#,
            r#"{"id":"a","command":"x","args":[1,2]}"#,
            r#"{"id":"a","command":"x","args":{"0":"-y"}}"#,
            r#"{"id":"a","command":"x","args":null}"#,
        ] {
            assert!(
                server(raw).expect("entry").args.is_none(),
                "args omitted: {raw}"
            );
        }
    }

    #[test]
    fn env_drops_proto_keys_non_strings_and_blank_values() {
        let entry = server(
            r#"{"id":"a","command":"x","env":{"__proto__":"x","constructor":"y","Z_TOKEN":" z ","OK":"v","NUM":1,"BLANK":"","WS":"   ","  ":"key"}}"#,
        )
        .expect("entry");
        let env = entry.env.expect("env survives");
        // Order follows the input object; trimmed? no — values are stored
        // verbatim, only blank ones are dropped.
        let keys: Vec<&str> = env.keys().map(String::as_str).collect();
        assert_eq!(keys, ["Z_TOKEN", "OK"]);
        assert_eq!(env["Z_TOKEN"].as_str(), Some(" z "));
        assert_eq!(env["OK"].as_str(), Some("v"));

        // Non-object env, and env where every value is blank → omitted.
        for raw in [
            r#"{"id":"a","command":"x","env":["K","V"]}"#,
            r#"{"id":"a","command":"x","env":{"A":"  ","B":""}}"#,
            r#"{"id":"a","command":"x","env":{}}"#,
        ] {
            assert!(server(raw).expect("entry").env.is_none(), "env omitted: {raw}");
        }
    }

    #[test]
    fn transport_specific_keys_are_omitted_in_the_json() {
        // stdio entries never carry url / authMode / headers, and the key
        // order matches the TS insertion order.
        let entry = server(
            r#"{"id":"a","transport":"stdio","command":"x","args":["1"],"env":{"K":"v"},"url":"https://ignored.example/","authMode":"oauth","headers":{"H":"v"}}"#,
        )
        .expect("entry");
        let object = serde_json::to_value(&entry).expect("serialize").as_object().unwrap().clone();
        let keys: Vec<&str> = object.keys().map(String::as_str).collect();
        assert_eq!(keys, ["id", "transport", "enabled", "command", "args", "env"]);

        // http entries never carry command / args / env (even when the
        // input does) and authMode always lands after url.
        let entry = server(
            r#"{"id":"b","transport":"http","url":"https://x.example/mcp","command":"npx","args":["1"],"env":{"K":"v"}}"#,
        )
        .expect("entry");
        let object = serde_json::to_value(&entry).expect("serialize").as_object().unwrap().clone();
        let keys: Vec<&str> = object.keys().map(String::as_str).collect();
        assert_eq!(keys, ["id", "transport", "enabled", "url", "authMode"]);
    }

    #[test]
    fn http_urls_must_parse_as_http_or_https() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://example.com/x",
            "ws://example.com/",
            "data:text/plain,hi",
            "not a url",
            "http://",
            "",
            "http://127.999.999.999/",
        ] {
            let raw = format!(r#"{{"id":"s","transport":"http","url":"{url}"}}"#);
            assert!(server(&raw).is_none(), "should reject url: {url}");
            let raw = format!(r#"{{"id":"s","transport":"sse","url":"{url}"}}"#);
            assert!(server(&raw).is_none(), "should reject sse url: {url}");
        }
        // Missing / blank / non-string url rejects too.
        for raw in [
            r#"{"id":"s","transport":"http"}"#,
            r#"{"id":"s","transport":"http","url":"   "}"#,
            r#"{"id":"s","transport":"http","url":42}"#,
        ] {
            assert!(server(raw).is_none(), "should reject: {raw}");
        }
        // The url is trimmed before parsing.
        let entry = server(r##"{"id":"s","transport":"http","url":"  https://example.com/mcp  "}"##)
            .expect("entry");
        assert_eq!(entry.url.as_deref(), Some("https://example.com/mcp"));
    }

    #[test]
    fn url_normalization_matches_new_url_stringify() {
        // Each expectation equals `new URL(input).toString()` in Node
        // (origin gets the trailing `/`, default port drops, host
        // lowercases, dot-segments clear, space percent-encodes).
        assert_eq!(
            normalized_url("https://mcp.higgsfield.ai"),
            "https://mcp.higgsfield.ai/"
        );
        assert_eq!(
            normalized_url("HTTPS://EXAMPLE.COM:443/a/../b?x=1#frag"),
            "https://example.com/b?x=1#frag"
        );
        assert_eq!(
            normalized_url("https://example.com/a b"),
            "https://example.com/a%20b"
        );
        assert_eq!(
            normalized_url("http://127.1:9/mcp"),
            "http://127.0.0.1:9/mcp"
        );
        assert_eq!(normalized_url("http://[::1]/"), "http://[::1]/");
        assert_eq!(
            normalized_url("https://example.com:8443"),
            "https://example.com:8443/"
        );
        assert_eq!(
            normalized_url("https://example.com/%2e%2e/x"),
            "https://example.com/x"
        );
        // Trailing dots survive normalization (they only vanish inside
        // `isLoopbackHost`).
        assert_eq!(
            normalized_url("http://Example.COM././"),
            "http://example.com./"
        );
    }

    #[test]
    fn auth_mode_is_inferred_from_the_normalized_url() {
        // Loopback → none, remote → oauth (parity:
        // defaults loopback/remote HTTP tests in mcp-config.test.ts).
        for url in [
            "http://localhost:38451/mcp",
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://127.1:9/mcp",
            "http://localhost./",
            "https://127.0.0.1:8443/mcp",
        ] {
            let entry = server(&format!(
                r#"{{"id":"s","transport":"http","url":"{url}"}}"#
            ))
            .expect("entry");
            assert_eq!(entry.auth_mode, Some(McpAuthMode::None), "loopback: {url}");
        }
        for url in [
            "https://mcp.higgsfield.ai/mcp",
            "http://example.com/",
            "http://[fd00::1]/",
        ] {
            let entry = server(&format!(
                r#"{{"id":"s","transport":"http","url":"{url}"}}"#
            ))
            .expect("entry");
            assert_eq!(entry.auth_mode, Some(McpAuthMode::Oauth), "remote: {url}");
        }
        // IPv4-mapped IPv6 literals canonicalize to hex on both sides
        // (`new URL('http://[::ffff:127.0.0.1]/').hostname` is
        // `[::ffff:7f00:1]` in V8 and in the `url` crate), so
        // `isLoopbackHost`'s `::ffff:127.x` branch never sees them and
        // TypeScript infers oauth for this URL as well.
        let entry = server(r#"{"id":"s","transport":"http","url":"http://[::ffff:127.0.0.1]/"}"#)
            .expect("entry");
        assert_eq!(entry.auth_mode, Some(McpAuthMode::Oauth));

        // An explicit valid mode wins over inference — pinning `oauth` on
        // a loopback URL and `none` on a remote one both stick.
        let entry = server(
            r#"{"id":"s","transport":"http","url":"http://localhost:1/mcp","authMode":"oauth"}"#,
        )
        .expect("entry");
        assert_eq!(entry.auth_mode, Some(McpAuthMode::Oauth));
        let entry = server(
            r#"{"id":"s","transport":"http","url":"https://remote.example/mcp","authMode":"none"}"#,
        )
        .expect("entry");
        assert_eq!(entry.auth_mode, Some(McpAuthMode::None));
        // Invalid modes fall back to inference.
        let entry = server(
            r#"{"id":"s","transport":"http","url":"http://localhost:1/mcp","authMode":"basic"}"#,
        )
        .expect("entry");
        assert_eq!(entry.auth_mode, Some(McpAuthMode::None));

        // Direct inference: missing / unparsable → oauth (backward compat).
        assert_eq!(infer_mcp_auth_mode_for_url(None), McpAuthMode::Oauth);
        assert_eq!(infer_mcp_auth_mode_for_url(Some("")), McpAuthMode::Oauth);
        assert_eq!(
            infer_mcp_auth_mode_for_url(Some("nope")),
            McpAuthMode::Oauth
        );
        assert_eq!(
            infer_mcp_auth_mode_for_url(Some("http://127.0.0.1:7456/")),
            McpAuthMode::None
        );
    }

    #[test]
    fn effective_auth_mode_only_applies_to_remote_transports() {
        let stdio = server(r#"{"id":"a","command":"x"}"#).expect("entry");
        assert_eq!(effective_mcp_auth_mode(&stdio), McpAuthMode::None);
        let http = server(r#"{"id":"s","transport":"http","url":"https://x.example/"}"#)
            .expect("entry");
        assert_eq!(effective_mcp_auth_mode(&http), McpAuthMode::Oauth);
        let loopback = server(r#"{"id":"s","transport":"http","url":"http://localhost:1/"}"#)
            .expect("entry");
        assert_eq!(effective_mcp_auth_mode(&loopback), McpAuthMode::None);
        let pinned = server(
            r#"{"id":"s","transport":"http","url":"https://x.example/","authMode":"none"}"#,
        )
        .expect("entry");
        assert_eq!(effective_mcp_auth_mode(&pinned), McpAuthMode::None);
    }

    #[test]
    fn config_sanitization_filters_and_dedupes_first_wins() {
        // Non-object roots and non-array `servers` sanitize to empty.
        for raw in ["null", "[]", "\"x\"", "42", "{\"servers\":\"nope\"}", "{}"] {
            assert_eq!(
                sanitize_mcp_config(&serde_json::from_str::<Value>(raw).expect("JSON")),
                McpConfig::empty(),
                "empty config for: {raw}"
            );
        }

        let raw = r#"{"servers":[
            {"id":"bad"},
            {"id":"dup","command":"echo"},
            {"id":"dup","command":"other"},
            {"id":"../evil","command":"echo"},
            {"id":"z","transport":"http","url":"https://z.example/"}
        ]}"#;
        let config = sanitize_mcp_config(&serde_json::from_str::<Value>(raw).expect("JSON"));
        let ids: Vec<&str> = config.servers.iter().map(|server| server.id.as_str()).collect();
        assert_eq!(ids, ["dup", "z"]);
        assert_eq!(
            config.servers[0].command.as_deref(),
            Some("echo"),
            "first occurrence wins"
        );
        // List order survives.
        assert_eq!(config.servers[1].transport, McpTransport::Http);
    }

    // ---- serialization byte-parity (JSON.stringify(next, null, 2)) ----------

    /// The exact text `JSON.stringify(sanitizeMcpConfig(input), null, 2)`
    /// produces in Node for [`STDIO_INPUT`] (verified against the TS
    /// sanitizer's insertion order: id, transport, enabled, label,
    /// templateId, command, args, env).
    const STDIO_INPUT: &str = r#"{
      "servers": [
        {
          "id": "github",
          "label": "  GitHub CLI  ",
          "templateId": " github-tpl ",
          "transport": "stdio",
          "enabled": true,
          "command": "  npx  ",
          "args": ["-y", "@modelcontextprotocol/server-github", 7],
          "env": {"Z_TOKEN": "z", "A_TOKEN": "   ", "API_KEY": "abc", "__proto__": "x", "NUM": 3, "constructor": "c"}
        }
      ]
    }"#;

    const STDIO_PRETTY: &str = r#"{
  "servers": [
    {
      "id": "github",
      "transport": "stdio",
      "enabled": true,
      "label": "GitHub CLI",
      "templateId": "github-tpl",
      "command": "npx",
      "args": [
        "-y",
        "@modelcontextprotocol/server-github"
      ],
      "env": {
        "Z_TOKEN": "z",
        "API_KEY": "abc"
      }
    }
  ]
}"#;

    const HTTP_INPUT: &str = r#"{
      "servers": [
        {
          "id": "higgsfield",
          "transport": "sse",
          "enabled": false,
          "label": " Higgsfield ",
          "templateId": "   ",
          "url": " https://mcp.higgsfield.ai ",
          "authMode": "oauth",
          "headers": {"X-Zeta": "1", "X-Alpha": "2", "Authorization": "  "}
        }
      ]
    }"#;

    const HTTP_PRETTY: &str = r#"{
  "servers": [
    {
      "id": "higgsfield",
      "transport": "sse",
      "enabled": false,
      "label": "Higgsfield",
      "url": "https://mcp.higgsfield.ai/",
      "authMode": "oauth",
      "headers": {
        "X-Zeta": "1",
        "X-Alpha": "2"
      }
    }
  ]
}"#;

    #[test]
    fn stdio_config_serializes_byte_identically_to_json_stringify() {
        let input: Value = serde_json::from_str(STDIO_INPUT).expect("input JSON");
        let config = sanitize_mcp_config(&input);
        assert_eq!(serde_json::to_string_pretty(&config).expect("pretty"), STDIO_PRETTY);
    }

    #[test]
    fn http_config_serializes_byte_identically_to_json_stringify() {
        let input: Value = serde_json::from_str(HTTP_INPUT).expect("input JSON");
        let config = sanitize_mcp_config(&input);
        assert_eq!(serde_json::to_string_pretty(&config).expect("pretty"), HTTP_PRETTY);
    }

    // ---- storage (parity: readMcpConfig / writeMcpConfig) -------------------

    #[test]
    fn read_returns_empty_for_missing_and_corrupt_files() {
        let dir = temp_data_dir("read");
        assert_eq!(read_mcp_config(&dir).expect("missing file"), McpConfig::empty());

        let file = dir.join("mcp-config.json");
        std::fs::write(&file, "{not valid").expect("write corrupt");
        assert_eq!(read_mcp_config(&dir).expect("corrupt file"), McpConfig::empty());

        // Valid JSON that sanitizes away also reads back empty.
        std::fs::write(&file, "[1, 2]").expect("write array");
        assert_eq!(read_mcp_config(&dir).expect("array file"), McpConfig::empty());

        // A real entry round-trips.
        std::fs::write(&file, STDIO_PRETTY).expect("write pretty");
        assert_eq!(
            read_mcp_config(&dir).expect("valid file"),
            sanitize_mcp_config(&serde_json::from_str::<Value>(STDIO_INPUT).expect("input"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_persists_pretty_json_and_leaves_no_temp_files() {
        let dir = temp_data_dir("write");
        let input: Value = serde_json::from_str(STDIO_INPUT).expect("input JSON");
        let next = write_mcp_config(&dir, &input).expect("write");

        let file = dir.join("mcp-config.json");
        let text = std::fs::read_to_string(&file).expect("read back");
        assert_eq!(text, STDIO_PRETTY, "on-disk text is 2-space pretty JSON");
        // Only the config file remains — the temp file was renamed away.
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(read_mcp_config(&dir).expect("re-read"), next);

        // A second write replaces the file (atomic rename over the first).
        write_mcp_config(&dir, &serde_json::json!({ "servers": [] })).expect("rewrite");
        assert_eq!(read_mcp_config(&dir).expect("re-read").servers.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_serializes_with_eight_hex_temp_suffixes() {
        let suffixes: std::collections::HashSet<String> =
            (0..16).map(|_| random_tmp_suffix()).collect();
        for suffix in &suffixes {
            assert_eq!(suffix.len(), 8, "suffix shape: {suffix}");
            assert!(
                suffix.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "lowercase hex: {suffix}"
            );
            assert!(
                suffix.bytes().all(|byte| !byte.is_ascii_uppercase()),
                "lowercase only: {suffix}"
            );
        }
        // Random enough that 16 draws are not all identical.
        assert!(suffixes.len() > 1, "suffixes should vary: {suffixes:?}");
    }

    // ---- embedded templates -------------------------------------------------

    #[test]
    fn embedded_templates_parse_with_plausible_ids() {
        let list = templates();
        assert!(!list.is_empty(), "template list should not be empty");
        for template in list {
            let id = template
                .get("id")
                .and_then(Value::as_str)
                .expect("template id");
            assert!(is_valid_server_id(id), "implausible template id: {id}");
            for key in ["label", "description", "category", "transport"] {
                assert!(
                    template.get(key).and_then(Value::as_str).is_some_and(|s| !s.is_empty()),
                    "template {id} needs a {key}"
                );
            }
        }
        // Key order inside each object follows the TS literal (verified by
        // the integration test against the source ids as well).
        let first = list[0].as_object().expect("object");
        assert_eq!(first.keys().next().map(String::as_str), Some("id"));
    }

    // ---- install payload (parity: buildMcpInstallPayload) -----------------

    /// A payload that is complete on a healthy install — each test tweaks the
    /// fields it cares about from here.
    fn payload_inputs() -> BuildMcpInstallPayloadInputs {
        BuildMcpInstallPayloadInputs {
            cli_path: "/app/dist/cli.js".to_string(),
            cli_exists: true,
            exec_path: "/usr/bin/node".to_string(),
            node_exists: true,
            port: 7456,
            platform: "linux".to_string(),
            data_dir: "/data/od".to_string(),
            electron_as_node: false,
            is_sidecar_mode: false,
            sidecar_env: BTreeMap::new(),
            web_base_url: None,
        }
    }

    #[test]
    fn install_payload_serializes_like_the_ts_return_literal() {
        // The exact text `JSON.stringify(buildMcpInstallPayload(inputs))`
        // produces, key order included (mcp-install-info.ts:94-110).
        let payload = build_mcp_install_payload(payload_inputs());
        assert_eq!(
            serde_json::to_string(&payload).expect("serialize"),
            concat!(
                r#"{"command":"/usr/bin/node","args":["/app/dist/cli.js","mcp","#,
                r#""--daemon-url","http://127.0.0.1:7456"],"env":{"OD_DATA_DIR":"/data/od"},"#,
                r#""daemonUrl":"http://127.0.0.1:7456","webBaseUrl":null,"platform":"linux","#,
                r#""cliExists":true,"nodeExists":true,"buildHint":null}"#,
            )
        );

        // Sidecar mode drops `--daemon-url` and carries webBaseUrl through.
        let inputs = BuildMcpInstallPayloadInputs {
            is_sidecar_mode: true,
            web_base_url: Some("http://127.0.0.1:65321".to_string()),
            ..payload_inputs()
        };
        let payload = build_mcp_install_payload(inputs);
        assert_eq!(
            serde_json::to_string(&payload).expect("serialize"),
            concat!(
                r#"{"command":"/usr/bin/node","args":["/app/dist/cli.js","mcp"],"#,
                r#""env":{"OD_DATA_DIR":"/data/od"},"daemonUrl":"http://127.0.0.1:7456","#,
                r#""webBaseUrl":"http://127.0.0.1:65321","platform":"linux","#,
                r#""cliExists":true,"nodeExists":true,"buildHint":null}"#,
            )
        );

        // `webBaseUrl` is null for absent *and* empty strings
        // (`typeof === 'string' && length > 0`).
        let payload = build_mcp_install_payload(BuildMcpInstallPayloadInputs {
            web_base_url: Some(String::new()),
            ..payload_inputs()
        });
        assert_eq!(payload.web_base_url, None);
        assert!(serde_json::to_string(&payload)
            .expect("serialize")
            .contains(r#""webBaseUrl":null"#));
    }

    #[test]
    fn build_hint_joins_the_missing_pieces_with_one_space() {
        // Complete → null (TS `hints.length ? hints.join(' ') : null`).
        assert_eq!(build_mcp_install_payload(payload_inputs()).build_hint, None);

        // Each hint alone, message text verbatim from mcp-install-info.ts:61
        // and :66.
        let payload = build_mcp_install_payload(BuildMcpInstallPayloadInputs {
            cli_exists: false,
            ..payload_inputs()
        });
        assert_eq!(
            payload.build_hint.as_deref(),
            Some(
                "OpenDesign CLI entry is missing at /app/dist/cli.js. \
                 Rebuild the daemon or packaged app and refresh."
            )
        );
        let payload = build_mcp_install_payload(BuildMcpInstallPayloadInputs {
            node_exists: false,
            ..payload_inputs()
        });
        assert_eq!(
            payload.build_hint.as_deref(),
            Some(
                "Node-compatible runtime at /usr/bin/node no longer exists. \
                 Reinstall OpenDesign or Node and restart the daemon."
            )
        );

        // Both → the two hints joined by exactly one space.
        let payload = build_mcp_install_payload(BuildMcpInstallPayloadInputs {
            cli_exists: false,
            node_exists: false,
            ..payload_inputs()
        });
        let hint = payload.build_hint.expect("both hints");
        assert_eq!(
            hint.matches(". Rebuild the daemon").count(),
            1,
            "hint: {hint}"
        );
        assert!(!hint.contains("  "), "single-space join: {hint}");
        assert!(hint.starts_with("OpenDesign CLI entry is missing at "));
        assert!(hint.contains(" Node-compatible runtime at /usr/bin/node "));
        assert!(!payload.cli_exists && !payload.node_exists);
    }

    #[test]
    fn env_merges_od_data_dir_then_sidecar_entries_then_electron_flag() {
        let inputs = BuildMcpInstallPayloadInputs {
            sidecar_env: BTreeMap::from([
                ("Z_VAR".to_string(), "z".to_string()),
                ("OD_DATA_DIR".to_string(), "/other".to_string()),
                ("A_VAR".to_string(), "a".to_string()),
            ]),
            electron_as_node: true,
            ..payload_inputs()
        };
        let payload = build_mcp_install_payload(inputs);
        // `OD_DATA_DIR` first (a sidecar entry of the same name overwrites
        // the value but keeps the key's position, like an object spread over
        // an existing key), the remaining sidecar keys in map order, then
        // `ELECTRON_RUN_AS_NODE` last.
        assert_eq!(
            serde_json::to_string(&payload.env).expect("env"),
            r#"{"OD_DATA_DIR":"/other","A_VAR":"a","Z_VAR":"z","ELECTRON_RUN_AS_NODE":"1"}"#
        );

        // Without the Electron flag it never appears.
        let payload = build_mcp_install_payload(payload_inputs());
        assert_eq!(
            serde_json::to_string(&payload.env).expect("env"),
            r#"{"OD_DATA_DIR":"/data/od"}"#
        );
    }

    #[test]
    fn web_base_url_follows_the_number_semantics_of_od_web_port() {
        assert_eq!(web_base_url(None), None);
        assert_eq!(web_base_url(Some("")), None);
        assert_eq!(
            web_base_url(Some("65321")),
            Some("http://127.0.0.1:65321".to_string())
        );
        // Whitespace trims (JS `Number(' 65321 ')`), fractional ports print
        // like JS string interpolation of the number.
        assert_eq!(
            web_base_url(Some(" 65321 ")),
            Some("http://127.0.0.1:65321".to_string())
        );
        assert_eq!(
            web_base_url(Some("8080.5")),
            Some("http://127.0.0.1:8080.5".to_string())
        );
        // Falsy / NaN / non-finite inputs are null in TS. `0x10` is the one
        // documented divergence (hex parses in JS, not here — see step 1).
        for raw in ["0", "-1", "abc", "NaN", "Infinity", "-Infinity", "0x10", "8080abc"] {
            assert_eq!(web_base_url(Some(raw)), None, "raw: {raw}");
        }
    }

    #[test]
    fn platform_maps_rust_os_to_the_node_platform_string() {
        assert_eq!(ts_platform("linux"), "linux");
        assert_eq!(ts_platform("macos"), "darwin");
        assert_eq!(ts_platform("windows"), "win32");
        // Unknown OS values pass through as-is.
        assert_eq!(ts_platform("freebsd"), "freebsd");
    }

    // ---- Codex CLI runner (parity: codex-cli.ts) ---------------------------

    #[test]
    fn failure_detail_prefers_stderr_then_stdout_then_the_exit_code() {
        let result = CodexRunnerResult {
            exit_code: 2,
            stdout: " out \n".to_string(),
            stderr: " err \n".to_string(),
        };
        assert_eq!(failure_detail(&result), "err");
        let result = CodexRunnerResult {
            exit_code: 2,
            stdout: "out".to_string(),
            stderr: "   \n".to_string(),
        };
        assert_eq!(failure_detail(&result), "out");
        let result = CodexRunnerResult {
            exit_code: 127,
            stdout: String::new(),
            stderr: "\n".to_string(),
        };
        assert_eq!(failure_detail(&result), "exit 127");
    }

    #[test]
    fn codex_mcp_add_argv_orders_env_pairs_before_the_command() {
        let mut env = Map::new();
        env.insert("OD_DATA_DIR".to_string(), json!("/data/od"));
        env.insert("ELECTRON_RUN_AS_NODE".to_string(), json!("1"));
        let spec = CodexInstallSpec {
            name: "open-design".to_string(),
            command: "/usr/bin/node".to_string(),
            args: vec![
                "/app/cli.js".to_string(),
                "mcp".to_string(),
                "--daemon-url".to_string(),
                "http://127.0.0.1:7456".to_string(),
            ],
            env,
        };
        assert_eq!(
            codex_mcp_add_argv(&spec),
            [
                "mcp", "add", "open-design",
                "--env", "OD_DATA_DIR=/data/od",
                "--env", "ELECTRON_RUN_AS_NODE=1",
                "--", "/usr/bin/node", "/app/cli.js", "mcp", "--daemon-url",
                "http://127.0.0.1:7456",
            ]
        );

        // An empty env emits no `--env` pairs at all — straight from
        // `mcp add <name> -- <command> <args…>`.
        let spec = CodexInstallSpec {
            name: "open-design".to_string(),
            command: "od".to_string(),
            args: vec!["mcp".to_string()],
            env: Map::new(),
        };
        assert_eq!(codex_mcp_add_argv(&spec), ["mcp", "add", "open-design", "--", "od", "mcp"]);
    }

    #[test]
    fn parse_codex_mcp_registration_reads_the_stdio_shape() {
        let parsed = parse_codex_mcp_registration(
            r#"{"name":"open-design","transport":{"type":"stdio","command":"node","args":["/app/cli.js","mcp"],"env":{"OD_DATA_DIR":"/data","PORT":"7456"}},"status":"ok"}"#,
        )
        .expect("stdio registration");
        assert_eq!(parsed.command, "node");
        assert_eq!(parsed.args, ["/app/cli.js", "mcp"]);
        assert_eq!(
            serde_json::to_string(&parsed.env).expect("env"),
            r#"{"OD_DATA_DIR":"/data","PORT":"7456"}"#
        );

        // Missing `args`, and a non-array / partially-non-string `args`
        // collapse to `[]` (TS `Array.isArray && every`).
        let parsed =
            parse_codex_mcp_registration(r#"{"transport":{"command":"node"}}"#).expect("args-free");
        assert!(parsed.args.is_empty());
        for raw in [
            r#"{"transport":{"command":"node","args":"-x"}}"#,
            r#"{"transport":{"command":"node","args":["ok",42]}}"#,
            r#"{"transport":{"command":"node","args":{}}}"#,
        ] {
            let parsed = parse_codex_mcp_registration(raw).expect("shape still parses");
            assert!(parsed.args.is_empty(), "args should be []: {raw}");
        }

        // Non-string env values are dropped; string ones keep their order.
        let parsed = parse_codex_mcp_registration(
            r#"{"transport":{"command":"node","env":{"A":1,"B":"two","C":null,"D":true}}}"#,
        )
        .expect("env shape");
        assert_eq!(
            serde_json::to_string(&parsed.env).expect("env"),
            r#"{"B":"two"}"#
        );

        // `Object.entries` walks arrays with index keys, so an array-shaped
        // env collects `"0"`, `"1"`, … with the non-strings skipped.
        let parsed =
            parse_codex_mcp_registration(r#"{"transport":{"command":"n","env":["a",2]}}"#)
                .expect("array env shape");
        assert_eq!(serde_json::to_string(&parsed.env).expect("env"), r#"{"0":"a"}"#);
    }

    #[test]
    fn parse_codex_mcp_registration_rejects_any_other_shape() {
        for raw in [
            "not json",
            "",
            "42",
            "[1]",
            "\"stdio\"",
            "null",
            r#"{"transport":null}"#,
            r#"{"transport":"stdio"}"#,
            r#"{"transport":{}}"#,
            r#"{"transport":{"args":["a"]}}"#,
            r#"{"transport":{"command":42}}"#,
            r#"{"transport":{"command":null}}"#,
            // Top-level command without a transport object is not the shape.
            r#"{"command":"node","args":["mcp"]}"#,
            // Truncated payload.
            r#"{"transport":{"command":"node""#,
        ] {
            assert!(
                parse_codex_mcp_registration(raw).is_none(),
                "should reject: {raw:?}"
            );
        }
    }
}
