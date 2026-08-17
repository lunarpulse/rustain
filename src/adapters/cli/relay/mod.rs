//! `relay` CLI subcommand family (Story 18.4c-b, FR159).
//!
//! Story 18.4c shipped the relay **client**: three modes (`disabled`,
//! `default`, `configured`), the URL judge, the disclosure. What it could not
//! ship was the other end — a relay of one's own was a config key naming
//! somebody else's server. `relay serve` is that end.
//!
//! # This is a role, ⛔ not a class of customer
//!
//! The three client modes are gated on **nothing**: the relay client works in
//! all three in every build, including a default one. `relay-server` gates only
//! whether *this* host can **also run a relay**, which is a separate capability
//! available to any operator who enables it. A class is something you are placed
//! in; a build is something you choose to make. ⛔ It would become a class the
//! moment any client *mode* required the feature — so no mode may.
//!
//! # One surface, one place for its copy
//!
//! Every operator string this family renders lives in [`serve`], which is
//! whole-file-scanned by the wording ceiling
//! (`conformance_p2p_ingress.rs::every_p2p_operator_string_stays_within_the_wording_ceiling`).
//! ⛔ The `startup.rs` dispatch arm carries **no** string literal: an arm is not
//! a function, so a bail message written there has no `fn <name>(` needle, could
//! not be added to the ceiling's hand-named list, and would be covered by
//! nothing. A ratchet asserts the arm stays literal-free.

use clap::Subcommand;

pub mod serve;

use crate::infrastructure::startup::SubcommandExit;

/// `relay` subcommand actions. Declared beside its handler, mirroring
/// `cli/peer/mod.rs` and `cli/team/mod.rs`.
#[derive(Subcommand, Debug, Clone)]
pub enum RelayAction {
    /// Run the relay this host offers, and print the URL peers can add.
    ///
    /// Binds three sockets: a plain-text TCP port for the captive-portal probe,
    /// a TLS TCP port carrying relay traffic, and a UDP port for address
    /// discovery. The URL printed is the one a rustain client will accept
    /// verbatim; peers must add it to their own `.rustain/relay.json` before
    /// anything reaches this host through it.
    Serve {
        /// Plain-text TCP bind. Default `0.0.0.0:80` — or `127.0.0.1:3340`
        /// under `--dev`. Carries the captive-portal probe when TLS is on, and
        /// the relay itself when it is off.
        #[arg(long)]
        http_addr: Option<String>,
        /// TLS TCP bind, the relay listener. Default `0.0.0.0:443`. Must differ
        /// from `--http-addr`: the probe runs in plain text and cannot share a
        /// port with the listener.
        #[arg(long, conflicts_with = "dev")]
        https_addr: Option<String>,
        /// UDP bind for address discovery. Default `0.0.0.0:7842`, the port
        /// every rustain client built from this relay's URL already probes.
        #[arg(long)]
        quic_addr: Option<String>,
        /// The name to share. Nothing can infer it: the relay server API holds
        /// no readable domain anywhere, so the URL is built from what you pass
        /// here. Without it, no URL is printed.
        #[arg(long)]
        hostname: Option<String>,
        /// PEM certificate chain for the relay listener.
        #[arg(long, conflicts_with = "dev", requires = "key")]
        cert: Option<std::path::PathBuf>,
        /// PEM key file for `--cert`.
        #[arg(long, conflicts_with = "dev", requires = "cert")]
        key: Option<std::path::PathBuf>,
        /// Plain HTTP on `127.0.0.1:3340`, no TLS and no address discovery, for
        /// local runs. Prints no shareable URL, because there is no `https`
        /// address to print.
        #[arg(long)]
        dev: bool,
        /// Render the systemd unit for these flags to stdout and exit. Binds
        /// nothing and writes no file.
        #[arg(long)]
        print_service_unit: bool,
    },
}

/// Dispatch a `relay` verb. Errors are reported here, inside the module the
/// wording ceiling covers.
pub async fn run_cli(action: &RelayAction) -> anyhow::Result<()> {
    let RelayAction::Serve {
        http_addr,
        https_addr,
        quic_addr,
        hostname,
        cert,
        key,
        dev,
        print_service_unit,
    } = action;
    let args = serve::ServeArgs {
        http_addr: http_addr.clone(),
        https_addr: https_addr.clone(),
        quic_addr: quic_addr.clone(),
        hostname: hostname.clone(),
        cert: cert.clone(),
        key: key.clone(),
        dev: *dev,
        print_service_unit: *print_service_unit,
    };
    match serve::run_serve(&args).await {
        Ok(0) => Ok(()),
        Ok(code) => Err(SubcommandExit(code).into()),
        Err(error) => {
            eprintln!("rustain relay serve: {error}");
            Err(SubcommandExit(SubcommandExit::GENERIC).into())
        }
    }
}
