//! Read-only Kubernetes access and the analysis built on top of it.
//!
//! Layering: [`client`] turns a kubeconfig context into a configured client, [`read`] is the
//! only gateway to the API and offers nothing but `get`/`list`, [`inspection`] orchestrates a
//! snapshot, and [`health`] interprets resource objects. Nothing in here depends on `ratatui`.

pub mod client;
#[cfg(test)]
pub mod fake_api;
pub mod health;
pub mod inspection;
pub mod read;

use std::time::Duration;

/// How an attempt to reach a cluster ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionState {
    /// A request is in flight.
    Connecting,
    /// The API server answered.
    Connected,
    /// The API server could not be reached.
    Unavailable,
    /// Credentials were rejected (HTTP 401) or could not be obtained.
    AuthenticationFailed,
    /// Credentials are valid but lack the necessary permissions (HTTP 403).
    PermissionDenied,
    /// The request did not complete within the timeout.
    TimedOut,
    /// The TLS handshake or certificate validation failed.
    TlsError,
    /// The API server answered with an unexpected error.
    ApiError,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Connecting => "Connecting...",
            Self::Connected => "Connected",
            Self::Unavailable => "Unavailable",
            Self::AuthenticationFailed => "Authentication failed",
            Self::PermissionDenied => "Permission denied",
            Self::TimedOut => "Timed out",
            Self::TlsError => "TLS error",
            Self::ApiError => "API error",
        })
    }
}

/// Why a read against a cluster failed.
///
/// Messages are built from our own classification plus the server's own error text; kctx never
/// forwards credential material, exec-plugin output or raw response bodies into them.
#[derive(Debug, Clone, thiserror::Error)]
pub enum InspectError {
    /// The context's kubeconfig could not be turned into a client configuration.
    #[error("kubeconfig is not usable for this context: {0}")]
    Config(String),
    /// The API server could not be reached.
    #[error("cluster is unreachable: {0}")]
    Unavailable(String),
    /// Credentials were rejected or could not be obtained.
    #[error("authentication failed")]
    AuthenticationFailed,
    /// The credentials lack permission for this read.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// The request outlived its timeout. `Duration::ZERO` means the transport reported the
    /// timeout itself, so no budget can be quoted.
    #[error("{}", timeout_message(*.0))]
    TimedOut(Duration),
    /// TLS handshake or certificate validation failure.
    #[error("TLS error: {0}")]
    Tls(String),
    /// Anything else the API server reported.
    #[error("Kubernetes API error: {0}")]
    Api(String),
}

impl InspectError {
    /// The connection state this failure implies.
    pub fn state(&self) -> ConnectionState {
        match self {
            Self::Config(_) => ConnectionState::Unavailable,
            Self::Unavailable(_) => ConnectionState::Unavailable,
            Self::AuthenticationFailed => ConnectionState::AuthenticationFailed,
            Self::PermissionDenied(_) => ConnectionState::PermissionDenied,
            Self::TimedOut(_) => ConnectionState::TimedOut,
            Self::Tls(_) => ConnectionState::TlsError,
            Self::Api(_) => ConnectionState::ApiError,
        }
    }

    /// True when the failure is specifically a permissions problem, which inspection degrades
    /// around instead of giving up.
    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::PermissionDenied(_))
    }
}

/// Map a `kube` failure onto [`InspectError`].
///
/// HTTP status codes are authoritative where present; otherwise the error chain is inspected
/// for timeout and TLS signals, since those surface through the tower/hyper stack rather than as
/// dedicated `kube::Error` variants.
pub fn classify(error: &kube::Error) -> InspectError {
    match error {
        kube::Error::Api(status) => match status.code {
            401 => InspectError::AuthenticationFailed,
            403 => InspectError::PermissionDenied(describe(&status.message, &status.reason)),
            _ => InspectError::Api(describe(&status.message, &status.reason)),
        },
        // Credential acquisition failed (bad token file, failing exec plugin, ...). The source
        // text can quote plugin output, so it is deliberately not included.
        kube::Error::Auth(_) => InspectError::AuthenticationFailed,
        kube::Error::RustlsTls(source) => InspectError::Tls(source.to_string()),
        kube::Error::InferConfig(source) => InspectError::Config(source.to_string()),
        kube::Error::InferKubeconfig(source) => InspectError::Config(source.to_string()),
        other => {
            let chain = error_chain(other);
            if looks_like_timeout(&chain) {
                // The concrete duration is filled in by the caller that owns the timeout.
                InspectError::TimedOut(Duration::ZERO)
            } else if looks_like_tls(&chain) {
                InspectError::Tls(chain)
            } else {
                InspectError::Unavailable(chain)
            }
        }
    }
}

/// Word a timeout with its budget when one is known.
fn timeout_message(timeout: Duration) -> String {
    if timeout.is_zero() {
        "timed out".to_string()
    } else {
        format!("timed out after {:.1}s", timeout.as_secs_f32())
    }
}

/// Prefer the server's message, fall back to its machine-readable reason.
fn describe(message: &str, reason: &str) -> String {
    if !message.is_empty() {
        message.to_string()
    } else if !reason.is_empty() {
        reason.to_string()
    } else {
        "no detail provided".to_string()
    }
}

/// Flatten an error and its sources into one line.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(current) = source {
        parts.push(current.to_string());
        source = current.source();
    }
    parts.dedup();
    parts.join(": ")
}

/// Connect/read timeouts arrive as I/O errors from the hyper stack rather than a typed variant.
fn looks_like_timeout(chain: &str) -> bool {
    let chain = chain.to_lowercase();
    chain.contains("timed out")
        || chain.contains("timeout")
        || chain.contains("deadline has elapsed")
}

/// Certificate and handshake failures surface through the connector as opaque service errors.
fn looks_like_tls(chain: &str) -> bool {
    let chain = chain.to_lowercase();
    [
        "certificate",
        "tls handshake",
        "handshake failure",
        "unknownissuer",
        "invalid peer",
    ]
    .iter()
    .any(|needle| chain.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::Status;

    fn api_error(code: u16, message: &str) -> kube::Error {
        kube::Error::Api(Box::new(Status {
            code,
            message: message.to_string(),
            reason: "Forbidden".to_string(),
            ..Status::default()
        }))
    }

    #[test]
    fn unauthorized_becomes_an_authentication_failure() {
        let error = classify(&api_error(401, "Unauthorized"));
        assert!(matches!(error, InspectError::AuthenticationFailed));
        assert_eq!(error.state(), ConnectionState::AuthenticationFailed);
    }

    #[test]
    fn forbidden_becomes_a_permission_error_that_keeps_the_server_message() {
        let error = classify(&api_error(
            403,
            "pods is forbidden: cannot list resource \"pods\"",
        ));
        assert!(error.is_permission_denied());
        assert_eq!(error.state(), ConnectionState::PermissionDenied);
        assert!(error.to_string().contains("cannot list resource"));
    }

    #[test]
    fn other_status_codes_stay_api_errors() {
        let error = classify(&api_error(500, "internal error"));
        assert_eq!(error.state(), ConnectionState::ApiError);
        assert!(!error.is_permission_denied());
    }

    #[test]
    fn a_status_without_text_still_produces_a_message() {
        let error = classify(&api_error(403, ""));
        assert!(error.to_string().contains("Forbidden"), "{error}");
    }

    #[test]
    fn transport_timeouts_are_recognised() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out");
        let error = classify(&kube::Error::Service(Box::new(io)));
        assert_eq!(error.state(), ConnectionState::TimedOut);
    }

    #[test]
    fn certificate_failures_are_recognised_as_tls_errors() {
        let io = std::io::Error::other("invalid peer certificate: UnknownIssuer");
        let error = classify(&kube::Error::Service(Box::new(io)));
        assert_eq!(error.state(), ConnectionState::TlsError);
    }

    #[test]
    fn anything_else_is_unavailable() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let error = classify(&kube::Error::Service(Box::new(io)));
        assert_eq!(error.state(), ConnectionState::Unavailable);
        assert!(error.to_string().contains("connection refused"));
    }

    #[test]
    fn states_render_the_strings_the_ui_promises() {
        assert_eq!(ConnectionState::Connecting.to_string(), "Connecting...");
        assert_eq!(ConnectionState::Connected.to_string(), "Connected");
        assert_eq!(ConnectionState::Unavailable.to_string(), "Unavailable");
        assert_eq!(
            ConnectionState::AuthenticationFailed.to_string(),
            "Authentication failed"
        );
        assert_eq!(
            ConnectionState::PermissionDenied.to_string(),
            "Permission denied"
        );
        assert_eq!(ConnectionState::TimedOut.to_string(), "Timed out");
    }
}
