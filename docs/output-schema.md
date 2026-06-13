# `rustain ask` Structured Output Schema

This document is the stable contract for the `--output-format json` and `--output-format stream-json` surfaces shipped in Story 13.1b. It is intended for automation consumers who parse `rustain ask` output.

## Overview

`rustain ask` supports three output formats selected by `--output-format`:

- `text` (default) — assistant text only on stdout; all narration, hints, and errors on stderr.
- `json` — a single structured JSON document on stdout.
- `stream-json` — newline-delimited JSON (NDJSON) event stream on stdout.

The `--format` alias does **not** exist and was intentionally dropped (it never shipped; see Story 13.1b party-mode O5).

## Schema version
Every JSON document and NDJSON event carries:

```json
"schema_version": "1.1"
```

(v1.0 shipped in Story 13.1b; v1.1 adds the dry-run fields in Story 13.1c.)

The version is a **string** following `major.minor` semantics:

- **Additive change** (new optional field) → bump the **minor** version.
- **Breaking change** (field removed, renamed, or its meaning/type changed) → bump the **major** version.

Within a major version (e.g. all `1.x` releases), the schema is **additive-only**. Field removal, rename, or retype requires a major bump.

Emitting a prior schema version on request (e.g. `--schema-version`) is **deferred** until a `2.0` schema actually exists; no consumer needs it at `1.0`.

## Key-case convention

All keys — envelope *and* nested — are `snake_case`. The shared domain types serialize with `camelCase` internally, but the CLI renderer coerces them to `snake_case` at the output boundary so the two formats are casing-identical.

## `--output-format json` document
A single pretty-printed JSON object on stdout.

| Field | Type | Description |
|-------|------|-------------|
| `schema_version` | string | Always `"1.1"`. |
| `response` | string \| null | Assistant text. `null` in the error envelope. |
| `model` | string | The resolved concrete model used for the turn. |
| `stop_reason` | string | One of: `end_turn`, `tool_use`, `max_tokens`, `cancelled`. |
| `usage` | object \| null | Token usage, or explicit `null` if the provider emitted no `Usage` chunk. |
| `usage.input_tokens` | number | Input tokens. |
| `usage.output_tokens` | number | Output tokens. |
| `usage.cache_creation_input_tokens` | number \| null | Optional cache-creation tokens. |
| `usage.cache_read_input_tokens` | number \| null | Optional cache-read tokens. |
| `usage.reasoning_tokens` | number \| null | Optional reasoning tokens. |
| `tool_calls` | array | List of tool calls made during the turn (may be empty). |
| `tool_calls[].id` | string | Tool-use ID. |
| `tool_calls[].name` | string | Tool name. |
| `tool_calls[].input` | any | Tool argument JSON. |
| `tool_calls[].result` | object \| null | Tool result, if the turn received one. |
| `tool_calls[].result.content` | string | Result content. |
| `tool_calls[].result.is_error` | boolean | Whether the result is an error. |
| `tool_calls[].started_at_ms` | number \| null | Optional start timestamp. |
| `tool_calls[].completed_at_ms` | number \| null | Optional completion timestamp. |
| `tool_calls[].status` | string \| null | Optional status chip. |
| `session_id` | string | Conversation/session ID; consumers should use this instead of scraping the stderr resume hint. |
| `deny_count` | number | Count of auto-denied tool calls in the turn. |
| `dry_run` | boolean | `true` when `--dry-run` was specified, `false` otherwise. |
| `plan` | object \| null | Proposed plan (see `PlanOut` shape), or `null` when no plan was proposed. |
| `tools_would_use` | array of string | Deduplicated, sorted tool names the model attempted that the plan-mode gate would block. |
| `error` | object \| null | Present only on turn failure. Same envelope as success; `response` is `null`. |
| `error.message` | string | Human-readable failure reason. |

### Error envelope

```json
{
  "schema_version": "1.1",
  "response": null,
  "model": "",
  "stop_reason": "",
  "usage": null,
  "tool_calls": [],
  "session_id": "",
  "deny_count": 0,
  "dry_run": false,
  "plan": null,
  "tools_would_use": [],
  "error": {
    "message": "query cannot be empty"
  }
}
```

## `--output-format stream-json` events

One compact JSON object per line. Every line carries `schema_version: "1.1"` and a `type` discriminator.

| `type` | Fields | Description |
|--------|--------|-------------|
| `text` | `schema_version`, `content` | Assistant text delta. |
| `tool_use` | `schema_version`, `id`, `name`, `input` | A tool call was requested. |
| `tool_result` | `schema_version`, `id`, `result` | A tool result arrived. `result` has the same shape as `tool_calls[].result` in `json` mode: `{ content: string, is_error: boolean }`. |
| `usage` | `schema_version`, `usage` | Token usage (same shape as the `json` `usage` object). Emitted once near the end of the stream, not per-chunk. |
| `turn_complete` | `schema_version`, `stop_reason`, `deny_count`, `dry_run`, `tools_would_use` | Terminal event; the turn finished. `deny_count` is the number of auto-denied tool calls. `dry_run` and `tools_would_use` carry the same values as the `json` envelope. |
| `plan_proposed` | `schema_version`, `dry_run`, `plan` | Emitted before `turn_complete` when a plan was proposed. `plan` has the same `PlanOut` shape as in `json` mode. |
| `error` | `schema_version`, `error` | Terminal event; the turn failed. Prior events remain valid. `error` has the same shape as the `json` error: `{ message: string }`. |
Only `turn_complete` and `error` are terminal. On error, only `error` is emitted — `turn_complete` is NOT emitted. Each event object is serialized with `serde_json::to_string` (compact, single-line) so no embedded newline can break the NDJSON framing.

## Denials
`deny_count` is a first-class scalar in both `json` and `stream-json`. It equals the number of tool calls auto-denied during the turn. This lets automation consumers detect denials without scraping stderr.

The exit code remains binary: `0` if the turn completed, non-zero if it failed to complete. A denial is **not** a turn failure.

In `text` mode, denials remain stderr-only as they were in Story 13.1a.

## v1.1 — Dry-run plan mode (Story 13.1c)

Three **always-present** fields were added to the `json` envelope (additive minor bump, no fields removed):

| Field | Type | Description |
|-------|------|-------------|
| `dry_run` | boolean | `true` when `--dry-run` was specified, `false` otherwise. |
| `plan` | object \| null | The proposed plan (via `propose_plan` tool), or `null` when no plan was proposed. See `PlanOut` shape below. |
| `tools_would_use` | array of string | Deduplicated, sorted list of tool names the model **attempted** via `ToolUse` chunks that the plan-mode gate would block. Excludes plan-control builtins (`propose_plan`, `exit_plan_mode`). This is a mutation-intent / safety-audit signal — it captures the model's tool-call **intent**, independent of whether the gate denied it. An empty array is honest (the model proposed a plan via `propose_plan` without attempting concrete tools). |

**Important:** `tools_would_use` is a different surface from `deny_count`. Plan-mode denials short-circuit **before** `ApprovalRuntime`, so `deny_count` (= `approval.rejected_count()`) is **structurally 0** in a pure dry-run even when tools were refused. A consumer reads blocked-intent from `tools_would_use`, never from `deny_count`. `deny_count` measures **approval-runtime rejections only**.

These three fields are **always present** in the JSON envelope — they are NOT conditionally omitted. On a normal `ask` (no `--dry-run`), the document carries `dry_run: false`, `plan: null`, `tools_would_use: []`. This ensures the field-path set is flag-invariant (the G4 fingerprint pins ONE shape).

### `PlanOut` shape

When `plan` is non-null, it is a snake_case output-only DTO:

| Field | Type | Description |
|-------|------|-------------|
| `plan.id` | string | Plan identifier (nanoid). |
| `plan.title` | string | Plan title. |
| `plan.tasks` | array | Ordered list of plan tasks. |
| `plan.tasks[].number` | number | 1-indexed task number. |
| `plan.tasks[].title` | string | Task title. |
| `plan.tasks[].description` | string | Task description (may be empty). |
| `plan.tasks[].depends_on` | array of number | 1-indexed task numbers this task depends on. |
| `plan.tasks[].sub_tasks` | array | Sub-tasks within this task. |
| `plan.tasks[].sub_tasks[].number` | number | 1-indexed sub-task number (intra-parent). |
| `plan.tasks[].sub_tasks[].title` | string | Sub-task title. |
| `plan.tasks[].sub_tasks[].description` | string | Sub-task description (may be empty). |
| `plan.estimated_effort` | object \| null | Model-supplied effort estimate, if present. |
| `plan.estimated_effort.tool_calls` | number \| null | Estimated number of tool calls. |
| `plan.estimated_effort.seconds` | number \| null | Estimated seconds. |
| `plan.status` | string | Plan status (e.g. `"pending"`). |
| `plan.created_at` | number | Unix timestamp (seconds) when `propose_plan` was invoked. |

The domain `Plan` type serializes with `camelCase` internally (for TUI/storage compatibility). The `PlanOut` DTO coerces all keys to `snake_case` at the output boundary. The G5 snake-lint recurses into nested plan keys.

### `stream-json` additions

| `type` | Fields | Description |
|--------|--------|-------------|
| `plan_proposed` | `schema_version`, `dry_run`, `plan` | Terminal event emitted before `turn_complete` when a plan was proposed. `plan` has the `PlanOut` shape (snake_case keys). |

`turn_complete` now additionally carries `dry_run: bool` and `tools_would_use: string[]`.

### `--dry-run` behavior

- The run writes **NO session state** — no conversation save, no resume hint. A dry-run leaves zero trace on disk.
- `--dry-run --yolo` is rejected by clap (`conflicts_with`) — `--yolo` is the literal inverse of `--dry-run`.
- In `text` mode, stdout renders the plan (title, numbered tasks, estimated effort, attempted-tools line). When no `propose_plan` call was made, falls back to assistant prose + attempted-tools line.
- The no-mutate guarantee is **model-independent** — even a model that ignores the plan-mode nudge and emits `Write`/`Bash` cannot mutate state, because the `permission_chain` gate denies them. Safe-risk tools (e.g. `read_file`, `propose_plan`) still execute.

### Reserved: `denied_tools`

The named `denied_tools` array is reserved for a future additive minor bump. When it lands, the invariant will be:

```
deny_count == denied_tools.length
```

The array will always be present (empty-safe) and the count will be the derived length of the array.

## Stdout/stderr discipline

- **stdout** carries only the rendered output for the selected format.
- **stderr** carries tool-call narration, the resume hint, the human-readable deny-count summary, and errors — unchanged from Story 13.1a.

Under `--final-message-only`, the JSON `response` field is narrowed to the final assistant block and stderr narration is quieted; the JSON shape and stdout channel do not change.

## Version-bump enforcement

A schema fingerprint test pins the v1.1 field-path set. If the fingerprint changes, the failure message instructs the maintainer to decide whether the change is additive (bump minor) or breaking (bump major) and then update the recorded fingerprint. This makes schema changes loud and intentional.

## See also

- Story 13.1c: Dry-Run Plan Mode (v1.1 additive fields)
- Story 13.1b: Structured Output & Schema Versioning
- Story 13.1a: Basic `rustain ask` and text output
- `rustain ask --help`
