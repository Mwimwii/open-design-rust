//! The desktop import token: the secret this shell mints at startup and the
//! HMAC tokens bound to a picked folder.
//!
//! The implementation lives in the daemon crate (`od_daemon::desktop_import`)
//! because the *verifying* side is the daemon's import route: one module owns
//! both halves of the token format, so a drift between the minting shell and
//! the verifying daemon cannot pass the local test suite.
//!
//! Parity source: `apps/desktop/src/main/runtime.ts` (`signDesktopImportToken`,
//! `mintImportToken`) and `apps/daemon/src/desktop-auth.ts` on the verifying
//! side. A token is `nonce~exp~signature`, and the signature covers
//! `baseDir\nnonce\nexp`, so a token authorizes exactly one folder for one
//! minute. That is what keeps a compromised renderer from naming an arbitrary
//! `baseDir`: the renderer never sees the secret, only a token this process
//! minted for a folder the *user* picked.
//!
//! The secret lives in this process and crosses into the embedded daemon as a
//! `DaemonConfig` field; the Electron pair hands its copy over sidecar IPC
//! (`REGISTER_DESKTOP_AUTH`) instead.

// The shell's own crate does not need every half of the token format; the
// daemon crate does. The re-export keeps one source of truth for the wire
// format, so the allowance is deliberate rather than a leftover.
#[allow(unused_imports)]
pub use od_daemon::desktop_import::{
    base64url, hmac_sha256, iso8601_utc_ms, mint_import_token, now_ms, random_nonce,
    sign_import_token, DesktopAuth, DESKTOP_IMPORT_TOKEN_HEADER, DESKTOP_IMPORT_TOKEN_TTL_MS,
    TOKEN_FIELD_SEP,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_and_the_daemon_share_one_token_format() {
        // The shell mints; the daemon's verifier must accept it. Both halves
        // now live in one crate, so this asserts the shared contract rather
        // than a cross-language agreement.
        let secret = [0x5au8; 32];
        let now = 1_700_000_000_000u64;
        let token = mint_import_token(&secret, "/tmp/od-parity", now);
        let verified =
            od_daemon::desktop_import::verify_import_token(&secret, "/tmp/od-parity", &token, now);
        assert!(verified.is_ok(), "the daemon rejected a token the shell minted");
    }
}
