// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! How a worker reaches its server, and where plain HTTP may go.
//!
//! BURN_WORKER_PROTOCOL.md 5: a remote worker uses TLS with certificate
//! validation, and plain HTTP is for Compose networking the operator chose.
//! Over plain HTTP the worker's credential and every image it stages or
//! uploads can be read by anything on the path, so the worker settles once,
//! at start, which of three ways it talks:
//!
//! - **TLS**, for an `https://` server URL. Every request must be https.
//! - **Plain HTTP the operator accepted**, for an `http://` URL with
//!   `TANGIBLE_WORKER_PLAIN_HTTP=true`.
//! - **Plain HTTP that stays local**, for an `http://` URL without it. The
//!   server's address must be loopback or, inside a container, on a network
//!   the container is directly attached to: a Docker network the worker and
//!   the server share.
//!
//! The attached networks are read from the container's own routing table
//! rather than from a list of Docker's address ranges. Docker falls back to
//! 192.168.0.0/16 once its 172.17 to 172.31 networks are used up, which is
//! where many home LANs are, and an operator can configure any pool at all;
//! the routing table names the networks this container actually has. The
//! default route does not count, or every address would, nor does a route
//! through a gateway. Every address a name resolves to must pass, at start
//! and again for every connection, and no proxy from the environment is
//! used, since one would carry the request off the network the check was
//! about.
//!
//! No redirect is ever followed, in any of the three: the protocol has none,
//! and one followed could carry a request, credential and all, somewhere these
//! checks never looked.
//!
//! What the routing table cannot tell: a container on the host's network, or
//! on a macvlan or ipvlan network, is attached to the LAN itself, so the LAN
//! passes. Tangible's Compose files run a worker on neither.

use std::io::Read as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs as _};
use std::path::Path;
use std::sync::Arc;

use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// The IPv4 routing table of this network namespace.
const IPV4_ROUTES: &str = "/proc/net/route";

/// The IPv6 routing table, absent where IPv6 is off.
const IPV6_ROUTES: &str = "/proc/net/ipv6_route";

/// Files a container runtime leaves at the root of a container: Docker's,
/// then Podman's.
const CONTAINER_MARKERS: [&str; 2] = ["/.dockerenv", "/run/.containerenv"];

/// The most of a routing table read. A container's is a few lines; a host
/// carrying a full routing table is not where a worker runs.
const ROUTE_TABLE_LIMIT: u64 = 1 << 20;

/// Route flags, from the kernel's `include/uapi/linux/route.h`.
const RTF_UP: u32 = 0x0001;
const RTF_GATEWAY: u32 = 0x0002;
const RTF_REJECT: u32 = 0x0200;

/// Whether plain HTTP may go beyond this machine and its container networks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlainHttp {
    /// Only to loopback, or to a network this container is attached to.
    #[default]
    LocalOnly,
    /// Anywhere: the operator set `TANGIBLE_WORKER_PLAIN_HTTP=true`.
    Allowed,
}

/// How the worker talks to its server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// TLS with certificate validation.
    Tls,
    /// Plain HTTP, to wherever the server is, because the operator said so.
    PlainAccepted,
    /// Plain HTTP that never leaves this machine or its container networks.
    PlainLocal,
}

impl Transport {
    /// A stable name, for logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tls => "tls",
            Self::PlainAccepted => "plain_http_accepted",
            Self::PlainLocal => "plain_http_local",
        }
    }
}

/// Why a server URL cannot be used.
///
/// None of these carries the URL itself: one with a password in it is
/// refused, and must not be repeated into a log while saying so.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServerUrlError {
    /// Not a URL at all.
    #[error("the server URL is not a URL: {0}")]
    Unparsable(String),

    /// A scheme other than https or http.
    #[error("the server URL must start with https:// or http://, not {scheme}://")]
    Scheme {
        /// The scheme given.
        scheme: String,
    },

    /// A user name or password in the URL, which would be logged with it.
    #[error("the server URL must not carry a user name or password")]
    Credentials,

    /// No host to talk to.
    #[error("the server URL names no host")]
    NoHost,

    /// Plain HTTP that would leave this machine and its container networks.
    #[error(
        "plain HTTP to {host} would leave this machine: {reason}. Use an https:// server URL, \
         or set TANGIBLE_WORKER_PLAIN_HTTP=true to accept plain HTTP there"
    )]
    NotLocal {
        /// The host in the URL.
        host: String,
        /// Which addresses would leave, and why.
        reason: String,
    },

    /// The server's name did not resolve.
    #[error("{host} did not resolve: {reason}")]
    Unresolved {
        /// The host in the URL.
        host: String,
        /// What the lookup said.
        reason: String,
    },
}

/// A network this machine reaches directly, without a gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Network {
    address: IpAddr,
    prefix: u8,
}

impl Network {
    fn contains(self, address: IpAddr) -> bool {
        match (self.address, address) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(network) & mask == u32::from(address) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(network) & mask == u128::from(address) & mask
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

/// Where plain HTTP may go without the operator's say-so.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalNetworks {
    in_container: bool,
    attached: Vec<Network>,
}

impl LocalNetworks {
    /// This machine's: whether it is a container, and the networks its
    /// routing tables reach directly.
    #[must_use]
    pub fn read() -> Self {
        let in_container = CONTAINER_MARKERS
            .iter()
            .any(|marker| Path::new(marker).exists());
        Self::parse(
            in_container,
            &read_bounded(IPV4_ROUTES),
            &read_bounded(IPV6_ROUTES),
        )
    }

    /// From routing tables in the kernel's `/proc/net/route` and
    /// `/proc/net/ipv6_route` formats. Lines that are not routes, or not
    /// routes to a directly attached network, are skipped.
    #[must_use]
    pub fn parse(in_container: bool, ipv4_routes: &str, ipv6_routes: &str) -> Self {
        // The IPv4 table has a header line; the IPv6 one does not.
        let mut attached: Vec<Network> = ipv4_routes
            .lines()
            .skip(1)
            .filter_map(ipv4_route)
            .chain(ipv6_routes.lines().filter_map(ipv6_route))
            .collect();
        attached.sort_unstable();
        attached.dedup();
        Self {
            in_container,
            attached,
        }
    }

    /// Why plain HTTP to `address` would leave this machine and its
    /// container networks, or `None` when it would not.
    #[must_use]
    pub fn refusal(&self, address: IpAddr) -> Option<String> {
        // An IPv4 address written as IPv6 is judged as itself.
        let address = address.to_canonical();
        if address.is_loopback() {
            return None;
        }
        if !self.in_container {
            return Some(format!(
                "{address} is not loopback, and this worker is not in a container"
            ));
        }
        if self
            .attached
            .iter()
            .any(|network| network.contains(address))
        {
            return None;
        }
        Some(if self.attached.is_empty() {
            format!("{address} is not on a network this container is attached to (it has none)")
        } else {
            format!(
                "{address} is not on a network this container is attached to ({})",
                self.attached
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }
}

/// A file's text, up to [`ROUTE_TABLE_LIMIT`]; empty if it cannot be read.
fn read_bounded(path: &str) -> String {
    let mut text = String::new();
    // A table that is missing, as the IPv6 one is where IPv6 is off, has no
    // routes. Nor does one that cannot be read: plain HTTP is then refused,
    // which is the safe way to be wrong.
    if let Ok(file) = std::fs::File::open(path)
        && file
            .take(ROUTE_TABLE_LIMIT)
            .read_to_string(&mut text)
            .is_err()
    {
        text.clear();
    }
    text
}

/// One line of `/proc/net/route`, if it is a route to a directly attached
/// network: `Iface Destination Gateway Flags RefCnt Use Metric Mask ...`.
fn ipv4_route(line: &str) -> Option<Network> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [_, destination, gateway, flags, _, _, _, mask, ..] = fields.as_slice() else {
        return None;
    };
    let flags = u32::from_str_radix(flags, 16).ok()?;
    if flags & RTF_UP == 0
        || flags & (RTF_GATEWAY | RTF_REJECT) != 0
        || !kernel_ipv4(gateway)?.is_unspecified()
    {
        return None;
    }
    let mask = u32::from(kernel_ipv4(mask)?);
    let prefix = mask.leading_ones();
    // Prefix zero is the default route, which reaches everything; a mask
    // with a hole in it is not one a network has.
    if prefix == 0 || prefix + mask.trailing_zeros() != 32 {
        return None;
    }
    Some(Network {
        address: IpAddr::V4(kernel_ipv4(destination)?),
        prefix: u8::try_from(prefix).ok()?,
    })
}

/// An IPv4 address as `/proc/net/route` prints it: the address's bytes, in
/// network order, read as a number in this machine's order and printed in
/// hex. Reading it back the same way gives the bytes on any machine.
fn kernel_ipv4(hex: &str) -> Option<Ipv4Addr> {
    if hex.len() != 8 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(Ipv4Addr::from(value.to_ne_bytes()))
}

/// One line of `/proc/net/ipv6_route`, if it is a route to a directly
/// attached network: `Destination Prefix Source Prefix NextHop Metric RefCnt
/// Use Flags Iface`.
fn ipv6_route(line: &str) -> Option<Network> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [destination, prefix, _, _, next_hop, _, _, _, flags, ..] = fields.as_slice() else {
        return None;
    };
    let flags = u32::from_str_radix(flags, 16).ok()?;
    if flags & RTF_UP == 0
        || flags & (RTF_GATEWAY | RTF_REJECT) != 0
        || !kernel_ipv6(next_hop)?.is_unspecified()
    {
        return None;
    }
    let prefix = u8::from_str_radix(prefix, 16).ok()?;
    let destination = kernel_ipv6(destination)?;
    if prefix == 0 || prefix > 128 || destination.is_multicast() {
        return None;
    }
    Some(Network {
        address: IpAddr::V6(destination),
        prefix,
    })
}

/// An IPv6 address as `/proc/net/ipv6_route` prints it: 32 hex digits, in
/// network order.
fn kernel_ipv6(hex: &str) -> Option<Ipv6Addr> {
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(hex, 16).ok().map(Ipv6Addr::from)
}

/// A server URL, checked against the rules in the module documentation.
#[derive(Debug, Clone)]
pub struct ServerUrl {
    base: String,
    host: String,
    /// Whether the host is an address rather than a name, so was checked
    /// when the URL was.
    literal: bool,
    transport: Transport,
    networks: Arc<LocalNetworks>,
}

impl ServerUrl {
    /// Check `text`, against this machine's networks where that matters.
    ///
    /// An address in the URL is checked here; a name is checked when it is
    /// resolved, by [`Self::check_resolution`] at start and again for every
    /// connection.
    ///
    /// # Errors
    ///
    /// [`ServerUrlError`] for a URL that is not usable, or plain HTTP to an
    /// address that would leave this machine and its container networks.
    pub fn parse(text: &str, plain_http: PlainHttp) -> Result<Self, ServerUrlError> {
        Self::check(text, plain_http, LocalNetworks::read)
    }

    /// As [`Self::parse`], judged against `networks` instead of this
    /// machine's. For tests.
    ///
    /// # Errors
    ///
    /// As [`Self::parse`].
    pub fn with_networks(
        text: &str,
        plain_http: PlainHttp,
        networks: LocalNetworks,
    ) -> Result<Self, ServerUrlError> {
        Self::check(text, plain_http, || networks)
    }

    fn check(
        text: &str,
        plain_http: PlainHttp,
        networks: impl FnOnce() -> LocalNetworks,
    ) -> Result<Self, ServerUrlError> {
        let url =
            Url::parse(text).map_err(|error| ServerUrlError::Unparsable(error.to_string()))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ServerUrlError::Credentials);
        }
        let transport = match (url.scheme(), plain_http) {
            ("https", _) => Transport::Tls,
            ("http", PlainHttp::Allowed) => Transport::PlainAccepted,
            ("http", PlainHttp::LocalOnly) => Transport::PlainLocal,
            (scheme, _) => {
                return Err(ServerUrlError::Scheme {
                    scheme: scheme.to_owned(),
                });
            }
        };
        let host = url
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or(ServerUrlError::NoHost)?;
        // An IPv6 address is bracketed in a URL.
        let address = host
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(host)
            .parse::<IpAddr>()
            .ok();
        // Only plain HTTP that must stay local looks at the networks.
        let networks = if transport == Transport::PlainLocal {
            networks()
        } else {
            LocalNetworks::default()
        };
        if transport == Transport::PlainLocal
            && let Some(address) = address
            && let Some(reason) = networks.refusal(address)
        {
            return Err(ServerUrlError::NotLocal {
                host: host.to_owned(),
                reason,
            });
        }
        Ok(Self {
            // Trailing slashes are trimmed once here rather than guarded at
            // every call site.
            base: text.trim_end_matches('/').to_owned(),
            host: host.to_owned(),
            literal: address.is_some(),
            transport,
            networks: Arc::new(networks),
        })
    }

    /// How the worker talks to this server.
    #[must_use]
    pub fn transport(&self) -> Transport {
        self.transport
    }

    /// The URL requests are made under, without a trailing slash.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// For plain HTTP that must stay local, that the server's name resolves
    /// only to addresses that do. Called at start, so a worker that would be
    /// refused says why before it enrolls rather than failing to connect.
    ///
    /// # Errors
    ///
    /// [`ServerUrlError::NotLocal`] or [`ServerUrlError::Unresolved`].
    pub async fn check_resolution(&self) -> Result<(), ServerUrlError> {
        if self.transport != Transport::PlainLocal || self.literal {
            return Ok(());
        }
        resolve_locally(&self.host, &self.networks)
            .await
            .map(|_| ())
    }

    /// Apply this transport's rules to an HTTP client being built.
    pub(crate) fn configure(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        let builder = builder.redirect(reqwest::redirect::Policy::none());
        match self.transport {
            Transport::Tls => builder.https_only(true),
            Transport::PlainAccepted => builder,
            Transport::PlainLocal => builder.no_proxy().dns_resolver(LocalResolver {
                networks: Arc::clone(&self.networks),
            }),
        }
    }
}

/// Resolve `host`, refusing it unless every address it has stays local.
async fn resolve_locally(
    host: &str,
    networks: &LocalNetworks,
) -> Result<Vec<IpAddr>, ServerUrlError> {
    let name = host.to_owned();
    let found = tokio::task::spawn_blocking(move || {
        (name.as_str(), 0)
            .to_socket_addrs()
            .map(|addresses| addresses.map(|address| address.ip()).collect::<Vec<_>>())
    })
    .await
    .map_err(|_| ServerUrlError::Unresolved {
        host: host.to_owned(),
        reason: "the lookup did not finish".to_owned(),
    })?
    .map_err(|error| ServerUrlError::Unresolved {
        host: host.to_owned(),
        reason: error.to_string(),
    })?;
    judge(host, found, networks)
}

/// The addresses `host` resolved to, if every one of them stays local.
///
/// All of them rather than any: a connection may use whichever it likes.
fn judge(
    host: &str,
    mut found: Vec<IpAddr>,
    networks: &LocalNetworks,
) -> Result<Vec<IpAddr>, ServerUrlError> {
    found.sort_unstable();
    found.dedup();
    if found.is_empty() {
        return Err(ServerUrlError::Unresolved {
            host: host.to_owned(),
            reason: "it has no address".to_owned(),
        });
    }
    let refused: Vec<String> = found
        .iter()
        .filter_map(|address| networks.refusal(*address))
        .collect();
    if refused.is_empty() {
        Ok(found)
    } else {
        Err(ServerUrlError::NotLocal {
            host: host.to_owned(),
            reason: refused.join("; "),
        })
    }
}

/// Resolves the server's name for each connection, refusing it unless it
/// still stays local.
#[derive(Debug, Clone)]
struct LocalResolver {
    networks: Arc<LocalNetworks>,
}

impl Resolve for LocalResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let networks = Arc::clone(&self.networks);
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found = resolve_locally(&host, &networks).await?;
            let addresses: Addrs =
                Box::new(found.into_iter().map(|address| SocketAddr::new(address, 0)));
            Ok(addresses)
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    const HEADER: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT";

    /// An IPv4 address as the kernel prints it on this machine.
    fn hex(address: [u8; 4]) -> String {
        format!("{:08X}", u32::from_ne_bytes(address))
    }

    /// A `/proc/net/route` line.
    fn route(destination: [u8; 4], gateway: [u8; 4], flags: u32, mask: [u8; 4]) -> String {
        format!(
            "eth0\t{}\t{}\t{flags:04X}\t0\t0\t0\t{}\t0\t0\t0",
            hex(destination),
            hex(gateway),
            hex(mask)
        )
    }

    /// A container on a Compose network, 172.20.0.0/16, with its default
    /// route through that network's gateway.
    fn compose_routes() -> String {
        [
            HEADER.to_owned(),
            route([0, 0, 0, 0], [172, 20, 0, 1], 0x0003, [0, 0, 0, 0]),
            route([172, 20, 0, 0], [0, 0, 0, 0], 0x0001, [255, 255, 0, 0]),
        ]
        .join("\n")
    }

    fn in_compose() -> LocalNetworks {
        LocalNetworks::parse(true, &compose_routes(), "")
    }

    fn not_in_a_container() -> LocalNetworks {
        LocalNetworks::parse(false, &compose_routes(), "")
    }

    fn local(text: &str, networks: LocalNetworks) -> Result<ServerUrl, ServerUrlError> {
        ServerUrl::with_networks(text, PlainHttp::LocalOnly, networks)
    }

    #[test]
    fn the_scheme_decides_the_transport() {
        let tls = local("https://tangible.example.lan", LocalNetworks::default()).expect("https");
        assert_eq!(tls.transport(), Transport::Tls);

        let accepted = ServerUrl::with_networks(
            "http://192.168.1.13:8080",
            PlainHttp::Allowed,
            LocalNetworks::default(),
        )
        .expect("plain HTTP the operator accepted goes anywhere");
        assert_eq!(accepted.transport(), Transport::PlainAccepted);

        let compose = local("http://server:8080", in_compose()).expect("a name");
        assert_eq!(compose.transport(), Transport::PlainLocal);
    }

    #[test]
    fn https_is_accepted_to_any_address_without_looking_at_networks() {
        // Nothing local about it, and nothing needs to be.
        let server = local("https://203.0.113.9:8443", not_in_a_container()).expect("tls");
        assert_eq!(server.transport(), Transport::Tls);
    }

    #[test]
    fn other_schemes_are_refused() {
        assert_eq!(
            local("ftp://server", in_compose()).unwrap_err(),
            ServerUrlError::Scheme {
                scheme: "ftp".to_owned()
            }
        );
    }

    #[test]
    fn a_url_that_is_not_one_is_refused() {
        assert!(matches!(
            local("server:8080/api", in_compose()).unwrap_err(),
            ServerUrlError::Unparsable(_) | ServerUrlError::Scheme { .. }
        ));
        assert!(matches!(
            local("not a url", in_compose()).unwrap_err(),
            ServerUrlError::Unparsable(_)
        ));
    }

    #[test]
    fn credentials_in_the_url_are_refused_without_repeating_them() {
        for text in [
            "https://worker:hunter2@tangible.example.lan",
            "https://worker@tangible.example.lan",
        ] {
            let error = local(text, in_compose()).unwrap_err();
            assert_eq!(error, ServerUrlError::Credentials);
            assert!(!error.to_string().contains("hunter2"));
        }
    }

    #[test]
    fn loopback_is_accepted_outside_a_container() {
        for text in [
            "http://127.0.0.1:8080",
            "http://127.1.2.3:8080",
            "http://[::1]:8080",
            "http://[::ffff:127.0.0.1]:8080",
        ] {
            let server =
                local(text, not_in_a_container()).unwrap_or_else(|error| panic!("{text}: {error}"));
            assert_eq!(server.transport(), Transport::PlainLocal);
        }
    }

    #[test]
    fn outside_a_container_only_loopback_is_local() {
        // The same routes a container would have, but this is a host, where a
        // directly attached network is the LAN.
        let error = local("http://172.20.0.5:8080", not_in_a_container()).unwrap_err();
        let ServerUrlError::NotLocal { host, reason } = &error else {
            panic!("expected NotLocal, got {error:?}");
        };
        assert_eq!(host, "172.20.0.5");
        assert!(reason.contains("not in a container"), "{reason}");
        assert!(
            error
                .to_string()
                .contains("TANGIBLE_WORKER_PLAIN_HTTP=true"),
            "the error says what to do: {error}"
        );
    }

    #[test]
    fn in_a_container_an_attached_network_is_local_and_the_lan_is_not() {
        local("http://172.20.0.5:8080", in_compose()).expect("the Compose network");

        let error = local("http://192.168.1.13:8080", in_compose()).unwrap_err();
        let ServerUrlError::NotLocal { reason, .. } = &error else {
            panic!("expected NotLocal, got {error:?}");
        };
        assert!(reason.contains("192.168.1.13"), "{reason}");
        assert!(
            reason.contains("172.20.0.0/16"),
            "names the networks it has: {reason}"
        );
    }

    #[test]
    fn a_compose_network_in_dockers_fallback_pool_does_not_open_the_lan() {
        // Docker hands out 192.168.x.0/20 once 172.17 to 172.31 are taken.
        // The /20 the container has is local; the LAN in the same /16 is not.
        let routes = [
            HEADER.to_owned(),
            route([0, 0, 0, 0], [192, 168, 16, 1], 0x0003, [0, 0, 0, 0]),
            route([192, 168, 16, 0], [0, 0, 0, 0], 0x0001, [255, 255, 240, 0]),
        ]
        .join("\n");
        let networks = LocalNetworks::parse(true, &routes, "");
        local("http://192.168.16.3:8080", networks.clone()).expect("the Compose network");
        local("http://192.168.31.250:8080", networks.clone()).expect("the same /20");
        assert!(local("http://192.168.1.13:8080", networks.clone()).is_err());
        assert!(local("http://192.168.32.1:8080", networks).is_err());
    }

    #[test]
    fn the_default_route_and_routes_through_a_gateway_reach_nothing_directly() {
        let routes = [
            HEADER.to_owned(),
            // Default route through a gateway.
            route([0, 0, 0, 0], [172, 20, 0, 1], 0x0003, [0, 0, 0, 0]),
            // Default route with no gateway, as on a point-to-point link.
            route([0, 0, 0, 0], [0, 0, 0, 0], 0x0001, [0, 0, 0, 0]),
            // A network reached through a gateway.
            route([10, 0, 0, 0], [172, 20, 0, 1], 0x0003, [255, 0, 0, 0]),
            // A gateway address without the flag is still a gateway.
            route([10, 9, 0, 0], [172, 20, 0, 1], 0x0001, [255, 255, 0, 0]),
            // A route that rejects.
            route([10, 8, 0, 0], [0, 0, 0, 0], 0x0201, [255, 255, 0, 0]),
            // A route that is down.
            route([10, 7, 0, 0], [0, 0, 0, 0], 0x0000, [255, 255, 0, 0]),
            // A mask with a hole in it.
            route([10, 6, 0, 0], [0, 0, 0, 0], 0x0001, [255, 0, 255, 0]),
        ]
        .join("\n");
        let networks = LocalNetworks::parse(true, &routes, "");
        for text in [
            "http://8.8.8.8",
            "http://10.1.2.3",
            "http://10.9.0.1",
            "http://10.8.0.1",
            "http://10.7.0.1",
            "http://10.6.0.1",
        ] {
            assert!(
                local(text, networks.clone()).is_err(),
                "{text} was accepted"
            );
        }
        assert!(
            networks.attached.is_empty(),
            "nothing was attached: {:?}",
            networks.attached
        );
    }

    #[test]
    fn garbage_in_a_routing_table_is_skipped() {
        let routes = format!(
            "{HEADER}\nnonsense\neth0\tZZZZZZZZ\t00000000\t0001\t0\t0\t0\t0000FFFF\n\n{}",
            route([172, 20, 0, 0], [0, 0, 0, 0], 0x0001, [255, 255, 0, 0])
        );
        let networks = LocalNetworks::parse(true, &routes, "short line\n");
        assert_eq!(networks.attached.len(), 1);
        local("http://172.20.9.9", networks).expect("the one good route");
    }

    #[test]
    fn ipv6_attached_networks_are_read_and_multicast_and_defaults_are_not() {
        let ipv6 = [
            // fd00:1::/64 on eth0.
            "fd000001000000000000000000000000 40 00000000000000000000000000000000 00 \
             00000000000000000000000000000000 00000100 00000001 00000000 00000001 eth0",
            // The default route through a gateway.
            "00000000000000000000000000000000 00 00000000000000000000000000000000 00 \
             fd000001000000000000000000000001 00000400 00000001 00000000 00000003 eth0",
            // Multicast.
            "ff000000000000000000000000000000 08 00000000000000000000000000000000 00 \
             00000000000000000000000000000000 00000100 00000001 00000000 00000001 eth0",
            // An unreachable route, which rejects.
            "fd000002000000000000000000000000 40 00000000000000000000000000000000 00 \
             00000000000000000000000000000000 00000100 00000001 00000000 00000201 lo",
        ]
        .join("\n");
        let networks = LocalNetworks::parse(true, "", &ipv6);
        local("http://[fd00:1::5]:8080", networks.clone()).expect("the attached /64");
        assert!(local("http://[fd00:2::5]:8080", networks.clone()).is_err());
        assert!(local("http://[2001:db8::1]:8080", networks.clone()).is_err());
        assert_eq!(networks.attached.len(), 1, "{:?}", networks.attached);
    }

    #[test]
    fn a_name_must_resolve_only_to_local_addresses() {
        let networks = in_compose();
        let compose: IpAddr = "172.20.0.2".parse().unwrap();
        let lan: IpAddr = "192.168.1.13".parse().unwrap();

        assert_eq!(
            judge("server", vec![compose, compose], &networks).expect("local"),
            vec![compose]
        );

        // One address off the network is enough to refuse: a connection may
        // pick whichever it likes.
        let error = judge("server", vec![compose, lan], &networks).unwrap_err();
        let ServerUrlError::NotLocal { host, reason } = &error else {
            panic!("expected NotLocal, got {error:?}");
        };
        assert_eq!(host, "server");
        assert!(reason.contains("192.168.1.13"), "{reason}");
        assert!(!reason.contains("172.20.0.2 is"), "{reason}");

        assert!(matches!(
            judge("server", Vec::new(), &networks).unwrap_err(),
            ServerUrlError::Unresolved { .. }
        ));
    }

    #[tokio::test]
    async fn a_name_is_checked_when_it_resolves() {
        // localhost is loopback wherever this runs.
        let server = local("http://localhost:8080", not_in_a_container()).expect("a name");
        server.check_resolution().await.expect("loopback");

        // A name is not judged until it is resolved, and one that will not
        // resolve says so.
        let server = local("http://tangible.invalid:8080", in_compose()).expect("a name");
        assert!(matches!(
            server.check_resolution().await.unwrap_err(),
            ServerUrlError::Unresolved { .. }
        ));

        // Nothing to resolve for TLS, or for plain HTTP the operator accepted.
        local("https://tangible.invalid", in_compose())
            .expect("tls")
            .check_resolution()
            .await
            .expect("not checked");
        ServerUrl::with_networks(
            "http://tangible.invalid",
            PlainHttp::Allowed,
            LocalNetworks::default(),
        )
        .expect("accepted")
        .check_resolution()
        .await
        .expect("not checked");
    }

    /// The tables of a real container, captured 2026-11-02 from
    /// `docker run --read-only --cap-drop ALL --network <a user-defined
    /// network> alpine:3.20` on x86_64: eth0 at 172.23.0.2/16, IPv6 off, so
    /// only loopback and unreachable routes in the IPv6 table.
    const CAPTURED_IPV4: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT  \n\
eth0\t00000000\t010017AC\t0003\t0\t0\t0\t00000000\t0\t0\t0  \n\
eth0\t000017AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0  \n";
    const CAPTURED_IPV6: &str = "\
00000000000000000000000000000000 00 00000000000000000000000000000000 00 \
00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo
00000000000000000000000000000001 80 00000000000000000000000000000000 00 \
00000000000000000000000000000000 00000000 00000004 00000000 80200001       lo
00000000000000000000000000000000 00 00000000000000000000000000000000 00 \
00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo
";

    // The captured IPv4 table is in a little-endian machine's order.
    #[cfg(target_endian = "little")]
    #[test]
    fn a_real_containers_tables_give_its_compose_network() {
        let networks = LocalNetworks::parse(true, CAPTURED_IPV4, CAPTURED_IPV6);
        assert_eq!(
            networks
                .attached
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["172.23.0.0/16", "::1/128"]
        );
        local("http://172.23.0.3:8080", networks.clone()).expect("the network it is on");
        assert!(local("http://172.24.0.3:8080", networks.clone()).is_err());
        assert!(local("http://192.168.1.13:8080", networks).is_err());
    }

    #[test]
    fn the_base_keeps_no_trailing_slash() {
        let server = local("http://server:8080/", in_compose()).expect("url");
        assert_eq!(server.base(), "http://server:8080");
    }

    #[test]
    fn this_machines_networks_can_be_read() {
        // Whatever they are, reading them does not fail, and loopback is
        // always local.
        let networks = LocalNetworks::read();
        assert!(networks.refusal(IpAddr::V4(Ipv4Addr::LOCALHOST)).is_none());
        assert!(networks.refusal(IpAddr::V6(Ipv6Addr::LOCALHOST)).is_none());
    }
}
