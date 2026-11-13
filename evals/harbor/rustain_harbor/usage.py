"""Token usage and cost of one `rustain ask --output-format stream-json` run.

The stream format is documented in `docs/output-schema.md`. Usage is read from
the `call_usage` lines (schema 1.2+): they exist for every completed provider
call, including the calls of a turn that later failed or was killed.
"""

from __future__ import annotations

import json
from collections.abc import Iterable
from dataclasses import dataclass, field

# First schema whose usage covers every provider call of the turn.
MIN_SCHEMA = (1, 2)


@dataclass(frozen=True)
class Usage:
    """Token counts as rustain reports them (provider-native semantics)."""

    input_tokens: int = 0
    output_tokens: int = 0
    cache_read_tokens: int = 0
    cache_creation_tokens: int = 0
    reasoning_tokens: int = 0

    def __add__(self, other: Usage) -> Usage:
        return Usage(
            self.input_tokens + other.input_tokens,
            self.output_tokens + other.output_tokens,
            self.cache_read_tokens + other.cache_read_tokens,
            self.cache_creation_tokens + other.cache_creation_tokens,
            self.reasoning_tokens + other.reasoning_tokens,
        )


@dataclass(frozen=True)
class Prices:
    """USD per token."""

    input: float
    output: float
    cache_read: float
    cache_creation: float
    # Largest prompt, in tokens, that these rates cover. LiteLLM lists dearer
    # rates above it for some models; those are not modelled.
    flat_up_to: int | None = None


@dataclass(frozen=True)
class RunSummary:
    schema_version: str | None = None
    # (model, usage) per provider call, in call order.
    calls: tuple[tuple[str, Usage], ...] = ()
    tool_calls: int = 0
    stop_reason: str | None = None
    deny_count: int | None = None
    error: str | None = None
    by_model: dict[str, Usage] = field(default_factory=dict)

    @property
    def total(self) -> Usage:
        total = Usage()
        for _, usage in self.calls:
            total += usage
        return total

    def first_call_input_tokens(self, *, input_includes_cache: bool) -> int | None:
        """Prompt size of the first provider call: system prompt, tool
        definitions and the task instruction, before any tool output."""
        if not self.calls:
            return None
        return total_input_tokens(
            self.calls[0][1], input_includes_cache=input_includes_cache
        )

    @property
    def has_per_call_usage(self) -> bool:
        """False for a stream from a binary older than schema 1.2, whose usage
        covers the last provider call only."""
        if self.schema_version is None:
            return False
        try:
            major, minor = (int(part) for part in self.schema_version.split(".")[:2])
        except ValueError:
            return False
        return (major, minor) >= MIN_SCHEMA

    @property
    def usage_observed(self) -> bool:
        """True when the stream holds per-call usage. A 1.2 stream without a
        `call_usage` line leaves usage unknown, not zero: no call reported
        any, or the run stopped before the first call ended."""
        return self.has_per_call_usage and bool(self.calls)

    def calls_of(self, model: str) -> list[Usage]:
        return [usage for name, usage in self.calls if name == model]


def _usage(raw: dict) -> Usage:
    return Usage(
        input_tokens=raw.get("input_tokens") or 0,
        output_tokens=raw.get("output_tokens") or 0,
        cache_read_tokens=raw.get("cache_read_input_tokens") or 0,
        cache_creation_tokens=raw.get("cache_creation_input_tokens") or 0,
        reasoning_tokens=raw.get("reasoning_tokens") or 0,
    )


def summarize(lines: Iterable[str]) -> RunSummary:
    """Fold stream-json lines into a summary.

    A line that is not a JSON object is skipped: a run killed mid-write leaves
    a truncated last line.
    """
    schema_version: str | None = None
    calls: list[tuple[str, Usage]] = []
    by_model: dict[str, Usage] = {}
    tool_calls = 0
    stop_reason: str | None = None
    deny_count: int | None = None
    error: str | None = None

    for line in lines:
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if not isinstance(event, dict):
            continue
        schema_version = event.get("schema_version", schema_version)
        kind = event.get("type")
        if kind == "call_usage":
            model = event.get("model") or ""
            usage = _usage(event.get("usage") or {})
            calls.append((model, usage))
            by_model[model] = by_model.get(model, Usage()) + usage
        elif kind == "tool_use":
            tool_calls += 1
        elif kind == "turn_complete":
            stop_reason = event.get("stop_reason")
            deny_count = event.get("deny_count")
        elif kind == "error":
            error = (event.get("error") or {}).get("message")

    return RunSummary(
        schema_version=schema_version,
        calls=tuple(calls),
        tool_calls=tool_calls,
        stop_reason=stop_reason,
        deny_count=deny_count,
        error=error,
        by_model=by_model,
    )


def uncached_input_tokens(usage: Usage, *, input_includes_cache: bool) -> int:
    """Input tokens billed at the full input rate.

    OpenAI-style usage counts cache hits inside `input_tokens`; Anthropic-style
    usage reports them separately.
    """
    if input_includes_cache:
        return max(usage.input_tokens - usage.cache_read_tokens, 0)
    return usage.input_tokens


def total_input_tokens(usage: Usage, *, input_includes_cache: bool) -> int:
    """All prompt tokens, cache hits and cache writes included."""
    if input_includes_cache:
        return usage.input_tokens
    return usage.input_tokens + usage.cache_read_tokens + usage.cache_creation_tokens


def cost_usd(
    usage: Usage, prices: Prices, *, input_includes_cache: bool
) -> float | None:
    """List-price cost of one provider call, or None when its prompt is larger
    than the flat rates cover."""
    if (
        prices.flat_up_to is not None
        and total_input_tokens(usage, input_includes_cache=input_includes_cache)
        > prices.flat_up_to
    ):
        return None
    return (
        uncached_input_tokens(usage, input_includes_cache=input_includes_cache)
        * prices.input
        + usage.cache_read_tokens * prices.cache_read
        + usage.cache_creation_tokens * prices.cache_creation
        + usage.output_tokens * prices.output
    )


def calls_cost_usd(
    calls: Iterable[Usage], prices: Prices, *, input_includes_cache: bool
) -> float | None:
    """Sum of the per-call costs, or None as soon as one call has no price."""
    total = 0.0
    for usage in calls:
        cost = cost_usd(usage, prices, input_includes_cache=input_includes_cache)
        if cost is None:
            return None
        total += cost
    return total
