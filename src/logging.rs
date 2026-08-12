//! Diagnostic logging that can never corrupt the TUI.
//!
//! No subscriber is installed unless a log file is requested, so by default kctx emits nothing
//! but its own deliberate output. Log records are only ever written to that file — never to
//! stdout (reserved for machine-readable results) or stderr (used by the TUI).
//!
//! Nothing in this crate logs credentials: only paths, resource names, HTTP status codes and
//! our own error enums are ever passed as fields.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

/// Default filter: quiet for dependencies, verbose for us.
const DEFAULT_FILTER: &str = "warn,kctx=debug";

/// Install the file logger if `log_file` or `$KCTX_LOG_FILE` names a destination.
///
/// Returns `Ok(false)` when logging stays disabled. Failure to open the log file is reported to
/// the caller rather than silently ignored, but is never fatal to the application.
pub fn init(log_file: Option<&Path>, level: Option<&str>) -> io::Result<bool> {
    let from_env = std::env::var_os("KCTX_LOG_FILE").map(std::path::PathBuf::from);
    let Some(path) = log_file.map(Path::to_path_buf).or(from_env) else {
        return Ok(false);
    };

    let file = OpenOptions::new().create(true).append(true).open(&path)?;
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
