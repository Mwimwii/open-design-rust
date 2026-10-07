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
//!   is itself acceptable.
//!
//! The MCP routes register here in the next port step.

use std::collections::HashSet;
use std::net::Ipv6Addr;

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

// Routes from `mcp-routes.ts` land here in the next port step; they will
// call `is_local_same_origin` / `is_local_same_origin_from_env` with the
// request headers plus `AppState::resolved_port`.

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
}
