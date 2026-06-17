# Session management

Rustain stores conversation sessions per workspace under `.claude/sessions/`,
next to the project they belong to (git-repo-local, like `.git`). This keeps
project history with the project and avoids a global registry of directories.

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
  "schema_version": "1.0",
  "sessions": [
    {
      "id": "sess-id",
      "index": 1,
      "title": "Session title",
      "message_count": 12,
      "created_at": 1730000000,
      "updated_at": 1730000100,
      "has_fork_source": false,
      "is_default_resume": true
    }
  ]
}
```

`id` is the stable address; `index` is snapshot-relative and valid only within
one listing output.

## Notes

- `session list` is read-only: it never writes, deletes, migrates, or creates
  files in `.claude/sessions/`.
- It is offline-safe: no provider is constructed and no network call is made.
- Empty sessions (zero messages) are excluded from the list.
- Sessions are sorted by `updated_at` descending, tie-broken by `id` ascending,
  so the order is deterministic and reproducible.

## Future

- `rustain session delete <id>` (Story 13.5b) will delete a session by its
  stable `id`, with an in-use guard for sessions held by a daemon.
- Cross-workspace listing (`--all`) is planned as Story 13.5a-1 and requires
  a workspace-registry primitive.
