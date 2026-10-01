//! Server origin validation. Credentials are bound to exactly one normalized origin; the host
//! never forwards them elsewhere (redirects are disabled for every request).
use crate::error::{BridgeError, BridgeResult};
use url::{Host, Url};

fn invalid(message: &'static str) -> BridgeError {
    BridgeError::new("invalid-origin", message)
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Returns the normalized `scheme://host[:port]` origin. HTTPS is required except for explicit
/// loopback development origins (`127.0.0.0/8`, `::1`, `localhost`).
pub fn validate_origin(value: &str) -> BridgeResult<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 2048 {
        return Err(invalid(
            "Enter a server origin such as https://messages.example.",
        ));
    }
    let url = Url::parse(value).map_err(|_| invalid("The server origin is not a valid URL."))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("Server origin must not include credentials."));
    }
    if url.query().is_some() || url.fragment().is_some() || !matches!(url.path(), "" | "/") {
        return Err(invalid(
            "Server origin must not include a path, query, or fragment.",
        ));
    }
    match url.scheme() {
        "https" if url.host().is_some() => {}
        "http" if is_loopback(&url) => {}
        _ => {
            return Err(invalid(
                "Use an HTTPS origin, or an explicit loopback HTTP origin for development.",
            ))
        }
    }
    Ok(url.origin().ascii_serialization())
}

/// True when plaintext HTTP is permitted for this (already validated) origin.
pub fn is_loopback_http(origin: &str) -> bool {
    Url::parse(origin).is_ok_and(|url| url.scheme() == "http" && is_loopback(&url))
}

/// WebSocket endpoint for a validated origin (`wss` for HTTPS, `ws` for loopback HTTP only).
pub fn websocket_url(origin: &str) -> BridgeResult<String> {
    let origin = validate_origin(origin)?;
    if let Some(rest) = origin.strip_prefix("https://") {
        Ok(format!("wss://{rest}/v1/ws"))
    } else if let Some(rest) = origin.strip_prefix("http://") {
        Ok(format!("ws://{rest}/v1/ws"))
    } else {
        Err(invalid("Unsupported server origin."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_origin_urls_and_allows_explicit_loopback() {
        assert!(validate_origin("https://example.test/v1").is_err());
        assert!(validate_origin("https://user@example.test").is_err());
        assert!(validate_origin("https://example.test/?x=1").is_err());
        assert!(validate_origin("https://example.test/#frag").is_err());
        assert!(validate_origin("http://example.test:8080").is_err());
        assert!(validate_origin("http://localhost.evil.test:8080").is_err());
        assert!(validate_origin("http://127.0.0.1.evil.test").is_err());
        assert!(validate_origin("ws://127.0.0.1:8080").is_err());
        assert!(validate_origin("javascript:alert(1)").is_err());
        assert!(validate_origin("file:///etc/passwd").is_err());
        assert_eq!(
            validate_origin("http://127.0.0.1:18333/").unwrap(),
            "http://127.0.0.1:18333"
        );
        assert_eq!(
            validate_origin(" HTTPS://Example.TEST:443/ ").unwrap(),
            "https://example.test"
        );
        assert_eq!(
            validate_origin("http://[::1]:8080").unwrap(),
            "http://[::1]:8080"
        );
        assert_eq!(
            validate_origin("http://localhost:1420").unwrap(),
            "http://localhost:1420"
        );
    }

    #[test]
    fn websocket_url_preserves_tls_requirement() {
        assert_eq!(
            websocket_url("https://example.test").unwrap(),
            "wss://example.test/v1/ws"
        );
        assert_eq!(
            websocket_url("http://127.0.0.1:8080").unwrap(),
            "ws://127.0.0.1:8080/v1/ws"
        );
        assert!(websocket_url("http://example.test").is_err());
        assert!(is_loopback_http("http://127.0.0.1:8080"));
        assert!(!is_loopback_http("https://example.test"));
    }
}
