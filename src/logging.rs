//! Diagnostic logging that can never corrupt the TUI.
//!
//! No subscriber is installed unless a log file is requested, so by default kctx emits nothing
//! but its own deliberate output. Log records are only ever written to that file — never to
//! stdout (reserved for machine-readable results) or stderr (used by the TUI).
//!
//! Nothing in this crate logs credentials: only paths, resource names, HTTP status codes and
//! our own error enums are ever passed as fields.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

/// Default filter: quiet for dependencies, verbose for us.
const DEFAULT_FILTER: &str = "warn,kctx=debug";

/// Install the file logger if `log_file` or `$KCTX_LOG_FILE` names a destination.
///
/// `--log-file` wins over the environment variable. Returns `Ok(false)` when logging stays
/// disabled. Failure to open the log file is reported to the caller rather than silently ignored,
/// but is never fatal to the application.
pub fn init(log_file: Option<&Path>, level: Option<&str>) -> io::Result<bool> {
    let from_env = std::env::var_os("KCTX_LOG_FILE").map(std::path::PathBuf::from);
    let Some(path) = log_file.map(Path::to_path_buf).or(from_env) else {
        return Ok(false);
    };

    let file = open_log_file(&path)?;
    let filter = match level {
        Some(level) => EnvFilter::new(level),
        None => {
            EnvFilter::try_from_env("KCTX_LOG").unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .with_target(true)
        .init();

    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "kctx starting");
    Ok(true)
}

/// Open the log destination for appending, refusing anything that is not a plain file.
///
/// Records name contexts, servers, namespaces and resources, so a new file is created `0600` rather
/// than left to the umask. Refusing symbolic links, fifos and device nodes keeps a `$KCTX_LOG_FILE`
/// that was inherited rather than intended from appending somewhere it has no business appending —
/// `--log-file` is the way to say a path is deliberate, and even it does not get to write through a
/// link. A file that already exists keeps whatever mode it has: `mode` only applies on creation.
fn open_log_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(io::Error::other(format!(
                "{} is not a regular file; refusing to log there",
                path.display()
            )));
        }
        Ok(_) => {}
        // Nothing there yet is the ordinary case: we are about to create it.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    // `init` installs a *global* subscriber and so can only be exercised once per process; the
    // filesystem behaviour worth guarding lives in `open_log_file`, which is free of that.

    #[test]
    fn a_new_log_file_is_private_to_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kctx.log");

        open_log_file(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "unexpected mode {:o}", mode & 0o777);
    }

    #[test]
    fn an_inherited_variable_cannot_append_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "precious").unwrap();
        let path = dir.path().join("kctx.log");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        let error = open_log_file(&path).unwrap_err();

        assert!(error.to_string().contains("not a regular file"), "{error}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    }

    #[test]
    fn an_existing_log_file_is_appended_to_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kctx.log");
        std::fs::write(&path, "earlier\n").unwrap();

        let mut file = open_log_file(&path).unwrap();
        file.write_all(b"later\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "earlier\nlater\n",
            "the previous run's log was lost"
        );
    }
}
