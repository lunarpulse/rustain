//! `peer` CLI subcommand family (Story 18.4b, FR157 / FR158).
//!
//! The **operator surface** over the transport substrate Story 18.4 shipped.
//! 18.4 landed `PeerTransport`, the iroh adapter, `p2p_config.rs`, `P2pPeerSpec`
//! and the pure `peer_dial` admission core, and no dedicated operator surface at
//! all. These verbs are that surface.
//!
//! # One core, two faces
//!
//! `rustain peer …` and the TUI's `/peer …` both parse into the same decision
//! cores ([`crate::domain::services::peer_admission`]) and render the same
//! strings ([`rows`]), dispatched through the same effect shell
//! (`infrastructure::runtime::peer_bridge`). ⛔ Two cores is the defect; the
//! faces differ only in how a confirm is taken and where output lands.
//!
//! # This surface writes `.rustain/p2p.json`, ⛔ never `.rustain/a2a.json`
//!
//! `ADR-18-4-01` D4 keeps the two apart so an A2A `allow` cannot open the
//! transport. No `A2aPeerSpec` value enters any verb here, and no resolver
//! shared with the A2A roster is called: an alias that exists only in the A2A
//! config must not resolve to a transport target it was never in.
//!
//! # ⛔ Not in this cut
//!
//! No tier, plan, edition or posture — no such mechanism exists in the tree. No
//! connection-state indicator — this transport observes none. No relay of any
//! kind. Nothing cryptographic beyond Ed25519 signature checking.

use clap::Subcommand;

pub mod add;
pub mod invite;
pub mod list;
pub mod ping;
pub mod revoke;
pub mod rows;
pub mod show;

/// `peer` subcommand actions. Declared beside its handlers, mirroring
/// `cli/team/mod.rs` and `cli/session/mod.rs`.
#[derive(Subcommand, Debug, Clone)]
pub enum PeerAction {
    /// Mint a ticket that lets one person reach this host, and print it.
    ///
    /// The copyable ticket is the artifact. It carries this host's network
    /// addresses and an expiry, and it grants reach rather than authority —
    /// hand it to one person. Read-only and offline-safe.
    Invite {
        /// How long the ticket stays usable: `30m`, `12h`, `7d`, or a second
        /// count. Defaults to 24h.
        #[arg(long)]
        ttl: Option<String>,
        /// Also render the same ticket as a QR, when the terminal is large
        /// enough for a complete code. Below that it names the requirement and
        /// prints the ticket only.
        #[arg(long)]
        qr: bool,
        /// A name to suggest to the recipient. Advisory: they choose the alias
        /// their own config records.
        #[arg(long)]
        name: Option<String>,
    },
    /// Import a ticket under an alias you choose, after confirming a fingerprint.
    ///
    /// Requires an interactive terminal: the confirm is the trust gate and there
    /// is no flag that skips it. An expired or altered ticket refuses with its
    /// own reason and writes nothing. A ticket offering a different key for an
    /// alias you already pinned refuses and shows both fingerprints.
    Add {
        /// The alias this peer gets in `.rustain/p2p.json`. You choose it; a
        /// name suggested by the ticket is never adopted for you.
        alias: String,
        /// The ticket blob, as printed by `peer invite`.
        ticket: String,
        /// Accept a claimed address that points into this machine or this local
        /// network. Two hosts on one machine need it; a ticket from a stranger
        /// naming your own loopback or metadata endpoint does not.
        ///
        /// ⛔ This is not a confirm bypass. The fingerprint confirm still
        /// happens and there is still no flag that skips it.
        #[arg(long)]
        allow_local_addresses: bool,
    },
    /// Send one signed frame to a pinned peer and report what they said.
    ///
    /// The first verb that actually reaches another host. It resolves the alias
    /// to its pinned key and its address on file, sends a frame, and prints the
    /// answer the peer gave — or says the outcome is unknown when none came
    /// back. It grants nothing and carries no authority.
    Ping {
        /// The alias to reach, as recorded by `peer add`.
        alias: String,
        /// How many frames to send on one connection. Defaults to 1.
        ///
        /// More than one exists so an operator can watch a revocation take
        /// effect between frames on a connection that is already open.
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// How long to wait between frames: `500ms`, `2s`, or a millisecond
        /// count. Ignored when `--count` is 1.
        #[arg(long)]
        interval: Option<String>,
    },
    /// Show the transport roster from `.rustain/p2p.json`.
    ///
    /// Configuration, not connection status. Read-only and offline-safe.
    List {
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Print one entry's pinned key and peer id in full.
    ///
    /// Both are printed whole: the short form used in rows is a recognition aid,
    /// not a comparison aid.
    Show {
        /// The alias to show.
        alias: String,
    },
    /// Remove a peer from the transport allowlist and record it.
    ///
    /// Their next frame is refused with no restart. It does not close an open
    /// connection and does not reach anything already delivered.
    Revoke {
        /// An alias, or a peer id that a configured entry derives.
        target: String,
        /// Accepted for compatibility with the planning text; changes nothing,
        /// because admission is already checked per frame.
        #[arg(long)]
        now: bool,
    },
}
