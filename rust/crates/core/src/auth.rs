//! Authentication for the endpoints that can loosen a limit or move money.
//!
//! Most of the core is read-only and stays open on localhost. The handful of
//! routes that can reset the breaker, hot-swap code, sign with the keystore or
//! broadcast a transaction are different: a request from the network must prove
//! it is the operator. The proof is a bearer token the operator sets, compared
//! in constant time, never the token itself in any response.

use axum::http::HeaderMap;

/// A token an operator configured, or nothing when auth is off.
#[derive(Clone)]
pub struct AdminAuth {
    token: Option<Vec<u8>>,
}

impl std::fmt::Debug for AdminAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdminAuth")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// What a request is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// No token is configured. The route is open and the caller should be told
    /// that, so nobody mistakes the absence of auth for auth.
    Disabled,
    Allowed,
    Refused,
}

impl AdminAuth {
    pub fn new(token: Option<String>) -> Self {
        Self {
            token: token
                .map(|token| token.trim().as_bytes().to_vec())
                .filter(|token| !token.is_empty()),
        }
    }

    pub fn from_env() -> Self {
        Self::new(std::env::var("OPENETHER_ADMIN_TOKEN").ok())
    }

    pub fn enabled(&self) -> bool {
        self.token.is_some()
    }

    pub fn authorize(&self, headers: &HeaderMap) -> Decision {
        let Some(expected) = self.token.as_deref() else {
            return Decision::Disabled;
        };
        match presented_token(headers) {
            Some(presented) if constant_time_eq(presented, expected) => Decision::Allowed,
            _ => Decision::Refused,
        }
    }
}

/// The token a request presents, from `Authorization: Bearer` or the
/// `X-Admin-Token` header. Blank values count as absent.
fn presented_token(headers: &HeaderMap) -> Option<&[u8]> {
    if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
        let raw = value.as_bytes();
        // The scheme name is case-insensitive per RFC 7235.
        if raw.len() > 7 && raw[..7].eq_ignore_ascii_case(b"bearer ") {
            let token = trim_ascii(&raw[7..]);
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    headers
        .get("x-admin-token")
        .map(|value| trim_ascii(value.as_bytes()))
        .filter(|token| !token.is_empty())
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

/// Compare without an early return, so the time taken does not depend on how
/// many leading bytes match.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let length = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for index in 0..length {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        diff |= (a ^ b) as usize;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn without_a_configured_token_every_request_is_allowed_but_reported() {
        let auth = AdminAuth::new(None);
        assert!(!auth.enabled());
        assert_eq!(auth.authorize(&headers(&[])), Decision::Disabled);
        // A blank variable is the same as an absent one: no accidental
        // "empty token accepted" state.
        let blank = AdminAuth::new(Some("   ".into()));
        assert!(!blank.enabled());
        assert_eq!(blank.authorize(&headers(&[])), Decision::Disabled);
    }

    #[test]
    fn a_configured_token_must_match_the_bearer_header() {
        let auth = AdminAuth::new(Some("s3cret-token".into()));
        assert!(auth.enabled());
        assert_eq!(
            auth.authorize(&headers(&[("authorization", "Bearer s3cret-token")])),
            Decision::Allowed
        );
        assert_eq!(
            auth.authorize(&headers(&[("authorization", "bearer s3cret-token")])),
            Decision::Allowed
        );
        assert_eq!(
            auth.authorize(&headers(&[("x-admin-token", "s3cret-token")])),
            Decision::Allowed
        );
        assert_eq!(
            auth.authorize(&headers(&[("authorization", "Bearer wrong")])),
            Decision::Refused
        );
        assert_eq!(
            auth.authorize(&headers(&[("authorization", "s3cret-token")])),
            Decision::Refused,
            "the scheme is part of the grammar"
        );
        assert_eq!(auth.authorize(&headers(&[])), Decision::Refused);
    }

    #[test]
    fn a_prefix_or_an_extra_suffix_is_not_the_token() {
        let auth = AdminAuth::new(Some("s3cret".into()));
        for presented in ["Bearer s3cre", "Bearer s3cret-and-more"] {
            assert_eq!(
                auth.authorize(&headers(&[("authorization", presented)])),
                Decision::Refused,
                "{presented}"
            );
        }
        assert_eq!(
            auth.authorize(&headers(&[("authorization", "Bearer  s3cret ")])),
            Decision::Allowed,
            "surrounding whitespace is not part of the token"
        );
    }

    #[test]
    fn the_comparison_runs_over_the_longer_of_the_two() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
