//! `relay serve` — resolve the flags, say what is listening, run the relay.
//!
//! # What is pure here and what is not
//!
//! Everything in this module except [`run_serve`] is effect-free: flags in, a
//! validated [`ServePlan`] / a rendered `Vec<String>` / a rendered unit out. The
//! socket-binding half lives in [`crate::adapters::relay_server`], behind the
//! `relay-server` feature. That split is why this module is **not** gated: a
//! build without the feature must still be able to say why it cannot serve, and
//! `--print-service-unit` writes text and installs nothing.
//!
//! # Three addresses, and rustain validates what iroh-relay does not
//!
//! `iroh-relay` has **no** check that the HTTP and HTTPS binds differ, and on a
//! fixed-port collision the failure surfaces as a bare `AddrInUse` from the
//! *relay* listener — the port the operator did **not** type. So
//! [`ServePlan::resolve`] refuses an equal, non-zero pair itself, before any
//! socket is touched. (Port `0` for both succeeds upstream, because the OS hands
//! out two distinct ports, so the refusal targets equal *non-zero* addresses.)
//!
//! The third address is UDP: every rustain client built from this relay's URL
//! probes port 7842 for address discovery, because `RelayMap`'s
//! `FromIterator<RelayUrl>` fills in the default discovery port. A relay that
//! leaves it dead does not stop relaying — it degrades public-address discovery,
//! so peers that could have connected directly stay relayed, and the operator
//! sees `peer ping` report a relayed path with nothing explaining why.
//!
//! # The URL is not built here
//!
//! The string this prints is the output of
//! [`canonical_relay_url`][crate::domain::services::peer_reach_filter::canonical_relay_url]
//! — *the same function the client uses to decide what it will accept*. ⛔ Do
//! not `format!` a URL: `canonical_relay_url` returns `Url::as_str()`, which
//! appends a trailing slash, and a peer's `RelaySet` membership test is an exact
//! string match. A hand-built `https://host` misses it.
//!
//! # ⛔ Not in this cut
//!
//! No allowlist over who may use the relay (`DF-18-4c-b-RELAY-ACCESS`); no ACME
//! certificate issuance (`DF-18-4c-b-ACME`); no `relay install` verb
//! (`DF-18-4c-b-RELAY-INSTALL`); no rustain-side health/reload supervision of
//! the embedded server (`DF-18-4c-b-SUPERVISION`) — the shipped unit hands that
//! to systemd's `Restart=on-failure`.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::adapters::template::render_template;
use crate::domain::services::peer_reach_filter::canonical_relay_url;

/// systemd unit template (checked-in reference, embedded so it can't drift).
const SERVICE_TEMPLATE: &str = include_str!("../../../../dist/rustain-relay.service.template");

/// TCP, plain text. The captive-portal probe when TLS is on; the relay itself
/// when it is off.
pub const DEFAULT_HTTP_ADDR: &str = "0.0.0.0:80";
/// TCP, TLS. The relay listener.
pub const DEFAULT_HTTPS_ADDR: &str = "0.0.0.0:443";
/// UDP. Address discovery, on the port the shipped client already probes.
pub const DEFAULT_QUIC_ADDR: &str = "0.0.0.0:7842";
/// The UDP port every rustain client probes for address discovery, taken from
/// `RelayMap`'s `FromIterator<RelayUrl>` (it fills in the default when a relay
/// is configured by URL). A relay URL cannot carry a discovery port, so this
/// port is a protocol constant, ⛔ not a tunable.
pub const DISCOVERY_PORT: u16 = 7842;
/// `--dev` binds loopback, ⚑ deliberately unlike upstream.
///
/// `iroh-relay`'s own reference binary puts its dev port on
/// `Ipv6Addr::UNSPECIFIED`. That serves a container; a plaintext relay on every
/// interface is not a default rustain should copy onto an operator's laptop.
/// ⛔ Do not "fix" this back to `[::]`.
pub const DEV_HTTP_ADDR: &str = "127.0.0.1:3340";

/// The flags of `relay serve`, already parsed but not yet validated.
///
/// Each address is an `Option` because the *default* is part of the decision:
/// `--dev` moves the HTTP bind to loopback, and the unit renderer has to name
/// whatever a bare `relay serve` would actually bind.
#[derive(Debug, Clone, Default)]
pub struct ServeArgs {
    /// `--http-addr`.
    pub http_addr: Option<String>,
    /// `--https-addr`.
    pub https_addr: Option<String>,
    /// `--quic-addr`.
    pub quic_addr: Option<String>,
    /// `--hostname`: the shareable name. The crate cannot supply it — an
    /// exhaustive sweep of the server API found exactly one field holding a
    /// domain, and it is `pub(crate)`, write-only, with no accessor.
    pub hostname: Option<String>,
    /// `--cert`, a PEM chain.
    pub cert: Option<PathBuf>,
    /// `--key`, a PEM key file.
    pub key: Option<PathBuf>,
    /// `--dev`: plain HTTP on loopback, no TLS, no address discovery.
    pub dev: bool,
    /// `--print-service-unit`: render the unit to stdout and exit.
    pub print_service_unit: bool,
}

/// A `relay serve` invocation rustain refuses before binding anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeRefusal {
    /// The two TCP binds name the same address.
    SameTcpAddress {
        /// The address both flags named.
        addr: SocketAddr,
    },
    /// Two TCP binds that are not equal but still cover each other: the same
    /// non-zero port with an unspecified address on either side. The wildcard
    /// listener owns the specific one too, so the second bind fails with
    /// iroh's bare `AddrInUse` — the exact failure this type exists to pre-empt.
    OverlappingTcpBinds {
        /// What `--http-addr` named.
        http: SocketAddr,
        /// What `--https-addr` named.
        https: SocketAddr,
    },
    /// A flag's value is not a socket address.
    UnparsableAddress {
        /// The flag that carried it.
        flag: &'static str,
        /// What the operator typed.
        value: String,
    },
    /// `--dev` was combined with a TLS or discovery flag.
    DevWithTls,
    /// `--https-addr` or `--quic-addr` without the certificate pair: there is
    /// no TLS listener to put them on, and falling back to plain HTTP would
    /// silently serve the opposite of what the operator asked for.
    TlsFlagsWithoutCert,
    /// One of `--cert` / `--key` without the other.
    IncompleteCertPair,
    /// `--quic-addr` on a non-zero port other than 7842: the relay URL cannot
    /// carry a discovery port, so every client built from it probes 7842 and
    /// would find this socket dead.
    QuicPortUndiscoverable {
        /// The address `--quic-addr` named.
        addr: SocketAddr,
    },
    /// `--print-service-unit` without the material the unit must name.
    UnitNeedsTls,
    /// A unit naming port `0`: every restart would bind a different port, and
    /// no shareable URL can name any of them.
    UnitNeedsFixedPorts,
    /// `--hostname` is not a bare host: it leaves path, query, fragment or
    /// user-info residue behind, or the URL parser reads a different host or
    /// port than the operator typed.
    BadHostname {
        /// What `--hostname` named.
        hostname: String,
    },
    /// A value substituted into the unit carries whitespace, a control
    /// character or a `%`: systemd would split it across arguments or expand it
    /// as a specifier.
    UnsafeUnitValue {
        /// What the value substitutes (`--cert`, `--key`, the executable…).
        field: &'static str,
        /// The value as typed.
        value: String,
    },
}

impl ServeRefusal {
    /// The operator-facing reason, naming the flags involved.
    #[must_use]
    pub fn statement(&self) -> String {
        match self {
            Self::SameTcpAddress { addr } => format!(
                "--http-addr and --https-addr both name {addr}. They must differ: \
                 the captive-portal probe runs in plain text on one and the relay \
                 listener runs on the other."
            ),
            Self::OverlappingTcpBinds { http, https } => format!(
                "--http-addr {http} and --https-addr {https} cover each other: an \
                 unspecified address owns every interface, so both listeners cannot \
                 share the port. Name two different ports, or two addresses that do \
                 not cover each other."
            ),
            Self::UnparsableAddress { flag, value } => {
                format!("{flag} {value} is not an address; write it as host:port.")
            }
            Self::DevWithTls => "--dev serves plain HTTP, so it cannot be combined with \
                                 --cert, --key, --https-addr or --quic-addr."
                .to_owned(),
            Self::TlsFlagsWithoutCert => {
                "--https-addr and --quic-addr name the TLS listener, so --cert and \
                 --key must come with them. Without a certificate there is no TLS \
                 listener: pass the pair, or drop these flags."
                    .to_owned()
            }
            Self::IncompleteCertPair => {
                "--cert and --key go together; pass both or neither.".to_owned()
            }
            Self::QuicPortUndiscoverable { addr } => format!(
                "--quic-addr {addr}: address discovery must stay on UDP port 7842. \
                 A relay URL cannot carry a discovery port, so every client probes \
                 7842 and would find another port dead."
            ),
            Self::UnitNeedsTls => "--print-service-unit needs --cert, --key and --hostname: \
                                   a unit that serves plain HTTP is not one to install."
                .to_owned(),
            Self::UnitNeedsFixedPorts => {
                "a unit file must name fixed ports: --http-addr, --https-addr and \
                 --quic-addr cannot be 0 here, because every restart would bind a \
                 different one and no URL could name it."
                    .to_owned()
            }
            Self::BadHostname { hostname } => format!(
                "--hostname {hostname} does not form a relay URL with this port: \
                 write the bare host name, with no path, query, fragment or user \
                 info in it."
            ),
            Self::UnsafeUnitValue { field, value } => format!(
                "the {field} value {value} carries whitespace, a control character \
                 or a %; systemd would split or rewrite it inside the unit. Use a \
                 value without those."
            ),
        }
    }
}

impl std::fmt::Display for ServeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.statement())
    }
}

impl std::error::Error for ServeRefusal {}

/// The TLS half of a plan: the relay listener, the discovery socket, the PEMs.
///
/// ⚑ Address discovery is served whenever TLS is — it inherits the same
/// `rustls::ServerConfig`, and without one it cannot spawn at all. So there is
/// no TLS-on/discovery-off shape to represent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsPlan {
    /// The TCP address the relay listener binds.
    pub https_addr: SocketAddr,
    /// The UDP address address-discovery binds.
    pub quic_addr: SocketAddr,
    /// PEM certificate chain.
    pub cert: PathBuf,
    /// PEM key file.
    pub key: PathBuf,
}

/// Everything `relay serve` resolved from its flags, before a socket is bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServePlan {
    /// The plain-text TCP bind: the captive-portal probe with TLS on, the relay
    /// itself with TLS off.
    pub http_addr: SocketAddr,
    /// `None` on the `--dev` path.
    pub tls: Option<TlsPlan>,
    /// The name the operator chose to share, if any.
    pub hostname: Option<String>,
}

impl ServePlan {
    /// Validate the flags and fill in the defaults.
    ///
    /// This is where the equality refusal lives, ⛔ not after `Server::spawn`.
    pub fn resolve(args: &ServeArgs) -> Result<Self, ServeRefusal> {
        let parse = |flag: &'static str, value: &str| -> Result<SocketAddr, ServeRefusal> {
            value
                .parse::<SocketAddr>()
                .map_err(|_| ServeRefusal::UnparsableAddress {
                    flag,
                    value: value.to_owned(),
                })
        };

        if args.cert.is_some() != args.key.is_some() {
            return Err(ServeRefusal::IncompleteCertPair);
        }
        if args.dev
            && (args.cert.is_some() || args.https_addr.is_some() || args.quic_addr.is_some())
        {
            return Err(ServeRefusal::DevWithTls);
        }
        // ⚠ `--https-addr` / `--quic-addr` name the TLS listener. Without the
        // certificate pair there is no listener to put them on, and the
        // alternative — silently dropping them and serving plain HTTP on
        // 0.0.0.0:80 — inverts the posture the operator asked for. Refuse.
        if !args.dev
            && args.cert.is_none()
            && (args.https_addr.is_some() || args.quic_addr.is_some())
        {
            return Err(ServeRefusal::TlsFlagsWithoutCert);
        }

        let http_default = if args.dev {
            DEV_HTTP_ADDR
        } else {
            DEFAULT_HTTP_ADDR
        };
        let http_addr = parse(
            "--http-addr",
            args.http_addr.as_deref().unwrap_or(http_default),
        )?;

        let tls = match (args.dev, args.cert.as_ref(), args.key.as_ref()) {
            (false, Some(cert), Some(key)) => {
                let https_addr = parse(
                    "--https-addr",
                    args.https_addr.as_deref().unwrap_or(DEFAULT_HTTPS_ADDR),
                )?;
                // ⚠ Equal port 0 is NOT a collision: the OS hands out two
                // distinct ports and the server starts fine. Only an equal,
                // fixed pair is unbuildable.
                if https_addr == http_addr && http_addr.port() != 0 {
                    return Err(ServeRefusal::SameTcpAddress { addr: http_addr });
                }
                // ⚠ Equality is not the only collision: `0.0.0.0:443` and
                // `127.0.0.1:443` differ as socket addresses yet the wildcard
                // owns the specific bind too, and the second listener would
                // die inside `Server::spawn` with iroh's bare `AddrInUse` —
                // the port the operator did not type. Any same non-zero port
                // with an unspecified address on either side is unbuildable.
                if http_addr.port() == https_addr.port()
                    && http_addr.port() != 0
                    && (http_addr.ip().is_unspecified() || https_addr.ip().is_unspecified())
                {
                    return Err(ServeRefusal::OverlappingTcpBinds {
                        http: http_addr,
                        https: https_addr,
                    });
                }
                let quic_addr = parse(
                    "--quic-addr",
                    args.quic_addr.as_deref().unwrap_or(DEFAULT_QUIC_ADDR),
                )?;
                // ⚠ The relay URL cannot carry a discovery port: every client
                // built from it probes UDP 7842 (A8b), so a non-7842 port is a
                // socket no peer will ever find — silently degraded
                // hole-punching. Port 0 stays legal for hermetic runs, where
                // the assigned port is read back and nothing is shared.
                if quic_addr.port() != 0 && quic_addr.port() != DISCOVERY_PORT {
                    return Err(ServeRefusal::QuicPortUndiscoverable { addr: quic_addr });
                }

                Some(TlsPlan {
                    https_addr,
                    quic_addr,
                    cert: cert.clone(),
                    key: key.clone(),
                })
            }
            _ => None,
        };

        Ok(Self {
            http_addr,
            tls,
            hostname: args.hostname.clone(),
        })
    }

    pub fn shareable_url(&self) -> Result<String, NoUrl> {
        let tls = self.tls.as_ref().ok_or(NoUrl::NoTls)?;
        let hostname = self.hostname.as_deref().ok_or(NoUrl::NoHostname)?;
        host_url(hostname, tls.https_addr.port()).ok_or(NoUrl::Refused)
    }
}

/// The canonical URL for `hostname` on `port`, or `None` when the text is not a
/// bare host.
///
/// ⚠ The URL parser is lenient in exactly the way a hand-typed `--hostname` is
/// not: `relay.example?x` becomes a host with a *query*, the `:port` after it
/// is swallowed into that query, and the URL names port 443 while the relay
/// listens elsewhere. So the candidate must survive [`canonical_relay_url`]
/// **and** re-parse to exactly the host asked for and the port the listener
/// binds, with no path, query or fragment residue.
fn host_url(hostname: &str, port: u16) -> Option<String> {
    let canonical = canonical_relay_url(&format!("https://{hostname}:{port}"))?;
    let parsed = url::Url::parse(&canonical).ok()?;
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return None;
    }
    if !hostname.eq_ignore_ascii_case(parsed.host_str()?) {
        return None;
    }
    match parsed.port() {
        Some(named) if named == port => {}
        // The canonical form drops an explicit :443, and only that.
        None if port == 443 => {}
        _ => return None,
    }
    Some(canonical)
}

/// Why `relay serve` printed no shareable URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoUrl {
    /// No TLS on this run, so no `https` address exists.
    NoTls,
    /// No `--hostname`, and the crate cannot supply one.
    NoHostname,
    /// The name and port do not form a URL a rustain client would keep.
    Refused,
}

impl NoUrl {
    /// The one line printed in place of a URL.
    #[must_use]
    pub fn statement(self) -> &'static str {
        match self {
            Self::NoTls => "no URL: TLS is off, so no https address exists",
            Self::NoHostname => "no URL: pass --hostname to name what to share",
            Self::Refused => "no URL: this --hostname and port form none",
        }
    }
}

/// The block printed once the relay is up: what is bound, and what to share.
#[must_use]
pub fn ready_lines(plan: &ServePlan) -> Vec<String> {
    let socket = |label: &str, addr: SocketAddr, note: &str| {
        format!("  {label:<6} {:<21} {note}", addr.to_string())
    };
    let mut lines = vec!["relay listening".to_owned()];
    match plan.tls.as_ref() {
        Some(tls) => {
            lines.push(socket("http", plan.http_addr, "captive-portal probe"));
            lines.push(socket("https", tls.https_addr, "relay traffic"));
            lines.push(socket("quic", tls.quic_addr, "address discovery"));
        }
        None => lines.push(socket("http", plan.http_addr, "relay traffic")),
    }
    match plan.shareable_url() {
        Ok(url) => lines.push(format!("  {:<6} {url}", "url")),
        Err(reason) => lines.push(format!("  {reason}", reason = reason.statement())),
    }
    lines.push("stop with Ctrl-C".to_owned());
    lines.push("unit file: relay serve --print-service-unit".to_owned());
    lines
}

/// What an operator has to know about a relay this host runs.
///
/// ⚑ Three statements, each naming a mechanism rather than a promise:
///
/// 1. **Membership.** The relay set is configuration on the *dialing* side. A
///    peer that has not put this URL in its own `.rustain/relay.json` is dropped
///    before dial, so handing out the URL is necessary and not sufficient.
///    Without this line an operator stands up a relay and watches nothing
///    connect.
/// 2. **Reach.** `RelayConfig::access` defaults to `AllowAll`, and this cut
///    ships that default, so bandwidth is available to whoever has the URL.
///    Silence here would let an operator assume the relay is restricted to their
///    own peers, which it is not.
/// 3. **Durability.** The relay is a forward-only conduit with no store and no
///    queue, so a restart ends the connections through it and there is nothing
///    to recall or replay.
#[must_use]
pub fn disclosure_lines() -> Vec<String> {
    vec![
        "peers reach this relay only after adding its URL to".to_owned(),
        "their own .rustain/relay.json; one they have not".to_owned(),
        "configured is dropped before dial.".to_owned(),
        "this relay carries traffic for anyone holding its URL.".to_owned(),
        "traffic passes through this host while it runs; a".to_owned(),
        "restart ends every connection through it, and nothing".to_owned(),
        "relayed is kept, queued or recoverable.".to_owned(),
    ]
}

/// Render the systemd unit for this plan.
///
/// The renderer's production caller is `relay serve --print-service-unit`
/// ([`run_serve`]): a template plus an `include_str!` with no non-test caller is
/// a mechanism without a producer, and the placeholders make the checked-in file
/// unusable on its own.
pub fn render_service_unit(exe: &str, plan: &ServePlan) -> Result<String, ServeRefusal> {
    let (Some(tls), Some(hostname)) = (plan.tls.as_ref(), plan.hostname.as_deref()) else {
        return Err(ServeRefusal::UnitNeedsTls);
    };
    // A unit names its ports for every restart to come: port 0 means a
    // different listener each time and no URL that names one.
    if plan.http_addr.port() == 0 || tls.https_addr.port() == 0 || tls.quic_addr.port() == 0 {
        return Err(ServeRefusal::UnitNeedsFixedPorts);
    }
    // The hostname lands in `Description=` and `ExecStart=`, so it must be the
    // bare host a URL would keep — the same judge the shareable URL uses.
    if host_url(hostname, tls.https_addr.port()).is_none() {
        return Err(ServeRefusal::BadHostname {
            hostname: hostname.to_owned(),
        });
    }
    // systemd tokenizes `ExecStart=` on whitespace and expands `%` specifiers,
    // so a value carrying either is not the value this unit meant to name.
    for (field, value) in [
        ("the executable", exe),
        ("--cert", &tls.cert.display().to_string()),
        ("--key", &tls.key.display().to_string()),
    ] {
        if !systemd_safe(value) {
            return Err(ServeRefusal::UnsafeUnitValue {
                field,
                value: value.to_owned(),
            });
        }
    }
    Ok(render_template(
        SERVICE_TEMPLATE,
        &[
            ("exe", exe),
            ("http_addr", &plan.http_addr.to_string()),
            ("https_addr", &tls.https_addr.to_string()),
            ("quic_addr", &tls.quic_addr.to_string()),
            ("hostname", hostname),
            ("cert", &tls.cert.display().to_string()),
            ("key", &tls.key.display().to_string()),
        ],
    ))
}

/// Whether a value survives systemd's tokenization and specifier expansion
/// unharmed: no whitespace, no control characters, no `%`.
fn systemd_safe(value: &str) -> bool {
    value
        .chars()
        .all(|c| !c.is_whitespace() && !c.is_control() && c != '%')
}

/// Refuse to serve when this build has no relay server in it.
///
/// ⚑ Only the *serving* half is gated. `--print-service-unit` is text and runs
/// in every build, exactly as it runs on every platform: an operator may
/// legitimately render a unit for a host other than the one they are typing on.
fn ensure_relay_server_feature_enabled() -> anyhow::Result<()> {
    #[cfg(feature = "relay-server")]
    {
        Ok(())
    }
    #[cfg(not(feature = "relay-server"))]
    {
        anyhow::bail!(
            "this build has the `relay-server` feature disabled, so it can print a unit \
             file but cannot run a relay; rebuild with \
             `cargo build --release --features relay-server`"
        )
    }
}

/// The note `--print-service-unit` adds when **this** build cannot serve.
///
/// The unit's `ExecStart` names the executable that rendered it, so on a
/// feature-less build it names a binary that refuses to serve the moment
/// systemd starts it — a restart loop against `StartLimitBurst`. `None` in a
/// build that can serve.
#[must_use]
pub fn featureless_unit_note() -> Option<&'static str> {
    #[cfg(feature = "relay-server")]
    {
        None
    }
    #[cfg(not(feature = "relay-server"))]
    {
        Some(
            "note: this build cannot run a relay; the unit above names this \
             executable, so put a relay-server build at that path before systemd \
             starts it",
        )
    }
}

/// The path `rustain relay serve` takes. Returns the process exit code.
pub async fn run_serve(args: &ServeArgs) -> anyhow::Result<i32> {
    let plan = ServePlan::resolve(args)?;

    if args.print_service_unit {
        let exe = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "rustain".to_owned());
        // ⛔ Prints and exits: no socket is bound on this path.
        println!("{}", render_service_unit(&exe, &plan)?);
        if let Some(note) = featureless_unit_note() {
            eprintln!("{note}");
        }
        return Ok(0);
    }

    ensure_relay_server_feature_enabled()?;

    #[cfg(feature = "relay-server")]
    {
        let server = crate::adapters::relay_server::spawn_relay(&plan).await?;
        // ⚠ Every address is read BEFORE the serve loop: `Server::shutdown`
        // consumes `self`, so `http_addr()` is uncallable afterwards.
        let observed = observed_plan(&plan, &server);
        for line in ready_lines(&observed) {
            println!("{line}");
        }
        for line in disclosure_lines() {
            println!("{line}");
        }
        let exit = crate::adapters::relay_server::serve(server, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
        Ok(exit.code())
    }
    #[cfg(not(feature = "relay-server"))]
    {
        unreachable!("the feature check above returned Err")
    }
}

/// The plan re-read from the sockets the relay actually bound.
///
/// A `0` port in the plan is an instruction, not a fact; what gets printed has
/// to be what an operator can dial.
#[cfg(feature = "relay-server")]
fn observed_plan(plan: &ServePlan, server: &iroh_relay::server::Server) -> ServePlan {
    let mut observed = plan.clone();
    if let Some(addr) = server.http_addr() {
        observed.http_addr = addr;
    }
    if let (Some(tls), Some(https), Some(quic)) = (
        observed.tls.as_mut(),
        server.https_addr(),
        server.quic_addr(),
    ) {
        tls.https_addr = https;
        tls.quic_addr = quic;
    }
    observed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tls_args() -> ServeArgs {
        ServeArgs {
            cert: Some(PathBuf::from("/etc/rustain/relay.crt")),
            key: Some(PathBuf::from("/etc/rustain/relay.key")),
            hostname: Some("relay.example".to_owned()),
            ..ServeArgs::default()
        }
    }

    #[test]
    fn the_defaults_are_three_addresses_two_tcp_and_one_udp() {
        let plan = ServePlan::resolve(&tls_args()).expect("plan");
        assert_eq!(plan.http_addr.to_string(), DEFAULT_HTTP_ADDR);
        let tls = plan.tls.expect("tls");
        assert_eq!(tls.https_addr.to_string(), DEFAULT_HTTPS_ADDR);
        assert_eq!(
            tls.quic_addr.port(),
            7842,
            "the shipped client probes this port for address discovery"
        );
    }

    #[test]
    fn an_equal_fixed_tcp_pair_is_refused_before_anything_binds() {
        let mut args = tls_args();
        args.http_addr = Some("0.0.0.0:443".to_owned());
        let refusal = ServePlan::resolve(&args).expect_err("equal addresses must refuse");
        assert_eq!(
            refusal,
            ServeRefusal::SameTcpAddress {
                addr: "0.0.0.0:443".parse().unwrap()
            }
        );
        let statement = refusal.statement();
        assert!(statement.contains("--http-addr"), "{statement}");
        assert!(statement.contains("--https-addr"), "{statement}");
    }

    #[test]
    fn an_equal_pair_on_port_zero_is_accepted_because_the_os_hands_out_two() {
        let mut args = tls_args();
        args.http_addr = Some("127.0.0.1:0".to_owned());
        args.https_addr = Some("127.0.0.1:0".to_owned());
        let plan = ServePlan::resolve(&args).expect("port 0 twice is not a collision");
        assert_eq!(plan.tls.expect("tls").https_addr.port(), 0);
    }

    #[test]
    fn dev_binds_loopback_and_carries_no_tls() {
        let args = ServeArgs {
            dev: true,
            ..ServeArgs::default()
        };
        let plan = ServePlan::resolve(&args).expect("plan");
        assert_eq!(plan.http_addr.to_string(), DEV_HTTP_ADDR);
        assert!(plan.tls.is_none());
        assert!(plan.http_addr.ip().is_loopback(), "⛔ not [::]:3340");
    }

    #[test]
    fn dev_with_a_tls_flag_is_refused() {
        for args in [
            ServeArgs {
                dev: true,
                cert: Some(PathBuf::from("c")),
                key: Some(PathBuf::from("k")),
                ..ServeArgs::default()
            },
            ServeArgs {
                dev: true,
                https_addr: Some("0.0.0.0:443".to_owned()),
                ..ServeArgs::default()
            },
            ServeArgs {
                dev: true,
                quic_addr: Some("0.0.0.0:7842".to_owned()),
                ..ServeArgs::default()
            },
        ] {
            assert_eq!(
                ServePlan::resolve(&args).expect_err("mutually exclusive"),
                ServeRefusal::DevWithTls
            );
        }
    }

    /// The sharpest trap this surface had: `--https-addr` without the
    /// certificate pair used to fall through to `tls: None` and serve a
    /// plaintext relay on 0.0.0.0:80 — the opposite of what was asked.
    #[test]
    fn tls_flags_without_a_cert_pair_are_refused_not_dropped() {
        for args in [
            ServeArgs {
                https_addr: Some("0.0.0.0:443".to_owned()),
                ..ServeArgs::default()
            },
            ServeArgs {
                quic_addr: Some("0.0.0.0:7842".to_owned()),
                ..ServeArgs::default()
            },
        ] {
            assert_eq!(
                ServePlan::resolve(&args).expect_err("orphaned TLS flags"),
                ServeRefusal::TlsFlagsWithoutCert
            );
        }
    }

    #[test]
    fn half_a_cert_pair_is_refused() {
        let args = ServeArgs {
            cert: Some(PathBuf::from("c")),
            ..ServeArgs::default()
        };
        assert_eq!(
            ServePlan::resolve(&args).expect_err("half a pair"),
            ServeRefusal::IncompleteCertPair
        );
    }

    /// `0.0.0.0:P` owns `127.0.0.1:P`: not equal as socket addresses, still the
    /// same listener — and without this refusal the failure would surface as
    /// iroh's bare `AddrInUse` from the port the operator did not type.
    #[test]
    fn a_wildcard_and_a_specific_address_on_one_port_are_refused() {
        for (http, https) in [
            ("0.0.0.0:443", "127.0.0.1:443"),
            ("127.0.0.1:443", "0.0.0.0:443"),
            ("[::]:8080", "0.0.0.0:8080"),
        ] {
            let mut args = tls_args();
            args.http_addr = Some(http.to_owned());
            args.https_addr = Some(https.to_owned());
            assert_eq!(
                ServePlan::resolve(&args).expect_err("overlap must refuse"),
                ServeRefusal::OverlappingTcpBinds {
                    http: http.parse().unwrap(),
                    https: https.parse().unwrap(),
                }
            );
        }
        // The same pair on distinct ports, or on port 0, stays buildable.
        let mut args = tls_args();
        args.http_addr = Some("0.0.0.0:0".to_owned());
        args.https_addr = Some("127.0.0.1:0".to_owned());
        assert!(ServePlan::resolve(&args).is_ok());
    }

    /// A relay URL cannot carry a discovery port, so a non-7842 `--quic-addr`
    /// is a socket every URL-configured client will probe past.
    #[test]
    fn a_quic_port_other_than_7842_is_refused_and_7842_and_zero_are_kept() {
        let mut args = tls_args();
        args.quic_addr = Some("127.0.0.1:9999".to_owned());
        assert_eq!(
            ServePlan::resolve(&args).expect_err("an undiscoverable port"),
            ServeRefusal::QuicPortUndiscoverable {
                addr: "127.0.0.1:9999".parse().unwrap()
            }
        );

        let mut args = tls_args();
        args.quic_addr = Some("127.0.0.1:7842".to_owned());
        let plan = ServePlan::resolve(&args).expect("7842 on another interface is fine");
        assert_eq!(plan.tls.expect("tls").quic_addr.port(), DISCOVERY_PORT);

        // Port 0 is the hermetic shape: the assigned port is read back and
        // nothing is shared from this run.
        let mut args = tls_args();
        args.quic_addr = Some("127.0.0.1:0".to_owned());
        assert!(ServePlan::resolve(&args).is_ok());
    }

    /// The URL parser is lenient in exactly the way a hand-typed hostname is
    /// not: a `?` swallows the `:port` into a query and the URL names 443.
    /// The printed value must name the host asked for on the port bound.
    #[test]
    fn a_hostname_that_leaves_residue_behind_prints_no_url() {
        for hostname in [
            "relay.example?x",
            "relay.example/other",
            "relay.example#frag",
            "operator:hunter2@relay.example",
            "rel ay.example",
        ] {
            let mut args = tls_args();
            args.hostname = Some(hostname.to_owned());
            let plan = ServePlan::resolve(&args).expect("resolve does not judge the name");
            assert_eq!(
                plan.shareable_url(),
                Err(NoUrl::Refused),
                "{hostname} must not become a URL"
            );
        }
        // An IPv6 literal in brackets is a bare host and stays shareable.
        let mut args = tls_args();
        args.hostname = Some("[::1]".to_owned());
        let plan = ServePlan::resolve(&args).expect("plan");
        let url = plan.shareable_url().expect("brackets name a host");
        assert!(url.starts_with("https://[::1]"), "{url}");
    }

    #[test]
    fn the_printed_url_is_canonical_and_a_client_would_keep_it() {
        let plan = ServePlan::resolve(&tls_args()).expect("plan");
        let url = plan.shareable_url().expect("a URL");
        assert_eq!(
            url, "https://relay.example/",
            "the canonical form carries the trailing slash a RelaySet compares"
        );
        // Idempotence: feeding the printed value back to the judge returns it.
        assert_eq!(canonical_relay_url(&url), Some(url.clone()));
    }

    #[test]
    fn a_non_default_port_survives_into_the_printed_url() {
        let mut args = tls_args();
        args.https_addr = Some("0.0.0.0:8443".to_owned());
        let plan = ServePlan::resolve(&args).expect("plan");
        assert_eq!(
            plan.shareable_url().expect("a URL"),
            "https://relay.example:8443/"
        );
    }

    #[test]
    fn every_arm_that_cannot_print_a_url_says_why() {
        // No TLS.
        let dev = ServePlan::resolve(&ServeArgs {
            dev: true,
            hostname: Some("relay.example".to_owned()),
            ..ServeArgs::default()
        })
        .expect("plan");
        assert_eq!(dev.shareable_url(), Err(NoUrl::NoTls));

        // No hostname.
        let mut args = tls_args();
        args.hostname = None;
        let plan = ServePlan::resolve(&args).expect("plan");
        assert_eq!(plan.shareable_url(), Err(NoUrl::NoHostname));

        // Port 0 names no listener, so it is refused at the judge.
        let mut args = tls_args();
        args.https_addr = Some("0.0.0.0:0".to_owned());
        let plan = ServePlan::resolve(&args).expect("plan");
        assert_eq!(plan.shareable_url(), Err(NoUrl::Refused));

        // A credential-bearing hostname is refused too.
        let mut args = tls_args();
        args.hostname = Some("operator:hunter2@relay.example".to_owned());
        let plan = ServePlan::resolve(&args).expect("plan");
        assert_eq!(plan.shareable_url(), Err(NoUrl::Refused));

        for reason in [NoUrl::NoTls, NoUrl::NoHostname, NoUrl::Refused] {
            assert!(
                reason.statement().starts_with("no URL:"),
                "{}",
                reason.statement()
            );
        }
    }

    #[test]
    fn the_rendered_unit_pins_every_load_bearing_token() {
        let plan = ServePlan::resolve(&tls_args()).expect("plan");
        let unit = render_service_unit("/usr/local/bin/rustain", &plan).expect("unit");

        assert!(unit.contains("Type=simple"), "{unit}");
        assert!(unit.contains("Restart=on-failure"), "{unit}");
        assert!(unit.contains("RestartSec="), "{unit}");
        assert!(unit.contains("StartLimitIntervalSec="), "{unit}");
        assert!(unit.contains("StartLimitBurst="), "{unit}");
        // The relay runs unprivileged: the dynamic user needs nothing but the
        // bind capability, and the PEM pair arrives through the credential
        // store rather than a world-readable path.
        assert!(unit.contains("DynamicUser=yes"), "{unit}");
        assert!(
            unit.contains("LoadCredential=relay.crt:/etc/rustain/relay.crt"),
            "{unit}"
        );
        assert!(
            unit.contains("LoadCredential=relay.key:/etc/rustain/relay.key"),
            "{unit}"
        );
        // `systemctl stop` runs the graceful path, which listens for SIGINT.
        assert!(unit.contains("KillSignal=SIGINT"), "{unit}");

        // All THREE addresses: a one-address unit cannot start with TLS on.
        let exec = unit
            .lines()
            .find(|line| line.starts_with("ExecStart="))
            .expect("an ExecStart line");
        assert!(exec.contains("--http-addr 0.0.0.0:80"), "{exec}");
        assert!(exec.contains("--https-addr 0.0.0.0:443"), "{exec}");
        assert!(exec.contains("--quic-addr 0.0.0.0:7842"), "{exec}");
        assert!(exec.contains("--hostname relay.example"), "{exec}");
        assert!(
            exec.contains("/usr/local/bin/rustain relay serve"),
            "{exec}"
        );
        // The certificates the process reads are the ones systemd loaded.
        assert!(exec.contains("--cert %d/relay.crt"), "{exec}");
        assert!(exec.contains("--key %d/relay.key"), "{exec}");
        // ⛔ THE NEGATIVE TWIN, over the WHOLE file including comments: a
        // service unit that silently serves plaintext on a host the operator
        // believes is running TLS is the failure this assertion exists to
        // prevent. The template's rationale says "the dev flag" precisely so
        // this can read every line.
        assert!(
            !unit.contains("--dev"),
            "the shipped unit must never carry --dev, not even in a comment: {unit}"
        );
    }

    /// Port 0 in a unit is a different listener on every restart, and no URL
    /// can name any of them.
    #[test]
    fn a_unit_naming_port_zero_is_refused() {
        let mut args = tls_args();
        args.http_addr = Some("0.0.0.0:0".to_owned());
        let plan = ServePlan::resolve(&args).expect("resolve allows assigned ports");
        assert_eq!(
            render_service_unit("rustain", &plan).expect_err("no ephemeral unit"),
            ServeRefusal::UnitNeedsFixedPorts
        );

        let mut args = tls_args();
        args.https_addr = Some("0.0.0.0:0".to_owned());
        let plan = ServePlan::resolve(&args).expect("plan");
        assert_eq!(
            render_service_unit("rustain", &plan).expect_err("no ephemeral unit"),
            ServeRefusal::UnitNeedsFixedPorts
        );

        let mut args = tls_args();
        args.quic_addr = Some("0.0.0.0:0".to_owned());
        let plan = ServePlan::resolve(&args).expect("plan");
        assert_eq!(
            render_service_unit("rustain", &plan).expect_err("no ephemeral unit"),
            ServeRefusal::UnitNeedsFixedPorts
        );
    }

    /// systemd tokenizes `ExecStart=` on whitespace and expands `%` specifiers:
    /// a value carrying either is not the value the unit meant to name.
    #[test]
    fn a_value_systemd_would_mangle_is_refused() {
        let plan = ServePlan::resolve(&tls_args()).expect("plan");
        for (exe, field) in [
            ("/opt/rustain two/rustain", "the executable"),
            ("/opt/rustain%{h}/rustain", "the executable"),
        ] {
            assert_eq!(
                render_service_unit(exe, &plan).expect_err("mangled by systemd"),
                ServeRefusal::UnsafeUnitValue {
                    field,
                    value: exe.to_owned(),
                }
            );
        }

        let mut args = tls_args();
        args.cert = Some(PathBuf::from("/etc/my certs/relay.crt"));
        args.key = Some(PathBuf::from("/etc/my certs/relay.key"));
        let plan = ServePlan::resolve(&args).expect("plan");
        assert_eq!(
            render_service_unit("rustain", &plan).expect_err("mangled by systemd"),
            ServeRefusal::UnsafeUnitValue {
                field: "--cert",
                value: "/etc/my certs/relay.crt".to_owned(),
            }
        );
    }

    /// A hostname with residue is refused by the unit renderer too — it lands
    /// in `Description=` and `ExecStart=`, not just in a URL.
    #[test]
    fn a_unit_for_a_residue_bearing_hostname_is_refused() {
        let mut args = tls_args();
        args.hostname = Some("relay.example?x".to_owned());
        let plan = ServePlan::resolve(&args).expect("plan");
        assert_eq!(
            render_service_unit("rustain", &plan).expect_err("not a bare host"),
            ServeRefusal::BadHostname {
                hostname: "relay.example?x".to_owned()
            }
        );
    }

    #[test]
    fn a_unit_cannot_be_rendered_for_a_plan_with_no_tls() {
        let dev = ServePlan::resolve(&ServeArgs {
            dev: true,
            ..ServeArgs::default()
        })
        .expect("plan");
        assert_eq!(
            render_service_unit("rustain", &dev).expect_err("no plaintext unit"),
            ServeRefusal::UnitNeedsTls
        );
    }

    #[test]
    fn a_substituted_value_carrying_a_placeholder_is_not_re_expanded() {
        // The positive control for the single-pass engine, driven through the
        // production renderer: `str::replace` would expand `{quic_addr}` inside
        // the substituted value. The hostname is judged as a bare host now, so
        // the control lives on a path — where braces are legal filesystem text.
        let mut args = tls_args();
        args.cert = Some(PathBuf::from("/etc/rustain/{quic_addr}/relay.crt"));
        args.key = Some(PathBuf::from("/etc/rustain/relay.key"));
        let plan = ServePlan::resolve(&args).expect("plan");
        let unit = render_service_unit("rustain", &plan).expect("unit");
        assert!(
            unit.contains("LoadCredential=relay.crt:/etc/rustain/{quic_addr}/relay.crt"),
            "{unit}"
        );
        assert!(
            !unit.contains("LoadCredential=relay.crt:/etc/rustain/0.0.0.0:7842/relay.crt"),
            "re-expansion is the template-injection regression: {unit}"
        );
    }

    /// AC3's negative arms, asserted where the operator reads them: not just
    /// that `shareable_url` errors, but that the ready block prints the reason
    /// and no URL.
    #[test]
    fn every_no_url_arm_renders_its_reason_and_no_url() {
        let arms: Vec<(ServeArgs, NoUrl)> = vec![
            (
                ServeArgs {
                    dev: true,
                    hostname: Some("relay.example".to_owned()),
                    ..ServeArgs::default()
                },
                NoUrl::NoTls,
            ),
            {
                let mut args = tls_args();
                args.hostname = None;
                (args, NoUrl::NoHostname)
            },
            {
                let mut args = tls_args();
                args.https_addr = Some("0.0.0.0:0".to_owned());
                (args, NoUrl::Refused)
            },
            {
                let mut args = tls_args();
                args.hostname = Some("operator:hunter2@relay.example".to_owned());
                (args, NoUrl::Refused)
            },
        ];
        for (args, reason) in arms {
            let plan = ServePlan::resolve(&args).expect("plan");
            let lines = ready_lines(&plan);
            let block = lines.join("\n");
            assert!(
                block.contains(reason.statement()),
                "the {reason:?} arm must say why: {block}"
            );
            assert!(
                !lines
                    .iter()
                    .any(|line| line.trim_start().starts_with("url ")),
                "no URL may be printed on the {reason:?} arm: {block}"
            );
        }
    }

    /// AC5's named mutant: `--print-service-unit` must exit cleanly **without
    /// binding anything**. The plan names two occupied TCP ports, so a path
    /// that spawned first would fail where this one succeeds. (This test runs
    /// in the default build, where the feature check is unreachable on the
    /// print path — both halves of that claim at once.)
    #[tokio::test]
    async fn the_print_service_unit_path_binds_nothing() {
        let squatter_http = std::net::TcpListener::bind("127.0.0.1:0").expect("a taken port");
        let squatter_https = std::net::TcpListener::bind("127.0.0.1:0").expect("another");
        let args = ServeArgs {
            http_addr: Some(squatter_http.local_addr().unwrap().to_string()),
            https_addr: Some(squatter_https.local_addr().unwrap().to_string()),
            // Discovery stays on its protocol port; the occupied TCP pair is
            // what discriminates, and no UDP socket is bound on this path.
            quic_addr: Some("127.0.0.1:7842".to_owned()),
            hostname: Some("relay.example".to_owned()),
            cert: Some(PathBuf::from("/etc/rustain/relay.crt")),
            key: Some(PathBuf::from("/etc/rustain/relay.key")),
            print_service_unit: true,
            ..ServeArgs::default()
        };
        let code = run_serve(&args)
            .await
            .expect("the print path exits cleanly over occupied ports");
        assert_eq!(code, 0);
    }

    #[test]
    fn the_ready_block_names_every_bound_socket_and_the_url() {
        let plan = ServePlan::resolve(&tls_args()).expect("plan");
        let lines = ready_lines(&plan);
        assert_eq!(
            lines.len(),
            7,
            "positive control: the measured ready block is 7 rows — {lines:#?}"
        );
        let block = lines.join("\n");
        for expected in [
            "0.0.0.0:80",
            "0.0.0.0:443",
            "0.0.0.0:7842",
            "https://relay.example/",
            "Ctrl-C",
            "--print-service-unit",
        ] {
            assert!(block.contains(expected), "{expected} missing from {block}");
        }
    }

    #[test]
    fn a_dev_run_shows_one_socket_and_no_url() {
        let plan = ServePlan::resolve(&ServeArgs {
            dev: true,
            ..ServeArgs::default()
        })
        .expect("plan");
        let lines = ready_lines(&plan);
        assert_eq!(lines.len(), 5, "{lines:#?}");
        let block = lines.join("\n");
        assert!(block.contains("127.0.0.1:3340"), "{block}");
        assert!(!block.contains("https://"), "{block}");
        assert!(block.contains(NoUrl::NoTls.statement()), "{block}");
    }

    #[test]
    fn both_printed_blocks_fit_the_narrowest_supported_terminal() {
        // The 60x16 floor. Both blocks together are 12 rows, so they fit one
        // screen with room to spare; each row has to fit the width.
        let plan = ServePlan::resolve(&tls_args()).expect("plan");
        let rows: Vec<String> = ready_lines(&plan)
            .into_iter()
            .chain(disclosure_lines())
            .collect();
        assert_eq!(rows.len(), 14, "{rows:#?}");
        for row in &rows {
            assert!(
                row.chars().count() <= 60,
                "{} columns: {row}",
                row.chars().count()
            );
        }
    }

    #[test]
    fn the_disclosure_states_membership_reach_and_the_restart_effect() {
        let block = disclosure_lines().join(" ");
        // 1 — D13 membership: handing out the URL is necessary, not sufficient.
        assert!(block.contains(".rustain/relay.json"), "{block}");
        assert!(block.contains("dropped before dial"), "{block}");
        // 2 — the first cut ships AllowAll.
        assert!(
            block.contains("carries traffic for anyone holding its URL"),
            "{block}"
        );
        // 3 — a restart does not preserve relayed traffic, and nothing is
        // recallable.
        assert!(block.contains("restart ends every connection"), "{block}");
        assert!(block.contains("kept, queued or recoverable"), "{block}");
    }

    // ⚑ The RENDERED-output wording ceiling lives in
    // `tests/conformance_p2p_relay_server.rs`, ⛔ not here, and the reason is a
    // measured collision rather than a preference: a needle array naming
    // `enterprise` / `free tier` is itself a shipped-source occurrence of those
    // words, and `conformance_18_4b_peer_surface.rs::the_surface_names_no_tier`
    // scans this file over `code_only()` — which strips comments but NOT
    // `#[cfg(test)]`. Keeping the array here turned that ratchet RED on its own
    // prohibition. ⛔ The fix was to move the array, never to teach a live
    // ratchet to skip test modules: that would have weakened it for all 23 paths
    // it guards to accommodate one.

    #[test]
    fn an_unparsable_address_names_the_flag_that_carried_it() {
        let args = ServeArgs {
            dev: true,
            http_addr: Some("not-an-address".to_owned()),
            ..ServeArgs::default()
        };
        let refusal = ServePlan::resolve(&args).expect_err("must refuse");
        assert_eq!(
            refusal,
            ServeRefusal::UnparsableAddress {
                flag: "--http-addr",
                value: "not-an-address".to_owned()
            }
        );
    }

    #[test]
    fn the_template_is_the_checked_in_file_not_a_copy() {
        let checked_in = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/rustain-relay.service.template"),
        )
        .expect("the shipped template");
        assert_eq!(
            SERVICE_TEMPLATE, checked_in,
            "include_str! is what keeps the generator and the reference from drifting"
        );
    }
}
