# Session management

Rustain stores conversation sessions per workspace under `.claude/sessions/`,
next to the project they belong to (git-repo-local, like `.git`). Cross-
workspace `session list --all` uses a separate hint registry at
`~/.rustain/workspaces.json` populated on the first successful save per workspace.

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
- Deleted sessions should stay deleted, but a daemon that still holds the
  session in memory could in theory write it back after deletion. For
  guaranteed removal, stop the daemon first.

## Future

- `session prune` (deferred) — deregister dead workspaces and remove their
  sessions.
