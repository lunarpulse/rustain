import asyncio
import json
import tomllib
from dataclasses import dataclass
from pathlib import Path

import pytest

from harbor.models.agent.context import AgentContext

from rustain_harbor import agent as agent_module
from rustain_harbor.agent import STREAM_LOG, RustainAgent, lookup_prices, render_config
from rustain_harbor.usage import Prices

SECRET = "sk-test-not-a-real-key"


def make_agent(tmp_path: Path, model: str = "deepseek/deepseek-v4-flash", **kwargs):
    binary = tmp_path / "rustain"
    binary.write_bytes(b"\x7fELF")
    logs = tmp_path / "logs"
    logs.mkdir(exist_ok=True)
    return RustainAgent(
        logs_dir=logs, model_name=model, binary_path=str(binary), **kwargs
    )


def write_stream(agent: RustainAgent, events: list[dict]) -> None:
    (agent.logs_dir / STREAM_LOG).write_text(
        "".join(json.dumps(event) + "\n" for event in events)
    )


def call_usage(input_tokens, output_tokens, cache_read=0, schema="1.2"):
    return {
        "schema_version": schema,
        "type": "call_usage",
        "model": "deepseek-v4-flash",
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cache_read_input_tokens": cache_read,
        },
    }


def test_config_activates_the_requested_provider():
    config = tomllib.loads(render_config("deepseek", "deepseek-v4-flash"))

    assert config["model"] == "deepseek-v4-flash"
    assert config["provider"]["deepseek"] == {
        "provider_id": "deepseek",
        "kind": "deepseek",
        "model_id": "deepseek-v4-flash",
        "api_key_env": "DEEPSEEK_API_KEY",
        "enabled": True,
    }


def test_anthropic_uses_the_default_route():
    config = tomllib.loads(render_config("anthropic", "claude-sonnet-4-5"))

    assert config == {"model": "claude-sonnet-4-5"}


def test_version_names_release_and_commit(tmp_path):
    stdout = (
        "rustain rustain 0.2.0 (x86_64-unknown-linux-musl)\n"
        "build: 2026-11-13T06:23:01Z\n"
        "commit: 3f91770b\n"
        "rust: 1.91\n"
    )

    assert make_agent(tmp_path).parse_version(stdout) == "0.2.0+3f91770b"


def test_unsupported_provider_is_rejected(tmp_path):
    agent = make_agent(tmp_path, model="xai/grok-4.6")

    with pytest.raises(ValueError, match="not supported"):
        agent._provider_and_model()


def test_context_reports_turn_totals_and_cost(tmp_path, monkeypatch):
    monkeypatch.setattr(
        agent_module,
        "lookup_prices",
        lambda provider, model: Prices(
            input=1e-6, output=2e-6, cache_read=1e-7, cache_creation=1e-6
        ),
    )
    agent = make_agent(tmp_path)
    write_stream(
        agent,
        [
            call_usage(7000, 50),
            {"schema_version": "1.2", "type": "tool_use", "id": "t1", "name": "Bash"},
            call_usage(7400, 80, cache_read=6900),
            {"schema_version": "1.2", "type": "turn_complete", "stop_reason": "end_turn"},
        ],
    )
    context = AgentContext()

    agent.populate_context_post_run(context)

    assert context.n_input_tokens == 14400
    assert context.n_cache_tokens == 6900
    assert context.n_output_tokens == 130
    # 7500 uncached input, 6900 cache hits, 130 output
    assert context.cost_usd == pytest.approx(7500 * 1e-6 + 6900 * 1e-7 + 130 * 2e-6)
    assert context.metadata["first_call_input_tokens"] == 7000
    assert context.metadata["provider_calls"] == 2


def test_unpriced_model_reports_tokens_without_a_cost(tmp_path, monkeypatch):
    monkeypatch.setattr(agent_module, "lookup_prices", lambda provider, model: None)
    agent = make_agent(tmp_path)
    write_stream(agent, [call_usage(7000, 50)])
    context = AgentContext()

    agent.populate_context_post_run(context)

    assert context.n_input_tokens == 7000
    assert context.cost_usd is None
    assert context.metadata["unpriced_models"] == ["deepseek-v4-flash"]


def test_pre_1_2_binary_reports_no_token_totals(tmp_path):
    agent = make_agent(tmp_path)
    write_stream(
        agent,
        [
            {
                "schema_version": "1.1",
                "type": "usage",
                "usage": {"input_tokens": 9000, "output_tokens": 20},
            }
        ],
    )
    context = AgentContext()

    agent.populate_context_post_run(context)

    assert context.n_input_tokens is None
    assert context.cost_usd is None


@dataclass
class ExecResult:
    return_code: int = 0
    stdout: str = ""
    stderr: str = ""


class RecordingEnvironment:
    """Stands in for a task container: records what the agent sends to it."""

    default_user = None

    def __init__(self):
        self.commands: list[str] = []
        self.envs: list[dict[str, str]] = []
        self.uploads: dict[str, str] = {}

    async def exec(self, command, cwd=None, env=None, timeout_sec=None, user=None):
        self.commands.append(command)
        self.envs.append(dict(env or {}))
        return ExecResult()

    async def upload_file(self, source_path, target_path):
        self.uploads[target_path] = Path(source_path).read_text(errors="replace")


def test_api_key_travels_as_a_file_and_never_as_an_exec_argument(tmp_path):
    agent = make_agent(tmp_path, extra_env={"DEEPSEEK_API_KEY": SECRET})
    environment = RecordingEnvironment()

    asyncio.run(agent.run("fix the build", environment, AgentContext()))

    # Harbor's Docker backend turns `env` and `command` into process arguments.
    assert all(SECRET not in value for env in environment.envs for value in env.values())
    assert all(SECRET not in command for command in environment.commands)
    run_env = environment.envs[-1]
    assert run_env["RUSTAIN_HARBOR_KEY_ENV"] == "DEEPSEEK_API_KEY"
    assert environment.uploads[run_env["RUSTAIN_HARBOR_KEY_FILE"]] == SECRET
    assert "fix the build" in environment.uploads.values()


def test_missing_api_key_fails_before_touching_the_container(tmp_path, monkeypatch):
    monkeypatch.delenv("DEEPSEEK_API_KEY", raising=False)
    agent = make_agent(tmp_path)
    environment = RecordingEnvironment()

    with pytest.raises(ValueError, match="DEEPSEEK_API_KEY"):
        asyncio.run(agent.run("fix the build", environment, AgentContext()))

    assert environment.commands == []
    assert environment.uploads == {}


def test_killed_trial_logs_are_redacted_before_they_are_read(tmp_path):
    # A timed-out run never reaches the runner's own redaction step.
    agent = make_agent(tmp_path, extra_env={"DEEPSEEK_API_KEY": SECRET})
    leaked = {
        "schema_version": "1.2",
        "type": "tool_result",
        "content": f"HOME=/root DEEPSEEK_API_KEY={SECRET}",
    }
    write_stream(agent, [call_usage(100, 10), leaked])
    session = agent.logs_dir / "sessions" / "s1" / "conversation.json"
    session.parent.mkdir(parents=True)
    session.write_text(json.dumps(leaked))
    context = AgentContext()

    agent.populate_context_post_run(context)

    stream = (agent.logs_dir / STREAM_LOG).read_text()
    assert SECRET not in stream
    assert SECRET not in session.read_text()
    assert "[redacted:DEEPSEEK_API_KEY]" in stream
    assert context.metadata["provider_calls"] == 1


def test_redaction_never_follows_a_link_out_of_the_logs(tmp_path):
    agent = make_agent(tmp_path, extra_env={"DEEPSEEK_API_KEY": SECRET})
    outside_file = tmp_path / "outside.env"
    outside_file.write_text(f"DEEPSEEK_API_KEY={SECRET}\n")
    outside_dir = tmp_path / "outside"
    outside_dir.mkdir()
    (outside_dir / "notes.txt").write_text(SECRET)
    (agent.logs_dir / "link").symlink_to(outside_file)
    (agent.logs_dir / "linked-dir").symlink_to(outside_dir, target_is_directory=True)
    write_stream(agent, [call_usage(100, 10)])

    agent.populate_context_post_run(AgentContext())

    assert outside_file.read_text() == f"DEEPSEEK_API_KEY={SECRET}\n"
    assert (outside_dir / "notes.txt").read_text() == SECRET


def test_run_without_reported_usage_leaves_token_fields_unset(tmp_path):
    agent = make_agent(tmp_path)
    write_stream(
        agent,
        [{"schema_version": "1.2", "type": "turn_complete", "stop_reason": "end_turn"}],
    )
    context = AgentContext()

    agent.populate_context_post_run(context)

    assert context.metadata["usage_observed"] is False
    assert context.n_input_tokens is None
    assert context.n_output_tokens is None
    assert context.cost_usd is None


def test_first_call_prompt_size_counts_anthropic_cache_tokens(tmp_path):
    agent = make_agent(tmp_path, model="anthropic/claude-sonnet-4-5")
    first_call = call_usage(12, 5, cache_read=6000)
    first_call["usage"]["cache_creation_input_tokens"] = 900
    write_stream(agent, [first_call])
    context = AgentContext()

    agent.populate_context_post_run(context)

    assert context.metadata["first_call_input_tokens"] == 6912


def test_a_call_above_the_flat_rate_threshold_leaves_the_cost_unset(tmp_path, monkeypatch):
    monkeypatch.setitem(
        agent_module.litellm.model_cost,
        "deepseek/deepseek-v4-flash",
        {
            "input_cost_per_token": 1e-6,
            "output_cost_per_token": 2e-6,
            "input_cost_per_token_above_200k_tokens": 2e-6,
            "output_cost_per_token_above_200k_tokens": 3e-6,
        },
    )
    assert lookup_prices("deepseek", "deepseek-v4-flash").flat_up_to == 200_000
    agent = make_agent(tmp_path)
    write_stream(agent, [call_usage(1000, 10), call_usage(200_001, 10)])
    context = AgentContext()

    agent.populate_context_post_run(context)

    assert context.n_input_tokens == 201_001
    assert context.cost_usd is None
    assert context.metadata["unpriced_models"] == ["deepseek-v4-flash"]
