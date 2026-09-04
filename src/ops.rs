//! Production-surface helpers: health-endpoint auth policy and TLS file loading.
//!
//! Kept out of the session loop so the rules are unit-testable without a TNS
//! handshake.

use std::path::{Path, PathBuf};

/// Bind address of the health HTTP server plus the optional shared token.
#[derive(Clone, Debug, Default)]
pub struct HealthAuth {
    pub bind: String,
    pub token: Option<String>,
}

impl HealthAuth {
    /// Session list/kill require a token when the health port is not loopback.
    /// `/healthz` and `/readyz` stay unauthenticated so orchestrators can probe.
    pub fn sessions_require_token(&self) -> bool {
        !is_loopback_bind(&self.bind)
    }

    /// `true` when this request may list or kill sessions.
    pub fn authorize_sessions(&self, header_line: Option<&str>) -> bool {
        if !self.sessions_require_token() {
            // Loopback: token is optional. If one is configured, still require it
            // so a local token setup is not silently ignored.
            return match self.token.as_deref() {
                None => true,
                Some(tok) => token_matches(header_line, tok),
            };
        }
        match self.token.as_deref() {
            None => false,
            Some(tok) => token_matches(header_line, tok),
        }
    }
}

fn token_matches(header_line: Option<&str>, token: &str) -> bool {
    let Some(h) = header_line else {
        return false;
    };
    let h = h.trim();
    let bearer = h
        .strip_prefix("Authorization:")
        .or_else(|| h.strip_prefix("authorization:"))
        .map(str::trim);
    if let Some(rest) = bearer {
        let rest = rest
            .strip_prefix("Bearer ")
            .or_else(|| rest.strip_prefix("bearer "))
            .unwrap_or(rest)
            .trim();
        return rest == token;
    }
    let xtok = h
        .strip_prefix("X-Dbsaci-Token:")
        .or_else(|| h.strip_prefix("x-dbsaci-token:"))
        .map(str::trim);
    xtok == Some(token)
}

/// Host of `host:port` (IPv6 `[::1]:9500` supported). Loopback if the host is
/// `127.0.0.0/8`, `::1`, or `localhost`.
pub fn is_loopback_bind(addr: &str) -> bool {
    let host = parse_bind_host(addr);
    let host = host.trim().trim_matches(['[', ']']);
    if host.eq_ignore_ascii_case("localhost") || host == "::1" {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

fn parse_bind_host(addr: &str) -> &str {
    if let Some(rest) = addr.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr)
}

/// Files needed to wrap the TNS listener in TLS (TCPS).
#[derive(Clone, Debug)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl TlsFiles {
    pub fn from_paths(cert: impl AsRef<Path>, key: impl AsRef<Path>) -> Self {
        Self {
            cert: cert.as_ref().to_path_buf(),
            key: key.as_ref().to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_binds_do_not_require_a_token() {
        for addr in ["127.0.0.1:9500", "localhost:9500", "[::1]:9500", "::1:9500"] {
            let h = HealthAuth {
                bind: addr.into(),
                token: None,
            };
            assert!(!h.sessions_require_token(), "{addr}");
            assert!(h.authorize_sessions(None), "{addr}");
        }
    }

    #[test]
    fn non_loopback_without_token_cannot_kill_sessions() {
        let h = HealthAuth {
            bind: "0.0.0.0:9500".into(),
            token: None,
        };
        assert!(h.sessions_require_token());
        assert!(!h.authorize_sessions(None));
        assert!(!h.authorize_sessions(Some("Authorization: Bearer secret")));
    }

    #[test]
    fn non_loopback_with_token_accepts_bearer_or_header() {
        let h = HealthAuth {
            bind: "0.0.0.0:9500".into(),
            token: Some("s3cret".into()),
        };
        assert!(h.authorize_sessions(Some("Authorization: Bearer s3cret")));
        assert!(h.authorize_sessions(Some("X-Dbsaci-Token: s3cret")));
        assert!(!h.authorize_sessions(Some("Authorization: Bearer no")));
        assert!(!h.authorize_sessions(None));
    }
}
