//! Error type shared by every online integration.

/// Errors returned by online integrations.
#[derive(Debug, thiserror::Error)]
pub enum OnlineError {
    /// The server answered with a non-success HTTP status.
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    /// DNS, connection, TLS or timeout failure: worth retrying later.
    #[error("network error: {0}")]
    Network(String),
    /// The response (or a local file) could not be decoded.
    #[error("parse error: {0}")]
    Parse(String),
    /// Credentials were rejected, or authorization is still pending.
    #[error("authentication error: {0}")]
    Auth(String),
    /// The service lacks the credentials it needs (empty token/key).
    #[error("service not configured")]
    NotConfigured,
    /// Local filesystem failure (queue or cache files).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl OnlineError {
    /// Whether retrying the same request later may succeed (offline, server
    /// overloaded, rate limited). Used to decide whether to queue a scrobble.
    pub fn is_transient(&self) -> bool {
        match self {
            OnlineError::Network(_) => true,
            OnlineError::Http { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, OnlineError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_classification() {
        assert!(OnlineError::Network("x".into()).is_transient());
        let http = |status| OnlineError::Http {
            status,
            body: String::new(),
        };
        assert!(http(503).is_transient());
        assert!(http(429).is_transient());
        assert!(!http(400).is_transient());
        assert!(!OnlineError::Auth("bad".into()).is_transient());
        assert!(!OnlineError::NotConfigured.is_transient());
    }
}
