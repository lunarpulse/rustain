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

## `rustain session list --all`

List sessions across every registered workspace:

```sh
rustain session list --all
rustain session list --all --json
```

Notes:

- The human table adds a `WORKSPACE` column and keeps a single `*` marker for
  the current workspace's default resume target.
- The JSON envelope stays `1.1`; every row carries an always-present absolute
  `workspace` path so scripts can address `(workspace, id)` pairs directly.
- `--all` is read-only for both session files and the registry. Dead/moved
  workspaces are omitted from output but not pruned from `workspaces.json`.
- The registry stores only `{path,last_seen}` per workspace, written `0600` on
  Unix. Home-abbreviation is display-only; the JSON `workspace` field is always
  absolute.


## Notes

- `session list` / `session list --all` are read-only: they never write,
  delete, migrate, or create files in `.claude/sessions/`. `--all` also never
  rewrites `~/.rustain/workspaces.json`.
- They are offline-safe: no provider is constructed and no network call is made.
- Empty sessions (zero messages) are excluded from the list.
- Sessions are sorted by `updated_at` descending, tie-broken by `id` ascending
  in single-workspace mode, or by `(workspace, id)` after `updated_at` in
  `--all`, so the order is deterministic and reproducible.

## Future

- `rustain session delete <id>` (Story 13.5b) will delete a session by its
  stable `id`, with an in-use guard for sessions held by a daemon.
- Cross-workspace delete will use the composite `(workspace, id)` address that
  `session list --all --json` already exposes.
