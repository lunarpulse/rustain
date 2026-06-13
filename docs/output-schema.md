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
"schema_version": "1.0"
```

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
| `schema_version` | string | Always `"1.0"`. |
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
| `error` | object \| null | Present only on turn failure. Same envelope as success; `response` is `null`. |
| `error.message` | string | Human-readable failure reason. |

### Error envelope

On a turn failure in `json` mode, stdout contains the same envelope with `response: null` and a non-empty `error` object. Exit code is non-zero. A JSON consumer always receives parseable output.

Example:

```json
{
  "schema_version": "1.0",
  "response": null,
  "model": "",
  "stop_reason": "",
  "usage": null,
  "tool_calls": [],
  "session_id": "",
  "deny_count": 0,
  "error": {
    "message": "query cannot be empty"
  }
}
```

## `--output-format stream-json` events

One compact JSON object per line. Every line carries `schema_version: "1.0"` and a `type` discriminator.

| `type` | Fields | Description |
|--------|--------|-------------|
| `text` | `schema_version`, `content` | Assistant text delta. |
| `tool_use` | `schema_version`, `id`, `name`, `input` | A tool call was requested. |
| `tool_result` | `schema_version`, `id`, `result` | A tool result arrived. `result` has the same shape as `tool_calls[].result` in `json` mode: `{ content: string, is_error: boolean }`. |
| `usage` | `schema_version`, `usage` | Token usage (same shape as the `json` `usage` object). Emitted once near the end of the stream, not per-chunk. |
| `turn_complete` | `schema_version`, `stop_reason`, `deny_count` | Terminal event; the turn finished. `deny_count` is the number of auto-denied tool calls (same value as the `json` envelope's `deny_count`). |
| `error` | `schema_version`, `error` | Terminal event; the turn failed. Prior events remain valid. `error` has the same shape as the `json` error: `{ message: string }`. |

Only `turn_complete` and `error` are terminal. On error, only `error` is emitted — `turn_complete` is NOT emitted. Each event object is serialized with `serde_json::to_string` (compact, single-line) so no embedded newline can break the NDJSON framing.

## Denials

`deny_count` is a first-class scalar in both `json` and `stream-json`. It equals the number of tool calls auto-denied during the turn. This lets automation consumers detect denials without scraping stderr.

The exit code remains binary: `0` if the turn completed, non-zero if it failed to complete. A denial is **not** a turn failure.

In `text` mode, denials remain stderr-only as they were in Story 13.1a.

## Reserved for future additive v1.1

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

A schema fingerprint test pins the v1.0 field-path set. If the fingerprint changes, the failure message instructs the maintainer to decide whether the change is additive (bump minor) or breaking (bump major) and then update the recorded fingerprint. This makes schema changes loud and intentional.

## See also

- Story 13.1b: Structured Output & Schema Versioning
- Story 13.1a: Basic `rustain ask` and text output
- `rustain ask --help`
