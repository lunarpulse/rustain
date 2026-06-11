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
records a self-authored **nonce** and the **boot id**, and on Linux the guard also
checks `/proc/<pid>/comm` names `rustain`. A recycled/foreign PID is treated as
**stale** (reclaimable), never `Running` — so `stop` never signals an innocent
process. (No extra dependency; the residual macOS same-boot nonce-collision window is
accepted risk, tracked for a macOS-hardening follow-up.)

## Verifying auto-restart (NFR50)

- **Generated policy** (every CI commit): template-content unit tests assert the
  rendered unit/plist carries `--foreground`, `Restart=on-failure` / `StartLimit*`,
  and launchd `KeepAlive { SuccessfulExit = false }`.
- **Detect + record + announce** (every CI commit): an in-process simulated
  crash-recovery cycle test (dead PID in the PID file → start → assert the crash
  record + `status --json last_crash` + normal startup).
- **Real init system** (gated): on a systemd-equipped lane, run the Linux P0 gate:

  ```sh
  cargo test --test daemon_crash_recovery --ignored daemon_real_systemd_recovery
  ```

  It installs the unit, `kill -9`s the daemon, and asserts systemd relaunches it AND
  the relaunched instance records the crash. macOS/launchd (P1) is a manual sign-off.

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

**Multi-attach** is read-only: the first writer holds the write slot; later attaches
are granted `ReadOnly` (daemon-enforced — a read-only client's writes are refused
with `ProtocolError::ReadOnly`). A read-only client shows a persistent
`read-only — another client holds write` status segment, a one-time notice, and an
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
streams and submits a turn per line; Ctrl-D detaches.

### Lazy runtime (NFR46)

`composition::build_daemon_core` eagerly composes only the cheap, connection-free
parts (memory, session storage, a `Normal`-mode security policy, persona) and
captures a `TurnRuntimeFactory` behind a single `OnceCell`. The live
provider/tools/scheduler/approval runtime is built on **first activity** and never
again (build-once), so an **idle daemon holds no live provider connection** and
stays under 30 MB. The laziness invariant is checked two ways: a fast in-process
unit gate (build-counter `0 → 1 → 1`) on every PR, and the `#[ignore]`d release
idle-RSS gate on the systemd nightly lane.

### Headless approval policy

The daemon has no human at a prompt, so tool approval (Story 12.2b AC6):

- **Attached writer** → the approval is forwarded as an `ApprovalRequest` frame; the
  client renders the permission card and replies. An unresponsive writer times out
  to a conservative **deny** (never an indefinite hang).
- **Unattended** → **deny-by-default**: read-only/`Safe` tools auto-proceed; anything
  mutating is denied and recorded as a visible, resumable transcript note
  (`⏸ Skipped: … needed approval — no one was attached`), surfaced on the next
  attach as an "N actions waiting on you" count. `Yolo` is unreachable headless.

### Wire protocol

Frames are a `u32` big-endian length prefix + a `serde_json` body (zero extra
dependencies). The client speaks `ClientFrame` (`Attach`/`UserMessage`/
`HistoryRequest`/`ApprovalResponse`/`Detach`); the daemon speaks `DaemonFrame`
(`AttachAck`/`Event`/`History`/`ApprovalRequest`/`Detached`/`Error`). Forwarded
turn events reuse the existing `RawEvent` projection (`ClientEvent = RawEvent`) — one
`AppEvent → wire` mapping, in one place. A `protocol_version` mismatch is rejected
with a clear `Error` frame (forward-compat for the Telegram/cron channels in
12.3/12.4). Each message carries its originating `ChannelKind` (`terminal` today),
**persisted** on `ChatMessage.origin` so the prefix survives a daemon restart /
crash-recovery replay.
