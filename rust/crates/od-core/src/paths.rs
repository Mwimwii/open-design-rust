use std::fs;
use std::path::{Path, PathBuf};

use crate::OdError;

/// The resolved daemon data root and every daemon-owned path derived from it.
///
/// This is the Rust half of the repository-wide contract in `AGENTS.md` →
/// "Daemon data directory contract":
///
/// * `OD_DATA_DIR` is the single active data-root truth source. It must be
///   supplied by the caller (the Tauri shell, the `od` CLI, or the operator);
///   nothing in Rust falls back to a cwd-relative legacy directory.
/// * Every daemon-owned path (SQLite, projects, artifacts, ...) derives from
///   this resolved root — never from an app name, port, channel, or namespace.
/// * Agent subprocesses receive the resolved root back as `OD_DATA_DIR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    data_dir: PathBuf,
}

impl RuntimePaths {
    /// Resolve `OD_DATA_DIR` into the runtime data root, creating it and
    /// verifying it is writable — mirroring the TypeScript daemon's
    /// `resolveDataDir` writability probe.
    pub fn resolve(od_data_dir: impl Into<PathBuf>) -> Result<Self, OdError> {
        let data_dir = od_data_dir.into();
        fs::create_dir_all(&data_dir).map_err(|source| OdError::DataDirUnusable {
            path: data_dir.clone(),
            source,
        })?;
        probe_writable(&data_dir)?;
        Ok(Self { data_dir })
    }

    /// Read `OD_DATA_DIR` from the process environment and resolve it.
    pub fn from_env() -> Result<Self, OdError> {
        match std::env::var_os("OD_DATA_DIR") {
            Some(value) if !value.is_empty() => Self::resolve(PathBuf::from(value)),
            _ => Err(OdError::DataDirUnset),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// SQLite database file (`<data root>/app.sqlite`, same as the
    /// TypeScript daemon's `openDatabase`).
    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("app.sqlite")
    }

    /// Managed-project root (`<data root>/projects`). Imported-folder
    /// projects are the documented exception: they live at
    /// `metadata.baseDir`, outside this root.
    pub fn projects_dir(&self) -> PathBuf {
        self.data_dir.join("projects")
    }

    /// Generated artifact root (`<data root>/artifacts`).
    pub fn artifacts_dir(&self) -> PathBuf {
        self.data_dir.join("artifacts")
    }

    /// Value handed to agent subprocesses as `OD_DATA_DIR`.
    pub fn child_env_value(&self) -> std::ffi::OsString {
        self.data_dir.as_os_str().to_os_string()
    }
}

/// Write-and-remove probe so a read-only mount fails at startup instead of on
/// the first save. Same behavior class as the TypeScript `resolveDataDir`
/// writability check.
fn probe_writable(data_dir: &Path) -> Result<(), OdError> {
    let probe = data_dir.join(".od-write-probe");
    match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(source) => Err(OdError::DataDirNotWritable {
            path: data_dir.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "od-core-paths-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn resolve_creates_and_derives_paths() {
        let dir = temp_data_dir("resolve");
        let paths = RuntimePaths::resolve(&dir).expect("resolve");
        assert_eq!(paths.data_dir(), dir);
        assert_eq!(paths.db_file(), dir.join("app.sqlite"));
        assert_eq!(paths.projects_dir(), dir.join("projects"));
        assert_eq!(paths.artifacts_dir(), dir.join("artifacts"));
        assert!(dir.is_dir());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_env_requires_od_data_dir() {
        // Safety: this test only mutates OD_DATA_DIR in-process and Rust
        // tests run on threads, but env mutation is still process-wide.
        // Serialize on the standard env lock via serial-style sequencing:
        // std has no env lock, so we restore the prior value immediately.
        let prior = std::env::var_os("OD_DATA_DIR");
        std::env::remove_var("OD_DATA_DIR");
        let result = RuntimePaths::from_env();
        match prior {
            Some(value) => std::env::set_var("OD_DATA_DIR", value),
            None => std::env::remove_var("OD_DATA_DIR"),
        }
        assert!(matches!(result, Err(OdError::DataDirUnset)));
    }

    #[test]
    fn write_probe_rejects_readonly_root() {
        let dir = temp_data_dir("readonly");
        fs::create_dir_all(&dir).unwrap();
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o555);
        }
        fs::set_permissions(&dir, perms).unwrap();
        let result = RuntimePaths::resolve(&dir);
        #[cfg(unix)]
        {
            let is_root =
                unsafe { libc_geteuid() } == 0;
            if is_root {
                // root bypasses DAC permission checks, so a read-only mode
                // bit does not make the probe fail for uid 0.
                assert!(result.is_ok(), "root can write anywhere: {result:?}");
            } else {
                assert!(matches!(result, Err(OdError::DataDirNotWritable { .. })));
            }
            let mut perms = fs::metadata(&dir).unwrap().permissions();
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
            fs::set_permissions(&dir, perms).unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// Tiny FFI shim instead of pulling in the `libc` crate for one call.
    #[cfg(unix)]
    unsafe fn libc_geteuid() -> u32 {
        extern "C" {
            fn geteuid() -> u32;
        }
        geteuid()
    }
}
