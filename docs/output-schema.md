# `rustain ask` Structured Output Schema

This document is the stable contract for the `--output-format json` and `--output-format stream-json` surfaces shipped in Story 13.1b. It is intended for automation consumers who parse `rustain ask` output.

## Overview

`rustain ask` supports three output formats selected by `--output-format`:

- `text` (default) — assistant text only on stdout; all narration, hints, and errors on stderr.
- `json` — a single structured JSON document on stdout.
- `stream-json` — newline-delimited JSON (NDJSON) event stream on stdout.

The `--format` alias does **not** exist and was intentionally dropped (it never shipped; see Story 13.1b party-mode O5).

## Schema version
Every JSON document and NDJSON event carries a `schema_version` string. With
no `ask` limits or daily budget configured, the version remains:

```json
"schema_version": "1.3"
```

(v1.0 shipped in Story 13.1b; v1.1 adds the dry-run fields in Story 13.1c; v1.2 adds the `call_usage` stream event; v1.3 makes the `usage` counts disjoint for every provider.)

With any `ask` limit or daily budget configured, the version is `"1.4"` on
the whole document or every NDJSON line, including structured setup errors.
The version does not depend on whether a limit is reached.

The version is a **string** following `major.minor` semantics:

- **Additive change** (new optional field) → bump the **minor** version.
- **Breaking change** (field removed, renamed, or its meaning/type changed) → bump the **major** version.

Within a major version (e.g. all `1.x` releases), the schema is **additive-only**. Field removal, rename, or retype requires a major bump.

**One exception: 1.3.** v1.3 changes the *meaning* of `usage.input_tokens` for requests sent over the OpenAI Chat Completions transport (see [v1.3](#v13--disjoint-usage-counts)) without changing any field name or type. By the rule above that is a breaking change, and the rule on its own would read 2.0. The spec's default (schema 1.3, a minor bump) was applied by the integrator; the owner did not answer the question and may veto it before landing. The field set, and so the schema fingerprint, is unchanged. An unchanged fingerprint does not mean the change is compatible: a consumer that already subtracts cache reads itself must read `schema_version` and stop subtracting from 1.3 on. The general rule stands for every later change: a change of meaning is a major bump.

Emitting a prior schema version on request (e.g. `--schema-version`) is **deferred** until a `2.0` schema actually exists; no consumer needs it at `1.0`.

## Key-case convention

All keys — envelope *and* nested — are `snake_case`. The shared domain types serialize with `camelCase` internally, but the CLI renderer coerces them to `snake_case` at the output boundary so the two formats are casing-identical.

## `--output-format json` document
A single pretty-printed JSON object on stdout.

| Field | Type | Description |
|-------|------|-------------|
| `schema_version` | string | `"1.3"` without limits; `"1.4"` with a limit or daily budget configured. |
| `response` | string \| null | Assistant text. `null` in the error envelope. |
| `model` | string | The resolved concrete model used for the turn. |
| `stop_reason` | string | One of: `end_turn`, `tool_use`, `max_tokens`, `cancelled`. |
| `limit_reason` | string | Optional; present only on a bounded stop. See [v1.4](#v14--ask-execution-limits). |
| `usage` | object \| null | Token usage summed over every provider call of the turn, or explicit `null` if the provider emitted no `Usage` chunk. |
| `usage.input_tokens` | number | Prompt tokens billed at the input rate. Excludes cache reads and cache writes (from v1.3, for every provider). |
| `usage.output_tokens` | number | Every generated token, reasoning included. |
| `usage.cache_creation_input_tokens` | number \| null | Optional prompt tokens written to the provider's cache. Disjoint from `input_tokens`. |
| `usage.cache_read_input_tokens` | number \| null | Optional prompt tokens read from the provider's cache. Disjoint from `input_tokens`. |
| `usage.reasoning_tokens` | number \| null | Optional part of `output_tokens` spent on reasoning; never greater than `output_tokens`. |
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
| `error` | object | Present on ordinary operational failure (`response` is `null`); omitted on a bounded stop. |
| `error.message` | string | Human-readable failure reason. |

### Error envelope

```json
{
  "schema_version": "1.3",
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

One compact JSON object per line. Every line carries the invocation's
`schema_version` and a `type` discriminator.

| `type` | Fields | Description |
|--------|--------|-------------|
| `text` | `schema_version`, `content` | Assistant text delta. |
| `tool_use` | `schema_version`, `id`, `name`, `input` | A tool call was requested. |
| `tool_result` | `schema_version`, `id`, `result` | A tool result arrived. `result` has the same shape as `tool_calls[].result` in `json` mode: `{ content: string, is_error: boolean }`. |
| `call_usage` | `schema_version`, `usage`, `model` | Token usage of one provider call (same shape as the `json` `usage` object) and the model that served it. One line per call that reported usage for display; a report of zero tokens is still a report. |
| `usage` | `schema_version`, `usage` | Turn total (same shape as the `json` `usage` object). Emitted once near the end of the stream, not per-chunk. Not emitted before a terminal `error`. |
| `turn_complete` | `schema_version`, `stop_reason`, `deny_count`, `dry_run`, `tools_would_use`, optional `limit_reason` | Terminal event for a completed or bounded turn. `deny_count`, `dry_run`, `tools_would_use`, and `limit_reason` carry the same values as the `json` document. |
| `plan_proposed` | `schema_version`, `dry_run`, `plan` | Emitted before `turn_complete` when a plan was proposed. `plan` has the same `PlanOut` shape as in `json` mode. |
| `error` | `schema_version`, `error` | Terminal event; the turn failed. Prior events remain valid. `error` has the same shape as the `json` error: `{ message: string }`. |
Only `turn_complete` and `error` are terminal. An ordinary operational
failure emits only `error` as its terminal event. Each event object is
serialized with `serde_json::to_string` (compact, single-line) so no
embedded newline can break the NDJSON framing.

## Denials
`deny_count` is a first-class scalar in both `json` and `stream-json`. It equals the number of tool calls auto-denied during the turn. This lets automation consumers detect denials without scraping stderr.

An auto-denial is **not** a turn failure and does not by itself change the
exit status. See [exit status](#exit-status).

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
| `plan_proposed` | `schema_version`, `dry_run`, `plan` | Nonterminal event emitted before `turn_complete` when a plan was proposed. `plan` has the `PlanOut` shape (snake_case keys). |

`turn_complete` now additionally carries `dry_run: bool` and `tools_would_use: string[]`.

### `--dry-run` behavior

- The run saves no reply and prints no resume hint. Its provider calls are still recorded in the usage ledger (`<data dir>/usage/`), as for every run. The prompt is not saved, and no checkpoint, snapshot or session authority entry is written: the session store is read-only for the run.
- `--dry-run --yolo` is rejected by clap (`conflicts_with`) — `--yolo` is the literal inverse of `--dry-run`.
- In `text` mode, stdout renders the plan (title, numbered tasks, estimated effort, attempted-tools line). When no `propose_plan` call was made, falls back to assistant prose + attempted-tools line.
- The no-mutate guarantee is **model-independent** — even a model that ignores the plan-mode nudge and emits `Write`/`Bash` cannot mutate state, because the `permission_chain` gate denies them. Safe-risk tools (e.g. `read_file`, `propose_plan`) still execute.

### Reserved: `denied_tools`

The named `denied_tools` array is reserved for a future additive minor bump. When it lands, the invariant will be:

```
deny_count == denied_tools.length
```

The array will always be present (empty-safe) and the count will be the derived length of the array.

## v1.2 — Per-call usage

A turn takes one provider call per tool round, so a turn that uses tools makes several.

- `usage` (the `json` field and the `stream-json` `usage` event) is the **sum over every provider call of the turn**. Before v1.2 it carried the last call only, which under-reported every turn that used tools. For a single-call turn the value is unchanged.
- `stream-json` gained the `call_usage` event (additive minor bump). It is written when a provider call finishes, so a run that is killed part-way still leaves the usage of the calls it completed.

`call_usage` lines appear in call order. A call's line is written after the last chunk of that call and before the first event of the next call.

On ordinary turn failure, the calls that reported usage are written before
the terminal `error` and no `usage` total follows. That includes a call that
reported usage and then failed.

## v1.3 — Disjoint usage counts

Before v1.3 the `usage` counts followed each provider's wire format: Anthropic reported `input_tokens` without its cache reads and writes, while requests sent over the OpenAI Chat Completions transport reported `input_tokens` **including** the cache reads. From v1.3 the counts mean the same for every provider:

- `input_tokens` excludes `cache_read_input_tokens` and `cache_creation_input_tokens`.
- `output_tokens` includes `reasoning_tokens`.
- The prompt a call sent is `input_tokens + cache_read_input_tokens + cache_creation_input_tokens`.
- Cost is `input × input rate + cache_read × cache-read rate + cache_creation × cache-write rate + output × output rate`; every token is priced once.

Only the meaning of `input_tokens` changes, and only for streams served over the OpenAI **Chat** transport. Before 1.3, whether `input_tokens` included cache reads depended on the transport, not on the provider name:

- Chat Completions transport: `input_tokens` **included** the cache reads. A pre-1.3 consumer subtracts `cache_read_input_tokens` from `input_tokens` for these. By default this is: OpenAI, DeepSeek, Moonshot, custom OpenAI-compatible endpoints, the Google AI **API-key** route (Google's OpenAI-compatible endpoint), GitHub Copilot models routed to Chat (e.g. `gpt-4o`), and OpenRouter only when `RUSTAIN_OPENROUTER_RESPONSES` is exactly `0`.
- `input_tokens` already **excluded** the cache reads (subtracting again would count the cache twice) for: Anthropic; Ollama; the Responses transport (Codex, xAI, GitHub Copilot models routed to Responses (e.g. `gpt-5`), and OpenRouter by default, i.e. unless the variable is `0`); GitHub Copilot models routed to Messages (e.g. Claude models), which use the Anthropic decoder; and Gemini CLI (Google OAuth), which is a different adapter from the Google API-key route.

A pre-1.3 stream does not say which transport served it, so the provider name alone cannot decide this for OpenRouter (default transport: Responses; Chat only when `RUSTAIN_OPENROUTER_RESPONSES` is `0`) or GitHub Copilot (Chat, Responses or Messages, chosen per model). A consumer of such old streams must obtain the transport elsewhere, or treat the cost as unknown. From 1.3 no such distinction is needed: read `schema_version`, and for `1.3` and later use the counts as they are.

## v1.4 — Ask execution limits

The optional `ask` limits are independent of one another and of the
configured daily budget. An omitted flag sets no bound of that kind.

| Option | Unit and boundary |
|--------|-------------------|
| `--max-turns` | Positive integer number of admitted provider calls, including continuations. The last allowed call can finish normally; if it requests tools that would require another provider call, neither those tools nor the next call are started. |
| `--max-duration` | Positive elapsed seconds on a monotonic clock. Starts at execution entry after file/stdin, prompt and configuration preparation. Includes admission, provider streaming and tool waits. |
| `--max-cost` | Positive USD ceiling for this invocation. Observed usage is priced at a settled call boundary; at or above the ceiling, no further work is admitted. |
| `[budget].daily_limit_usd` | Configured USD ceiling for the local budget day. `ask` checks a strict snapshot of available local ledger records before the first provider call and again at each settled call boundary. This includes prior spend and is separate from the invocation ceiling. |

Cost bounds are best-effort and checked after a call, not an invoice
guarantee or a prediction of in-flight spend. The local daily snapshot
cannot reconstruct missing historical rows or reserve spend against
concurrent writers. An unreadable snapshot, records that may belong to
the budget day but cannot be parsed, or usage that cannot be proved and
priced block further work. A reported zero at a known price is valid;
an absent counter or synthesized zero is not. A stop cancels blocked
work and waits for call settlement; it does not undo side effects that
already started. A bounded stop writes no final session save. A session write
that had already started keeps running to completion, or, if `ask` exits first,
is finished or discarded by the next process that opens the session (the
process-exit recovery described in `docs/sessions.md`); a limited stop never
leaves a session another process cannot open.
Unproved or unpriceable usage from an admitted call also blocks an
invocation that uses `--max-cost` without a daily budget.

At the same decision boundary, the precedence is `max_duration`,
`accounting_error`, `daily_budget`, `max_cost`, `max_turns`, then normal
completion. The effective reason appears as `limit_reason` on the final
JSON document or on the single NDJSON `turn_complete` event. It is
omitted for normal completion and for ordinary operational errors.
`stop_reason` stays separate: the provider's reported reason is
unchanged, or is `cancelled` if a bounded turn stopped before any
provider reason was seen.

A bounded stop preserves assistant output already observed (which can
be empty or partial), along with usage already reported for display.
The JSON `response` is a string even if empty. In NDJSON, prior events
remain valid and any remaining usage or plan events precede the terminal
event. The existing `--final-message-only` response selection still
applies. For text output, the reason is printed on stderr even with
that option.

### Usage ledger compatibility

New shared JSONL receipts can carry optional `provenance`: `observed`
when the serving transport establishes the required counters, `unknown`
when it does not. Legacy rows remain readable and are not rewritten;
strict daily `ask` refuses unproved legacy or estimated rows in the
applicable day. Other ledger readers retain their existing policies.
This metadata is not part of the `ask` output usage objects.

An interrupted call in a limited `ask` run settles its latest trustworthy
observed usage once. If none is available, it writes one `unknown`
receipt only when `--max-cost` or a daily budget is configured. A denied
provider admission has no receipt. Without limits, the existing
interruption and append-failure behavior is unchanged.
If a limited call's append fails, `accounting_error` stops further
admission unless a higher-priority deadline has already expired.

### Exit status

| Code | Meaning |
|------|---------|
| `0` | Normal completion. |
| `1` | A bounded stop or an operational error. |
| `2` | Arguments rejected by the CLI parser. |

## Stdout/stderr discipline

- **stdout** carries only the rendered output for the selected format.
- **stderr** carries tool-call narration, the resume hint, the human-readable deny-count summary, errors, and bounded-stop reasons.

Under `--final-message-only`, the JSON `response` field is narrowed to the final assistant block and stderr narration is quieted; the JSON shape and stdout channel do not change.

## Version-bump enforcement

A schema fingerprint test pins the no-limit v1.3 field-path set. The
v1.4 `limit_reason` is additive and only appears on bounded terminal
output. If the field paths change again, decide whether the change is
additive or breaking before updating the corresponding fingerprint.

## See also

- Story 13.1c: Dry-Run Plan Mode (v1.1 additive fields)
- Story 13.1b: Structured Output & Schema Versioning
- Story 13.1a: Basic `rustain ask` and text output
- `rustain ask --help`
