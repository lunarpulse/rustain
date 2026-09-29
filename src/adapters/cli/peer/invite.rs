//! `rustain peer invite` — mint a ticket and print it (Story 18.4b, AC2).
//!
//! # The blob is primary; the QR is opt-in and size-gated (`UX-DR-PT-03`)
//!
//! `CC:104` prescribed printing the blob **and** rendering an in-terminal QR.
//! That narrows here, deliberately and on a measurement: a ticket-sized code is
//! 45–57 modules square plus a 4-module quiet zone on each side, so it needs
//! 53–65 columns against a layout floor of 60×16. The copyable text is therefore
//! the primary artifact and the QR is requested with `--qr`; below the threshold
//! the surface **names the requirement and prints the blob**. ⛔ Never a partial
//! QR, never a scaled one, never a silent omission.
//!
//! Two modules per row (`Dense1x2`) is what makes the affordance reachable at
//! all — it halves the row requirement, which is the basis the UX addendum's own
//! measurement used.

use std::io::Write;

use anyhow::{Context, Result};

use crate::adapters::cli::peer::rows::{
    expiry_label, reach_limit, reachable_clause, relay_disclosure, sanitize_for_terminal,
};
use crate::domain::models::{PeerTicket, peer_fingerprint};

/// Default ticket lifetime when `--ttl` is not given.
///
/// Short on purpose: a ticket embeds this host's network addresses, so its
/// blast radius is bounded by how long it stays usable.
pub const DEFAULT_TTL_SECONDS: i64 = 24 * 60 * 60;

/// A QR's quiet zone, in modules per side, as the specification requires.
const QUIET_ZONE_MODULES: usize = 4;

/// Parse `--ttl`: a bare number of seconds, or a suffixed `30m`, `12h`, `7d`.
pub fn parse_ttl(spec: &str) -> Result<i64, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("--ttl needs a duration, for example 30m, 12h or 7d".to_owned());
    }
    let (digits, multiplier) = match spec.as_bytes()[spec.len() - 1] {
        b's' => (&spec[..spec.len() - 1], 1),
        b'm' => (&spec[..spec.len() - 1], 60),
        b'h' => (&spec[..spec.len() - 1], 3_600),
        b'd' => (&spec[..spec.len() - 1], 86_400),
        _ => (spec, 1),
    };
    let value: i64 = digits
        .parse()
        .map_err(|_| format!("'{spec}' is not a duration — use 30m, 12h, 7d or a second count"))?;
    if value <= 0 {
        return Err(
            "--ttl must be positive: a ticket that has already expired invites nobody".to_owned(),
        );
    }
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("'{spec}' is longer than this format carries"))
}

/// What the QR affordance did, so the caller can render one honest sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QrOutcome {
    /// The operator did not ask for one.
    NotRequested,
    /// Rendered. The payload is byte-identical to the printed blob.
    Rendered { art: String },
    /// Requested but refused, with the requirement named.
    TooSmall {
        needed_columns: usize,
        needed_rows: usize,
        have_columns: u16,
        have_rows: u16,
    },
    /// The payload does not fit any QR version.
    Unencodable { reason: String },
}

/// Render a QR of exactly `payload`, or say why not.
///
/// `terminal` is `(columns, rows)`; `None` when the size could not be measured,
/// which is treated as "does not fit" rather than as permission to try.
pub fn qr_for(payload: &str, terminal: Option<(u16, u16)>, requested: bool) -> QrOutcome {
    if !requested {
        return QrOutcome::NotRequested;
    }
    let code = match qrcode::QrCode::new(payload.as_bytes()) {
        Ok(code) => code,
        Err(error) => {
            return QrOutcome::Unencodable {
                reason: error.to_string(),
            };
        }
    };
    let modules = code.width() + 2 * QUIET_ZONE_MODULES;
    let needed_columns = modules;
    // Two modules per rendered row, rounded up.
    let needed_rows = modules.div_ceil(2);
    let (have_columns, have_rows) = terminal.unwrap_or((0, 0));
    if usize::from(have_columns) < needed_columns || usize::from(have_rows) < needed_rows {
        return QrOutcome::TooSmall {
            needed_columns,
            needed_rows,
            have_columns,
            have_rows,
        };
    }
    QrOutcome::Rendered {
        art: code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build(),
    }
}

/// Mint a ticket, then print the blob, the fingerprint, the honest caveats and
/// — only if it fits — the QR.
pub fn render_invite(
    ticket: &PeerTicket,
    terminal: Option<(u16, u16)>,
    want_qr: bool,
    relay: &crate::domain::models::RelayConfigState,
    out: &mut impl Write,
) -> Result<()> {
    let blob = ticket
        .encode()
        .map_err(|error| anyhow::anyhow!("could not encode the ticket: {error}"))?;
    let peer_id = ticket
        .peer_id()
        .map_err(|error| anyhow::anyhow!("could not derive this host's peer id: {error}"))?;

    writeln!(out, "Peer invite — hand this to one person\n").context("writing the invite")?;
    writeln!(out, "{blob}\n")?;
    if ticket.addresses.is_empty() {
        // ⛔ Never claim the ticket carries an address it does not carry, and
        // ⛔ never report this as unreachability: it is a missing configuration,
        // not a network observation (`UX-DR-PT-07`).
        writeln!(
            out,
            "This ticket carries no network address, because none is configured for this\n\
             host. It carries this host's key and an expiry, so the recipient can pin\n\
             you — they will still need a reachable address from you by another route.\n\
             It expires {}, and grants reach, not authority.",
            expiry_label(ticket.not_after)
        )?;
    } else {
        writeln!(
            out,
            "This ticket carries this host's network addresses ({}), expires\n\
             {}, and grants reach, not authority.",
            reachable_clause(ticket),
            expiry_label(ticket.not_after)
        )?;
    }
    writeln!(
        out,
        "\nHand it to one person. It is not a secret and not a credential: its reach\n\
         is bounded by that expiry, and holding it admits nobody — the recipient\n\
         still has to pin your key, and you still have to pin theirs."
    )?;
    writeln!(
        out,
        "\nThe recipient will confirm this fingerprint out of band:\n  {}\n  \
         (peer id digest: sha-256 of the key, hex, head…tail)",
        peer_fingerprint(&peer_id)
    )?;
    if let Some(name) = ticket.suggested_name.as_deref() {
        writeln!(
            out,
            "\nIt suggests the name {:?}. That is advisory: the recipient chooses the\nalias their config records.",
            sanitize_for_terminal(name)
        )?;
    }
    // ⚠ This used to duplicate the constant as a literal, which is a live
    // drift hazard: two spellings of one sentence stay equal only until one is
    // edited. It now reads the shared builder, so the mode this host composed
    // reaches every surface at once.
    writeln!(out, "\n{}", reach_limit(relay))?;
    if let Some(disclosure) = relay_disclosure(relay) {
        writeln!(out, "\n{disclosure}")?;
    }

    match qr_for(&blob, terminal, want_qr) {
        QrOutcome::NotRequested => {}
        QrOutcome::Rendered { art } => {
            writeln!(out, "\nThe same ticket, as a QR:\n")?;
            write!(out, "{art}")?;
        }
        QrOutcome::TooSmall {
            needed_columns,
            needed_rows,
            have_columns,
            have_rows,
        } => {
            writeln!(
                out,
                "\nNo QR: a code for this ticket needs {needed_columns} columns by \
                 {needed_rows} rows and this\nterminal is {have_columns} by {have_rows}. \
                 A partial or scaled code would not scan, so\nthe copyable ticket above is \
                 the whole artifact — it carries the same payload."
            )?;
        }
        QrOutcome::Unencodable { reason } => {
            writeln!(
                out,
                "\nNo QR: this ticket does not fit any QR version ({reason}). The copyable\n\
                 ticket above is the whole artifact."
            )?;
        }
    }
    Ok(())
}
