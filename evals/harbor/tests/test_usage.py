import json

import pytest

from rustain_harbor.usage import (
    Prices,
    Usage,
    calls_cost_usd,
    cost_usd,
    summarize,
    total_input_tokens,
)


def call_usage(model, input_tokens, output_tokens, cache_read=None, schema="1.2"):
    usage = {"input_tokens": input_tokens, "output_tokens": output_tokens}
    if cache_read is not None:
        usage["cache_read_input_tokens"] = cache_read
    return json.dumps(
        {"schema_version": schema, "type": "call_usage", "usage": usage, "model": model}
    )


def event(kind, **fields):
    return json.dumps({"schema_version": "1.2", "type": kind, **fields})


def test_totals_cover_every_provider_call():
    summary = summarize(
        [
            event("tool_use", id="t1", name="Bash", input={}),
            call_usage("flash", 7000, 50, cache_read=0),
            event("tool_result", id="t1", result={"content": "ok", "is_error": False}),
            event("tool_use", id="t2", name="Read", input={}),
            call_usage("flash", 7400, 80, cache_read=6900),
            call_usage("pro", 9000, 300),
            event("usage", usage={"input_tokens": 23400, "output_tokens": 430}),
            event("turn_complete", stop_reason="end_turn", deny_count=0),
        ]
    )

    assert summary.total == Usage(
        input_tokens=23400, output_tokens=430, cache_read_tokens=6900
    )
    assert summary.by_model["flash"] == Usage(
        input_tokens=14400, output_tokens=130, cache_read_tokens=6900
    )
    assert summary.by_model["pro"] == Usage(input_tokens=9000, output_tokens=300)
    assert summary.first_call_input_tokens(input_includes_cache=True) == 7000
    assert summary.tool_calls == 2
    assert summary.stop_reason == "end_turn"
    assert summary.usage_observed


def test_killed_run_keeps_the_calls_it_completed():
    lines = [
        call_usage("flash", 7000, 50),
        call_usage("flash", 7400, 80),
        '{"schema_version":"1.2","type":"tool_us',
    ]

    summary = summarize(lines)

    assert summary.total == Usage(input_tokens=14400, output_tokens=130)
    assert summary.stop_reason is None
    assert summary.error is None


def test_failed_turn_reports_error_and_completed_calls():
    summary = summarize(
        [
            call_usage("flash", 7000, 50),
            event("error", error={"message": "turn failed: rate limit"}),
        ]
    )

    assert summary.total == Usage(input_tokens=7000, output_tokens=50)
    assert summary.error == "turn failed: rate limit"


def test_pre_1_2_stream_is_flagged_incomplete():
    old = json.dumps(
        {
            "schema_version": "1.1",
            "type": "usage",
            "usage": {"input_tokens": 9000, "output_tokens": 20},
        }
    )

    summary = summarize([old])

    assert not summary.has_per_call_usage
    assert summary.calls == ()


PRICES = Prices(input=1e-6, output=2e-6, cache_read=1e-7, cache_creation=1.25e-6)


def test_openai_style_usage_bills_cache_hits_once():
    usage = Usage(input_tokens=1000, output_tokens=100, cache_read_tokens=400)

    assert total_input_tokens(usage, input_includes_cache=True) == 1000
    assert cost_usd(usage, PRICES, input_includes_cache=True) == pytest.approx(
        600 * 1e-6 + 400 * 1e-7 + 100 * 2e-6
    )


def test_anthropic_style_usage_adds_cache_tokens_to_input():
    usage = Usage(
        input_tokens=1000,
        output_tokens=100,
        cache_read_tokens=400,
        cache_creation_tokens=200,
    )

    assert total_input_tokens(usage, input_includes_cache=False) == 1600
    assert cost_usd(usage, PRICES, input_includes_cache=False) == pytest.approx(
        1000 * 1e-6 + 400 * 1e-7 + 200 * 1.25e-6 + 100 * 2e-6
    )


def test_stream_without_a_usage_line_leaves_usage_unknown():
    summary = summarize([event("turn_complete", stop_reason="end_turn", deny_count=0)])

    assert summary.has_per_call_usage
    assert not summary.usage_observed


def test_cost_stops_at_the_first_call_above_the_flat_rates():
    tiered = Prices(
        input=1e-6, output=2e-6, cache_read=1e-7, cache_creation=1e-6, flat_up_to=200_000
    )
    at_threshold = Usage(input_tokens=200_000, output_tokens=10)
    above = Usage(input_tokens=200_001, output_tokens=10)

    assert calls_cost_usd([at_threshold], tiered, input_includes_cache=True) == (
        pytest.approx(200_000 * 1e-6 + 10 * 2e-6)
    )
    assert calls_cost_usd([at_threshold, above], tiered, input_includes_cache=True) is None
