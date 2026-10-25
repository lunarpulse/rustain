# Running rustain as a supervised daemon

`rustain daemon` runs the agent headless for a single workspace. Story 12.1b adds
**service-manager supervision** (systemd / launchd auto-restart on crash, NFR50) and
a **crash post-mortem** record. This doc covers install, uninstall, and reading crash
state. (Unix only — Linux is P0, macOS P1; Windows is not supported, NFR33.)

> **Scope.** "Crash recovery" here means **detect the unclean exit → persist a
> post-mortem record → announce it on restart**. It does NOT replay conversations or
> re-run a missed flush — there is no message runtime until Story 12.2. Durability of
> your daily log is already guaranteed by Epic 11 / Story 12.0; the fresh daemon
> composes fresh memory that loads it.

## The load-bearing fact: supervisors run the **foreground** entrypoint

The generated unit/plist always invokes:

```
rustain --profile <profile> daemon start --foreground
```

`--foreground` runs the lifecycle loop in-process so the supervisor owns the daemon
directly. A bare `daemon start` re-execs and `setsid`-detaches a child, so the
launcher would exit immediately and the supervisor would restart-loop the launcher
instead of supervising the real daemon. The installer never generates a bare
`daemon start`.

## Install + enable

`rustain daemon install` detects your platform and renders the correct service file
(systemd unit on Linux, launchd plist on macOS). It embeds the **absolute** exe path,
the workspace (cwd), the active profile, and the user. The file name embeds a
workspace hash, so multiple workspaces install non-colliding services.

```sh
# Inspect what would be written (stdout only — no file written):
rustain daemon install --print

# Write it to the per-user location, then run the printed follow-up:
rustain daemon install
```

### Linux (systemd)

- **User scope (default, no root):** writes
  `~/.config/systemd/user/rustain-<hash>.service`, then run:

  ```sh
  systemctl --user daemon-reload && systemctl --user enable --now rustain-<hash>.service
  ```

- **System scope (`--system`, needs root):** writes
  `/etc/systemd/system/rustain-<hash>.service` with `User=<you>`, then run:

  ```sh
  sudo systemctl daemon-reload && sudo systemctl enable --now rustain-<hash>.service
  ```

The unit declares `Restart=on-failure` + `RestartSec=2` (restart on crash, **not** on
a clean `stop`) and a crash-loop guard (`StartLimitIntervalSec` / `StartLimitBurst`)
so a daemon that crashes immediately on boot does not spin forever.

### macOS (launchd)

Writes `~/Library/LaunchAgents/com.rustain.<hash>.plist`, then:

```sh
launchctl load ~/Library/LaunchAgents/com.rustain.<hash>.plist
```

`KeepAlive = { SuccessfulExit = false }` relaunches the daemon **only** on an unclean
exit, so a clean `stop` is honored. `RunAtLoad = true` starts it at load/login.

### Env pass-through

If `RUSTAIN_DATA_DIR` / `RUSTAIN_CONFIG_DIR` are set when you run `install`, they are
baked into the unit (`Environment=` / launchd `EnvironmentVariables`) so test/CI
overrides survive under supervision. Default installs omit them and rely on `$HOME`.

## Uninstall

```sh
rustain daemon uninstall            # matches the default --user scope
rustain daemon uninstall --system   # matches a --system install
```

`uninstall` first prints the disable/unload step you should run, then removes the
service file. It is **idempotent** — a second uninstall (file already gone) exits 0 as
a no-op. It touches **no** daemon runtime state (PID file / socket / crash records).

Disable/unload first:

```sh
# Linux user / system:
systemctl --user disable --now rustain-<hash>.service
sudo systemctl disable --now rustain-<hash>.service
# macOS:
launchctl unload ~/Library/LaunchAgents/com.rustain.<hash>.plist
```

## Session-boundary memory hooks (Story 12.1c)

The daemon fires a **`SessionBoundary`** on each of `daily_reset`, `idle_timeout`
(configurable), and graceful `Shutdown`. All three route through **one** code path
(`emit_session_boundary`) — never a parallel path — which, after finalizing the daily
log, drives three memory hooks:

- **`on_session_end` (recall).** The (optional) `RecallProviderPort` is invoked
  unconditionally at every boundary. The default is an explicit offline no-op
  (`NoopRecallProvider`); the headless daemon has no message runtime until attach
  (Story 12.2), so the transcript is **empty** (logged honestly — the emptiness comes
  from the missing source, not from the provider short-circuiting).

- **MEMORY.md file-edit auto-honor — hand-edit = consent, purged LIVE.** When you
  hand-delete a fact from `{workspace}/.rustain/MEMORY.md`, the daemon detects it on
  reload and **purges that fact from the search index immediately** — the deletion is
  the consent, so there is **no confirmation prompt**. The purge funnels through the
  exact same `RedactionRecord` + `refresh()` redaction sink that `/memory forget` uses
  (a content-stable token suppresses the fact across both the `MEMORY.md` copy and its
  daily-log re-derivation). "Never silent" is satisfied by a durable **audit notice**
  (`memory-md-purge-notice.json`) surfaced at the next attach — NOT by withholding the
  purge. **Daily logs are never deleted.** Under a non-vector memory profile there is
  no search index to purge, so this is a documented no-op (the hand-edit still
  self-heals the curated copy on reload).

- **Consolidation suggested at session end.** The daemon cannot run an LLM
  consolidation sub-turn (it has no provider headless), so it **queues** a durable
  "consolidation-due" marker (`consolidation-queue.json`, latest-only) referencing the
  daily-log slice to consolidate. It is **never auto-applied**. When a TUI later
  attaches (Story 12.2), the marker surfaces through the existing 11.2a
  propose→confirm consolidation card — no new approval grammar, and again **daily logs
  are never deleted**.

Both queue files live under `{workspace}/.rustain/`, are written atomically
(temp→rename), and are latest-only so repeated boundaries don't grow them.

## Crash records & recovery

When the daemon (re)starts and finds a **leftover PID file whose process is dead**, it
treats that as an unclean exit (graceful shutdown always removes the PID file, so a
leftover one means the previous instance died via panic / SIGKILL / OOM / power loss).
It records the crash, logs a recovery line, then starts normally — it never refuses to
start or requires manual cleanup.

Two artifacts, both under `{workspace}/.rustain/`:

- **`daemon-crash.json`** — the latest crash only (machine state `daemon status`
  reads). Overwritten atomically each crash. Carries a bounded `restart_count` and the
  recent crash timestamps (`last_n_crash_unix`, cap 5) so a crash **loop** is visible
  in a single read without unbounded growth. `reason` is `"stale-pidfile"` for
  restart-side detection or `"panic: <message>"` for the panic path.
- **`crash-<ts>.log`** — immutable timestamped backtrace files (the forensic trail,
  written by the panic path). Capped to the newest 10 so a tight crash loop can't fill
  the disk.

Read the last crash via `daemon status`:

```sh
rustain daemon status          # human: a "Last crash:" line ("none" when clean)
rustain daemon status --json   # scriptable: a "last_crash" object (null when none)
```

`last_crash` / `restart_count` are how you diagnose a flapping daemon: a high
`restart_count` with tightly-clustered `last_n_crash_unix` is a crash loop — check the
`crash-<ts>.log` backtraces (panic) or `journalctl --user -u rustain-<hash>` (systemd).

## PID-ownership hardening (no innocent kills)

After a crash the dead PID can be **recycled** by an unrelated process. `stop` /
`status` / the already-running guard verify ownership before acting: the PID file
records a self-authored **nonce** and the **boot id**, and both platforms introspect
the live process before acting:

- **Linux:** `/proc/<pid>/comm` exact-match + `/proc/<pid>/cmdline` daemon-body token
  + `/proc/<pid>/environ` nonce round-trip. Zombie detection via `/proc/<pid>/stat`.
- **macOS:** `kinfo_proc` via `sysctl(KERN_PROC_PID)` for `p_comm` exact-match +
  `p_stat == SZOMB` zombie detection + `kern.boottime` boot-id; `KERN_PROCARGS2` via
  `sysctl` for `cmdline_args` + `env_nonce` round-trip through a pure
  `parse_procargs2` parser (compiled and fixture-tested on all targets).

A recycled/foreign PID is treated as **stale** (reclaimable), never `Running` — so
`stop` never signals an innocent process. `ownership_ok` precedence is unchanged.

## Platform support matrix

| Platform | TUI | Daemon supervision | CI evidence |
|----------|-----|-------------------|-------------|
| **Linux** (Tier 1) | Full | systemd `Restart=on-failure`, CI-proven | `ci.yml` unit tests every PR + `nightly-daemon.yml` systemd L3 weekly |
| **macOS** (Tier 2) | Builds + unit-tested | launchd `KeepAlive` — see NFR50 outcome below | `ci.yml` `macos-unit` job + `nightly-daemon.yml` launchd L3 weekly |
| **Windows** | Not supported (NFR33) | Not supported | — |

## Verifying auto-restart (NFR50)

- **Generated policy** (every CI commit): template-content unit tests assert the
  rendered unit/plist carries `--foreground`, `Restart=on-failure` / `StartLimit*`,
  and launchd `KeepAlive { SuccessfulExit = false }`.
- **Detect + record + announce** (every CI commit): an in-process simulated
  crash-recovery cycle test (dead PID in the PID file → start → assert the crash
  record + `status --json last_crash` + normal startup).
- **Real init system — systemd** (`nightly-daemon.yml` `daemon-systemd` job):

  ```sh
  cargo test --test daemon daemon_crash_recovery::daemon_real_systemd_recovery_system -- --ignored --nocapture
  ```

  Uses `--system` scope + passwordless `sudo` on `ubuntu-latest` (no user session
  needed). Installs the unit, `kill -9`s the daemon, asserts systemd relaunches it AND
  the relaunched instance records the crash. Wait budgets are env-overridable
  (`RUSTAIN_L3_PID_WAIT_SECS`, `RUSTAIN_L3_CRASH_WAIT_SECS`).

- **Real init system — launchd** (`nightly-daemon.yml` `daemon-launchd` job):

  ```sh
  cargo test --test daemon daemon_crash_recovery::daemon_real_launchd_recovery -- --ignored --nocapture
  ```

  Bootstraps the plist into `gui/$(id -u)`, `kill -9`s the daemon, asserts launchd
  relaunches it. 60s poll budget accounts for launchd's default `ThrottleInterval`.

- **Local `--user` test** (for real-session/local runs):

  ```sh
  cargo test --test daemon daemon_crash_recovery::daemon_real_systemd_recovery -- --ignored --nocapture
  ```

### CI lane signal policy

- One automatic retry per job maximum (green-on-retry logged with cause in the
  `$GITHUB_STEP_SUMMARY`).
- ENVIRONMENT-gap panics (precondition guard fired) are waivable with written cause.
  **Assertion failures are never waivable.**
- Two consecutive scheduled reds = quarantine issue filed + gate RED.
- `continue-on-error` is banned — an ignored lane gets deleted, not muted.

### Epic-12 close gate

One green `workflow_dispatch` run of `nightly-daemon.yml` (both jobs, job-level reads)
is required before Epic 12 closes. The evidence table in the Dev Agent Record captures
run URL + run ID + commit SHA for each lane. Escape hatch: if two distinct cron
cadences ever become necessary, split into two workflow files rather than reintroduce
schedule-string `if:` guards.

## Templates

The checked-in templates the generator embeds (`include_str!`) live in
[`dist/`](../dist):

- [`dist/rustain.service.template`](../dist/rustain.service.template) (systemd)
- [`dist/com.rustain.daemon.plist.template`](../dist/com.rustain.daemon.plist.template) (launchd)

## Telegram channel setup (Story 12.3)

Telegram is a **daemon-only** channel adapter. The local TUI continues to use the
`terminal` channel; Telegram messages enter through the headless daemon and appear in
attached scrollback with the `[telegram]` prefix.

1. Create a bot with Telegram's **BotFather** and copy the bot token.
2. Add a channel adapter to the active profile:

   ```toml
   [channels]
   adapter = "telegram"

   [channels.config]
   bot_token = "7123456789:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi"
   allowed_chat_ids = [123456789]
   ```

3. Prefer keeping the token out of profile files in production:

   ```sh
   export TELEGRAM_BOT_TOKEN="7123456789:..."
   ```

   The profile still needs `[channels] adapter = "telegram"` and
   `allowed_chat_ids = [...]`; the environment variable only overrides `bot_token`.

Build/install with the feature enabled:

```sh
cargo install --path . --features telegram
```

Only `allowed_chat_ids` are accepted. Unknown chats are ignored. Non-text Telegram
messages receive a short "Text messages only for now" reply; media support is a future
adapter extension.

## Cron scheduler setup (Story 12.4)

Cron is a **daemon-only** scheduler adapter behind `--features cron`. The local TUI
continues to use `[scheduler] adapter = "none"`; the daemon can run scheduled jobs
and surfaces completed results to attached clients with `[cron]` origin plus an
inline `[cron: job-name]` label.

Build/install with the feature enabled:

```sh
cargo install --path . --features cron
```

Select the adapter in the active profile:

```toml
[scheduler]
adapter = "cron"
```

Jobs live in a separate config file:

```toml
# ~/.config/rustain/cron.toml
[[jobs]]
name = "morning-briefing"
schedule = "0 9 * * *" # 09:00 every day, five-field cron, 0=Sunday
prompt = "Summarize yesterday's commits and today's calendar"
forward = true          # optional: forward result to the loaded channel

[[jobs]]
name = "hourly-healthcheck"
schedule = "0 * * * *"
prompt = "Run the project health checks and report failures"
# forward defaults to false: result is stored in the cron session and surfaced on attach
```

Missing or malformed `cron.toml` does not crash the daemon; it logs and runs with
zero scheduled jobs. Invalid job schedules are skipped individually.

Reload after editing `cron.toml`:

```sh
kill -HUP $(cat <workspace>/.rustain/daemon.pid)
```

`rustain daemon status` shows configured jobs and their next local run when the
cron feature is compiled. `forward = true` is best-effort and uses the loaded
channel adapter's default destination (for example, Telegram's first allowed chat);
without a channel, results are still persisted and visible on attach.

## Attach & turn runtime (Story 12.2b)

Since 12.2b the daemon is a **headless turn-processing agent**, not just a memory
process. It holds one per-process conversation and drives turns over a framed,
versioned wire protocol on its Unix socket.

The [agent-proposed message card](a2a.md#agent-proposed-messages-standalone-and-attached-tui)
uses the daemon's own A2A egress and a construction-owned, in-memory proposal
registry. Raising returns immediately without holding the turn lock. Only the
authenticated TrustedLocal read-write writer that opts in to proposal decisions
receives the exact snapshot and can choose Send or Dismiss. The rich TUI opts in
after `AttachAck`; `--plain` does not. Review is local and sends nothing; it
closes any received-item overlay and returns a scrolled transcript to the current
proposal. A generic `ApprovalRequest`, YOLO, grants, subagent policy, peer context
or elapsed time never authorizes a proposal. No capable writer (including plain,
read-only or Peer-only attachments) means refusal at raise, not a retained
invisible proposal.

Detach or replacement retains a pending proposal and re-emits it when the next
trusted writer opts in. Conversation reset/close, shutdown and process restart withdraw
it without peer I/O. A full writer queue produces an explicit terminal protocol
error and closes the connection, retaining the proposal for the next writer;
this same terminal-close/retention rule also applies to exact tool approvals.
Saturating the eight-send limit refuses Send while leaving its proposal pending for an
explicit human retry. Decisions enter the operator FIFO and stale-writer fence;
duplicate or unknown decisions cannot create a second send.

An accepted Send retains its deciding response endpoint across connection EOF;
writer-replacement and decision-identity fences still apply. Disconnecting is not
a second authorization and cannot retarget a later proposal.

Received-item cards prefer a configured policy label, then the durable bound-ingress
alias when present, then the original principal label. The alias is display-only:
it changes neither the policy principal nor consent and does not assert a person's
identity. When an inline card reduces the transcript area, PageUp/PageDown use the
remaining painted height so Review text is not skipped between pages.

### Attaching a client

```sh
rustain daemon start          # start the background daemon
rustain daemon attach         # connect the rich multi-channel TUI (default)
rustain daemon attach --plain # connect the minimal line-based client (scripting)
```

`attach` performs the protocol handshake and opens the **rich multi-channel TUI**
(`run_attached`, Story 12.2c): the daemon's full current-session transcript
rendered bottom-anchored with an honest `— session start — · Earlier sessions not
loaded` top boundary, dimmed `[terminal]`/`[telegram]`/`[cron]` channel-origin
prefixes in attach-mode scrollback (provenance that survives re-attach), an
`attached` status indicator, and an `Esc/Ctrl+D/Ctrl+C detach` hint. Type a
message and press Enter to submit; **`Esc`, `Ctrl+D`, or `Ctrl+C` detaches** —
there is no `daemon detach` CLI verb (detach is a client keybinding).
The daemon and any in-flight turn keep running across detach (the turn is driven on
the daemon-owned event bus, not the socket, so it completes and persists).

The TUI holds **no local agent core** — it forwards each turn as a
`ClientFrame::UserMessage` (the `SocketTurnDriver`, the second `TurnDriver` impl)
and renders the daemon's streamed events through the same `reduce()` path the local
TUI uses. Memory recall rides a normal daemon turn (no client provider needed).

**Multi-attach** is read-only: the first trusted-local writer holds the write slot;
later attaches are granted `ReadOnly` (daemon-enforced — ordinary read-only writes
are refused with `ProtocolError::ReadOnly`; tool decisions use `ApprovalRefused`).
A read-only client shows a persistent `read-only — another client holds write` status segment, a one-time notice, and an
inert (dimmed `read-only — can't send here`) input box. Promotion happens by the
next *new* attach after the writer detaches.

On writer-attach the daemon drains the Story 12.1c boundary queues onto the event
stream: a MEMORY.md **purge notice** (`N facts removed from MEMORY.md …`, consumed
once) and, when a consolidation-due marker is pending, the **rich consolidation card**
(Story 12.2d). The daemon generates proposals with its own provider (post-`AttachAck`,
off the NFR49 handshake path), retains them keyed by the marker's `queued_at_unix`, and
pushes a `DaemonFrame::ConsolidationProposed { token, proposals }` to the **writer only**.
The attached TUI renders the same bottom-anchored `[mem] Consolidate memory — N proposals`
card the local TUI shows (`[y] promote all  [n] decline`); the reply rides back as a
`ClientFrame::ConsolidationResolve { token, accept }`. Apply is **daemon-authoritative**:
the client never echoes fact content — the daemon re-scans its own retained proposals for
secrets and writes them through the single hardened memory port, then clears the marker.
The resolution **token** is a confused-deputy guard (a stale/superseded resolve is
refused, writing nothing), and a read-only attachment that sends the mutation frame is
refused with `ProtocolError::ReadOnly`. Re-attaching the same marker **reuses** the
retained proposals (one generation per marker, not per connect); a new boundary marker
evicts the old set and regenerates. A transient write error or generation timeout leaves
the marker in place to retry on the next attach (never silent-loss). Per-item accept is a
future fast-follow (AI-12.2d-2) — the wire already carries a stable `ProposalId` per
proposal so that lands client-side with no protocol change.

`--plain` selects the minimal line-based stdin/stdout client (Story 12.2b
`run_attach`) for scripting / non-TTY contexts: it prints the conversation as it
streams and submits a turn per line; Ctrl-D detaches. It remains an ordinary
authenticated writer, but cannot submit proposal decisions. A model's proposal
therefore receives the no-surface refusal rather than creating a new pending card.

### Lazy runtime (NFR46)

`composition::build_daemon_core` eagerly composes only the cheap, connection-free
parts (memory, session storage, a `Normal`-mode security policy, persona) and
captures a `TurnRuntimeFactory` behind a single `OnceCell`. The live
provider/tools/scheduler/approval runtime is built on **first activity** and never
again (build-once), so an **idle daemon holds no live provider connection** and
stays under 30 MB. The laziness invariant is checked two ways: a fast in-process
unit gate (build-counter `0 → 1 → 1`) on every PR, and the `#[ignore]`d release
idle-RSS gate on the systemd nightly lane.
Refused tool-approval responses do not initialize this runtime: authority is
checked first, and pending-request lookup uses only an already-built runtime.

### Headless approval policy

The daemon has no human at a prompt, so tool approval (Story 12.2b AC6):

- **Attached TrustedLocal writer** → a writer-only `ApprovalRequest`, ordered
  exact-input chunks and an explicit completion boundary. The client renders the
  permission card and can Review the complete pending input before replying.
  There is **no reading deadline**. Read-only mirrors and Peer-tier attachments
  receive neither the approval card nor these chunks.
- **Detach, replacement or a full writer queue** → pending tool approvals remain
  pending. A full queue publishes a terminal protocol error and closes the
  connection, never silently dropping or denying its approvals. After the next
  trusted-local writer's `AttachAck`, requests are reissued in request order from
  the authoritative pending input, with fresh chunks. If replay itself fills the
  queue, the same close-and-retain rule applies.
- **Waiting turns** → a retained approval keeps the turn lock. Later operator and
  channel turns, `/clear`, notify-and-draft generation and inbound A2A peer turns
  queue behind it until a writer decides or shutdown cancels it. Cron completions
  are committed by the forwarder and do not queue behind that lock. A read-only
  attach reports the point-in-time retained count as
  `<N> approval(s) waiting for the writer`, without a decision card.
- **Unattended at raise** (or never delivered and recovered by lag rescan with no
  writer) → **deny-by-default**: read-only/`Safe` tools auto-proceed once; anything
  mutating is denied and recorded as a visible, resumable transcript note
  (`⏸ Skipped: … needed approval — no one was attached`), surfaced on the next
  attach as an "N actions waiting on you" count. Detach never invokes this policy.
  `Yolo` is unreachable headless.
- **Decision authority** → only the connection that received the request and is
  still the registered TrustedLocal ReadWrite writer may decide. Other responses
  receive `ApprovalRefused` (`writerOnly`, `issuedElsewhere` or `noLongerPending`) and
  resolve nothing; duplicate decisions cannot execute a tool again. This control
  lane is independent of the operator turn FIFO. Sender-consent handling is
  unchanged and does not authorize generic tools or proposals.
- **Shutdown** → close approval admission, cancel every pending request with
  `RuntimeShutdown`, stop exact input delivery and join the daemon-owned approval
  gates. This includes requests waiting on the session-grant lock and runtimes
  first registered after the gate closes. A late decision executes nothing.
  An interrupted inbound admission is not an operator refusal: its durable pending
  record and parked node remain available to cold-restart reconciliation, which
  reports it failed with the existing restart detail. Shutdown does not authorize
  or resume that task.

### Wire protocol

Frames use a `u32` big-endian length prefix and a `serde_json` body. The signed
attach handshake negotiates **protocol v13**; a client speaking another version
receives `VersionMismatch` before proof verification, registration or other
attach side effects.
Forwarded turn events reuse the existing `RawEvent` projection
(`ClientEvent = RawEvent`), followed by one daemon attach-audience projection.
TrustedLocal attachments, including read-only mirrors, retain tool arguments in
live events, snapshots, history pages and local peer rows. A mirror never receives
the writer's `ApprovalRequest`, exact-input chunks or Review and cannot decide.
Peer-tier attachments receive tool identity, status, results and prose, but never
tool arguments: `ToolEventWithheld` marks live calls, and `withheldToolInputs`
marks transcript calls in AttachAck and History. Their tool blocks show
`input withheld — not shared with peer attachments`; the corresponding `input`
key is absent, not replaced with a sentinel or an empty object. Every message
in a Peer-tier AttachAck snapshot or History page also has send authorization
reset to Unknown (omitted), including messages without tool calls. TrustedLocal
writers and read-only mirrors retain that fact. Live peer-row updates remain
TrustedLocal-only.

The underlying public event projection still omits sender-local team
action/settlement events and strips additive action references from legacy
dispatch events. It resets both authorship and send authorization on dispatch
and disclosure events, and send authorization alone on reply events; existing
reply authorship exposure is unchanged. Proposal inputs, results and scheduler
transitions remain omitted; structured internal conversation history retains them,
but attach snapshots, history pages and peer-row projections omit their private
tool-call data. This proposal privacy exclusion is independent of the tier rule.
Completed tool responses precede later assistant messages when model history is
rebuilt. Interrupted calls remain retained internally but are not replayed as
unmatched invocations.

For every tier, an input exceeding `MAX_FRAME_BYTES - MAX_FRAME_BYTES / 128`
serialized bytes reserves envelope headroom and is withheld from the tool block.
TrustedLocal clients show `input too large to show here (<size>)`; Peer-tier
clients keep their peer-withheld form. The writer receives the exact approval
input separately in ordered chunks. If a snapshot or History page is still over
the cap, the largest remaining inputs are withheld first as typed `TooLarge`
entries. An oversized initial `AttachAck` is bounded to a suffix of complete
transcript messages, followed by a `FrameTooLarge` notice; stored history is
unchanged. If the acknowledgement's essential metadata alone cannot fit, attach
fails with an explicit protocol error rather than an invalid first frame.
Other frames still over the cap are replaced by `FrameTooLarge`, naming their
variant and serialized byte count; later frames continue on the same connection.

The client speaks `ClientFrame`, including `Attach`, `UserMessage`,
`HistoryRequest`, `ApprovalResponse`, `Detach`, and the request-keyed team
commands `TeamSend`, `TeamFollowup`, `TeamBoard`, `TeamThread`, `TeamStatus`,
`TeamRetractPreview`, `TeamRetractDecision`, and `RevokeSenderConsent`.
`rustain team send` is a one-shot trusted-local writer client of `TeamSend`: it requires a free writer slot and leaves A2A composition, validation and the send journal to the daemon.
`EnableTeamProposals` is an additive v7 opt-in sent by a capable rich client after
its authenticated writer acknowledgement. It enters the same operator FIFO and
stale-writer fence as decisions; read-only and Peer connections cannot enable it.
The daemon answers boards, threads and the board re-rendered after a landed
retract with request-correlated `TeamViewChunk` frames (sequence numbers start
at zero) and a `TeamViewDone` frame naming the complete chunk count; clients
reassemble only the matching request and publish the view after a valid done
boundary. The stream applies backpressure through only the
originating connection's writer; it stops if that connection closes and does
not hold the command reader while chunks drain. Team request keys are limited
to 128 bytes. Each JSON frame, including escaped text and request overhead,
fits the 8 MiB `MAX_FRAME_BYTES` limit. Team status uses
`TeamStatusResult`; revocation uses `SenderConsentRevoked`; sends use
`TeamSendStarted` plus typed, per-recipient `TeamSendSettled` frames. A retract
preview answers with `TeamRetractPreviewed` (a card without any task, or an
answer); a landed decision streams the view and then `TeamRetractAnswered`.
Refusals use a `RecipientActionResult` targeted at the originating request key.

`TeamProposal` carries the sender-local proposal id, ordered aliases, semantic type
and body only to the trusted writer. `TeamProposalDecision` carries Send or Dismiss;
`TeamProposalClosed` reports dismissal, withdrawal or a correlated refusal.
Send uses the ordinary daemon send coordinator with the proposal id as its
request key, no subject, and sender-local `AgentComposed` dispatch provenance.
The deciding connection alone receives `TeamSendStarted`/`TeamSendSettled`, bound
to its native send block. Proposal/receipt DTOs remain feature-independent.

Team send and sender-consent revocation require a TrustedLocal read-write
attachment. Board and status are explicit TrustedLocal reads and are available
to read-only attachments. Peer-tier connections are refused for all four
commands. Team sends enter the bounded operator FIFO and a separate finite
daemon-owned in-flight limit. Once a send's action record is durable, it
continues across client detach; settlements remain journaled and are published
only to the connection that originated the request. A replacement writer does
not inherit old results, and a command still queued when its writer is replaced
is discarded by the stale-writer fence.

The daemon injects the same composed outbound A2A runtime used by its core;
the attach client composes no provider or egress. Protocol v7 changes only the
local daemon attach protocol. It does **not** discharge the separate A2A
boundary tracked by AD-1826, add a peer-visible authorship field, or close full
J10. `/team log` renders the journal's two independent facts on dispatch,
disclosure and reply rows: `[agent-composed]` for agent-composed text, then
`[auto-sent]` only for automatic sends. Approved proposals record
`agent-composed` + `human-decided`, while typed sends record
`human-written` + `human-decided`. Neither is labelled `[auto-sent]`.
The daemon still carries no custom-agent selection/restriction context, and NotifyAndAuto remains
conservative template-only, not a model turn. Each message also carries its
originating `ChannelKind` (`terminal` today), persisted on `ChatMessage.origin`
so the prefix survives restart and crash-recovery replay.

### Message provenance and retraction

Authorship identifies who composed the text; send authorization records how
this host authorized the act. `[agent-composed]` is not an automatic-send claim.
An approved or edited draft keeps that authorship label without `[auto-sent]`.
Unknown facts add no label.

In the attached TUI, **Ctrl+X** retracts automatic sends only.
`RetractAutoResponse` refuses any row whose send authorization is not
`automatic` with `retraction refused: NotAutomatic`. The existing gates are
unchanged: the connection must be a trusted-local, same-host read-write
attachment; the row must have its `peer-response-<node>` identity and a
journal-derived disclosure sequence greater than zero, and must not already
be retracted. Settled controls remain excluded. This is not a reply-turn or
cross-host retract command.

RAP application messages are one-way, regardless of the configured response
mode. Each accepted message is shown with a host-generated notice:
`[no reply over RAP] RAP carries messages one way: nothing can be sent back, so your <mode> response setting does not apply here.`
Here `<mode>` is `notify-and-wait`, `notify-and-draft` or `notify-and-auto`.
No template, draft, agent turn or pending response is created. A pending RAP
card saved by an older build refuses resolution with
`RAP carries messages one way; no reply was sent.` and settles as the same
notice. The sender still receives only the truthful accepted/refused verdict,
never response text. Conversational replies use A2A.

At daemon startup, a persisted `Drafting` response row without a live draft
phase settles before attachments are accepted:
`[draft interrupted] The daemon restarted while this reply was being drafted; nothing was sent.`
RAP-rooted interrupted drafts use the one-way notice above instead. Live draft
phases and other stages are unchanged. Settlement creates no reply, provider
turn or journal event; a direct `/team reply <item-id> …` remains available.

Legacy rows are never backfilled: without recorded send authorization, that
fact is Unknown. A legacy approved draft loses its false `[auto-sent]` label.
A legacy genuine auto-response shows `[agent-composed]` only and is no longer
Ctrl+X-retractable. A legacy retracted row keeps its append-only record:
`┆ [retracted HH:MM]  [✗] Retract (used)`. Neither journal history nor authorship
is used to infer automatic-send authorization.

Loading a messages-only session preserves each migrated assistant row's origin,
completion state, synthetic marker, bookmarks, fork marker, and search highlights.
Turn-backed rendering keeps these decorations separate from provenance labels.
Migrated tool blocks retain a consistent selection identity, so copying a selected
block returns that tool's result before and after the message mirror is rebuilt.
Fold navigation counts the same provenance and metadata rows that are painted.

Protocol v10 replaces v9 (Story 19.21c): shared request-keyed view streams and
attached thread/followup/retract frames cannot be decoded by older clients.
They receive `VersionMismatch` before proof verification or registration; the
historical v7 introductions and v9 authorship/authorization change remain valid.

Protocol v11 (Story 19.21d) adds the optional `TeamSend.stories` list and
`PeerRowControl.stories` summary. After attach, an undecodable but completely
length-framed client body receives `Malformed` on that connection only:
`this daemon could not decode a frame from this client; the frame was ignored`.
The daemon skips it and continues reading; it never echoes serde diagnostics,
frame bytes, or an unknown variant name. I/O, oversized, malformed-length and
truncated frames still close the session because their stream cannot be resynchronised.
An undecodable first frame receives
`the first frame could not be decoded; attach with a client of the same version`
and closes; the client shows its existing `daemon rejected attach: …` message.
A request-keyed frame that cannot be decoded cannot echo its key, so that
request gets no settle. Between same-version builds this is unreachable.

Protocol v12 (Story 19.24): `ShareRecipientStatus`; `PeerRowControl` carries
the status preview. A trusted-local writer can Share the stored arrival-time
preview or Decline on a NotifyAndWait `status_request` card that awaits the
operator, including one restored after a daemon restart (the turn is then
appended directly and the card settled). Read-only and Peer-tier attachments
cannot decide. Share and Decline append human-decided, agent-composed Status
turns; retrying the same decision is idempotent (and settles a card whose save
failed), and a different decision cannot replace a settled turn.

The member's `status_detail_minimum` chooses `none`, `minimal` (held story keys)
or `status` (keys plus received/acknowledged receipt state), with absent/invalid
values applying `none`. The team norm is advisory and never changes that level.
Decline, `none` and no held stories have the same canonical empty status and
reply text; the local preview reason is never served. No conversation, plan,
file, activity or other-item metadata is added. See
[Sharing status under your policy](a2a.md#sharing-status-under-your-policy-story-1924)
for the exhaustive field/privacy rules.

Protocol v13 (Story 19.22): `ReplyToRecipientItem` carries an optional
Accept/Discuss verdict. Omitting it preserves the plain Answer path. A
trusted-local writer can Accept a readable story list that gives this host a
story, or Discuss concerns on any readable story list; empty Accept text becomes
`Accepted.`, while Discuss requires text. A verdict is a human-written,
human-decided reply turn, never an acknowledgement or item-state change.
A live card settles through the same kinded resolution; a settled/orphan item
gets one direct turn. A retry after a partial live-card failure must match
content, kind and status to reuse the durable turn; a conflicting kind is refused.
Read-only and Peer-tier attachments cannot give verdicts. A pre-v13 daemon
would ignore the optional field and record an Answer, so mixed versions are
rejected before proof verification. Story lists also ride `scope_change`;
see [Sending to configured peers](a2a.md#sending-to-configured-peers-from-the-tui)
for release notices and the displaced-holder rule.

The status action board pulls existing threads under their normal
`threadReads`, bound-peer and item-capability gates; read failures do not become
empty status. The statement is the sharing host's report, not authenticated
evidence of progress. NotifyAndAuto remains template-only and leaves the report
awaiting a reply; no automatic Status act or new inference path is introduced.

Only host act producers write send authorization. It is never imported from an
attached client's frame or an A2A/RAP peer payload. Session and journal files
are trusted local inputs: editing a local row can change its retract
eligibility. Provenance is not authenticated evidence.
