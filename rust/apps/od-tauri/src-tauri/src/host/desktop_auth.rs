//! The desktop import token: the secret this shell mints at startup and the
//! HMAC tokens bound to a picked folder.
//!
//! Parity source: `apps/desktop/src/main/runtime.ts` (`signDesktopImportToken`,
//! `mintImportToken`) and `apps/daemon/src/desktop-auth.ts` on the verifying
//! side. A token is `nonce~exp~signature`, and the signature covers
//! `baseDir\nnonce\nexp`, so a token authorizes exactly one folder for one
//! minute. That is what keeps a compromised renderer from naming an arbitrary
//! `baseDir`: the renderer never sees the secret, only a token this process
//! minted for a folder the *user* picked.
//!
//! The secret lives in this process. Electron hands its copy to the daemon over
//! sidecar IPC (`REGISTER_DESKTOP_AUTH`); the Rust daemon has no import routes
//! and no auth gate yet, so there is nothing to register with — tracked
//! separately, see the port status in `rust/README.md`.

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Parity: `DESKTOP_IMPORT_TOKEN_HEADER` in `apps/desktop/src/main/runtime.ts`.
pub const DESKTOP_IMPORT_TOKEN_HEADER: &str = "x-od-desktop-import-token";

/// Parity: `DESKTOP_IMPORT_TOKEN_TTL_MS`.
pub const DESKTOP_IMPORT_TOKEN_TTL_MS: u64 = 60_000;

/// Parity: `DESKTOP_IMPORT_TOKEN_FIELD_SEP`. `~` appears in neither base64url
/// nor ISO 8601, so the three fields split unambiguously.
const TOKEN_FIELD_SEP: char = '~';

/// The per-process secret. Electron mints 32 random bytes at startup
/// (`randomBytes(32)`); the Rust shell does the same.
#[derive(Clone)]
pub struct DesktopAuth {
    secret: Vec<u8>,
}

impl DesktopAuth {
    /// Mint a fresh per-process secret.
    pub fn generate() -> Self {
        let mut secret = vec![0u8; 32];
        rand::thread_rng().fill_bytes(&mut secret);
        Self { secret }
    }

    /// The raw secret bytes.
    pub fn secret(&self) -> &[u8] {
        &self.secret
    }

    /// Mint a token for `base_dir`, valid for [`DESKTOP_IMPORT_TOKEN_TTL_MS`].
    pub fn mint_import_token(&self, base_dir: &str) -> String {
        mint_import_token(self.secret(), base_dir, now_ms())
    }
}

/// Current wall clock in milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// Parity: `mintImportToken` — a 16-byte base64url nonce, an ISO 8601 expiry
/// `TTL` ahead of `now_ms`, and the signature over both.
pub fn mint_import_token(secret: &[u8], base_dir: &str, now_ms: u64) -> String {
    let nonce = random_nonce();
    let exp = iso8601_utc_ms(now_ms + DESKTOP_IMPORT_TOKEN_TTL_MS);
    sign_import_token(secret, base_dir, &nonce, &exp)
}

/// Parity: `signDesktopImportToken`.
pub fn sign_import_token(secret: &[u8], base_dir: &str, nonce: &str, exp: &str) -> String {
    let message = format!("{base_dir}\n{nonce}\n{exp}");
    let signature = base64url(&hmac_sha256(secret, message.as_bytes()));
    [nonce, exp, signature.as_str()].join(&TOKEN_FIELD_SEP.to_string())
}

/// A fresh 16-byte base64url nonce (parity: `randomBytes(16).toString("base64url")`).
pub fn random_nonce() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64url(&bytes)
}

/// Base64url without padding, the encoding Node's `"base64url"` produces.
pub fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// HMAC-SHA256 (RFC 2104).
///
/// The `hmac` crate is not in this workspace's dependency set and the token
/// format is the only consumer, so the construction lives here where it can be
/// pinned against the RFC test vectors.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut key_block = [0u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        let digest = Sha256::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner = Sha256::new();
    inner.update(key_block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(key_block.map(|byte| byte ^ 0x5c));
    outer.update(inner_digest);
    outer.finalize().into()
}

/// ISO 8601 UTC with milliseconds — parity: `new Date(ms).toISOString()`.
pub fn iso8601_utc_ms(ms_since_epoch: u64) -> String {
    let seconds = (ms_since_epoch / 1000) as i64;
    let millis = (ms_since_epoch % 1000) as u32;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        second_of_day / 3600,
        (second_of_day % 3600) / 60,
        second_of_day % 60,
    );
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    )
}

/// Days since the Unix epoch → (year, month, day), Howard Hinnant's
/// `civil_from_days` (the inverse of `days_from_civil`), valid for the whole
/// range the token needs.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_handles_keys_longer_than_the_block() {
        // RFC 4231 case 6: a 131-byte key has to be hashed down first.
        let key = vec![0xaa; 131];
        let mac = hmac_sha256(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn base64url_matches_nodes_encoding() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"\xf0\x9f\x92\xa9"), "8J-SqQ");
        assert_eq!(base64url(&[0xff, 0xef, 0xfe]), "_-_-");
        // 16 random bytes → 22 padding-free base64url characters.
        assert_eq!(random_nonce().len(), 22);
    }

    #[test]
    fn iso8601_matches_toisostring() {
        assert_eq!(iso8601_utc_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601_utc_ms(1_700_000_000_000), "2023-11-14T22:13:20.000Z");
        // A leap day, to exercise the calendar conversion.
        assert_eq!(iso8601_utc_ms(1_709_209_999_123), "2024-02-29T12:33:19.123Z");
    }

    #[test]
    fn token_shape_is_nonce_exp_signature() {
        let secret = [7u8; 32];
        let exp = iso8601_utc_ms(1_700_000_060_000);
        let token = sign_import_token(&secret, "/tmp/project", "nonce-value", &exp);
        let fields: Vec<&str> = token.split(TOKEN_FIELD_SEP).collect();
        assert_eq!(fields.len(), 3, "token must have exactly three fields");
        assert_eq!(fields[0], "nonce-value");
        assert_eq!(fields[1], "2023-11-14T22:14:20.000Z");
        assert_eq!(
            fields[2],
            base64url(&hmac_sha256(&secret, b"/tmp/project\nnonce-value\n2023-11-14T22:14:20.000Z"))
        );
        // base64url only, no separators that could split a field.
        assert!(!fields[2].contains(TOKEN_FIELD_SEP));
        assert!(!fields[2].contains('='));
    }

    #[test]
    fn signature_matches_the_typescript_implementation() {
        // Cross-language vector: the same inputs signed by
        // `signDesktopImportToken` in `apps/daemon/src/desktop-auth.ts`
        // (secret = 32 × 0x5a). Pinning the exact bytes here means a drift in
        // the HMAC, the field order or the base64url alphabet fails locally
        // instead of silently invalidating every token the shell mints.
        let secret = [0x5au8; 32];
        let token = sign_import_token(
            &secret,
            "/tmp/od-parity",
            "fixednonce",
            "2023-11-14T22:14:20.000Z",
        );
        assert_eq!(
            token,
            "fixednonce~2023-11-14T22:14:20.000Z~yogdgR0vBpxRg8t80vc1DmKj547CYfhmdl5RJ4d56SA"
        );
    }

    /// The minted token must be *accepted by the daemon's own verifier*, not
    /// merely look right: this runs the TypeScript implementation behind
    /// `POST /api/import/folder` (`apps/daemon/src/desktop-auth.ts`) against a
    /// token minted here, which also pins the ISO 8601 expiry formatting.
    ///
    /// Skipped when node or the checkout's TypeScript sources are unavailable,
    /// so the crate still tests standalone.
    #[test]
    fn minted_token_is_accepted_by_the_typescript_verifier() {
        let daemon_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../apps/daemon/src/desktop-auth.ts");
        let Ok(daemon_src) = daemon_src.canonicalize() else {
            eprintln!("skipping: {} is not present", daemon_src.display());
            return;
        };
        let secret = [0x5au8; 32];
        let now = 1_700_000_000_000u64;
        let token = mint_import_token(&secret, "/tmp/od-parity", now);
        let secret_literal = format!(
            "[{}]",
            secret
                .iter()
                .map(|byte| byte.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let script = format!(
            "const auth = await import({source:?});\n\
             const secret = Buffer.from({secret_literal});\n\
             const result = auth.verifyDesktopImportToken(\n\
               secret, '/tmp/od-parity', {token:?}, {now}, new Map());\n\
             console.log(JSON.stringify(result));\n",
            source = daemon_src.to_string_lossy(),
            secret_literal = secret_literal,
            token = token,
            now = now,
        );
        let Ok(output) = std::process::Command::new("node")
            .arg("--input-type=module")
            .arg("-e")
            .arg(&script)
            .output()
        else {
            eprintln!("skipping: node is not on PATH");
            return;
        };
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() && stderr.contains("ERR_UNKNOWN_FILE_EXTENSION") {
            // A Node without TypeScript stripping; the vector test above still
            // covers the wire format.
            eprintln!("skipping: this node cannot run TypeScript directly");
            return;
        }
        assert!(
            output.status.success(),
            "node could not run the TypeScript verifier: {stderr}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("\"ok\":true"),
            "the daemon's verifier rejected a token minted here: {stdout} (token {token})"
        );
    }

    #[test]
    fn minted_tokens_expire_one_minute_ahead_and_bind_the_folder() {
        let secret = [3u8; 32];
        let now = 1_700_000_000_000u64;
        let token = mint_import_token(&secret, "/tmp/a", now);
        let fields: Vec<&str> = token.split(TOKEN_FIELD_SEP).collect();
        assert_eq!(fields[1], "2023-11-14T22:14:20.000Z");
        assert_eq!(
            fields[2],
            base64url(&hmac_sha256(&secret, format!("/tmp/a\n{}\n{}", fields[0], fields[1]).as_bytes()))
        );
        // A different folder, secret or nonce must not verify.
        let other = sign_import_token(&secret, "/tmp/b", fields[0], fields[1]);
        assert_ne!(other.split(TOKEN_FIELD_SEP).nth(2), Some(fields[2]));
    }

    #[test]
    fn generated_secrets_are_per_process_and_32_bytes() {
        let first = DesktopAuth::generate();
        let second = DesktopAuth::generate();
        assert_eq!(first.secret().len(), 32);
        assert_ne!(first.secret(), second.secret());
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
