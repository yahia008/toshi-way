//! Who a request really came from, when it arrives through proxies.
//!
//! In production a request passes through a reverse proxy that ends TLS, so the address the server
//! sees on the connection is the proxy's, the same for every user. Rate limits keyed on it would be
//! one shared bucket. A reverse proxy adds the address it saw to the `X-Forwarded-For` header, so the
//! real client can be read from there, but only a proxy we trust may be believed: anyone can send
//! that header themselves.
//!
//! Send `/api/v1/*` from the reverse proxy straight to this server. The web app's own built-in proxy
//! (the Next.js rewrite used in development) does **not** add the client's address, so behind it the
//! header can only be what the visitor wrote and must not be trusted.
//!
//! The rule: if the connection does not come from a trusted proxy, the connection's address is the
//! client and the header is ignored. If it does, read the header from the right, skipping addresses
//! that are trusted proxies too; the first one that is not trusted is the client. An attacker can
//! only add entries to the left of the ones our own proxies write, which this never reaches.

use std::net::IpAddr;

/// One address range, such as `10.0.0.0/8`, or a single address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (addr, prefix) = match text.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (text, None),
        };
        let network: IpAddr = addr.parse().map_err(|_| format!("'{text}' is not an IP address or range"))?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) => p.parse::<u8>().ok().filter(|&p| p <= max).ok_or_else(|| format!("'{text}' has a bad prefix length"))?,
        };
        Ok(Self { network, prefix })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) => bits_match(&n.octets(), &i.octets(), self.prefix),
            (IpAddr::V6(n), IpAddr::V6(i)) => bits_match(&n.octets(), &i.octets(), self.prefix),
            // An IPv4 address that arrived inside an IPv6 socket is the same machine.
            (IpAddr::V4(_), IpAddr::V6(i)) => i.to_ipv4_mapped().is_some_and(|v4| self.contains(IpAddr::V4(v4))),
            _ => false,
        }
    }
}

/// Whether the first `prefix` bits of `a` and `b` are equal.
fn bits_match(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let (whole, rest) = ((prefix / 8) as usize, prefix % 8);
    if a[..whole] != b[..whole] {
        return false;
    }
    rest == 0 || (a[whole] ^ b[whole]) >> (8 - rest) == 0
}

/// The proxies whose `X-Forwarded-For` we believe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedProxies(Vec<Cidr>);

impl TrustedProxies {
    /// Nobody: every connection's own address is the client. Right when the server faces the internet.
    pub fn none() -> Self {
        Self(Vec::new())
    }

    /// This machine only (`127.0.0.0/8` and `::1`): for a reverse proxy running on the same host.
    pub fn loopback() -> Self {
        Self::parse("127.0.0.0/8,::1").expect("a fixed list")
    }

    /// A comma-separated list of addresses and ranges, for example `127.0.0.1,10.0.0.0/8`. Empty means none.
    pub fn parse(list: &str) -> Result<Self, String> {
        list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(Cidr::parse).collect::<Result<Vec<_>, _>>().map(Self)
    }

    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|c| c.contains(ip))
    }
}

impl Default for TrustedProxies {
    /// Nobody. The safe default: the header is never believed, so the worst case is that everyone
    /// behind a proxy shares one limit, never that a visitor can pick their own address.
    fn default() -> Self {
        Self::none()
    }
}

/// The client's address: `peer` is the connection's, `forwarded` the `X-Forwarded-For` header value.
pub fn client_ip(peer: IpAddr, forwarded: Option<&str>, trusted: &TrustedProxies) -> IpAddr {
    if !trusted.is_trusted(peer) {
        return peer;
    }
    let Some(forwarded) = forwarded else { return peer };
    let mut nearest = peer;
    for entry in forwarded.rsplit(',') {
        // Something that is not an address cannot be placed: stop at the last hop we could read.
        let Ok(ip) = entry.trim().parse::<IpAddr>() else { return nearest };
        if !trusted.is_trusted(ip) {
            return ip;
        }
        nearest = ip;
    }
    nearest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn ranges_match_by_prefix() {
        let t = TrustedProxies::parse("10.0.0.0/8, 192.168.1.5, 2001:db8::/32").unwrap();
        for yes in ["10.1.2.3", "10.255.255.255", "192.168.1.5", "2001:db8::1", "::ffff:10.0.0.9"] {
            assert!(t.is_trusted(ip(yes)), "{yes}");
        }
        for no in ["11.0.0.1", "192.168.1.6", "2001:db9::1", "8.8.8.8"] {
            assert!(!t.is_trusted(ip(no)), "{no}");
        }
        // A prefix that is not a whole number of bytes.
        let t = TrustedProxies::parse("172.16.0.0/12").unwrap();
        assert!(t.is_trusted(ip("172.31.255.255")) && !t.is_trusted(ip("172.32.0.0")));
    }

    #[test]
    fn bad_lists_are_refused_and_empty_means_none() {
        for bad in ["nonsense", "10.0.0.0/33", "10.0.0.0/x", "::1/129"] {
            assert!(TrustedProxies::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(TrustedProxies::parse("").unwrap(), TrustedProxies::none());
        assert!(!TrustedProxies::none().is_trusted(ip("127.0.0.1")));
    }

    #[test]
    fn without_a_trusted_proxy_the_header_is_ignored() {
        let t = TrustedProxies::loopback();
        // A direct visitor who writes the header themselves gains nothing.
        assert_eq!(client_ip(ip("203.0.113.7"), Some("198.51.100.1"), &t), ip("203.0.113.7"));
        assert_eq!(client_ip(ip("203.0.113.7"), Some("127.0.0.1"), &t), ip("203.0.113.7"));
        assert_eq!(client_ip(ip("203.0.113.7"), None, &TrustedProxies::none()), ip("203.0.113.7"));
    }

    #[test]
    fn behind_a_trusted_proxy_the_client_is_read_from_the_right() {
        let t = TrustedProxies::parse("127.0.0.1,10.0.0.0/8").unwrap();
        // reverse proxy (10.0.0.2) -> web app (127.0.0.1) -> server: the header reads "client, reverse proxy".
        assert_eq!(client_ip(ip("127.0.0.1"), Some("203.0.113.7, 10.0.0.2"), &t), ip("203.0.113.7"));
        // One hop only.
        assert_eq!(client_ip(ip("127.0.0.1"), Some("203.0.113.7"), &t), ip("203.0.113.7"));
        // No header: the proxy itself is all there is.
        assert_eq!(client_ip(ip("127.0.0.1"), None, &t), ip("127.0.0.1"));
        // Every address trusted: the farthest one we can see.
        assert_eq!(client_ip(ip("127.0.0.1"), Some("10.0.0.5, 10.0.0.2"), &t), ip("10.0.0.5"));
    }

    #[test]
    fn a_client_cannot_name_themselves_by_adding_entries_on_the_left() {
        let t = TrustedProxies::loopback();
        // The attacker sends "X-Forwarded-For: 1.2.3.4"; the web app appends the address it saw, 203.0.113.7.
        assert_eq!(client_ip(ip("127.0.0.1"), Some("1.2.3.4, 203.0.113.7"), &t), ip("203.0.113.7"));
        // Many forged entries change nothing: the first untrusted one from the right wins.
        assert_eq!(client_ip(ip("127.0.0.1"), Some("9.9.9.9, 8.8.8.8, 203.0.113.7"), &t), ip("203.0.113.7"));
    }

    #[test]
    fn rubbish_in_the_header_stops_at_the_last_readable_hop() {
        let t = TrustedProxies::loopback();
        assert_eq!(client_ip(ip("127.0.0.1"), Some("not an ip"), &t), ip("127.0.0.1"));
        assert_eq!(client_ip(ip("127.0.0.1"), Some("1.2.3.4, 127.0.0.1, junk, 127.0.0.2"), &t), ip("127.0.0.2"));
        assert_eq!(client_ip(ip("127.0.0.1"), Some(""), &t), ip("127.0.0.1"));
    }
}
