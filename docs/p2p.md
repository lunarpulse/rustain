# P2P peer transport

`rustain peer` is the operator surface over the QUIC peer transport: a
per-workspace allowlist of peers that may reach this host, plus a small set of
verbs for exchanging liveness and context with them. The same verbs exist as
`/peer …` in the TUI; both faces parse into the same decision cores and print
the same strings.

P2P is off by default for `cargo install`. **Published release assets up to and
including `v0.1.3` do not contain it** — it is on in every release built after
Story 19.5; source builds need `--features p2p`:

```bash
cargo build --features p2p
```

The verbs themselves compile in every build. In a build without the feature,
`peer ping` and `peer share` refuse with "this build was compiled without the
peer transport", while `peer invite`, `peer add`, `peer list`, `peer show`,
and `peer revoke` are transport-independent and work normally.

## What `peer` manages

Every verb in this family uses `.rustain/p2p.json` as the transport allowlist.
Admission-changing verbs write it; the rest read it without rewriting it. The
file is deliberately independent of
`.rustain/a2a.json`, the HTTP A2A allowlist: an A2A `allow` never opens the
transport, and an alias that exists only in the A2A config never resolves to
a transport target. The two rosters share no resolver.

Two sibling files complete the picture, kept separate so an older binary can
never misread one file's keys as another's:

| File | Role |
|---|---|
| `.rustain/p2p.json` | Transport allowlist: which pinned keys may reach this host |
| `.rustain/p2p-reach.json` | Reach store: this host's own advertised address, and addresses imported from peers' tickets |
| `.rustain/relay.json` | Relay mode: `disabled` (default), n0's default relay, or a relay you configured |

Three invariants hold across every verb:

- **Tickets grant reach, not authority.** A ticket lets one host dial
  another; it confers no tool, action, or capability authority. `peer ping`
  and `peer share` mint no capability token and reference none.
- **Importing requires fingerprint confirmation.** `peer add` pins a key only
  after an explicit interactive confirm of the fingerprint it prints. There
  is no `--yes` and no `--force`.
- **Peer context is tainted.** What a peer sends — a ping answer, a shared
  summary — is attributed, never verified. A signature establishes who said
  it; it says nothing about whether it is true.

## Reach and relays

Every surface that displays peer configuration ends with the reach limit. On
an install with no `.rustain/relay.json` it reads exactly:

```
Reach limit: directly-addressable peers only; relay disabled.
```

That is the default mode, not a permanent property. Configuring a relay in
`.rustain/relay.json` — n0's default set or your own host — widens the same
sentence to name the relay that composes. A relay a *peer* names in its
ticket is a claim, recorded and shown as such; it is dialed only if you
configured that relay yourself.

Where a relay carries traffic, `peer list` and `peer invite` print a
disclosure: the relay host observes which endpoints exchanged traffic, when,
and how much. Running your own relay moves that observer from the vendor to
you; it does not remove it. A change to `.rustain/relay.json` takes effect
when the daemon restarts.

**A ticket without an address grants no reach.** If no address is configured
for this host when you run `peer invite` (nothing in
`.rustain/p2p-reach.json`), the ticket carries your key and an expiry only.
The recipient can pin you, but cannot dial you: you still owe them a
reachable address by another route. The CLI says so on the ticket itself,
and it never reports the case as unreachability — it is a missing
configuration, not a network observation.

## The verbs

| Verb | Purpose | Requirements |
|---|---|---|
| `list` | Show the transport roster | none; read-only |
| `show` | Print one entry's key and peer id whole | none; read-only |
| `invite` | Mint a ticket that lets one person reach this host | none; read-only, offline-safe |
| `add` | Import a ticket after a fingerprint confirm | interactive terminal |
| `revoke` | Remove a peer and journal it | none |
| `ping` | Send one signed frame and report the answer | pinned peer with an address on file |
| `share` | Advertise an artifact's signed handle into a Topic | pinned peer with an address, running daemon |

### `rustain peer list`

```bash
rustain peer list [--json]
```

Read-only roster of `.rustain/p2p.json`, headed `Transport peers` with the
listener flag (`on`/`off`) when it could be read. Four input states render
distinctly: no allowlist at all ("this host admits no peer"), a present and
empty allowlist ("admits nobody" — a deliberate posture, not an error), an
unreadable allowlist, and a populated roster. The footer states the count,
that admission is checked per frame, the reach limit, and — on a
relay-composed host — the relay disclosure.

This list is **configuration, not connection status**: it binds no transport
and reports no last-seen, connection state, or tier. Absent facts render as
`—`, never as a zero.

`--json` emits schema version `1.1`: `schema_version`, `config_path`,
`state` (`absent` | `empty` | `unreadable` | `populated`), `reason` (only
when unreadable), `peers` (each with `alias`, `peer_id` — `null` when the
entry carries no pinned key — and `pinned`), `reach`, `relay_disclosure`
(only on a relay-composed host), and
`configuration_not_connection_status: true`.

### `rustain peer show <alias>`

Prints one entry's pinned key and peer id **in full**. The short
head…tail form used in roster rows is a recognition aid, not a comparison
aid, so fingerprint comparisons happen here. An unknown alias prints the
same four-state copy as `peer list` plus the configured count.

### `rustain peer invite`

```bash
rustain peer invite [--ttl <duration>] [--name <name>] [--qr]
```

Mints a ticket and prints it. The ticket is Ed25519-signed with this host's
local identity key and carries the host's public key, its claimed network
addresses (from `.rustain/p2p-reach.json`), an expiry, and optionally a
suggested name. Output includes the fingerprint — sha-256 of the key, hex,
head…tail — for the recipient to compare out of band.

The copyable blob is the artifact. The ticket is not a secret and not a
credential: holding it admits nobody, its reach is bounded by the expiry,
and both sides still pin each other's keys.

- `--ttl` — how long the ticket stays usable. `30m`, `12h`, `7d`, or a
  second count; must be positive; defaults to 24h.
- `--name` — a name to suggest to the recipient. Advisory only: they choose
  the alias their own config records.
- `--qr` — also render the ticket as a QR. Opt-in and size-gated: below the
  terminal size a complete code needs, the CLI names the requirement and
  prints the blob only — never a partial or scaled code.

Addresses in a ticket are **claimed**, nothing stronger: the self-signed
signature proves possession of the key, not ownership of the sockets.

### `rustain peer add <alias> <ticket> [--allow-local-addresses]`

The trust gate. `add` decodes the ticket, verifies its signature and expiry
(a malformed, expired, or altered ticket refuses with its own reason and
writes nothing), filters the claimed addresses, then shows a confirm card:
the fingerprint, the claimed addresses themselves, the expiry, and the
suggested name. Pinning happens only on an explicit `y`; anything else —
`n`, an empty line, EOF — declines and writes nothing.

- **No bypass exists.** There is no `--yes` and no `--force`. Without a
  terminal, `add` refuses with a named reason and writes nothing.
- **Key mismatch refuses.** A ticket offering a different key for an alias
  you already pinned prints an alarm with both fingerprints (head…tail, so
  both ends stay checkable) and binds no key. The only accept path is two
  separate verbs: `revoke` the alias, then `add` the new ticket.
- **Same key, new address = reach refresh.** Re-importing a ticket whose key
  matches the pin updates the address on file only; the card says the key is
  not up for decision and no new trust decision is recorded.
- **One identity, one alias.** Pinning an already-pinned key under a second
  alias refuses; revoke the first alias instead.
- **Addresses are filtered before anything is persisted.** A refused address
  refuses the whole import — the pin is never kept while the address is
  silently dropped. `--allow-local-addresses` accepts claimed addresses that
  point into this machine or local network (two hosts on one machine need
  it); it is not a confirm bypass.

An unreadable `.rustain/p2p.json` refuses the write so nothing hand-edited
is overwritten. After a successful pin, admission takes effect on the peer's
next frame, with no restart.

### `rustain peer ping <alias> [--count N] [--interval <duration>]`

Sends one signed frame to a pinned peer and reports what they said. The
frame carries a fixed body, `ping` — no operator-supplied content — with a
unique nonce per frame and a 60-second validity window.

The outcome vocabulary is exact: **accepted** or **refused** only when this
host actually received and validated a verdict; an unanswered frame reports
**outcome unknown** and points at the peer's transport-admission log. Never
"delivered", never "verified". A separate line reports how the frame that
produced the verdict travelled: carried directly, carried through a relay
(which therefore saw the exchange), or over a transport this build does not
name. Because iroh holepunches after connecting, a multi-frame run can
honestly migrate relay→direct mid-run.

- `--count` — frames sent on one connection; default 1; `0` is refused at
  parse. More than one exists to watch a revocation take effect between
  frames on an open connection.
- `--interval` — wait between frames; `500ms`, `2s`, `1m`, or a bare
  millisecond count; zero means "as fast as the connection allows"; ignored
  when `--count` is 1.

Every refusal names what was **not** sent: unknown alias, no pinned key, no
address on file (import a ticket that carries one), the peer names a relay
you do not use — a posture, not a verdict about the peer — dial failure,
feature disabled, or a local fault.

### `rustain peer share <alias> <artifact> --topic <id> [--summary <text>]`

Advertises one room artifact's **signed handle** into a Topic. What crosses
is the artifact id, its content hash, its producer, and a ≤240-byte summary.
The artifact's contents stay on this host; this cut ships no body-fetch
protocol at all.

- `--topic` — required. A correlation id: teammates use the same string to
  share one thread of context.
- `--summary` — a ≤240-byte human summary. Empty or oversized is refused,
  never truncated: a shortened summary is a different claim than the one the
  signature covers. Without it, a derived line states what the artifact
  *is* — "record artifact produced by …" — never what it means.

Disclosure is an explicit operator act: nothing sweeps the memory store,
nothing shares on capture, and no turn publishes context as a side effect.
The verb is fire-and-forget — output says the handle was *advertised*;
nothing comes back to confirm the peer took it or read it.

`share` needs a running daemon (`rustain daemon run`), which owns the Topic
store and the bound transport. The daemon-side share act grants the
addressed peer **membership of this Topic** — that, and nothing beyond it.
The receiving agent reads the summary as tainted context: the signature
establishes attribution, never truth.

Refusals name what was not shared: unknown alias, no pinned key, no address
on file, a relay-only address you have not configured, feature disabled, no
daemon, no such artifact in this workspace's room (see `rustain team log`
or `/artifacts` for ids), an unusable summary, or a local fault.

### `rustain peer revoke <alias-or-peer-id> [--now]`

Removes a peer from the transport allowlist and journals the removal.
Accepts an alias, or a peer id that a configured entry derives.

Because admission is re-evaluated per inbound frame, the revoked peer's
**next frame is refused, with no restart**. That is all revocation does: it
does not close an open connection, does not unsend frames already delivered,
and does not rotate keys.

`--now` is accepted for compatibility with the planning text and changes
nothing — the CLI prints a note saying so. An unrecorded target writes
nothing: revocation never manufactures a record of removing something that
was not there. An unreadable allowlist refuses the write.

## A first connection

```bash
# Host A — mint a ticket and hand the blob to one person
rustain peer invite --name "laptop-b"

# Host B — import it; compare the fingerprint out of band, then confirm
rustain peer add laptop-a <blob>

# Host B — prove the reach
rustain peer ping laptop-a

# Host B — share one artifact's handle into a thread
rustain peer share laptop-a <artifact-id> --topic arch-review

# Host A — end access; their next frame is refused
rustain peer revoke laptop-b
```

Admission is directional — a pin says who may reach *this* host — and
`ping` dials only peers pinned locally with an address on file, so two-way
reach means both hosts mint and import each other's tickets, and both
tickets must be minted while an address is configured (or the address
supplied by another route).
