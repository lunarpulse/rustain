# Harbor adapter for rustain

Runs `rustain ask` as a [Harbor](https://www.harborframework.com/) agent, so rustain can be measured on Terminal-Bench and other Harbor datasets: pass rate, tokens and dollars per task.

## What it does

`rustain_harbor.agent:RustainAgent` uploads a prebuilt rustain binary into the task container and runs one turn:

```sh
rustain ask --yolo --sandbox-adapter noop --output-format stream-json [--max-cost <usd>] "<task instruction>"
```

- The provider key is read from the host environment and uploaded as a private file. The runner reads it into rustain's environment and deletes the file. The key is not passed through `env` or the command of a Harbor exec call, because Harbor's Docker backend puts those on a `docker compose exec` command line.
- rustain's shell tool inherits that environment, so the agent can read the key and a command such as `env` prints it. The runner replaces the key with `[redacted:<VARIABLE>]` in the stream, the session and stderr when the run ends, and the adapter does the same for a trial that was killed first. Until then the trial logs hold the key, and the model has seen it; use a key you can rotate.
- rustain's config and data live under `/installed-agent/rustain-state`. The sessions a run saves in `./.claude/sessions` are moved to the trial logs, so the verifier sees only what the agent did to the workspace. Sessions that were there before stay. A session that cannot be copied stays too, and the run then exits 3.
- Token usage comes from the `call_usage` lines of the stream (schema 1.2+, see `docs/output-schema.md`). A trial that fails or times out still reports the calls that reported usage. A stream without such a line leaves the token and cost fields empty and sets `usage_observed` to false: unknown is not zero.
- From schema 1.3 every provider's counts are disjoint (`input_tokens` excludes cache reads, `output_tokens` includes reasoning), and the adapter prices them that way. A 1.2 stream keeps the per-provider table in `agent.py`: providers on the Chat transport counted cache hits inside `input_tokens`. That table marks `openrouter` as inclusive, but OpenRouter's default transport is Responses, whose 1.2 counts were already disjoint, and a 1.2 stream does not say which transport served it. For such a run the adapter subtracts the cache hits a second time, so cached rows are **under-priced** and the first-call prompt size is understated; do not trust the cost of a 1.2 OpenRouter run. 1.3 streams are not affected.
- Cost comes from the dated price table `rustain_harbor/prices-2026-11-26.json`, call by call. The adapter reads the table from disk; Harbor's LiteLLM price map is not used. A model missing from the table reports tokens and no cost, and is listed in `unpriced_models`. The adapter also renders the table's rates as a `[pricing]` entry in rustain's config, so `--max-cost` (agent option `max_cost_usd`) prices calls the way the harness does.
- A bounded stop (schema 1.4) is recorded as `limit_reason` in the trial metadata.

Each trial records, besides Harbor's own token and cost fields, `provider_calls` and `first_call_input_tokens`: the prompt size of the first provider call, which is the system prompt, the tool definitions and the task instruction.

## Binary

Task images range from Debian 11 to Ubuntu 24.04, so the binary must be static. The x86_64 Linux release asset is a glibc build whose version needs include GLIBC_2.39, which Debian 12 images (glibc 2.36) lack. `build-musl.sh` builds a static musl binary of a given commit, with the features its `release.yml` ships, and refuses a result that is not statically linked:

```sh
rustup target add x86_64-unknown-linux-musl
uv tool install cargo-zigbuild
CARGO_TARGET_DIR=<target-dir> evals/harbor/build-musl.sh <clean-checkout> <commit> <output-binary>
```

A binary older than schema 1.2 reports the last provider call only; the adapter then leaves the token and cost fields empty. A binary older than schema 1.3 counts cache hits inside `input_tokens` for requests sent over the Chat Completions transport only (see `docs/output-schema.md`, v1.3); the adapter's per-provider table approximates that by provider name.

## Run

Task containers need Docker, and the devcontainer has none, so the run happens on a machine with Docker. The eval-4 run, with its plan, owner commands and outputs, is described in [README-eval4.md](README-eval4.md). In short:

```sh
evals/harbor/run-baseline.sh check <static-rustain-binary> <jobs-dir> <run-id>   # offline
export DEEPSEEK_API_KEY=...
evals/harbor/run-isolated.sh <static-rustain-binary> <jobs-dir> <run-id>
evals/harbor/run-baseline.sh aggregate <jobs-dir> <run-id>                       # offline
```

`run-isolated.sh` keeps that machine clean. It starts one disposable Docker-in-Docker container and runs everything inside it: Harbor, its caches, the task images and the task containers. Only the jobs directory survives. The stock `docker:dind` image is removed afterwards unless it was already present. The container is privileged, which a nested Docker daemon requires. Only the key variables that the run plan's treatments name enter the container.

- The keys are checked before any task image is pulled. DeepSeek keys are verified against the API; for other providers, the check only confirms the variable is set.
- The nested daemon's log is written to `<jobs-dir>/dind-dockerd.log`.
- Results are handed back to the owner of the jobs directory when the run ends or is interrupted. If the container is killed instead, they may be left owned by root.

`run-baseline.sh run` is the same run without the isolation, for an environment that already has Docker and uv and may be written to. It installs Harbor from `uv.lock` into `<jobs-dir>/.rustain-harbor/venv`, and it leaves Harbor's task cache in `~/.cache/harbor` and the pulled task images behind. It takes no other argument: every setting comes from the run plan.

Supported providers are rustain's built-in keyed ones: `anthropic`, `openai`, `openrouter`, `google`, `deepseek`, `moonshot`. Only `deepseek` has been exercised.

## Clean up

A run that ends normally leaves only its jobs directory. `clean-host.sh` handles a run that was killed. It lists by default and removes with `--apply`, and it removes only what these scripts recorded as theirs:

```sh
evals/harbor/clean-host.sh                                   # list
evals/harbor/clean-host.sh --apply                           # stopped runner containers
evals/harbor/clean-host.sh --apply --jobs-dir <dir>          # and the docker:dind image a killed run pulled
evals/harbor/clean-host.sh --apply --jobs-dir <dir> --results  # and the recorded job directories
```

- Containers are found by the label `run-isolated.sh` gives them. One that is still running is left alone.
- The `docker:dind` image is removed only when `<dir>/.rustain-harbor/pulled-image` shows that a run pulled it and could not remove it.
- `--results` removes the run directories listed in `<dir>/.rustain-harbor/jobs`, the nested daemon's log and the record (including the Harbor environment under it). Anything else in `<dir>` stays, including the eval-4 spend ledger and launcher lock. A cleaned run id cannot be reused; use a new one.
- Harbor's own task images and `~/.cache/harbor`, which `run-baseline.sh` leaves outside the container, are reported and not removed: they may serve other Harbor work. `harbor cache clean` removes them.

The uv and pip caches are shared with other projects and are not touched.

## Tests

```sh
cd evals/harbor
uv sync --locked --extra dev
.venv/bin/python -m pytest -q
```

The launcher tests run `run-baseline.sh` as a process, with doubles for uv, Harbor and the provider on `PATH`. They need no network and no Docker.
