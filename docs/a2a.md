# A2A AgentCard discovery

Rustain discovers allowlisted A2A peers and registers their AgentCard skills in the internal capability inventory (Story 17.4a), and serves its own signed card plus a task lifecycle to allowlisted callers (`--serve-a2a`, and the daemon's inbound path). A2A skills still do not appear in the LLM-facing `@` dropdown; § Task lifecycle below is the authority on what a caller can invoke.

A2A is off by default for `cargo install`. **Published release assets up to and including `v0.1.3` do not contain it; `v0.2.0` and later do** (Story 19.5); source builds need `--features a2a`:

```bash
cargo build --features a2a
```

## Workspace configuration

Create `.rustain/a2a.json` in the workspace:

```json
{
  "agents": {
    "security-peer": {
      "url": "https://agent.example"
    },
    "verified-ci": {
      "url": "https://ci.example",
      "pinnedKey": {
        "alg": "EdDSA",
        "x": "Pii06SUCwAi0D_BTTOeCsD5XSSrjqFqw0nXF8STr14w",
        "kid": "ci-key-2026"
      }
    },
    "remote-reviewer": {
      "url": "https://reviewer.example",
      "auth": "RUSTAIN_REVIEWER_API_KEY",
      "caCert": "certs/reviewer-ca.pem"
    }
  }
}
```

Workspace entries override profile entries with the same peer ID. Peer IDs must be non-empty and cannot contain `__`, which is reserved by the `a2a::<peer>::<skill>` capability name.

Profile configuration uses the active profile's tools config:

```toml
[tools.config.a2a.security-peer]
url = "https://agent.example"

[tools.config.a2a.verified-ci]
url = "https://ci.example"

[tools.config.a2a.verified-ci.pinned_key]
alg = "EdDSA"
x = "Pii06SUCwAi0D_BTTOeCsD5XSSrjqFqw0nXF8STr14w"
kid = "ci-key-2026"

[tools.config.a2a.remote-reviewer]
url = "https://reviewer.example"
auth = "RUSTAIN_REVIEWER_API_KEY"
ca_cert = "certs/reviewer-ca.pem"
```

If peers are configured but the binary was built without `a2a`, startup fails loudly instead of silently omitting them.

## Reaching a credentialed peer across a network boundary

Two optional roster fields let a rustain process — not `curl` — reach a peer that
demands TLS and an API key. They are independent: either may be used alone.

| Key | Value | Meaning |
|---|---|---|
| `auth` | env var **name** | The variable holding this peer's API key. The key itself never lives in the file, and the roster is safe to commit |
| `caCert` / `ca_cert` | path to a PEM file | The trust anchor this peer's server certificate must chain to |

### `auth` — the client credential

The value is a **variable name**, exactly like the server block's `apiKeyEnv`. The
variable is read **on every outbound JSON-RPC call**, so exporting it after
rustain starts works, and rotating it takes effect on the next send without a
restart.

The key is sent as `x-api-key`, on `POST` only. The AgentCard fetch is
**unauthenticated** — the served card is the document that explains how to get
past the gate, so gating it would gate the instructions.

**The credential is sent only to the origin your roster `url` names** — scheme,
host and port. An AgentCard decides where the JSON-RPC request goes, and a card
is written by the peer; without this bound, a card naming another host would
collect your key. If a peer's card advertises a different origin, the send is
refused and nothing is sent. So a credentialed peer's server must advertise an
authority equal to your roster `url` (the card's endpoint is built from the
server's `advertisedHost`).

### `caCert` — the trust anchor

A relative path is resolved against the directory that `.rustain/a2a.json` was
found in — **not** against `.rustain/` itself and not against a profile's
directory, so `"certs/ca.pem"` means `<workspace>/certs/ca.pem` for workspace and
profile peers alike. An absolute path is used as given.

Parsing never touches the file — a roster loads whether or not the anchor
exists. The client then reads it **once, at startup**, when the peer is
composed: a missing or unparseable anchor is kept as that peer's refusal, and
every send to it fails with the `<alias>'s pinned anchor could not be loaded: …`
form. Repairing or replacing the file therefore takes effect **only after a
restart**, like any other roster change.

`caCert` requires an `https` roster `url`. A plain-HTTP (loopback) peer never
performs a TLS handshake, so an anchor there could never be consulted — the
roster refuses to load that combination and says so, rather than silently
ignoring your trust setting.

An anchored peer's client trusts **only** that anchor. The platform root store is
switched off for it, so a public CA mis-issuance cannot impersonate that peer. A
peer without `caCert` is unchanged and keeps the system trust store.

This is **anchor validation**, not certificate fingerprint pinning: rustain
installs the file's certificates as roots and validates the presented chain
against them. Two certificate shapes therefore work, and one that looks like it
should does not.

**Form 1 — a private CA that issues the server's certificate.** Commands as run
on OpenSSL 3.5.5:

```bash
# The anchor you put in caCert
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout ca.key -out ca.pem -days 3650 -subj /CN=rustain-peer-ca \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

# The certificate the peer's server presents
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout server.key -out server.csr -subj /CN=peer.example
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:peer.example\n' > ext.cnf
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -out server.pem \
  -days 825 -extfile ext.cnf
openssl verify -CAfile ca.pem server.pem      # -> server.pem: OK
```

**Form 2 — a non-CA self-signed certificate, pinned as its own anchor.** The
`-addext` line is the load-bearing part:

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout leaf.key -out leaf.pem -days 365 -subj /CN=localhost \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "subjectAltName=DNS:localhost" \
  -addext "extendedKeyUsage=serverAuth"
```

⚠ **`openssl req -x509` without that line emits `CA:TRUE`** (verified on OpenSSL
3.5.5), and a `CA:TRUE` certificate presented as a server's own certificate is
**refused** — rustain reports that the peer presents a CA certificate as its
server certificate. Add `basicConstraints=critical,CA:FALSE`, or use form 1.

⚠ **An expired anchor is still trusted.** A trust anchor's own validity period is
not checked (RFC 5280 §6.1.1 leaves it optional and the verifier here omits it),
so an expired CA keeps validating leaves it issued. The server's *own*
certificate is checked normally. Rotate anchors on a calendar, not on an error
message.

⚠ **A certificate fixed on the server needs a rustain restart.** The anchor
decision is made during the one AgentCard fetch at startup and retained, so a
peer whose certificate was wrong when rustain started stays refused until the
next start even after the server is repaired.

### When it refuses

Each failure names one trust decision, because each has a different fix:

| Rendered | What to change |
|---|---|
| `no credential for <alias>: set the env var named in its auth field` | export the variable your `auth` field names |
| `no credential configured for <alias>: add an auth field naming the env var that holds its key` | the peer wants a key and your roster entry has no `auth` field |
| `<alias> rejected this credential` | either side — a wrong key here, or a revoked grant there |
| `<alias>'s card sends requests to another host; its credential is only sent to <origin>` | the peer's `advertisedHost`, or your roster `url` |
| `<alias>'s certificate does not match the pinned anchor` | the wrong file in `caCert`, or a certificate not issued under it |
| `<alias>'s certificate has expired or is not yet valid` | the peer's certificate, or a clock on either host |
| `<alias>'s certificate is not valid for its roster address` | the certificate's SAN, or the host in your roster `url` |
| `<alias> presents a CA certificate as its server certificate` | the peer's certificate shape — see form 2 above |
| `<alias>'s pinned anchor could not be loaded: <reason>` | the `caCert` path or its contents; the reason names both |

There is still **no** mutual TLS: rustain presents a bearer secret, never a client
certificate (`DF-18-1-MTLS`).

## Sending to configured peers from the TUI

Peer entries are loaded once when rustain starts. After adding, removing, or changing an entry in `.rustain/a2a.json` or the active profile, restart rustain; `/team send` does not re-read configuration or discover a second source of truth.

Address one or more roster IDs as a comma list, optionally preceded by a semantic type, followed by the message body:

```text
/team send security-peer,remote-reviewer review the authentication changes
/team send --type consultation security-peer,remote-reviewer compare the old and new authentication code
```

For story distribution, place `--stories` after the optional sender-local `--subject`:

```text
/team send --type story_assignment [--subject <label>] --stories <key>[=<alias>],… <aliases> <body>
/team send --type story_assignment --stories S1,S2=jun-dev,S3=tom-dev jun-dev,tom-dev Sprint stories
```

A key is 1–32 ASCII characters from `A–Z a–z 0–9 . _ -`, starting with a letter or digit. A send lists 1–64 unique keys. A bare key has no holder; `KEY=ALIAS` states a holder who must be addressed by that send, with an alias of at most 256 UTF-8 bytes. Only `--type story_assignment` accepts this option. Agent proposals carry no stories.

Both rails use the same refusals (the parser appends `. Use: …`):
- `'--stories' needs '--type story_assignment'`
- `Missing story list after '/team send --stories'`
- `story key '<key>' is not valid: 1–32 characters from A–Z a–z 0–9 . _ -, starting with a letter or digit`
- `story key '<key>' is listed twice`
- `a send lists at most 64 stories`
- `story '<key>' is assigned to '<alias>', who is not addressed by this send`
- `story '<key>' is assigned to an alias longer than 256 bytes`

A refusal records no action, creates no recipient rows, and sends no `message/send` request.

Every listed ID must be a configured, distinct, non-empty roster ID. An empty segment, duplicate, unknown ID, or missing body refuses the whole action before network I/O and writes no journal row. A roster ID containing `,` is therefore not addressable by this verb.

The optional leading `--type <type>` accepts `consultation`, `story_assignment`, `design_update`, `status_request`, `bug_report`, `scope_change`, `architecture_update` or `retro_request`. It uses the existing A2A `message_type` metadata carrier for every recipient; the answering host stores the parsed type on the received item, including after sender-consent approval, so journal replay preserves it. The old untyped form is unchanged. An unknown operator token refuses the entire action by name with the valid vocabulary before any network I/O or journal write; this is distinct from the incoming-wire unknown-type fallback.

Inherited outer whitespace trimming is unchanged. Interior spaces, tabs, newlines and code snippets remain verbatim; the parser consumes the type/recipient prefix by token position, not by searching for repeated words in the body.

The action creates one stable, keyless `Info` block with one row per alias in the order typed. It has no header or aggregate result. Each recipient settles independently at its **first** `message/send` answer: `● delivered` means that answer carried a non-empty recipient item ID; `✗ declined` is a send-time rejection (including a `-32040` policy refusal); `⚠ unreachable` names a send that did not land, with a named preflight cause on its indented line when available. Tokenless rows name `sending…`, `awaiting their approval` (`auth-required` without an item), `accepted — this peer keeps no item id`, `no usable answer — '/team board' shows whether it arrived`, `asked a question this verb cannot answer (task … cancelled) — this peer uses A2A input-required, which rustain does not continue yet`, or `not sent — this host could not record the attempt`. The recipient's `server.admission` defaults to `deny`, so an unconfigured recipient declines a team message.

There is exactly one `message/send` attempt per recipient and no automatic retry or queue. To send again, re-type `/team send` with the recipients you choose; the block has no retry control. A recipient with an item ID is delivered even when its first answer is `auth-required`: this rail does not poll or cancel that task. An item-less `input-required` triggers a single-turn `tasks/cancel`; if that cleanup fails or its answer is not `canceled`, the row says `no usable answer` rather than claiming cancellation. A peer that answers with an A2A Message instead of a task, and no item ID, is `accepted — this peer keeps no item id`.

Each recipient is journaled independently. A request that may leave the host records `dispatched` before its POST; a delivered or accepted answer adds `accepted`; a proved rejection (including JSON-RPC `-32040`), a connection that never opened, or a post-dispatch credential/trust-anchor refusal adds `refused`; and an approval-needed or no-usable-answer row (including a timeout, HTTP or other JSON-RPC error, or unreadable answer after dispatch) remains dispatch-only. A pre-I/O refusal writes one outbound `refused` row and no `dispatched` row. If durable dispatch recording fails, nothing is sent and no row is written. An accepted answer contributes a content hash, not stored reply text; a decline stores its sanitized reason. `/team send` never renders a peer reply and never materializes an `a2a-peer` node in the Agents panel. FR169 replies use `/team thread` as the later answer channel.

`/team send` is available in both the interactive TUI and an attached trusted-local writer session. The attached client sends it through the daemon's already-composed A2A runtime; it creates no client egress. Read-only and Peer-tier attachments are refused. There is no headless `rustain team send` command. A roster entry supplies client credentials for a non-loopback peer through its `auth` and `caCert` fields — see [Reaching a credentialed peer across a network boundary](#reaching-a-credentialed-peer-across-a-network-boundary).

### Agent-proposed messages (standalone and attached TUI)

With A2A egress configured, a model's own interactive turn can call
`propose_team_message` with `recipients`, `type` and `body`. Recipients must be
distinct configured aliases, the type must be one of the eight send types above,
and the body must contain 1–131,072 UTF-8 bytes. Control and directional-formatting
characters that could disguise the reviewed text are refused; tabs and newlines
remain literal. A refusal performs no peer I/O and records no send action.

Raising a proposal returns immediately. It displays the frozen aliases, type and
body in a separate approval card; it does not send, start a timer, create a generic
tool-approval request or insert text into the composer. There is one pending proposal per
conversation. A second raise cannot replace it. Without an interactive approval
surface, the tool refuses rather than parking a draft for later delivery.

With an empty composer, **y** sends exactly that snapshot once its decision footer
has been painted, **r** opens its full, delimited Review, and **n** dismisses it.
Enter and Escape never send. With text in the composer, y/r/n remain text input;
higher-priority approval prompts and large-image confirmations keep their keys.
A tool peek takes its dismissing key before any proposal decision; Ctrl+R still
opens composer history search. Clearing or closing the conversation withdraws its
draft without peer I/O. Switching conversations never approves or retargets it.

Long recipient lists remain complete and scrollable, including at 60×16. Use
**PgUp/PgDn** to page an overflowing card; Send stays disabled until the decision
footer is in view. In Review, paging at a card boundary continues through the
full transcript body. Review selects the current proposal even when older Review
blocks remain, returns from task/agent detail to the conversation, and preserves
the tab bar and readable transcript space. Attached Review also closes an open
received-item view and returns the transcript from history scrolling to the
current proposal. It sends nothing. After reviewing in standalone Chat focus,
**i** returns to the composer.

YOLO, AutoEdit, session or saved tool grants, subagent auto-approval and selected
peer context never authorize this Send decision. Plan mode and carried agent or
skill restrictions still deny the tool; subagent, background, remote, cron and
channel routes cannot raise a proposal. The daemon does not carry custom-agent
selection or restrictions; this adds neither that carrier nor model inference to
NotifyAndAuto, which remains template-only. The model-facing capability is not a
new CLI command or a `describe` registry entry.

Human Send enters the same validated operator send path as `/team send`, with
the exact reviewed body, ordered recipients and type, and no subject or appended
conversation context. This host records two independent facts on dispatch,
disclosure and reply journal rows: **authorship** (`human-written`,
`agent-composed`, or `unknown`) and **send authorization** (`human-decided`,
`automatic`, or `unknown`). `/team log` appends `[agent-composed]` for
agent-composed text, then `[auto-sent]` only for automatic sends. Approved
proposals record `agent-composed` + `human-decided`, so they never gain
`[auto-sent]`; typed sends record `human-written` + `human-decided`.
Unknown facts add no label, and unknown future tokens remain readable as Unknown.

Outbound `message/send` carries both recorded facts in message metadata as
`x-rustain-authorship` and `x-rustain-send-authorization`; unknown facts omit
their keys. Recipients retain them across consent deferral and journal replay.
The trusted-local `/team log` received row and `/team view` item turns render
the same marks. The marks are the **sending host's report**, not independent
evidence about the person. An absent mark does not establish that a human
wrote or sent the message. Received log rows append the stored arrival alias
while keeping the peer column; View headers use that alias when present.

This adds no task-answer metadata or thread-read send authorization; thread
reads retain their existing reply-authorship field. Raw event projections omit
both facts on dispatch, disclosure and received-item rows. Raw reply events keep
authorship and omit send authorization. Peer-tier attach transcripts still omit
send authorization; trusted-local writers and read-only mirrors retain it.

For model delegation, `human-decided` means an operator-surface resolver
authorized the exact fingerprint-bound invocation, with its full input available
for review on that surface (Story 19.20c). It does not claim the input was read
or attest human presence. A policy-resolved invocation records `automatic`;
without a gate decision, send authorization is `unknown`. Proposal Send remains
a distinct per-proposal consent decision.

On protocol v7, only an authenticated TrustedLocal read-write attachment that
opts in to proposal decisions receives the daemon's card. The rich TUI opts in
after its attach acknowledgement; the line-based `--plain` client does not.
Read-only, Peer-only and plain attachments are not approval surfaces: a valid
raise without a capable writer returns the no-surface refusal without retaining
a new proposal. Pending proposals remain in memory across writer detach or
replacement and reappear with the same id when the next capable writer opts in;
a conversation reset, close or daemon restart withdraws them without sending.
They hold no turn lock, so peer turns and subsequent operator prompts can finish.
If all eight send slots are occupied, Send refuses without consuming the proposal:
the operator may explicitly retry. A full writer queue terminates that attachment
with an explicit protocol error while retaining the proposal for the next writer.
Proposal state and ids are sender-local, never journaled or exposed on the raw
event bus. Proposal tool arguments and results remain in internal conversation
history but are omitted from public event and transcript projections. These
attached proposal semantics do not close the full J10 journey,
recipient authorship carrier, or AD-1826's separate A2A version boundary.

### The send action, its record and the action board (Story 19.21)

A typed or untyped `/team send` is also one **send action**. After the whole action validates and **before** any recipient preflight, dispatch or POST, this host journals one sender-local action record: a host-minted id (`sa_…`, at most 16 characters), the addressed aliases in the order typed with the identity each resolved to, the semantic type, the optional label, and the body's hash and byte count — never the body and nothing derived from it. Each recipient's `dispatched` row carries that action's id and its position, and after every row a recipient's delivery path emits, its **typed settlement** (delivered with the item id, declined, unreachable with the complete cause, awaiting approval, accepted without an item, asked a question, no usable answer, not sent) is journaled before its row settles on screen. None of it is ever sent to a peer: no recipient can see or address the action id or the label, and there is no group identity, aggregate outcome or retry. If the record cannot be appended, nothing is sent and every row reads `not sent — this host could not record the attempt`. A settlement whose append fails after the recipient was reached is not shown and is never rewritten as `not sent`; the board reconciles it through the recipient's own item. `/team followup`, retract dispatches and model delegations record no action.

```text
/team send --type story_assignment --subject sprint-7 jun-dev,tom-dev,yuki-dev,nina-ux <stories…>
```

`--subject <label>` is an optional label **only you see**: one token of at most 64 characters, after `--type` and before the aliases. It is stored in the action record and shown on the board; it is never added to the message a recipient receives and is never derived from the body.

`/team board` reads, never pushes:

| Command | Shows |
|---|---|
| `/team board` | the peer item inventory (every roster peer's items correlated to your sends), headed `Peer item inventory · N rows` |
| `/team board <peer-id>` | one peer's items, uncapped |
| `/team board --last` | the board of your newest send action in this workspace journal |
| `/team board --action <id>` | the board of one recorded action |
| `/team board --actions` | every recorded action, newest first: id, type, age and addressed count |

An action board is headed by the type, the label (only when one was given) and `attempted <age> · <N> addressed`. `N` is the number of aliases the record froze — not the rows rendered and not how many have settled. Beneath it, one row per addressed alias in the order typed: `✓ acknowledged` (the recipient's own deliberate acknowledgement was reported), `● delivered`, `✗ declined`, `⚠ unreachable` (with the recorded cause on its own line), or a tokenless state in words (`sending…` while this process is still sending, `no outcome recorded` after an interruption or when another process sent it, `awaiting their approval`, `no usable answer`, `not sent — …`). There is no completion count, percentage or list of who has not answered. The recorded send-time outcome always wins; the board then asks each addressed peer that could change the answer (one `x-rustain-items/list` each, never the whole roster) and joins only the items that arrived through **this action's** dispatches, so an earlier send's acknowledgement is never shown as this one's. A recipient that was reached but whose read fails is never `⚠`, and a recipient whose peer binding changed since the send (a changed key, a removed alias) is not read at all and says so. A newer action never hides an older one's declines: `--action <id>` and `--actions` reach them. Refreshing is an explicit command; the board has a 500 ms refresh floor and does not poll. Attached trusted-local clients read these same daemon-owned action facts; a writer may send and a read-only observer may read the board.

On the receiving host, an opened `story_assignment` item offers `[a] Acknowledge` to an attached writer while the item is `received`. Viewing it, discussing it, or an automatic reply never acknowledges it, and none of them selects the item as model context. Acknowledging records receipt, not acceptance.

A story relationship exists only when the sender explicitly supplies `--stories`: the sender journals indexed holders in its send-action record before contacting anyone, and each recipient records the readable `x-rustain-stories` list on its received item. A `story_assignment` without a list proves receipt only; neither its body, subject, type, view nor acknowledgement establishes a story relationship. Every addressed recipient receives the **whole list**, including the sending host's aliases and actual peer pins for other holders (never alias-derived pseudonyms). `you: true` means this recipient host; other unpinned names are display-only, never resolved by alias text.

The arrival card and opened item show `8 stories, 2 assigned to you` (singular `1 story, 1 assigned to you`; zero uses `none assigned to you`). The opened item lists keys in manifest order as `S1`, `S2  assigned to you`, or `S3  assigned to <alias>`, followed by `other names are the sender's roster aliases` when a member is named. Unreadable or future lists say `story list not readable by this build`; future holders say `holder not readable by this build`. No list means no summary, even if the body imitates it. Removed items show no story block. Lines wrap without clipping at the 60-column floor.

The list is the sending host's statement; it starts no work and grants nothing. It changes no response mode, policy, context selection, capability, or acknowledgement transition. Acknowledgement remains receipt, not an Accept/Discuss verdict.

A retract (`/team retract`) whose dispatch row cannot be journaled is not sent either: it renders `not sent — this host could not record the attempt` (a local fact: nothing is attributed to the peer).

If this host cannot read its own send journal, `/team board --last`, `--action <id>` and `--actions` read exactly `· this host could not read its send journal.` — never *no send action recorded*, which an unreadable journal cannot establish. For the same reason a `/team send` whose new action id cannot be checked against that journal is not sent: every row reads `not sent — this host could not record the attempt`, nothing is journaled or delivered, and the next send reads the journal again.

A board field wider than the block (a long alias or id) wraps at grapheme
boundaries using terminal-cell width, never byte/character clipping. The byte
bounds on peer- and operator-influenced text (alias and id fields 256 bytes,
ratified remedies 512) are disclosure bounds, not a width rule, and keep their
`…[truncated]` marker.

## Live daemon recipient rows

Daemon attachments use protocol **v12**. Continuously attached trusted-local
writers and read-only observers receive revisioned peer-row upserts and removals
only after successful persistence. Snapshot and live updates use the same row
identity; duplicate or older updates cannot restore a settled/deleted card.
Failed persistence restores the staged transcript, control metadata and revision.
Draft-ready, resolution, cancellation, queued/digest and RAP delivery, and
auto-response retraction use this boundary.

The pending card header takes its canonical type and policy-derived sender label
from host-owned control metadata, never from the peer body. Controller capability
is checked separately from saved placeholder text: an orphaned Drafting row keeps
its item address but has no live node-resolution target. Control metadata and
ordering revisions survive conversation persistence and trusted-local reattach.

The new operations and snapshot control targets are omitted for authenticated
Peer-tier attachments. The raw event projection also omits sender-local
`TeamSendActionRecorded`/`TeamSendRecipientSettled` events and strips the
additive action reference from legacy `RemoteEnvelopeDispatched`. Peer-reply
bodies (`PeerReplyObserved.content`) remain in the journal but are stripped from
raw-event fanout to every attachment; explicit thread views are origin-only.
This is not a blanket confidentiality guarantee for the legacy stream.
This local synchronization is not A2A reply push. The plain attach reader emits
addressed row-operation JSON, rather than presenting replacements as new replies.

### Attached operator commands (Stories 19.21 and 19.21c)

The attached client composes no A2A egress. It sends `/team send` to the daemon's already-composed runtime, preserving the parsed multiline body, type and sender-local subject; sends and `/team untrust` require a trusted-local read/write attachment. `/team board`, `/team status` and `/team thread <peer> <item-id>` are explicit daemon-side reads available to trusted-local read-only attachments. Peer-tier attachments are refused for all team commands. Board and thread text arrive in bounded request-correlated chunks and are painted only after an in-order sequence and explicit completion boundary. Send rows use the same feature-independent typed renderer and remain in their own stable `team-send-N` Info block.

An unresolved `sending…` row remains visible when a recorded action's settlement cannot be published, including a `NotSent` outcome whose dispatch append and typed-settlement append both failed. The distinct action-record-failure branch has no action or settlement and may show visible `NotSent`. This matches standalone send-board behavior: the client does not invent a terminal outcome or add a timeout.

`/team followup <peer> <turn-id> <text…>` requires a trusted-local writer. It sends to exactly one peer with the reply link, human-written authorship and human-decided authorization; it creates no new send action, and its pending row settles without a `TeamSendStarted` frame. It shares the writer FIFO and the eight-send in-flight limit.

`/team retract <peer> <item-id>` also requires a trusted-local writer. Its FIFO-admitted preview raises the same armed/disarmed confirmation card as standalone. Only an armed `y` confirms; `n` or `Esc` declines without detaching, and the card consumes all other keys. The daemon retains the target and task privately: decisions name only the preview request and take that connection's current slot once. Stale, replaced, disarmed or saturated decisions never POST. Confirmation requires the current writer and one send permit. An accepted dispatch survives detach — including a confirmation still queued in the writer's FIFO when it detaches; the target is dropped once that queue drains, or at once when another writer attaches. It journals before the single retract POST, re-renders any remembered board without another list, then returns the standalone sentence to the origin connection only.

Thread reads and retract previews consume no send permit; threads remain readable while all eight sends are held. Thread views, retract cards and answers are never broadcast and never invoke a provider/model turn. All spawned work is daemon-owned and joined at shutdown. No board or thread is pushed or refreshed by a timer. Builds without an A2A runtime return a request-keyed refusal after the tier/write gates, before any retract target lookup.

## Reading a received item locally

**Updated 2026-10-09 (Story 19.19).** `/team view <item-id>` reads this host's
workspace journal, not the remote asker's journal or model history. It works in
attached writer/read-only sessions and standalone offline sessions. A
notify-and-auto arrival includes `Open: /team view <item-id>` when the response
has a journal-backed item; a pending card opens with Tab.

The Info reader pins the explicitly opened item. Its canonical type, current
principal label, raw body-byte manifest and full available body are scrollable;
the thread root remains separately identified. The opened body appears once,
not again in its chronological row. Missing ids refuse; removed bodies remain
unavailable. Unknown legacy types use a fixed disclosure, not body-derived type.

Each local page reserves the root and opened item, deduplicated within the
50-turn/896-KiB bounds, then retains remaining turns chronologically. Bodies over
64 KiB and display-expanded turns over 448 KiB are withheld with metadata, not
silently clipped. `[p] Earlier` and `[l] Newer` deliberately read local journal
pages; PgUp/PgDn scroll within the page. Header/footer remain outside the scroll
area, including the 60×16 floor. No read/page acknowledges, sends, selects model
context, contacts a peer or creates a model request.

At a byte-saturated pinned page, root/linked-turn bodies can be unavailable on
that page while their metadata remains readable; the opened body stays pinned
and earlier/newer navigation advances actual linked turns within the same caps.

For an item-backed card, Tab switches between Item actions and its verified
response-node target when that target exists. The full Ready draft is scrollable;
Respond/Edit require empty input and pin the exact target before returning to
composition. Read-only observers cannot mutate either target. Offline sessions
show local-view-only controls.

Native confirmations take priority over an open reader, including the complete
proposed plan and its approval controls. Rejecting the plan resumes the same
pinned reader; opening a reader never accepts a hidden confirmation.

### Explicit item-backed model context

`/team context <item-id>` or `[c] Use as context` selects only that received
body, by its existing principal/item reference. It does not adopt the displayed
thread or replies. `[c] Stop using` and `/team context off <item-id>` remove
the reference prospectively, including when its content became unavailable.
An unselected id is a truthful no-op. Retracted text remains available with its
mark; removed or withheld content is unavailable.

Selections belong to the daemon's current process-lifetime conversation.
Writer/read-only handovers share them; detach does not reset them. `/clear`
and daemon restart do reset them, even when the restored conversation id is unchanged.
Reset is ordered after earlier operator turns and their committed output; the
socket remains available for permission responses while those turns finish.
Managed A2A/RAP turns likewise finish their queued tool-display fold before a
reset can be acknowledged, including turns with no emitted provider events.
History fences and context commands follow those earlier turns in the writer's
FIFO. Independent recipient actions remain available while a turn is composing.
Only a trusted-local writer may select/remove. Bare `/team context`
shows cached inventory to a writer or trusted-local reader without assembling
context or calling the model. All context forms require daemon attachment.

The next interactive turn resolves each currently selected body from the
current journal projection. Default local memory keeps its own 2000-token cap;
item data spends only the remaining shared 4096-token content-tier budget in
deterministic item-id order. Inventory distinguishes not-used-yet, full,
partial, skipped and unavailable entries, with last-use turn/cost. Selection
is not a promise the complete body fits. Included bodies are attributed
untrusted data and carry item-origin peer taint through the real tool gate.
They are not signed RAP summaries or persona/system instructions.
The Anthropic, OpenAI-compatible and Ollama adapters retain this attributed
prefix in the outgoing user message, without changing tool-message ordering.

Peer-display history stays visible but is excluded before ordinary
Interactive/Channel/Cron assembly and windowing can promote it into context.
Typed source lineage survives Turn migration and mirror rebuilds; known legacy
`PeerText` remains display-only. Authorized inbound response drafting still
receives its question and tools, isolated from the operator's tool buffer.
Channel, cron and another runtime do not consume these selections. Off is not
erasure of earlier consented or derived model history.

### Deterministic operator-dispatch capture

Story 19.19's committed `tests_tui/test_story_19_19_act2_consultation.py`
executes the consultation/Busy/pull/answer/restart/ordinary ADR Write/independent
notification/Ack/Discuss/context on-off path through real native clients.
Build an A2A release binary, then name that immutable executable explicitly:

```bash
RUSTAIN_TUI_BINARY=/absolute/path/to/built/release/rustain \
  tests_tui/.venv/bin/python -m pytest \
  tests_tui/test_story_19_19_act2_consultation.py::test_act2_consultation_adr_fanout \
  -p no:rerunfailures -n 0 --timeout=420 -v
```

Both 80-column and 130-column cases must run; zero selected tests or skips are
not capture evidence. Receipts retain actual product team-log milliseconds,
scene/driver hashes and binary identity. Opt-in SceneStub observations inspect
the complete decoded provider request without storing raw system text.
This is operator-approved dispatch, not the agent-mediated 19.26 journey.
Loopback shares a principal; native Normal-mode tool blocking is not proof of
differential taint enforcement or a nonexistent attached generic approval UI.


## Replying to a team message (FR169)

⚑ **Updated 2026-09-29 (Story 19.18).** A thread lives on the answering host: the
original `/team send` mints its root item there, a reply is a durable reply turn
there, and a follow-up is a new item there that names the turn it answers. The
asker pulls that host; there is no reverse `message/send`, push, notification,
background thread poll, or `tasks/get` reply channel.

Use the verb on the rail that holds its object:

- `/team reply <item-id> <text…>` is rail 4 only. It answers an item received
  on this host. Rail 3 refuses it because a recipient item needs a
  daemon-attached session.
- `/team defer <item-id>` is rail 4 only. It records the fixed `Busy — defer
  30m` body as an independent human-written reply on this host. Standalone
  sessions refuse it; it does not resolve an answer card or select context.
- `/team ack <item-id>` is rail 4 only. It records receipt, not an answer.
  Standalone sessions refuse it; viewing or responding does not imply Ack.
- `/team thread <peer-id> <item-id>` and
  `/team followup <peer-id> <turn-id> <text…>` work on rails 3 and 4 through
  their session's A2A egress.
- `/team board <peer-id>` is the asker's discovery step: its correlated item
  row supplies the root item id to paste into `/team thread`.

`/team reply` records a reply turn on the answering host. When the item's
pending peer-response card has a live process-owned controller, it resolves
that card as the operator's own answer (`WriteOwn`) and records one turn;
otherwise it records a direct turn without reviving the task. A later
`ResolvePeerDraft` for the same node in the same
process is an idempotent no-op; after restart it is refused as already settled.
On restart, turns and follow-up links are rebuilt from the journal; the waiting
node still fails, but the item remains answerable with `/team reply`.

On an attached consultation card, Tab explicitly opens Item actions. `[b] Busy
— defer 30m` and `/team defer` use the same trusted-local writer/room-content
gate and journal-backed item lookup. Confirmation follows the durable append.
The pending real-answer card, draft buffer and waiting task remain unchanged;
there is no acknowledgement, deadline, timer, automatic retry or reverse send.
Another deliberate defer creates another fresh reply turn. Busy is also legal
while a live draft is composing.
Defer requires the `a2a` feature, just like the existing item-reply command;
a build without it refuses without recording an unservable turn.

Outside the explicitly focused item/response surface, `y/e/n/b` are ordinary
input and Enter does not implicitly answer the FIFO head. Respond and Edit
require an empty composer, pin their target, and return to Input; `/team` and
`/clear` take precedence over that target. Esc closes the surface and restores
the frozen input; Ctrl+D/Ctrl+C retain attached detach semantics. Only
unmodified lowercase Press events invoke surface actions; reported Repeat,
Release and modified action letters cannot mutate the item.

On an architecture-update Item surface, `[a] Acknowledge` records receipt and
`[d] Discuss` seeds an explicit `/team reply <item-id>` composition. Ack confirms
only after its durable append; a repeated/raced Ack reports already recorded
without another append. Its hint disappears after acknowledgement. It does not
resolve a pending answer or select model context.

Action results carry their originating action and item/node target. Each
pending marker survives closing/reopening the reader until its own outcome;
unrelated completions cannot clear it. Item-turn outcomes show the recorded turn
and pull-only disclosure. Applied node actions without a turn, settled no-ops
and refusals remain distinct. Stale/refused reply composition retains its text
and explicit target; an idempotent Ack consumes its command rather than
restoring it as unsent reply text.

Reply-kind metadata is additive. Missing journal `kind` means Answer and
Answer omits the field; Defer uses `{"type":"defer","minutes":30}`. Served
turns keep structural `kind:"reply"` and add `replyKind:"defer"` with
`deferMinutes`. The sender's explicit thread read marks the responder alias
with `[defer <N>m — reported expectation]`; incoming valid intervals are
1–65,535 minutes, not local timer values. Future or malformed kinds/intervals
retain the body and render `[reply kind not recognised]`, never a peer-chosen
badge or a fabricated interval. Old readers retain the ordinary Busy body.

The answering host records received items (including raw
`x-rustain-in-reply-to` links), reply turns, and the existing draft-resolution
row. The asker records a `PeerReplyObserved` row once for each peer turn it
first reads; its dispatch row carries the sent-content hash used for the
`you` label. A turn is `you` only when both its task id and content hash match
one of this asker's dispatches to that peer. All other item turns, including
ones whose dispatch row predates 19.18 and has no hash, render as
`another sender`.

Thread reads are side-effect free on the answering host and bounded to the root
plus the newest fitting turns: at most 50 turns, an 896 KiB response budget, and
a 448 KiB per-turn budget; an over-budget body is shown as withheld. Reply text
is limited to 65,536 bytes. The root is always present and `omittedBefore`
counts non-root turns. The rendered block is titled `thread · <alias> · <root>`;
it names only the configured peer alias, never a person.

### Thread access with API keys

`server.threadReads` in `.rustain/a2a.json` is `"off"` by default or
`"credential-scoped"` to opt in. An unknown value is treated as off and produces
a startup warning. The opt-in asserts that each peer has its own key; the host
cannot discover whether key holders share one. With the default, an API-key
caller that presents no item capability is refused before a thread lookup or
follow-up mutation. Loopback is always served, and the thread block discloses
its collapsed principal. Story
19.18c's per-item capability nonce (below) shipped the first follow-up
hardening row; Story 19.18d's per-peer credential binding (see
*Binding API keys to pinned peers*) is the next. `threadReads` governs unbound
API-key callers only: a key bound to a pinned peer reads that peer's own legacy
items' threads with `threadReads` off (a capability-bearing item still needs its
capability).

## Sharing status under your policy (Story 19.24)

For `status_request` messages handled by NotifyAndWait, the answering host
prepares a status preview once, at arrival. The member owns
`[interaction.defaults] status_detail_minimum` in `.rustain/a2a-interaction.toml`:

| Level | Shared fields |
|---|---|
| `none` | Nothing. This is the default when the key is absent. |
| `minimal` | Keys of stories this host currently holds. |
| `status` | Those keys plus receipt state: `received` or `acknowledged`. These are receipt facts, not work progress. |

An unrecognised level loads safely as `none`; the policy explainer names the raw
token. Held stories come from the latest readable received story lists per
sender naming the key with holder `You`; removed/retracted items are excluded.
Keys are deduplicated, in holding-item journal order, with at most 64 entries.
The newest holding item supplies the receipt state when senders share a key.

**Never reported at any level:**

- Conversation or transcript content and titles.
- Plans, tasks and their status.
- Files, diffs and paths.
- Tool, model or agent activity.
- Operator activity, idle/busy state and timestamps.
- Any other item's body, subject, sender, alias or pin, including who assigned a story.
- Consent, roster or policy contents beyond the member's own level name.

The request's body, subject and sender cannot select more fields. The statement
is the sharing host's report, not authenticated evidence of work or progress.

On the attached, focused status-request card, `[s] Share status` sends exactly
the stored preview; later assignments and policy changes do not expand it.
`[n] Decline` sends no status. Both are human-decided, agent-composed Status
reply turns, with at most one per item. `[r] Respond` still lets you write your
own Answer, and `[b] Busy — defer 30m` remains available.

Decline, sharing at `none`, and sharing when no stories are held are deliberately
indistinguishable on the wire (apart from ids): `status: {"stories":[]}` and
`No status shared.` No level or local reason is exposed. Non-empty replies use
`replyKind:"status"` with `status: {"level":"minimal","stories":[{"key":"S7-03"}]}`;
at `status`, each story also has `"state":"received"` or `"state":"acknowledged"`.
Withheld content also withholds the status object. Thread and item views mark
the result as status shared under the host's policy, or no status shared.

The requester's `/team board --action <action-id>` (or `--last`) is the report. It pulls the
thread of each row's latest action-correlated item through that row's bound
peer, using the existing reader and capability derivation. It counts only
replies to that item, not sibling or follow-up replies. The report therefore
depends on the same `server.threadReads`, peer-credential binding and per-item
capability rules described above/below; it does not bypass a refused or failed
read. Rows show shared keys and their source, limited-detail disclosure for
`minimal`, and any manually shared Answer as a quoted note. Empty status and
send-time decline show `no status shared` / `Source: —`; unanswered or Defer-only
rows show `awaiting reply` (with the reported defer expectation). Unknown or
withheld status shows `status not readable by this build`, never raw content.
A row whose send never reached an item (sending, awaiting approval, a question
asked, accepted without an item, no usable answer, no outcome recorded, or an
unrecognised outcome), a failed read, unreachable and not sent keep their usual
board labels. There are no participation counts or story-progress aggregates.

The team norm `[team.overrides] status_detail_minimum` in `team-policy.toml`
is **advisory**: the explainer compares narrower/broader/the same and says yours
applies. It never raises or caps your level, prompts for acknowledgement, or
reveals the norm to the requester.

NotifyAndAuto remains **template-only**: it adds no automatic Status reply,
so its report row remains `awaiting reply`. This feature adds no inference
path; NotifyAndDraft retains its existing inference/approval path.

## Per-item capabilities (Story 19.18c, 2026-09-30)

A capability is a keyed value derived from a sent message id. Only the sender's
host can re-derive it; the recipient keeps only its hash. The sender key is
`item-capability.key` in the per-user data directory (`~/.rustain/` or
`RUSTAIN_DATA_DIR`): exactly 32 raw bytes and mode 0600 on Unix. Back it up with
your home directory; losing it makes your earlier items unreachable to API-key
callers. Windows has no permission check.

For API-key callers, a capability-bearing item's thread read, follow-up
reference, retract, `tasks/get`, and `tasks/cancel` require that item's
capability. Loopback is unchanged. Items from older or non-rustain senders are
legacy: thread and follow-up access still follow `server.threadReads`; retract
and `tasks/*` remain credential-scoped. The list remains credential-scoped and
can still show co-holders item ids, states, and task ids; per-peer binding is
Story 19.18d.

If this host's key cannot be read, sends to a credentialed peer are refused
before dispatch and show `⚠ <alias>  unreachable` followed by
`    this host's item key can't be read (<path>: <cause>). Fix its permissions (chmod 600) or restore it; don't delete it.`
Uncredentialed peers send legacy. Repair or restore an existing key file; never
delete it.

For sender-side thread and follow-up resolution, rustain makes one
`x-rustain-items/list` read. Its JSON-RPC body is capped at 1 MiB, so after a
few thousand items `/team thread` and `/team followup` stop resolving. Read a
thread before following up on a reply: an unobserved `rr_` has no reply
capability. A 19.18c follow-up on a pre-19.18c item starts its own thread.

The reply observation records the peer binding so a previously read `rr_`
remains resolvable after a supported rename or pin-add; another local process
can observe it through the shared journal.

Dispatch history follows the 19.18c `PQ2` rule. A pinned peer survives a rename
and a move. An unpinned peer survives either a rename or a move, but not both.
An unpinned alias re-pointed at another host, or an alias reused for a different
peer, receives this host's capabilities for the old host's items. Pin before
dispatch to avoid those cases for new pinned rows; a later pin-add does not
revoke historical endpoint-bearing correlation for that alias.

The pulled thread's `you` label requires this host's matching task **and
content hash**. Its dispatch correlation uses the same PQ2 alias/origin rule:
an unpinned rename at the same origin retains `you`; the same alias retains
`you` after a move, including pin-add plus move. Historical same-identity rows
without an endpoint retain exact task/hash correlation. Changed/missing hashes,
model-rail rows without a human-text hash, and task/hash claims from an unrelated
origin under a different alias remain `another sender`. This is correlation of
this host's sent bytes, not proof of host/personhood continuity. Reply-observation
deduplication is unchanged; no rename-transparent observation-dedup claim.

An older build treats capability-bearing items as legacy and can rewrite
`a2a-pending.json` without the capability hash. There is no capability
revocation (`D3`, `DF-19-18C-CAPABILITY-NO-REVOCATION`) and no key rotation
(`DF-19-18C-CAPABILITY-KEY-ROTATION`).

## The `log: N` reminder

When the transparency log holds rows newer than your last visit, the right edge of the status bar shows a muted `log: N` (`log: 99+` above 99). Your client reads its own workspace journal about once a second; nothing is pushed to it, and no peer, item, task or event name ever appears in the segment. It counts every transparency row of every kind and direction — a teammate's retract of an item you received is one such row — so the count is a reason to look, not a description of what happened.

- **How to clear it.** Open the log unfiltered: `Ctrl+X, L` or `/team log` in the standalone TUI; `/team log` in a daemon-attached client (there `Ctrl+X` retracts this host's automatic sends only, and a read-only attach may still read the log). The reminder clears only once the view is actually drawn on screen. A filtered command (`--filter=…`), a panel with an active search, a failed read, a result scrolled off screen, a panel export, an unrelated key, a restart, or the offline `rustain team log` command never clears it. `--json` and `--export` clear it like the plain view, because the rows were shown — whether or not the export file was written.
- **What clearing means.** It records that this client *presented* that snapshot. It is a local visit reminder: ⛔ not `/team ack`, not `/team remove`, not `/team retract`, not an acknowledgement or a receipt to anyone, and not proof that you read a row. The in-chat `/team log` shows at most 20 rows and says so; presenting it clears the reminder for the whole snapshot it read, while the older rows remain in `rustain team log` and the panel. Clearing does not mean those older rows were read.
- **What it counts.** Rows newer than the durable boundary your last presented visit recorded; rows that arrive after a view was read stay counted until the next visit. On first use every existing row counts. The boundary lives in `.rustain/transparency-seen.json`, is shared by the standalone and attached clients of this workspace, survives restarts, and is never written by polling.
- **`log: ?`** means the reminder cannot currently be trusted — the journal or the preference could not be read, a visit could not be saved, or the journal's head fell below the saved boundary (a reset, which the client confirms and then counts from zero). It is never shown as an apparently current zero; open the log for details.

## Trust tiers

Trust comes only from configuration; an AgentCard cannot promote itself.

| Configuration | Registry trust | Behavior |
|---|---|---|
| No `pinnedKey` | `Unverified` | Fetch over HTTPS, validate required fields, register inventory stamped `Unverified` |
| Ed25519 `pinnedKey` | `Verified` | Require a valid EdDSA JWS over the raw card before caching or registration |
| Unsupported or unusable pin | none | Fail startup; never silently degrade to `Unverified` |

An unverified card can surface in the internal inventory because the operator explicitly allowlisted its origin. It still cannot enter the `@` dropdown. A pinned peer with a missing, forged, tampered, wrong-key, wrong-algorithm, or wrong-`kid` signature never surfaces: removing `signatures` is treated as a downgrade attempt.

Revocation is removal of the peer from `.rustain/a2a.json` or the active profile. The A2A specification supplies no implementable key-expiry or revocation mechanism for this flow.

## Pinning an Ed25519 peer

1. Obtain the peer's public key through an operator-trusted channel. Do not trust a `jku` URL supplied by the card itself; rustain never fetches it.
2. From the peer's JWK, require `kty: "OKP"`, `crv: "Ed25519"`, and `alg: "EdDSA"`.
3. Copy the JWK's base64url `x` value into `pinnedKey.x`.
4. If the JWK has a `kid`, copy it into `pinnedKey.kid`. A configured `kid` must match the protected JWS header.
5. Restart rustain. An invalid base64 value, wrong key length, or unsupported algorithm is a boot error.

Do not paste a full JWK and do not configure `jku`; the allowlist and pin are the trusted inputs.

## Fetch and validation policy

Rustain requests exactly `<peer-base>/.well-known/agent-card.json` with a 30-second timeout, a five-redirect cap, per-hop URL validation, JSON content-type enforcement, and a 1 MiB body cap. HTTPS is required except for loopback HTTP used by local/manual tests.

The decoder accepts unknown vendor fields and both measured v0.3/v1.0 card shapes. It explicitly requires card `name`, a present `skills` array, and each skill's `id` and `name`. Signature verification canonicalizes the raw JSON value using RFC 8785 JCS; it never verifies a typed-struct round trip.

## Offline verification

Pinned fixtures and provenance live under `tests/fixtures/a2a/`. Re-run real captured signatures and controlled mutants without network access:

```bash
./tests/fixtures/a2a/REVERIFY_REAL_SIGNATURES.sh
./tests/fixtures/a2a/REPRODUCE_TEST_SIGNATURE.sh
```

The deterministic Ed25519 seed in that directory is marked TEST-only and must never be used in production.

## Serving A2A (`--serve-a2a`)

Rustain can also *be* an A2A agent: `--serve-a2a=ADDR` exposes a signed AgentCard
and a JSON-RPC endpoint. Two shapes, and the difference is not cosmetic:

```bash
# Discovery only. No execution core, so every inbound task is refused with a
# policy verdict that says so.
rustain --serve-a2a=127.0.0.1:8080

# Full: the listener runs inside the daemon lifecycle, sharing its node tree,
# core and event bus. Inbound tasks execute as local peer nodes.
rustain --serve-a2a=127.0.0.1:8080 daemon start
```

`--serve-a2a` may be combined only with the daemon actions that start its
lifecycle (`daemon start`, including its internal `__run` child), and with
nothing else. It is refused for `stop`, `status`, `attach`, `install`, and
`uninstall` rather than silently discarding the listener request.

For `daemon start`, the PID readiness marker is written only after the A2A
listener has passed configuration, any required TLS, and bind startup. If the
listener cannot start, daemon startup fails instead of reporting a ready daemon
with no listener.
After that handshake, an unexpected listener exit is recorded as a daemon error.

Standalone discovery-only refusals are also appended through the workspace's
canonical room journal. They do not use an inert or separate transparency sink.

### Server configuration

The `server` block of `.rustain/a2a.json`:

```json
{
  "server": {
    "admission": "ask",
    "apiKeyEnv": "RUSTAIN_A2A_API_KEY",
    "apiKeys": ["RUSTAIN_A2A_API_KEY_NEXT"],
    "peerKeys": {
      "RUSTAIN_A2A_KEY_ALICE": "alice-dev",
      "RUSTAIN_A2A_KEY_BOB": { "peer": "bob-ci", "adoptPreBinding": true }
    },
    "requirePeerBinding": false,
    "advertisedHost": "a2a.example.com:8443",
    "tls": { "cert": "certs/server.pem", "key": "certs/server.key" }
}
```

| Key | Values | Meaning |
|---|---|---|
| `admission` | `deny` (default), `ask`, `allow` | What to do with a task from a remote agent |
| `apiKeyEnv` / `api_key_env` | env var **name** | Legacy primary API-key variable. It remains honored. The key itself never lives in the file |
| `apiKeys` / `api_keys` | array of env var **names** | Additional API keys. The effective set is the union with `apiKeyEnv`; each configured key is a distinct submitting principal |
| `peerKeys` / `peer_keys` | map of env var **name** to a roster alias, or to `{ "peer": "<alias>", "adoptPreBinding": bool }` (`adopt_pre_binding`, default `false`) | Binds an inbound API key to one **pinned** roster peer. Several names may bind one alias. A name is either bound here or unbound in `apiKeyEnv`/`apiKeys`, never both. See *Binding API keys to pinned peers* |
| `requirePeerBinding` / `require_peer_binding` | `false` (default), `true` | Startup validation: every configured key must be bound in `peerKeys`, so no unbound caller can exist |
| `threadReads` / `thread_reads` | `off` (default), `credential-scoped` | Thread and follow-up access for legacy items from API-key callers |
| `advertisedHost` / `advertised_host` | public `host[:port]` authority | Authority published in the AgentCard. Required for `0.0.0.0` or `::` binds |
| `tls.cert` / `tls.key` | workspace-relative PEM paths | Certificate chain and private key |

`admission` defaults to `deny`. An endpoint that starts executing strangers'
instructions the moment it is reachable is a footgun, so enabling execution is
an explicit act. `ask` prompts the operator; `allow` does not.

Note that `admission` is deliberately **not** `[subagents] auto_approve`. That
knob governs subagents you launched; inheriting it here would silently hand a
network peer the auto-approval you granted to your own local work.

### Loopback vs the network

Loopback (`127.0.0.0/8`, `::1`, `localhost`) serves plaintext and
unauthenticated: an attacker who can reach it already runs code on your machine.

**Any other address requires TLS *and* at least one API key *and* a signed
identity — together.** Configure only some of them and rustain refuses to bind,
naming what is missing. There is no flag to serve a non-loopback address in the
clear.

Wildcard binds (`0.0.0.0` or `::`) additionally require `advertisedHost`. Set it
to the reachable authority clients should use, such as
`a2a.example.com:8443`; it is published in the AgentCard instead of the
unroutable wildcard. Use a hostname covered by the TLS certificate.

Present one configured key as `x-api-key: <key>`. An `Authorization: Bearer …`
token is treated as *no credential* rather than a wrong one: OAuth2 is not an
accepted scheme yet (`DF-18-1-OAUTH2`), and telling a client its key is invalid
when it never sent one is a lie. The served AgentCard publishes
`securitySchemes` and `security` describing exactly what the server enforces, and
the card itself stays reachable unauthenticated — gating it would gate the
document that explains how to get past the gate.

### Binding API keys to pinned peers (Story 19.18d)

`server.peerKeys` lets the operator bind each inbound API key to one roster peer
that declares a `pinnedKey`. A call made with a bound key is attributed to that
pinned peer instead of to a pseudonym derived from the key. A binding is the
operator's assertion that the holder of the key is that peer: it records
possession of the key, never the person. Several keys may be bound to the same
peer; they are then one principal, so moving a peer from one bound key to
another, with both configured, keeps its items, tasks and consent. Two aliases
that share one pin are also one principal, but each item records the alias of
the key that was actually presented.

Both forms are equivalent in meaning; the object form adds the adoption switch:

```json
"peerKeys": {
  "RUSTAIN_A2A_KEY_ALICE": "alice-dev",
  "RUSTAIN_A2A_KEY_BOB": { "peer": "bob-ci", "adoptPreBinding": true }
}
```

Startup refuses, in this order and naming environment variable names and aliases
(never values): a name in both `peerKeys` and `apiKeyEnv`/`apiKeys`; an alias
that is not in the roster; an alias with no pinned key; `requirePeerBinding`
with an unbound key; an unset or empty variable; and one key value bound to two
different bindings (same alias, peer and `adoptPreBinding` is accepted).

The roster is the active profile's `a2a.json`, for the daemon and for standalone
`--serve-a2a` alike. An absent or empty roster fails as a missing alias. A
refusal on `daemon start --foreground` and on standalone `--serve-a2a` reaches
stderr (`rustain: daemon failed: <error>` and `rustain: A2A server failed:
<error>`); a detached `daemon start` still shows the child's refusal in the log
only.

**Listener behaviour.** Bindings take effect on a non-loopback listener only. A
loopback listener ignores the keys: every caller is the one local principal, and
a bound key configured there logs `A2A peer bindings: this server listens on
loopback, where every caller is one local principal; server.peerKeys takes effect
only on a non-loopback listener.` A bound-only configuration counts as having an
API key for the non-loopback bind check.

**What a bound caller sees.** Keys bound to different peers are separate
principals on list, thread, follow-up, retract, `tasks/get` and `tasks/cancel`;
a foreign id answers the same not-found as a never-existing id. Successful list,
thread and retract answers to a bound caller carry `principalBound: true` and
`principalCollapsed: false` (an additive field; unbound and loopback answers are
byte-identical to before). A sender omits its "one credential" sentence for a
host that answers `principalBound: true`. A bound caller is not refused by
`threadReads = off` for its own legacy items; a capability-bearing item still
needs its item capability, and a missing or wrong one is the same not-found.
The startup posture lines about shared keys count only the unbound keys.

**Adoption.** Items received under a key before it was bound belong to that
key's pseudonym and are not visible to the bound peer. They stay in the journal,
and startup logs the count: `A2A recipient items: {count} item(s) belong to
identities that no caller of this listener has, so no caller can list, read or
retract them; they stay in the journal. Off loopback, configuring the original
API key again (unbound, or in server.peerKeys with adoptPreBinding) or binding
any key to the same pinned peer makes them reachable.` With
`adoptPreBinding: true`, every key bound to that peer reads the legacy items of
that same key value: a read union for list, thread and retract, and the
recovered tasks of `tasks/get` and `tasks/cancel`, in one global journal order.
Adoption does not change the capability-class edge: a legacy follow-up joins an
adopted legacy thread, while a capability-bearing follow-up of a legacy root
starts its own root. The opt-in was renamed to `adoptPreBinding` before its
first release; no other spelling is read. A bound peer's items and a RAP peer's
items of the same pin stay separate (`DF-19-18D-CROSS-RAIL-PRINCIPAL`).

**Adopted tasks.** Bindings apply only at startup, and startup fails every task
that was in flight. With adoption, `tasks/get` and `tasks/cancel` from any key
bound to the peer also reach the original key's tasks recovered at the restart
that applied the binding: they answer `failed` with the restart reason, and
`tasks/cancel` changes nothing. A capability-bearing task still needs its item
capability; a missing or wrong one is the same not-found. No live task is ever
adopted. Recovered tasks last until the next restart or until evicted, and tasks
that had already finished before the restart are not kept; a standalone
`--serve-a2a` start recovers only approval-pending tasks, so an admitted task
surfaces at the next daemon start. The bound peer's own task with the same id
takes precedence, and an id that two adopted keys both used answers not-found.
Without adoption, earlier tasks answer not-found. New tasks are always created
under the bound peer. This is visible to an A2A client that polls or cancels
after the host restarts.

| Change applied at a restart | Who reads the earlier tasks through `tasks/get` / `tasks/cancel` |
|---|---|
| Unbound key bound to a peer **with** `adoptPreBinding` | every key bound to that peer: the recovered `failed` projection; cancel changes nothing |
| Unbound key bound **without** adoption | nobody: not-found (its items are unreachable likewise) |
| Two keys bound to one peer, only one adopting | both read the adopting key's recovered tasks; neither reads the other key's pre-binding tasks |
| A bound key moved from peer P to peer Q | Q never reads P's tasks; P's tasks recovered at that restart stay readable by keys still bound to P |
| A key removed from the configuration | its unbound tasks are unreachable; its bound peer's tasks stay readable by that peer's other keys |
| Any later restart | the recovered projections are gone; finished tasks are never kept |
| The key's value rotated at the binding restart | nothing is adopted: adoption follows the key value |

The orphan count also includes bound items when every key bound to their pin is
removed. On a loopback listener, all bound items and all non-loopback API-key
pseudonym items count as unreachable, because configured keys are ignored.
On a non-loopback listener, binding any key to the original pin restores access
to its bound items. Legacy pseudonymous items instead require the original key
value, configured either unbound or bound with `adoptPreBinding: true`; binding
a different key to the same pin does not adopt that original key's items.

**Older builds and changes (disclosed).**

- (a) An older build ignores `peerKeys`: a bound-only configuration has no key
  there and refuses to start off loopback; a mixed configuration answers a
  bound-only credential 401.
- (b) Journal addresses of bound items, on received items and on reply turns,
  read as unknown in an older build, one item at a time, and are not served
  there.
- (c) Binding a key that was unbound, without adoption, leaves its earlier items
  unreachable and its earlier tasks answering not-found; with adoption, its
  tasks recovered at that restart answer `failed` with the restart reason.
- (d) Two aliases with one pin are one item and consent principal; distinct keys
  journal their own matched aliases, and one key value under two different
  aliases is refused.

A restart keeps identity, not execution: a task in flight at restart becomes a
failed projection under the same bound handle, and another key bound to that
peer may query it. A task in flight under an unbound key that the same restart
binds with adoption becomes a failed projection under that key's own handle,
which every key bound to the peer may query.

### Task lifecycle

| Method | Behaviour |
|---|---|
| `message/send` | Admits the task and answers immediately. Never blocks on a human |
| `tasks/get` | Current state, scoped to the submitting credential (a bound key with adoption also reaches its adopted keys' recovered tasks) |
| `tasks/cancel` | Cancels the running turn, scoped to the submitting credential (a bound key with adoption also reaches its adopted keys' recovered tasks) |

Under `admission: "ask"`, `message/send` answers `auth-required` and the client
polls `tasks/get`; the operator's decision arrives out of band. The request is
never held open across a keypress — it would hit the 30-second request deadline
and each retry would queue another prompt.

`tasks/get` and `tasks/cancel` for a task belonging to another credential return
exactly what a task that does not exist returns. That is deliberate: telling
"not yours" apart from "not found" would let a peer enumerate task ids and map
the host's federation for free.

If the host restarts mid-task, a task whose capability class can be recovered
resolves to `failed` with a restart reason — never a zombie `working`. If the
pending record is missing and no item holds its class, the orphan is not
restored as a legacy task: `tasks/get` reports not found. Durable resumption
is not implemented (`DF-18-1-HOSTRECONCILE`).

### JSON-RPC profile (narrow, and stated)

- A **notification** (a request object with no `id` member) receives `204 No
  Content`.
- An explicit `"id": null` is rejected with `-32600`.
- **Batch arrays are not supported**; send one request object per HTTP POST.
- The interface speaks A2A 0.3, and the AgentCard advertises 0.3. An absent or empty `A2A-Version` (header, or query parameter when the header is absent or empty) means 0.3, and a valid patch component is ignored. Any other or malformed version returns JSON-RPC `-32009` over HTTP 200, and this is answered even for a notification.

### What a remote peer can see

Served payloads carry capability-scoped projections only: no filesystem path, no
workspace root, no system prompt, no internal node/room/journal state. This is
enforced by the *type* of the projection, not by a redaction pass, so it holds
even against a modified requesting client.

## Sender consent and typed grants (Story 19.18d0)

Sender-consent grants saved by the approval card's **Always** action now carry a
`principal` kind:

- `pinned`: a sender identified by a roster pin. Either a RAP sender whose
  identity the transport authenticated (`via` RAP), or an inbound A2A caller
  whose API key the operator bound to that pin in `server.peerKeys` (`via`
  bound A2A: the operator's assertion that the key holder is that peer).
- `pseudonymous`: an A2A submitter identified by the loopback or API-key handle.

These are separate consent slots even when their `PeerId` hex labels are
identical. Trusting the A2A pseudonym does not trust the colliding RAP principal,
and trusting the RAP principal does not trust the A2A pseudonym. Per-sender
policy overrides, including their implied consent, apply to pinned principals
(RAP senders and operator-bound A2A callers of that pin); an unbound A2A
pseudonym cannot inherit one by matching its label. An override applies only when
it is applicable to the sender and not in conflict with another override. Global
and message-type policy still apply.

Existing grants without `principal` remain **legacy grants**. They still admit
RAP and unbound A2A callers exactly as before, unless that caller has an explicit
typed-slot decision. Loading or using a legacy grant does not rewrite it
as a typed grant. Newly saved sender-consent grants are typed; this is not a
migration of old journal rows.

`/team untrust <alias-or-peer-id>` revokes every stored kind for that peer:
pinned, pseudonymous and legacy. A later typed grant re-trusts only its own
kind. There is no grant verb: standing trust is granted only from a sender's
consent card (`[a] Always allow`). The retired `/team trust` name refuses with
that explanation; use `/team status` to read standing consent.

From a shell, `rustain team status` and
`rustain team untrust <alias-or-peer-id>` are the offline twins of those TUI
commands, using the same durable room journal, active-profile/workspace roster
and status renderer. Neither needs a daemon or provider. Status is text-only
and creates no workspace files; an untrust with no active grant changes nothing.
A running daemon applies a CLI revoke at its next admission or status read,
without restarting. Status and untrust refresh the journal authoritatively and
refuse without reporting a cached success or mutating trust when that read
fails; admission remains best-effort and may continue using the cached consent
snapshot.

`/team status` keeps one row per roster alias, including aliases sharing a pin,
plus one row per distinct journal-only peer. Multiple consent slots do not
create duplicate peer rows. The display aggregates them as trusted if any
slot is trusted, otherwise revoked if any is revoked, otherwise no grant;
that display aggregate is not the admission decision.

Approval-source provenance is carried in memory. Its serialized `RemotePeer`
shape remains unchanged, and decoding it does not reconstruct trusted
provenance from `peer_id`.

### Consent for a bound caller (Story 19.18d)

A bound caller's consent identity is the pin it is bound to. A typed `pinned`
grant therefore covers every key bound to that peer and the same peer over RAP,
and a grant written through either path holds on the other. A legacy grant
(one saved before typed grants) never admits a bound caller, whichever kind of
caller it was recorded for, so the first call after a key is bound asks once
again; legacy grants still admit RAP and unbound A2A callers exactly as before.

A RAP sender and the bound callers of one pin share **one** pending consent card
and one answer: answering it releases all of them. The card's wording and
instruction are those of whichever delivery arrived first. A bound caller's card
says `[a] trusts the pinned peer this key is bound to, not the person; every key
bound to that peer shares this decision.`; the cards for RAP and unbound callers
are unchanged.

The startup warning about auto-response authority is no longer silenced by a
legacy grant for a pin that a `server.peerKeys` entry reaches; a typed pinned
grant or a per-sender override silences it.

**Downgrade caveat:** an older build ignores `principal` and folds a typed grant
as a bare grant. Its reach then equals pre-19.18d0 behavior; typed separation
does not survive that downgrade.
