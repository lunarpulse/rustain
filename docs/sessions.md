# Session management

Rustain stores conversation sessions per workspace under `.claude/sessions/`,
next to the project they belong to (git-repo-local, like `.git`). Cross-
workspace `session list --all` uses a separate hint registry at
`~/.rustain/workspaces.json` populated on the first successful save per workspace.

## Ordering and shared writers

Several Rustain processes (a TUI, the daemon, `ask`, an ACP client) may open and
write the same session. Every writer coordinates through one authority per
session: `.claude/sessions/.rustain-authority/<id>.{lock,seq,auth.json}`, outside
both transcript layouts and outside the deletable `<id>/` directory.

- **Capture and refusal.** A conversation is loaded or created against a base
  (an incarnation plus an accepted version). Every clone of it is a capture that
  binds that base, a per-writer operation id and, once the session has an
  authority, a sequence issued across processes. A save is admitted only when
  its base is still the accepted one (or is the validated causal successor of
  its own writer) **and** its sequence is newer than the last accepted one. An
  older snapshot, a snapshot delivered late, a replay (even of identical bytes)
  and a snapshot of a deleted or recreated session are refused with a typed
  *stale* error; nothing is written and the accepted version does not move.
- **Whole operations.** A save, checkpoint, truncate, revert, fork and rewind
  each hold the session from before their first dependent read to their final
  publication, cleanup and fence update, even if the caller that started them
  is cancelled. Other sessions are not blocked. Fork and rewind consume the
  version their flush was accepted at; if another writer was accepted after it,
  they are refused before any new-session write, journal or workspace effect.
- **Crash class: process exit.** An operation first records its successor
  version, the publication witness and the effects that remain. If the process
  is killed, the next opener proves whether the first file was published and
  either finishes the missing effects once or clears the marker. A publication
  whose later sidecar/log/cleanup effect failed is a *partial commit*: the new
  version is accepted and the remaining effects stay recorded; further writes
  are refused until they can finish. No `fsync` is promised, so a power loss or
  OS crash is outside this guarantee.
- **Rewind journals.** A rewind started by the TUI first completes the pending
  save, flushes the live transcript, then writes a journal bound to that
  accepted version. Startup recovery skips a session that another process is
  using, never acts on a journal bound to another version, keeps a journal whose
  file work had begun, and removes the journal only for a provably abandoned
  rewind.

### Supported stores and upgrade

- Linux and macOS, on a **local** filesystem. The lock is `flock`, the sequence
  is a memory-mapped counter. A network or overlay filesystem is not supported
  for shared writers; there is no process-local fallback that pretends
  otherwise. Windows is not a supported platform.
- Upgraded writers coordinate with each other. An **old** Rustain does not know
  the authority and writes unlocked, so an upgrade must never overlap old and
  new writers on one store: stop every Rustain process (including the daemon)
  that uses the sessions directory, upgrade, then start; to roll back, stop
  all processes again before starting the old build. Alternatively run the two
  builds on separate sessions directories. The new authority cannot stop an old
  binary.
- The first upgraded write of an existing session adopts it: its transcript is
  kept and a new authority record starts at version 1.

### Abandoned write scratch

Every session write goes through a short temporary file next to its target
(`.rustain-<pid>-<n>.tmp` in the sessions directory or in `<id>/`) that is renamed
into place. A process killed between the two leaves that file behind. A writable
Rustain removes such leftovers once, when it opens the store, and only when it can
prove nobody can still write them:

- **When.** The standalone TUI (default restore, `--session` and `--new`), the
  foreground daemon (before it restores or saves its conversation and before the
  scheduler and socket start), a writable `ask` (new or `--session`, never
  `--dry-run`), ACP `session/new` and a successful `session/load` or
  `session/resume` (the request's workspace only), `migrate` after you accept
  the import (never for `--dry-run` or a declined prompt) and a confirmed
  `session delete` (each selected workspace once, never `--dry-run` or an
  unconfirmed run) run the sweep before recovery or their first session write.
  `session list`, `daemon status`, `attach` and every preview write and remove
  nothing.
- **What.** Only an exact regular file named `.rustain-<pid>-<n>.tmp` in the
  sessions directory or directly inside a session directory (an incomplete first
  migration included), or one of the fixed names an older Rustain used:
  `<id>.meta.json.tmp`, `<id>.session.json.tmp` in the sessions directory and
  `conversation.json.tmp`, `meta.json.tmp`, `checkpoints.json.tmp`,
  `rewind_txn.json.tmp` inside `<id>/`. Symlinks, directories, malformed names,
  `snapshots/`, `images/`, the authority directory and everything else stay.
- **Proof.** The pid in a name is never trusted. Each write holds a shared lock on
  `.rustain-authority/.scratch-<pid>.guard` from before it creates its temporary
  file until it has renamed or removed it; the sweep removes a name only while it
  holds that guard exclusively, so a live writer (or a reused pid that is writing)
  keeps its file, and a dead one's lock is gone with the process. Fixed legacy
  names are removed only while the sweep holds that session's authority; a
  session an upgraded writer holds right now keeps them. The directory and file
  are pinned and checked again just before removal (no symlink is followed, a
  replaced file stays). A kept leftover is logged and retried by the next
  startup.
- **Upgrade.** An old Rustain takes neither lock, so the upgrade rule above is what
  makes the sweep safe: no old writer may run on the store when an upgraded one
  opens it. A leftover from an old crash is reclaimed by the first upgraded
  startup after the old processes are stopped.
- Not removed: leftovers inside `snapshots/` and the authority directory, and
  anything on a platform without `flock` (Windows).

## Temporary TUI displays

Cancelled local replies and standalone plan cards remain visible in their
current tab, including across ordinary tab switches and transcript rebuilds.
They disappear when the tab is destroyed or the session is reloaded or
restarted. These display rows are excluded from saved sessions and model
history; they are not bookmark, fork or rewind targets.

Plan and task data and their status remain durable, but a temporary card's
host-row reference is not saved and the card is not reconstructed on reload.
Existing unmarked session rows, including older interrupted replies and ACP
notices, retain their prior migration and persistence behavior.

## The launchpad and the Stage tool rows

An empty conversation shows the launchpad instead of a blank pane: the
workspace and its git branch (read from `.git/HEAD`, no `git` process), the
active model, provider and sign-in state, three things to try, up to three
recent sessions of this workspace (newest first, with their age) and the keys
that work in this host at this width. The recent sessions are informational;
open one from the History window (`Ctrl+X h`, at every terminal width; `Enter` opens
the selected conversation, `d` asks to delete it, `/` searches across them, `Esc`
closes it); the key line names it. The attached
client shows the workspace, its branch and the daemon it is attached to. The
launchpad facts (git branch, sign-in state) are recomputed only when the
conversation, model, provider or its lock state changes; the recent-session
list is refreshed from memory on every draw of an empty conversation.

Every tool call on the Stage is one row that reads like a sentence:
`✓ Read src/x.rs:1-80 · 0.3s`, `✓ Edit src/parser.rs +12 −3 · 0.1s`,
`✓ Write src/new.rs +40 · 0.1s`, `✓ Bash cargo test --lib · 2.0s`. The glyph is
the status (`●` running, `✓` done, `✗` failed, `⊘` cancelled, `⋯` validating,
`⧖` scheduled, `?` awaiting approval). Paths under the workspace show relative.
Change counts are exact or absent: a Write whose diff was capped or never
captured shows its line total (`40 lines`), never a partial count. Raw argument
JSON is never shown, and terminal controls in arguments never reach the screen.

## ACP display-only diagnostics

ACP warnings and errors are saved as display-only notices, separate from provider
text, reasoning and tool history. They do not become assistant-authored model
context or invalidate otherwise faithful signed provider-response replay.

`session/load` replays each notice with its original `Warning:` or `Error:`
prefix, in order among the response's text chunks; consecutive notices retain
their stored order. `session/resume` restores context without historical updates.
Notice-only responses survive reload even though they supply no model message.

Legacy sessions without explicit notice metadata are unchanged: rustain does
not guess whether warning-like prose was model output. Edited notice offsets
that are invalid replay the full response text once, then notices in stored order.

## `rustain session list`

List the saved sessions in the current workspace:

```sh
rustain session list
```

The human table shows:

| Column | Meaning |
|--------|---------|
| `#` | 1-based positional index (presentation only) |
| `ID` | Stable session identifier — use this with `delete`, not the index |
| `TITLE` | Conversation title, truncated if very long |
| `LAST ACTIVITY` | Absolute local timestamp (`YYYY-MM-DD HH:MM`) |
| `MESSAGES` | Number of messages in the session |

A `*` fused to the index gutter marks the session that a bare `rustain`
command resumes by default (the most-recent session). It does **not** indicate
that a daemon is currently attached to that session.

For scripting:

```sh
rustain session list --json
```

Emits a versioned snake_case envelope:

```json
{
  "schema_version": "1.1",
  "sessions": [
    {
      "id": "sess-id",
      "index": 1,
      "title": "Session title",
      "message_count": 12,
      "created_at": 1730000000,
      "updated_at": 1730000100,
      "has_fork_source": false,
      "is_default_resume": true,
      "workspace": "/absolute/workspace/path"
    }
  ]
}
```

`id` is the stable address; `index` is snapshot-relative and valid only within
one listing output.

## `rustain session delete`

Delete a session by its stable `id`. The positional argument is always an id
(exact full id or unique prefix), never the 1-based `index` shown by `list`.
IDs beginning with `-` are accepted as values, including `rustain --session -ID`
and `rustain --session=-ID`. For deletion, put the ID before other options,
as in `rustain session delete -ID --force`.

```sh
rustain session delete sess-id
rustain session delete sess-id --force
rustain session delete --dry-run sess-id
```

Resolution rules:

- Exact full id wins.
- Otherwise a **unique** prefix is accepted; an ambiguous prefix (two or more
  matches) is a hard error and nothing is deleted.
- A miss in the current workspace can optionally hint at other workspaces where
  the id was found.

By default the command is interactive and asks for confirmation. `--force`
skips the confirmation prompt, but it does **not** bypass the in-use guard.

### Deleting sessions in bulk

`--all` deletes **all sessions in the current workspace only**:

```sh
rustain session delete --all
rustain session delete --all --force
rustain session delete --all --dry-run
```

When more than one session matches, the prompt asks you to type the literal
session count as a speed bump (`1` is a normal `[y/N]` prompt; `0` is a no-op).
The id-set is captured at confirm time; if the typed count does not match, the
operation is cancelled and nothing is deleted.

`--all-workspaces` deletes **all sessions across every registered, live
workspace**. This is the loudest gate: the prompt echoes the total session count
and the workspace count, and you must type the total number of sessions.

```sh
rustain session delete --all-workspaces
rustain session delete --all-workspaces --force
rustain session delete --all-workspaces --dry-run
```

For a single session in another workspace, use `--workspace` with an explicit
path:

```sh
rustain session delete sess-id --workspace /path/to/other/project
```

`--workspace` is only valid with a single id; it cannot be combined with `--all`
or `--all-workspaces`.

### `--dry-run`

`--dry-run` runs the in-use guard and prints what *would* be deleted and what
*would* be skipped, but removes nothing and leaves the filesystem byte-
identical.

### `--json`

Scripting mode emits a versioned snake_case envelope:

```json
{
  "schema_version": "1.0",
  "dry_run": false,
  "deleted": [
    {
      "id": "sess-id",
      "title": "Session title",
      "workspace": "/absolute/workspace/path"
    }
  ],
  "refused": [
    {
      "id": "sess-id",
      "title": "Session title",
      "workspace": "/absolute/workspace/path",
      "reason": "in_use",
      "holder": {
        "pid": 12345,
        "channel": "ops"
      }
    }
  ]
}
```

`--json` requires `--force` (or `--dry-run`) because it is intended for
non-interactive use — it never interleaves a yes/no prompt with the JSON
envelope. The command exits non-zero if any session was refused or errored.

### In-use guard

Before deleting any target, rustain asks the workspace daemon whether it is
currently holding that session. The possible answers are:

- **No daemon running** — proceed.
- **Held by daemon** for this exact session — refuse, naming the daemon pid and
  channel. Even `--force` will not delete a session reported as in use.
- **Daemon alive but unqueryable** — fail-closed and refuse. Use `--force` to
  proceed anyway; this is the only state `--force` overrides.

The guard is a **check, not a lock**: there is a residual race between the check
and the delete. For deterministic deletion, stop the daemon first
(`rustain daemon stop`). Foreground TUIs are not detectable; close any session
windows you care about before bulk deletion.

### Exit codes

Declining the prompt returns `0`. Distinct non-zero codes are used for:

| Code | Meaning |
|------|---------|
| `2`  | No such session |
| `3`  | Ambiguous prefix |
| `4`  | Session in use (confirmed holder) |
| `5`  | Daemon unqueryable (`--force` overrides) |
| `6`  | Needs confirmation (non-TTY without `--force`) |
| `7`  | Path escapes the sessions directory |
| `8`  | Storage error |

For bulk deletes, partial success (some deleted, some refused) is non-zero.

## Notes

- `session list` / `session list --all` are read-only: they never write,
  delete, migrate, or create files in `.claude/sessions/`. `--all` also never
  rewrites `~/.rustain/workspaces.json`.
- `session delete` is offline-safe: no provider is constructed and no network
  call is made. The only process communication is a local Unix-socket query to
  the workspace daemon.
- Empty sessions (zero messages) are excluded from `list` but are addressable
  by explicit id in `delete`.
- Sessions are sorted by `updated_at` descending, tie-broken by `id` ascending
  in single-workspace mode, or by `(workspace, id)` after `updated_at` in
  `--all`, so the order is deterministic and reproducible.
- `delete --all` is current-workspace only; `list --all` spans workspaces. This
  asymmetry is intentional: a destructive `--all` that silently crossed every
  registered workspace would be too easy to trigger.
- `--all-workspaces` reads the workspace registry but never rewrites it.
- `delete` removes only the session footprint (`{id}/` in directory layout, or
  `{id}.meta.json` + `{id}.session.json` in flat layout). It does not mutate
  memory indexes, vector stores, or other sessions.
- A deleted session stays deleted: delete records a tombstone before it removes
  any file, so a save that was captured before the delete (a late turn, a
  daemon that still holds the session in memory) is refused instead of writing
  the session back. Creating a session with the same id again is an explicit
  creation that starts a new incarnation.

## Future

- `session prune` (deferred) — deregister dead workspaces and remove their
  sessions.
