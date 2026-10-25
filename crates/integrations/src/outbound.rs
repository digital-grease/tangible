// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The one way Tangible fetches from an address somebody gave it (ADR 0016).
//!
//! A URL an operator pastes, a download client's base URL, an S3 endpoint:
//! each is an address chosen by someone other than whoever deployed the
//! server, and a server that fetches whatever it is told is a way into the
//! network it sits on (SECURITY.md 3.5). Every such fetch goes through
//! [`OutboundClient`], which refuses before a byte is sent:
//!
//! - any scheme but `https`, and `http` too unless the policy allows it;
//! - a URL carrying a username or password;
//! - a port other than 80 and 443, unless the policy allows any;
//! - an address in a refused range ([`classify`]): loopback, private,
//!   link-local (the cloud metadata service lives there), shared CGNAT,
//!   multicast, broadcast, unspecified, documentation, benchmarking and
//!   reserved space, IPv4 hidden inside IPv6 included, unless the policy lets
//!   private networks through or an administrator listed a range.
//!
//! The address checked is the address connected to. Names are resolved by
//! the client's own resolver ([`CheckingResolver`]), which hands the
//! connector only addresses that passed, so a name cannot answer one address
//! to a check and another to the connection. A literal IP never reaches a
//! resolver, so it is checked with the URL, and again on every redirect.
//! Redirects are followed at most [`MAX_REDIRECTS`] times, never from
//! `https` to `http`, each one checked as the first was. The environment's
//! proxy settings are ignored; a proxy is used only when the policy names
//! one, and it then resolves names itself, so only names and literal
//! addresses can be checked here (documented in ADR 0016).

use std::error::Error as StdError;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// The most redirects one fetch follows.
pub const MAX_REDIRECTS: usize = 5;

/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the server may go without sending anything.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// What an address is, when it is not an ordinary public one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// This machine: `127.0.0.0/8`, `::1`.
    Loopback,
    /// A private network: RFC 1918, IPv6 unique local (`fc00::/7`).
    Private,
    /// Carrier-grade NAT shared space, `100.64.0.0/10`; also where some
    /// overlay networks put their addresses.
    SharedCgnat,
    /// Link-local: `169.254.0.0/16` (the cloud metadata service) and
    /// `fe80::/10`.
    LinkLocal,
    /// Multicast.
    Multicast,
    /// `255.255.255.255`.
    Broadcast,
    /// `0.0.0.0/8`, `::`.
    Unspecified,
    /// Documentation examples, never routed.
    Documentation,
    /// Benchmarking space, never routed.
    Benchmarking,
    /// Anything else set aside: future use, protocol assignments, 6to4 relay,
    /// deprecated site-local, discard.
    Reserved,
}

impl Range {
    /// A name for logs and refusals.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::Private => "private network",
            Self::SharedCgnat => "shared (CGNAT) network",
            Self::LinkLocal => "link-local",
            Self::Multicast => "multicast",
            Self::Broadcast => "broadcast",
            Self::Unspecified => "unspecified",
            Self::Documentation => "documentation",
            Self::Benchmarking => "benchmarking",
            Self::Reserved => "reserved",
        }
    }

    /// Whether a policy that lets private networks through lets this through.
    #[must_use]
    pub const fn is_private_network(self) -> bool {
        matches!(self, Self::Private | Self::SharedCgnat)
    }
}

/// Where an address falls, or `None` for an ordinary public address.
///
/// IPv4 carried inside IPv6 (mapped, NAT64, 6to4) is classified as the IPv4
/// address it carries, so `::ffff:127.0.0.1` is loopback.
#[must_use]
pub fn classify(address: IpAddr) -> Option<Range> {
    match address {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn classify_v4(address: Ipv4Addr) -> Option<Range> {
    let [a, b, c, _] = address.octets();
    let range = match (a, b, c) {
        (0, ..) => Range::Unspecified,
        (10, ..) | (172, 16..=31, _) | (192, 168, _) => Range::Private,
        (100, 64..=127, _) => Range::SharedCgnat,
        (127, ..) => Range::Loopback,
        (169, 254, _) => Range::LinkLocal,
        (192, 0, 0) | (192, 88, 99) => Range::Reserved,
        (192, 0, 2) | (198, 51, 100) | (203, 0, 113) => Range::Documentation,
        (198, 18 | 19, _) => Range::Benchmarking,
        (224..=239, ..) => Range::Multicast,
        _ if address == Ipv4Addr::BROADCAST => Range::Broadcast,
        (240..=255, ..) => Range::Reserved,
        _ => return None,
    };
    Some(range)
}

fn classify_v6(address: Ipv6Addr) -> Option<Range> {
    if address.is_unspecified() {
        return Some(Range::Unspecified);
    }
    if address.is_loopback() {
        return Some(Range::Loopback);
    }
    let segments = address.segments();
    let octets = address.octets();
    let embedded =
        |at: usize| Ipv4Addr::new(octets[at], octets[at + 1], octets[at + 2], octets[at + 3]);
    // ::ffff:a.b.c.d, and the deprecated IPv4-compatible ::a.b.c.d.
    if segments[..5] == [0; 5] && (segments[5] == 0xffff || segments[5] == 0) {
        return classify_v4(embedded(12)).or(Some(Range::Reserved).filter(|_| segments[5] == 0));
    }
    // NAT64, 64:ff9b::/96: the IPv4 address it translates to decides.
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return classify_v4(embedded(12));
    }
    // 6to4, 2002::/16: the relay's IPv4 address is in the next 32 bits.
    if segments[0] == 0x2002 {
        return classify_v4(embedded(2));
    }
    let range = match segments[0] {
        0xfc00..=0xfdff => Range::Private,
        0xfe80..=0xfebf => Range::LinkLocal,
        0xfec0..=0xfeff => Range::Reserved,
        0xff00..=0xffff => Range::Multicast,
        0x2001 if segments[1] == 0x0db8 => Range::Documentation,
        0x2001 if segments[1] == 0x0002 && segments[2] == 0 => Range::Benchmarking,
        // IETF protocol assignments, 2001::/23, Teredo among them.
        0x2001 if segments[1] < 0x0200 => Range::Reserved,
        0x0100 if segments[1..4] == [0, 0, 0] => Range::Reserved,
        _ => return None,
    };
    Some(range)
}

/// A block of addresses an administrator allowed: `192.168.1.0/24`,
/// `fd00::/8`, or one address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

/// Why a CIDR did not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not an address or an address/prefix")]
pub struct CidrError(String);

impl std::str::FromStr for Cidr {
    type Err = CidrError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || CidrError(text.to_owned());
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let network: IpAddr = address.trim().parse().map_err(|_| invalid())?;
        let bits = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => bits,
            Some(prefix) => prefix.trim().parse::<u8>().map_err(|_| invalid())?,
        };
        if prefix > bits {
            return Err(invalid());
        }
        Ok(Self { network, prefix })
    }
}

impl Cidr {
    /// Whether `address` is in this block. An IPv4 address mapped into IPv6
    /// is compared as IPv4.
    #[must_use]
    pub fn contains(&self, address: IpAddr) -> bool {
        let address = match address {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
            v4 @ IpAddr::V4(_) => v4,
        };
        match (self.network, address) {
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

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

/// What an outbound fetch may reach.
#[derive(Debug, Clone, Default)]
pub struct OutboundPolicy {
    /// Allow plain `http`. Off by default.
    pub allow_http: bool,
    /// Let private networks (RFC 1918, ULA, CGNAT) through: for an address an
    /// administrator configured, such as a download client on the LAN, never
    /// for one an operator supplied (ADR 0016).
    pub allow_private_networks: bool,
    /// Allow any port, not only 80 and 443: likewise for administrators'
    /// addresses.
    pub allow_any_port: bool,
    /// Blocks an administrator allowed whatever range they fall in.
    pub allowed: Vec<Cidr>,
    /// A proxy to send everything through, when the deployment needs one.
    pub proxy: Option<Url>,
}

impl OutboundPolicy {
    /// The policy for a URL an operator supplied: `https`, ports 80 and 443,
    /// public addresses only, plus the administrator's allowed blocks.
    #[must_use]
    pub fn for_operator_urls(allow_http: bool, allowed: Vec<Cidr>, proxy: Option<Url>) -> Self {
        Self {
            allow_http,
            allow_private_networks: false,
            allow_any_port: false,
            allowed,
            proxy,
        }
    }

    /// The policy for an address an administrator configured for an
    /// integration: private networks and any port as well, `http` allowed
    /// (a LAN client rarely has a certificate). Loopback and link-local stay
    /// refused unless allowed by block.
    #[must_use]
    pub fn for_integrations(allowed: Vec<Cidr>, proxy: Option<Url>) -> Self {
        Self {
            allow_http: true,
            allow_private_networks: true,
            allow_any_port: true,
            allowed,
            proxy,
        }
    }

    /// Why `address` may not be reached, or `None` when it may.
    #[must_use]
    pub fn refuses(&self, address: IpAddr) -> Option<Range> {
        let range = classify(address)?;
        if self.allowed.iter().any(|cidr| cidr.contains(address)) {
            return None;
        }
        if self.allow_private_networks && range.is_private_network() {
            return None;
        }
        Some(range)
    }

    /// Check a URL before anything is sent: scheme, credentials, port, and a
    /// literal address. A name is checked when it is resolved.
    ///
    /// # Errors
    ///
    /// The [`Refusal`] saying why.
    pub fn check_url(&self, url: &Url) -> Result<(), Refusal> {
        match url.scheme() {
            "https" => {}
            "http" if self.allow_http => {}
            "http" => return Err(Refusal::PlainHttp),
            other => return Err(Refusal::Scheme(other.chars().take(20).collect())),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Refusal::Credentials);
        }
        let Some(host) = url.host() else {
            return Err(Refusal::NoHost);
        };
        if let Some(port) = url.port()
            && !self.allow_any_port
            && port != 80
            && port != 443
        {
            return Err(Refusal::Port(port));
        }
        let literal = match host {
            url::Host::Ipv4(v4) => Some(IpAddr::V4(v4)),
            url::Host::Ipv6(v6) => Some(IpAddr::V6(v6)),
            url::Host::Domain(_) => None,
        };
        if let Some(address) = literal
            && let Some(range) = self.refuses(address)
        {
            return Err(Refusal::Address { address, range });
        }
        Ok(())
    }
}

/// Why an outbound fetch was refused before it was made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// Plain `http`, which the policy does not allow.
    #[error("plain http is not allowed; use https")]
    PlainHttp,
    /// A scheme that is not http or https.
    #[error("the {0} scheme is not allowed")]
    Scheme(String),
    /// A username or password in the URL.
    #[error("a URL may not carry a username or password")]
    Credentials,
    /// No host.
    #[error("the URL names no host")]
    NoHost,
    /// A port other than 80 and 443.
    #[error("port {0} is not allowed")]
    Port(u16),
    /// An address in a refused range.
    #[error("{address} is a {} address", range.as_str())]
    Address {
        /// The address.
        address: IpAddr,
        /// Its range.
        range: Range,
    },
    /// A name with no address that may be reached.
    #[error("{name} has no address that may be reached ({reason})")]
    Name {
        /// The name, as asked.
        name: String,
        /// Why: every address refused, or none found.
        reason: String,
    },
    /// Too many redirects.
    #[error("more than {MAX_REDIRECTS} redirects")]
    TooManyRedirects,
    /// A redirect from https to http.
    #[error("a redirect from https to http is not followed")]
    Downgrade,
}

impl Refusal {
    /// The stable code, for every refusal alike (SECURITY.md 8).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        "IMPORT_SSRF_BLOCKED"
    }
}

/// How names are turned into addresses. The system resolver in a server; a
/// fixed table in tests.
pub trait Lookup: Send + Sync + 'static {
    /// The addresses `name` resolves to.
    fn lookup(
        &self,
        name: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<Vec<IpAddr>>> + Send>>;
}

/// The system's resolver, through tokio.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemLookup;

impl Lookup for SystemLookup {
    fn lookup(
        &self,
        name: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<Vec<IpAddr>>> + Send>>
    {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((name.as_str(), 0)).await?;
            Ok(addresses.map(|address| address.ip()).collect())
        })
    }
}

/// A resolver that hands the connector only addresses the policy allows.
pub struct CheckingResolver {
    policy: Arc<OutboundPolicy>,
    lookup: Arc<dyn Lookup>,
}

impl CheckingResolver {
    /// Resolve with `lookup`, keeping only what `policy` allows.
    #[must_use]
    pub fn new(policy: Arc<OutboundPolicy>, lookup: Arc<dyn Lookup>) -> Self {
        Self { policy, lookup }
    }

    /// The addresses `name` may be reached at, in the order found.
    ///
    /// # Errors
    ///
    /// [`Refusal::Name`] when it has none: every address refused, or none.
    pub async fn allowed(&self, name: &str) -> Result<Vec<IpAddr>, Refusal> {
        let found = self
            .lookup
            .lookup(name.to_owned())
            .await
            .map_err(|error| Refusal::Name {
                name: name.to_owned(),
                reason: format!("it did not resolve: {}", error.kind()),
            })?;
        let mut refused = Vec::new();
        let allowed: Vec<IpAddr> = found
            .into_iter()
            .filter(|address| match self.policy.refuses(*address) {
                Some(range) => {
                    refused.push(format!("{address} is {}", range.as_str()));
                    false
                }
                None => true,
            })
            .collect();
        if allowed.is_empty() {
            return Err(Refusal::Name {
                name: name.to_owned(),
                reason: if refused.is_empty() {
                    "it resolved to nothing".to_owned()
                } else {
                    refused.join(", ")
                },
            });
        }
        Ok(allowed)
    }
}

impl Resolve for CheckingResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = Self {
            policy: Arc::clone(&self.policy),
            lookup: Arc::clone(&self.lookup),
        };
        let name = name.as_str().to_owned();
        Box::pin(async move {
            let allowed = resolver.allowed(&name).await?;
            let addrs: Addrs = Box::new(
                allowed
                    .into_iter()
                    .map(|address| SocketAddr::new(address, 0)),
            );
            Ok(addrs)
        })
    }
}

/// Why an outbound fetch failed.
#[derive(Debug, thiserror::Error)]
pub enum OutboundError {
    /// Refused before anything was sent.
    #[error("refused: {0}")]
    Refused(Refusal),
    /// The transfer itself failed.
    #[error("the request failed: {0}")]
    Transport(#[source] reqwest::Error),
}

impl OutboundError {
    /// Classify a reqwest error, finding a refusal however deep the
    /// connector or redirect policy wrapped it.
    #[must_use]
    pub fn from_reqwest(error: reqwest::Error) -> Self {
        let mut source: Option<&(dyn StdError + 'static)> = Some(&error);
        while let Some(current) = source {
            if let Some(refusal) = current.downcast_ref::<Refusal>() {
                return Self::Refused(refusal.clone());
            }
            source = current.source();
        }
        Self::Transport(error)
    }
}

/// An HTTP client that reaches only what its policy allows.
#[derive(Debug, Clone)]
pub struct OutboundClient {
    http: reqwest::Client,
    policy: Arc<OutboundPolicy>,
}

impl OutboundClient {
    /// A client for `policy`, resolving names through the system.
    ///
    /// # Errors
    ///
    /// [`OutboundError::Transport`] if the client cannot be built, such as for
    /// an unusable proxy.
    pub fn new(policy: OutboundPolicy, user_agent: &str) -> Result<Self, OutboundError> {
        Self::with_lookup(policy, user_agent, Arc::new(SystemLookup))
    }

    /// A client resolving names with `lookup`. For tests.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn with_lookup(
        policy: OutboundPolicy,
        user_agent: &str,
        lookup: Arc<dyn Lookup>,
    ) -> Result<Self, OutboundError> {
        let policy = Arc::new(policy);
        let redirects = Arc::clone(&policy);
        let mut builder = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .https_only(!policy.allow_http)
            .referer(false)
            .dns_resolver(CheckingResolver::new(Arc::clone(&policy), lookup))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > MAX_REDIRECTS {
                    return attempt.error(Refusal::TooManyRedirects);
                }
                let downgrade = attempt
                    .previous()
                    .last()
                    .is_some_and(|from| from.scheme() == "https")
                    && attempt.url().scheme() == "http";
                if downgrade {
                    return attempt.error(Refusal::Downgrade);
                }
                match redirects.check_url(attempt.url()) {
                    Ok(()) => attempt.follow(),
                    Err(refusal) => attempt.error(refusal),
                }
            }));
        builder = match &policy.proxy {
            // The environment's proxy settings would send requests around
            // the resolver; only a proxy the policy names is used.
            None => builder.no_proxy(),
            Some(proxy) => builder
                .proxy(reqwest::Proxy::all(proxy.as_str()).map_err(OutboundError::Transport)?),
        };
        let http = builder.build().map_err(OutboundError::Transport)?;
        Ok(Self { http, policy })
    }

    /// The policy this client keeps to.
    #[must_use]
    pub fn policy(&self) -> &OutboundPolicy {
        &self.policy
    }

    /// Start a GET of `url`, checked first. The caller adds headers (such as
    /// `Range`) and sends it.
    ///
    /// # Errors
    ///
    /// [`OutboundError::Refused`] for a URL the policy refuses.
    pub fn get(&self, url: &Url) -> Result<reqwest::RequestBuilder, OutboundError> {
        self.policy.check_url(url).map_err(OutboundError::Refused)?;
        Ok(self.http.get(url.clone()))
    }

    /// Send a request built from [`Self::get`], classifying a refusal made
    /// while connecting or redirecting.
    ///
    /// # Errors
    ///
    /// [`OutboundError`].
    pub async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, OutboundError> {
        request.send().await.map_err(OutboundError::from_reqwest)
    }
}

/// A URL as it may be recorded or logged: scheme, host, port and path, with
/// any credentials, query and fragment removed. Signed URLs carry their
/// secret in the query.
#[must_use]
pub fn redacted(url: &Url) -> String {
    let mut shown = url.clone();
    let _ = shown.set_username("");
    let _ = shown.set_password(None);
    shown.set_query(None);
    shown.set_fragment(None);
    shown.to_string()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn every_refused_range_is_named() {
        for (address, range) in [
            ("0.0.0.0", Range::Unspecified),
            ("0.1.2.3", Range::Unspecified),
            ("10.0.0.1", Range::Private),
            ("100.64.0.1", Range::SharedCgnat),
            ("100.127.255.255", Range::SharedCgnat),
            ("127.0.0.1", Range::Loopback),
            ("127.255.255.254", Range::Loopback),
            ("169.254.169.254", Range::LinkLocal),
            ("172.16.0.1", Range::Private),
            ("172.31.255.255", Range::Private),
            ("172.17.0.1", Range::Private),
            ("192.0.0.8", Range::Reserved),
            ("192.0.2.1", Range::Documentation),
            ("192.88.99.1", Range::Reserved),
            ("192.168.1.13", Range::Private),
            ("198.18.0.1", Range::Benchmarking),
            ("198.51.100.7", Range::Documentation),
            ("203.0.113.9", Range::Documentation),
            ("224.0.0.1", Range::Multicast),
            ("239.255.255.250", Range::Multicast),
            ("240.0.0.1", Range::Reserved),
            ("255.255.255.255", Range::Broadcast),
            ("::", Range::Unspecified),
            ("::1", Range::Loopback),
            ("::ffff:127.0.0.1", Range::Loopback),
            ("::ffff:169.254.169.254", Range::LinkLocal),
            ("::ffff:10.1.2.3", Range::Private),
            ("::127.0.0.1", Range::Loopback),
            ("64:ff9b::7f00:1", Range::Loopback),
            ("2002:7f00:1::", Range::Loopback),
            ("2002:c0a8:101::1", Range::Private),
            ("fc00::1", Range::Private),
            ("fd12:3456::1", Range::Private),
            ("fe80::1", Range::LinkLocal),
            ("fec0::1", Range::Reserved),
            ("ff02::1", Range::Multicast),
            ("2001:db8::1", Range::Documentation),
            ("2001:2::1", Range::Benchmarking),
            ("2001::1", Range::Reserved),
            ("100::1", Range::Reserved),
        ] {
            assert_eq!(classify(ip(address)), Some(range), "{address}");
        }
    }

    #[test]
    fn public_addresses_are_ordinary() {
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.169.0.1",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
            "2606:4700:4700::1111",
            "2a00:1450:4001::200e",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::",
        ] {
            assert_eq!(classify(ip(address)), None, "{address}");
        }
    }

    #[test]
    fn blocks_parse_and_contain() {
        let lan: Cidr = "192.168.1.0/24".parse().unwrap();
        assert!(lan.contains(ip("192.168.1.13")));
        assert!(
            lan.contains(ip("::ffff:192.168.1.13")),
            "mapped compares as IPv4"
        );
        assert!(!lan.contains(ip("192.168.2.1")));
        let one: Cidr = "10.0.0.5".parse().unwrap();
        assert!(one.contains(ip("10.0.0.5")) && !one.contains(ip("10.0.0.6")));
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains(ip("127.0.0.1")) && !all.contains(ip("::1")));
        let ula: Cidr = "fd00::/8".parse().unwrap();
        assert!(ula.contains(ip("fd12::1")) && !ula.contains(ip("fe80::1")));
        assert_eq!(ula.to_string(), "fd00::/8");
        for bad in [
            "",
            "10.0.0.0/33",
            "fd00::/129",
            "example.com/8",
            "10.0.0.0/x",
            "10/8",
        ] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_operators_url_reaches_only_the_public_internet_over_https() {
        let policy = OutboundPolicy::for_operator_urls(false, Vec::new(), None);
        let url = |text: &str| Url::parse(text).unwrap();
        assert_eq!(
            policy.check_url(&url("https://example.com/disc.iso")),
            Ok(())
        );
        assert_eq!(policy.check_url(&url("https://1.1.1.1/x")), Ok(()));
        assert_eq!(policy.check_url(&url("https://example.com:443/x")), Ok(()));
        for (text, expected) in [
            ("http://example.com/x", Refusal::PlainHttp),
            ("ftp://example.com/x", Refusal::Scheme("ftp".to_owned())),
            ("file:///etc/passwd", Refusal::Scheme("file".to_owned())),
            ("https://user:pass@example.com/x", Refusal::Credentials),
            ("https://user@example.com/x", Refusal::Credentials),
            ("https://example.com:8443/x", Refusal::Port(8443)),
            ("https://example.com:22/x", Refusal::Port(22)),
        ] {
            assert_eq!(policy.check_url(&url(text)), Err(expected), "{text}");
        }
        // Every way of writing a refused address, as the URL parser reads it.
        for text in [
            "https://127.0.0.1/x",
            "https://127.1/x",
            "https://2130706433/x",
            "https://0x7f000001/x",
            "https://0177.0.0.1/x",
            "https://[::1]/x",
            "https://[::ffff:127.0.0.1]/x",
            "https://[0:0:0:0:0:ffff:7f00:1]/x",
            "https://169.254.169.254/latest/meta-data/",
            "https://[fe80::1]/x",
            "https://10.0.0.1/x",
            "https://0.0.0.0/x",
        ] {
            assert!(
                matches!(policy.check_url(&url(text)), Err(Refusal::Address { .. })),
                "{text}"
            );
        }
        // An administrator's block lets one through.
        let allowed =
            OutboundPolicy::for_operator_urls(false, vec!["10.0.0.0/8".parse().unwrap()], None);
        assert_eq!(allowed.check_url(&url("https://10.0.0.1/x")), Ok(()));
        // Plain http when allowed.
        let http = OutboundPolicy::for_operator_urls(true, Vec::new(), None);
        assert_eq!(http.check_url(&url("http://example.com/x")), Ok(()));
    }

    #[test]
    fn an_integrations_address_may_be_on_the_lan_but_not_this_machine_or_metadata() {
        let policy = OutboundPolicy::for_integrations(Vec::new(), None);
        let url = |text: &str| Url::parse(text).unwrap();
        for text in [
            "http://192.168.1.13:8080/",
            "http://172.17.0.1:6789/jsonrpc",
            "http://100.101.102.103:8080/",
            "http://[fd00::5]:8080/",
        ] {
            assert_eq!(policy.check_url(&url(text)), Ok(()), "{text}");
        }
        for text in [
            "http://127.0.0.1:8080/",
            "http://169.254.169.254/",
            "http://[::1]:8080/",
            "http://224.0.0.1/",
        ] {
            assert!(policy.check_url(&url(text)).is_err(), "{text}");
        }
    }

    #[test]
    fn urls_are_recorded_without_credentials_query_or_fragment() {
        let url = Url::parse(
            "https://user:secret@example.com:8443/files/disc.iso?X-Amz-Signature=abc&token=t#part",
        )
        .unwrap();
        assert_eq!(redacted(&url), "https://example.com:8443/files/disc.iso");
    }

    /// Answers names from a table, as a resolver an attacker controls might.
    struct Table(Vec<(&'static str, Vec<IpAddr>)>);

    impl Lookup for Table {
        fn lookup(
            &self,
            name: String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<Vec<IpAddr>>> + Send>>
        {
            let found = self
                .0
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, addresses)| addresses.clone());
            Box::pin(async move {
                found.ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
            })
        }
    }

    #[tokio::test]
    async fn a_name_is_reached_only_at_addresses_that_pass() {
        let policy = Arc::new(OutboundPolicy::for_operator_urls(false, Vec::new(), None));
        let resolver = CheckingResolver::new(
            policy,
            Arc::new(Table(vec![
                ("public.test", vec![ip("93.184.215.14")]),
                ("internal.test", vec![ip("10.0.0.5")]),
                ("metadata.test", vec![ip("169.254.169.254")]),
                // One good answer and one bad: only the good one is used.
                ("mixed.test", vec![ip("127.0.0.1"), ip("93.184.215.14")]),
                ("mapped.test", vec![ip("::ffff:127.0.0.1")]),
                ("empty.test", vec![]),
            ])),
        );
        assert_eq!(
            resolver.allowed("public.test").await.unwrap(),
            [ip("93.184.215.14")]
        );
        assert_eq!(
            resolver.allowed("mixed.test").await.unwrap(),
            [ip("93.184.215.14")]
        );
        for name in [
            "internal.test",
            "metadata.test",
            "mapped.test",
            "empty.test",
            "missing.test",
        ] {
            assert!(
                matches!(resolver.allowed(name).await, Err(Refusal::Name { .. })),
                "{name}"
            );
        }
    }

    /// An HTTP server on 127.0.0.1 answering every connection with
    /// `response`, counting the connections it accepted.
    async fn server(response: String) -> (SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 4096];
                    let _ = stream.read(&mut buffer).await;
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (address, accepted)
    }

    fn client(policy: OutboundPolicy, table: Vec<(&'static str, Vec<IpAddr>)>) -> OutboundClient {
        OutboundClient::with_lookup(policy, "tangible-test", Arc::new(Table(table))).unwrap()
    }

    const OK: &str = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";

    #[tokio::test]
    async fn this_machine_is_refused_before_a_connection_is_made() {
        let (address, accepted) = server(OK.to_owned()).await;
        let policy = OutboundPolicy::for_operator_urls(true, Vec::new(), None);
        let outbound = client(policy, vec![("loopback.test", vec![ip("127.0.0.1")])]);

        for text in [
            format!("http://127.0.0.1:{}/", address.port()),
            format!("http://loopback.test:{}/", address.port()),
        ] {
            // Ports other than 80 and 443 are refused as well; allow them to
            // show the address is what refuses.
            let mut policy = outbound.policy().clone();
            policy.allow_any_port = true;
            let outbound = client(policy, vec![("loopback.test", vec![ip("127.0.0.1")])]);
            let url = Url::parse(&text).unwrap();
            let result = match outbound.get(&url) {
                Ok(request) => outbound.send(request).await.map(|_| ()),
                Err(error) => Err(error),
            };
            assert!(
                matches!(result, Err(OutboundError::Refused(_))),
                "{text}: {result:?}"
            );
        }
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "nothing reached the server"
        );
    }

    #[tokio::test]
    async fn an_allowed_block_is_reached() {
        let (address, accepted) = server(OK.to_owned()).await;
        let mut policy =
            OutboundPolicy::for_operator_urls(true, vec!["127.0.0.0/8".parse().unwrap()], None);
        policy.allow_any_port = true;
        let outbound = client(policy, vec![("local.test", vec![ip("127.0.0.1")])]);
        let url = Url::parse(&format!("http://local.test:{}/", address.port())).unwrap();
        let response = outbound.send(outbound.get(&url).unwrap()).await.unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_redirect_to_a_refused_address_is_not_followed() {
        // The first server is allowed (by its own /32) and redirects to
        // another loopback address that is not.
        let (target, target_accepted) = server(OK.to_owned()).await;
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.2:{}/\r\ncontent-length: 0\r\n\
             connection: close\r\n\r\n",
            target.port()
        );
        let (first, _) = server(redirect).await;
        let mut policy =
            OutboundPolicy::for_operator_urls(true, vec!["127.0.0.1/32".parse().unwrap()], None);
        policy.allow_any_port = true;
        let outbound = client(policy, Vec::new());
        let url = Url::parse(&format!("http://127.0.0.1:{}/", first.port())).unwrap();
        let result = outbound.send(outbound.get(&url).unwrap()).await;
        assert!(
            matches!(result, Err(OutboundError::Refused(Refusal::Address { .. }))),
            "{result:?}"
        );
        assert_eq!(target_accepted.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn redirects_stop_at_the_limit() {
        // A server that redirects to itself forever.
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let location = format!("http://127.0.0.1:{}/again", address.port());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let location = location.clone();
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 4096];
                    let _ = stream.read(&mut buffer).await;
                    let response = format!(
                        "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\n\
                         connection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        let mut policy =
            OutboundPolicy::for_operator_urls(true, vec!["127.0.0.1/32".parse().unwrap()], None);
        policy.allow_any_port = true;
        let outbound = client(policy, Vec::new());
        let url = Url::parse(&format!("http://127.0.0.1:{}/", address.port())).unwrap();
        let result = outbound.send(outbound.get(&url).unwrap()).await;
        assert!(
            matches!(
                result,
                Err(OutboundError::Refused(Refusal::TooManyRedirects))
            ),
            "{result:?}"
        );
    }

    #[test]
    fn the_environments_proxy_is_never_used() {
        // A client is built with no_proxy unless the policy names one; an
        // unusable named proxy is an error, not silently ignored.
        let policy = OutboundPolicy {
            proxy: Some(Url::parse("http://proxy.internal:3128").unwrap()),
            ..OutboundPolicy::for_operator_urls(false, Vec::new(), None)
        };
        assert!(OutboundClient::new(policy, "tangible-test").is_ok());
    }

    #[test]
    fn every_refusal_has_the_one_code() {
        assert_eq!(Refusal::PlainHttp.code(), "IMPORT_SSRF_BLOCKED");
        assert_eq!(
            Refusal::Address {
                address: ip("127.0.0.1"),
                range: Range::Loopback
            }
            .to_string(),
            "127.0.0.1 is a loopback address"
        );
    }
}
