//! Plain settings types: every bound the library applies, with its default.

use core::time::Duration;

/// How a [`ProxyConnector`](crate::ProxyConnector) connects and how much of a proxy's reply it
/// accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConnectorSettings {
    /// The time limit of each connect and each read or write on the proxy connection (30 s).
    pub timeout: Duration,
    /// The `User-Agent` sent in a `CONNECT` (`coqui-proxy`).
    pub user_agent: String,
    /// The longest reply head read from a proxy (64 KiB).
    pub max_response_head_bytes: usize,
    /// The most headers a proxy's reply may carry (100).
    pub max_response_headers: usize,
    /// The longest reply body drained between the legs of a login (64 KiB).
    pub max_response_body_bytes: usize,
}

impl ConnectorSettings {
    /// The default time limit.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
    /// The default `User-Agent`.
    pub const DEFAULT_USER_AGENT: &'static str = "coqui-proxy";
    /// The default longest reply head.
    pub const DEFAULT_MAX_RESPONSE_HEAD_BYTES: usize = 64 * 1024;
    /// The default most reply headers.
    pub const DEFAULT_MAX_RESPONSE_HEADERS: usize = 100;
    /// The default longest drained reply body.
    pub const DEFAULT_MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;
}

impl Default for ConnectorSettings {
    fn default() -> Self {
        Self {
            timeout: Self::DEFAULT_TIMEOUT,
            user_agent: Self::DEFAULT_USER_AGENT.to_string(),
            max_response_head_bytes: Self::DEFAULT_MAX_RESPONSE_HEAD_BYTES,
            max_response_headers: Self::DEFAULT_MAX_RESPONSE_HEADERS,
            max_response_body_bytes: Self::DEFAULT_MAX_RESPONSE_BODY_BYTES,
        }
    }
}
