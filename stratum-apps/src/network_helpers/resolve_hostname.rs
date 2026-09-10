use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use tracing::{debug, info};

/// Maximum time to wait for a DNS lookup before giving up.
/// DNS resolution should complete in milliseconds on a healthy network;
/// 5 seconds is generous enough for slow links while still failing fast
/// when DNS is broken.
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors that can occur during address resolution.
#[derive(Debug)]
pub enum ResolveError {
    /// DNS lookup returned no results for the given hostname.
    NoResults(String),
    /// DNS lookup failed with an IO error.
    LookupFailed(std::io::Error),
    /// DNS lookup did not complete within the timeout.
    Timeout(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NoResults(host) => {
                write!(f, "DNS resolution returned no results for '{host}'")
            }
            ResolveError::LookupFailed(e) => write!(f, "DNS resolution failed: {e}"),
            ResolveError::Timeout(host) => {
                write!(
                    f,
                    "DNS resolution for '{host}' timed out after {}s",
                    DNS_TIMEOUT.as_secs()
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolves a host string and port to every [`SocketAddr`] it maps to.
///
/// This function first attempts to parse the host as an IP address (fast path, no DNS).
/// If that fails, it performs an async DNS lookup via [`tokio::net::lookup_host`].
///
/// Every resolved address is returned, in resolver order. Pass the slice straight to
/// [`tokio::net::TcpStream::connect`], which attempts each address until one succeeds, so a
/// dual-stack host still connects over IPv4 when its IPv6 address is unreachable.
///
/// This should be called at connection time (not config parse time) so that DNS changes
/// are picked up on reconnection attempts.
///
/// # Examples
///
/// ```ignore
/// // IP address (fast path)
/// let addrs = resolve_host("127.0.0.1", 3333).await?;
///
/// // Hostname (DNS lookup)
/// let addrs = resolve_host("pool.example.com", 3333).await?;
/// let stream = TcpStream::connect(&addrs[..]).await?;
/// ```
pub async fn resolve_host(host: &str, port: u16) -> Result<Vec<SocketAddr>, ResolveError> {
    // Fast path: try parsing as an IP address directly (no DNS needed)
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    // Slow path: perform async DNS resolution
    info!("Resolving hostname '{host}' via DNS...");
    let lookup = format!("{host}:{port}");
    let addrs: Vec<SocketAddr> =
        tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host(&lookup))
            .await
            .map_err(|_| ResolveError::Timeout(host.to_string()))?
            .map_err(ResolveError::LookupFailed)?
            .collect();

    if addrs.is_empty() {
        return Err(ResolveError::NoResults(host.to_string()));
    }

    debug!("Resolved '{host}' -> {addrs:?}");
    Ok(addrs)
}

/// Resolves a `"host:port"` string to a single [`SocketAddr`].
///
/// Accepts both IP addresses and hostnames in the `"host:port"` format.
/// For hostnames, performs async DNS resolution via [`tokio::net::lookup_host`].
///
/// Only the first resolved address is returned, which suits callers that need one concrete
/// address to describe an endpoint. To open a connection, use [`resolve_host`] or hand the
/// `"host:port"` string to [`tokio::net::TcpStream::connect`] instead, so that every address
/// behind the name gets an attempt.
///
/// # Examples
///
/// ```ignore
/// // IP address (fast path)
/// let addr = resolve_host_port("127.0.0.1:3333").await?;
///
/// // Hostname (DNS lookup)
/// let addr = resolve_host_port("pool.example.com:3333").await?;
/// ```
pub async fn resolve_host_port(addr: &str) -> Result<SocketAddr, ResolveError> {
    // Fast path: try parsing as a SocketAddr directly (no DNS needed)
    if let Ok(socket) = addr.parse::<SocketAddr>() {
        return Ok(socket);
    }

    // Slow path: perform async DNS resolution
    info!("Resolving address '{addr}' via DNS...");
    let resolved = tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host(addr))
        .await
        .map_err(|_| ResolveError::Timeout(addr.to_string()))?
        .map_err(ResolveError::LookupFailed)?
        // DNS can return multiple addresses; take the first one
        .next()
        .ok_or_else(|| ResolveError::NoResults(addr.to_string()))?;

    debug!("Resolved '{addr}' -> {resolved}");
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_ipv4_address() {
        let addrs = resolve_host("127.0.0.1", 3333).await.unwrap();
        assert_eq!(
            addrs,
            vec![SocketAddr::new("127.0.0.1".parse().unwrap(), 3333)]
        );
    }

    #[tokio::test]
    async fn resolve_ipv6_address() {
        let addrs = resolve_host("::1", 3333).await.unwrap();
        assert_eq!(addrs, vec![SocketAddr::new("::1".parse().unwrap(), 3333)]);
    }

    #[tokio::test]
    async fn resolve_localhost_hostname() {
        let addrs = resolve_host("localhost", 3333).await.unwrap();
        assert!(!addrs.is_empty());
        // localhost can resolve to 127.0.0.1, ::1, or both depending on the system
        assert!(
            addrs
                .iter()
                .all(|a| a.port() == 3333 && a.ip().is_loopback())
        );
    }

    /// Every address behind a name must survive resolution: dropping any of them leaves a
    /// dual-stack pool unreachable whenever the resolver happens to list a dead address first.
    #[tokio::test]
    async fn resolve_keeps_every_address_dns_returns() {
        let expected: Vec<SocketAddr> = tokio::net::lookup_host("localhost:3333")
            .await
            .unwrap()
            .collect();
        assert_eq!(resolve_host("localhost", 3333).await.unwrap(), expected);
    }

    #[tokio::test]
    async fn resolve_invalid_hostname_fails() {
        let result = resolve_host("this.hostname.definitely.does.not.exist.invalid", 3333).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn resolve_host_port_ipv4() {
        let addr = resolve_host_port("127.0.0.1:3333").await.unwrap();
        assert_eq!(addr, SocketAddr::new("127.0.0.1".parse().unwrap(), 3333));
    }

    #[tokio::test]
    async fn resolve_host_port_localhost() {
        let addr = resolve_host_port("localhost:3333").await.unwrap();
        assert_eq!(addr.port(), 3333);
        assert!(addr.ip().is_loopback());
    }

    #[tokio::test]
    async fn resolve_host_port_invalid_fails() {
        let result =
            resolve_host_port("this.hostname.definitely.does.not.exist.invalid:3333").await;
        assert!(result.is_err());
    }
}
