"""Harbor agent that runs `rustain ask` inside the task container.

    harbor run -d <dataset> \
        --agent-import-path rustain_harbor:RustainAgent \
        --model deepseek/deepseek-v4-flash \
        --ak binary_path=/path/to/static/rustain

The binary is uploaded as built; it must run in the task image (a static musl
build does). The provider key is read from the host environment and passed to
the container for the run only.
"""

from __future__ import annotations

import os
import re
import stat
from dataclasses import dataclass
from pathlib import Path
from typing import override

import litellm
from pydantic import Field

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.agents.options import InstalledAgentOptions
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext, ModelUsage

from rustain_harbor.usage import (
    Prices,
    calls_cost_usd,
    summarize,
    total_input_tokens,
)

_REMOTE_DIR = "/installed-agent"
_STATE_DIR = f"{_REMOTE_DIR}/rustain-state"
_KEY_FILE = f"{_STATE_DIR}/api-key"
_RUNNER = Path(__file__).with_name("run-ask.sh")
STREAM_LOG = "rustain.stream.jsonl"


@dataclass(frozen=True)
class Provider:
    """How rustain reaches one provider, from `rustain/src/adapters/cli/auth/providers.rs`
    and `rustain/src/adapters/openai/variant.rs`."""

    api_key_env: str
    # OpenAI-style usage counts cache hits inside `input_tokens`.
    input_includes_cache: bool
    # Anthropic is rustain's default route and needs no `[provider.*]` entry.
    needs_config_entry: bool = True
    # GET that answers 401 to a wrong bearer key. Set only where that was observed.
    key_check_url: str | None = None


PROVIDERS: dict[str, Provider] = {
    "anthropic": Provider("ANTHROPIC_API_KEY", False, needs_config_entry=False),
    "openai": Provider("OPENAI_API_KEY", True),
    "openrouter": Provider("OPENROUTER_API_KEY", True),
    "google": Provider("GOOGLE_API_KEY", True),
    "deepseek": Provider(
        "DEEPSEEK_API_KEY", True, key_check_url="https://api.deepseek.com/models"
    ),
    "moonshot": Provider("MOONSHOT_API_KEY", True),
}


def render_config(provider_id: str, model: str) -> str:
    """rustain user config that makes `provider_id` the active provider.

    rustain activates the first configured provider that builds; the provider
    kind equals its id for every built-in listed in `PROVIDERS`.
    """
    lines = [f'model = "{model}"']
    if PROVIDERS[provider_id].needs_config_entry:
        lines += [
            "",
            f"[provider.{provider_id}]",
            f'provider_id = "{provider_id}"',
            f'kind = "{provider_id}"',
            f'model_id = "{model}"',
            f'api_key_env = "{PROVIDERS[provider_id].api_key_env}"',
            "enabled = true",
        ]
    return "\n".join(lines) + "\n"


def lookup_prices(provider_id: str, model: str) -> Prices | None:
    """List price from LiteLLM's table, or None when the model is not in it."""
    entry = litellm.model_cost.get(f"{provider_id}/{model}") or litellm.model_cost.get(
        model
    )
    if not entry:
        return None
    input_rate = entry.get("input_cost_per_token")
    output_rate = entry.get("output_cost_per_token")
    if input_rate is None or output_rate is None:
        return None
    cache_read = entry.get("cache_read_input_token_cost")
    cache_creation = entry.get("cache_creation_input_token_cost")
    # Keys such as `input_cost_per_token_above_200k_tokens` give dearer rates
    # for larger prompts. The base rates hold up to the lowest such threshold.
    thresholds = [
        int(match.group(1)) * 1000
        for key in entry
        if (match := re.search(r"_above_(\d+)k_tokens$", key))
    ]
    return Prices(
        input=input_rate,
        output=output_rate,
        cache_read=input_rate if cache_read is None else cache_read,
        cache_creation=input_rate if cache_creation is None else cache_creation,
        flat_up_to=min(thresholds) if thresholds else None,
    )


class RustainOptions(InstalledAgentOptions):
    binary_path: str | None = Field(
        default=None,
        description="Host path of the rustain binary to upload. "
        "Falls back to the RUSTAIN_BINARY environment variable.",
    )


class RustainAgent(BaseInstalledAgent):
    """Uploads a prebuilt rustain binary and runs one `rustain ask` turn."""

    options_model = RustainOptions

    @staticmethod
    @override
    def name() -> str:
        return "rustain"

    @override
    def get_version_command(self) -> str | None:
        return f"{_REMOTE_DIR}/rustain --version"

    @override
    def parse_version(self, stdout: str) -> str:
        # First line: "rustain rustain 0.2.0 (<target>)"; later: "commit: <sha>".
        version = re.search(r"\d+\.\d+\.\d+[^\s)]*", stdout)
        commit = re.search(r"^commit:\s*(\S+)", stdout, re.MULTILINE)
        parts = [version.group(0) if version else stdout.strip()]
        if commit:
            parts.append(commit.group(1))
        return "+".join(parts)

    def _binary(self) -> Path:
        configured = self.options.binary_path or os.environ.get("RUSTAIN_BINARY")
        if not configured:
            raise ValueError(
                "rustain binary not given: pass --ak binary_path=<path> or set "
                "RUSTAIN_BINARY"
            )
        binary = Path(configured).expanduser()
        if not binary.is_file():
            raise FileNotFoundError(f"rustain binary not found: {binary}")
        return binary

    def _provider_and_model(self) -> tuple[str, str]:
        provider_id, model = self._parsed_model_provider, self._parsed_model_name
        if not provider_id or not model:
            raise ValueError(
                "model must be given as <provider>/<model>, e.g. "
                "deepseek/deepseek-v4-flash"
            )
        if provider_id not in PROVIDERS:
            raise ValueError(
                f"provider '{provider_id}' is not supported by this adapter; "
                f"supported: {', '.join(sorted(PROVIDERS))}"
            )
        return provider_id, model

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        binary = self._binary()
        await self.ensure_system_dependencies(environment, ("bash", "ca_certificates"))
        await environment.upload_file(binary, f"{_REMOTE_DIR}/rustain")
        await environment.upload_file(_RUNNER, f"{_REMOTE_DIR}/run-ask.sh")
        await self.exec_as_root(
            environment,
            command=(
                f"chmod 755 {_REMOTE_DIR}/rustain {_REMOTE_DIR}/run-ask.sh && "
                f"ln -sf {_REMOTE_DIR}/rustain /usr/local/bin/rustain && "
                f"mkdir -p {_STATE_DIR}/config {_STATE_DIR}/data && "
                f"chmod -R 1777 {_STATE_DIR}"
            ),
        )

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        provider_id, model = self._provider_and_model()
        api_key_env = PROVIDERS[provider_id].api_key_env
        api_key = self._get_env(api_key_env)
        if not api_key:
            raise ValueError(f"{api_key_env} is not set in the host environment")

        await self._upload_config_text(
            environment,
            content=render_config(provider_id, model),
            remote_path=f"{_STATE_DIR}/config/config.toml",
            filename="config.toml",
        )
        await self._upload_config_text(
            environment,
            content=instruction,
            remote_path=f"{_STATE_DIR}/instruction.txt",
            filename="instruction.txt",
        )
        # The key travels as a private file. An `env` entry of the exec call
        # would put it on the `docker compose exec` command line.
        await self._upload_config_text(
            environment,
            content=api_key,
            remote_path=_KEY_FILE,
            filename="api-key",
        )
        logs = self.environment_logs_dir
        await self.exec_as_agent(
            environment,
            command=f"{_REMOTE_DIR}/run-ask.sh {logs} {_STATE_DIR}/instruction.txt",
            env={
                "RUSTAIN_HARBOR_KEY_ENV": api_key_env,
                "RUSTAIN_HARBOR_KEY_FILE": _KEY_FILE,
                "RUSTAIN_CONFIG_DIR": f"{_STATE_DIR}/config",
                "RUSTAIN_DATA_DIR": f"{_STATE_DIR}/data",
                "RUSTAIN_LOG_PATH": f"{logs}/rustain.log",
            },
        )

    def _redact_api_key(self) -> None:
        """Removes the provider key from the collected logs.

        rustain's shell tool inherits the key, so an agent command such as
        `env` writes it into the stream and the saved session. The runner
        redacts on a normal exit; this covers trials that were killed first.
        """
        try:
            provider_id, _ = self._provider_and_model()
        except ValueError:
            return
        name = PROVIDERS[provider_id].api_key_env
        key = self._get_env(name)
        if not key:
            return
        needle, replacement = key.encode(), f"[redacted:{name}]".encode()
        # The agent wrote these files. Open each one relative to its directory
        # and without following links, so nothing outside the logs is touched.
        for root, _dirs, files, dir_fd in os.fwalk(self.logs_dir):
            for filename in files:
                try:
                    fd = os.open(
                        filename,
                        os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK,
                        dir_fd=dir_fd,
                    )
                except OSError as error:
                    if not os.path.islink(os.path.join(root, filename)):
                        self.logger.warning(
                            "could not check %s/%s for %s: %s",
                            root,
                            filename,
                            name,
                            error,
                        )
                    continue
                with os.fdopen(fd, "r+b") as handle:
                    if not stat.S_ISREG(os.fstat(handle.fileno()).st_mode):
                        continue
                    data = handle.read()
                    if needle in data:
                        handle.seek(0)
                        handle.write(data.replace(needle, replacement))
                        handle.truncate()

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        self._redact_api_key()
        stream = self.logs_dir / STREAM_LOG
        if not stream.is_file():
            return
        summary = summarize(stream.read_text(errors="replace").splitlines())
        provider_id, _ = self._provider_and_model()
        includes_cache = PROVIDERS[provider_id].input_includes_cache
        context.metadata = {
            "schema_version": summary.schema_version,
            "usage_observed": summary.usage_observed,
            "provider_calls": len(summary.calls),
            "first_call_input_tokens": summary.first_call_input_tokens(
                input_includes_cache=includes_cache
            ),
            "tool_calls": summary.tool_calls,
            "stop_reason": summary.stop_reason,
            "deny_count": summary.deny_count,
            "error": summary.error,
        }
        if not summary.has_per_call_usage:
            # A pre-1.2 binary reports the last provider call only; a partial
            # number would read as a real total.
            self.logger.warning(
                "rustain stream schema %s has no per-call usage; token and cost "
                "fields are left empty",
                summary.schema_version,
            )
            return
        if not summary.usage_observed:
            # No provider call reported usage, or the run stopped before the
            # first one ended. Unknown is not zero.
            return

        total = summary.total
        context.n_input_tokens = total_input_tokens(
            total, input_includes_cache=includes_cache
        )
        context.n_cache_tokens = total.cache_read_tokens
        context.n_output_tokens = total.output_tokens

        model_usage: dict[str, ModelUsage] = {}
        unpriced: list[str] = []
        for model, usage in summary.by_model.items():
            prices = lookup_prices(provider_id, model)
            # Priced call by call: a list price can depend on the prompt size.
            cost = (
                None
                if prices is None
                else calls_cost_usd(
                    summary.calls_of(model), prices, input_includes_cache=includes_cache
                )
            )
            if cost is None:
                unpriced.append(model)
            model_usage[model] = ModelUsage(
                n_input_tokens=total_input_tokens(
                    usage, input_includes_cache=includes_cache
                ),
                n_cache_tokens=usage.cache_read_tokens,
                n_output_tokens=usage.output_tokens,
                cost_usd=cost,
            )
        context.model_usage = model_usage
        context.metadata["price_source"] = "litellm.model_cost (list price)"
        context.metadata["unpriced_models"] = unpriced
        if not unpriced:
            context.cost_usd = sum(m.cost_usd or 0.0 for m in model_usage.values())
