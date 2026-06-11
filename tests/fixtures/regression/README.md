# Regression Fixtures

Hand-crafted JSONL fixtures that lock dead specific reducer/streaming bugs.
Each fixture is one `StreamChunk` per line, externally-tagged, camelCase
(matches `#[derive(Serialize, Deserialize)] #[serde(rename_all = "camelCase")]`
on `domain::models::stream::StreamChunk`).

## `concat_prose_2026_04_22.jsonl`

### What it represents

A real assistant turn captured from screenshots
`evidences/Screenshot_20260422_201801.png` and
`evidences/Screenshot_20260428_165314.png` — the user asked for A2A protocol
research, the assistant emitted four distinct prose runs interleaved with six
`Bash`/`curl` tool calls. The fixture replays that arrival order:

1. Prose run A (4 deltas): "Understood — let me put curl through its paces."
2. ToolUse + ToolResult (curl A2A spec)
3. Prose run B (5 deltas): "I'll hit the key sources for A2A protocol research simultaneously."
4. Three parallel ToolUses + their ToolResults
5. Prose run C (5 deltas): "Good — curl is working and we're pulling live data."
6. Two more ToolUses + ToolResults
7. Prose run D (6 deltas): "Let me get the official docs and read them carefully before drafting the design."
8. `TurnComplete { stopReason: "endTurn" }`

Total: 33 events, 4 distinct prose runs (well over the AC's `>= 3` minimum),
6 tool invocations with results.

### The bug it locks dead (Epic 16)

Current `domain::models::stream::apply_chunk` accumulates all `StreamChunk::Text`
deltas into a single `streaming.current_text_buffer`, regardless of intervening
`ToolUse` events. When the message is finalized at `TurnComplete`, every prose
run between every tool call is concatenated into one `content` string and
rendered as a single paragraph **before** the tool flood — the visual pattern
in the screenshots.

Replaying this fixture through the **current** reducer produces a `ChatMessage`
with `content_blocks` containing only the final tool calls and a single
glommed-together `content` string with all 4 prose runs merged.

Replaying it through the **fixed** reducer (Story 16.2) must produce a
`ChatMessage` whose `content_blocks` interleave at least 3 (target: 4) distinct
`Prose` parts with the `ToolInvocation` parts in arrival order.

### Which AC consumes it

- **Story 16.2 AC7** — regression test replays this exact fixture and asserts:
  - `>= 3` distinct `Prose` content parts in the finalized message
  - `Prose` and `ToolInvocation` parts interleaved in arrival order
  - The first prose run is rendered before the first tool call (positional,
    not just present)

### Load-bearing finding for Story 16.2 AC2 (prose-flush rule)

`StreamChunk` has **no explicit text-end / text-stop / content-block-stop event**.
Variants in `src/domain/models/stream.rs`:

```
Text { content, parent_tool_use_id }
Thinking { content, parent_tool_use_id }
ToolUse { id, name, input }
ToolResult { id, content, is_error }
Error { content }
Blocked { content }
TurnComplete { stop_reason }
Usage { usage, session_id }
```

That means **candidate (a)** in Story 16.2 AC2 (explicit `TextEnd` event from
the wire) is not available without an adapter-layer change. Story 16.2 must
adopt **candidate (b): the implicit-flush rule** — flush the current prose
buffer into a `Prose` content block whenever the next chunk is `ToolUse`,
`Thinking`, `Error`, `Blocked`, or `TurnComplete`. The fixture is constructed
to exercise exactly that boundary at four distinct points.

Caveats:
- The fixture uses `parentToolUseId: null` on all `Text` chunks (no nested
  sub-agent streams). A separate fixture should cover the nested case.
- `Usage` chunks are intentionally omitted — they are non-content and
  orthogonal to the concat bug.
- Tool inputs/results are short but plausible curl output; not real fetches.
