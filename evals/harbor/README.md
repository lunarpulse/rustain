# Harbor adapter for rustain

Runs `rustain ask` as a [Harbor](https://www.harborframework.com/) agent, so rustain can be measured on Terminal-Bench and other Harbor datasets: pass rate, tokens and dollars per task.

## What it does

`rustain_harbor:RustainAgent` uploads a prebuilt rustain binary into the task container and runs one turn:

```sh
rustain ask --yolo --output-format stream-json "<task instruction>"
```

- The provider key is read from the host environment and uploaded as a private file. The runner reads it into rustain's environment and deletes the file. The key is not passed through `env` or the command of a Harbor exec call, because Harbor's Docker backend puts those on a `docker compose exec` command line.
- rustain's shell tool inherits that environment, so the agent can read the key and a command such as `env` prints it. The runner replaces the key with `[redacted:<VARIABLE>]` in the stream, the session and stderr when the run ends, and the adapter does the same for a trial that was killed first. Until then the trial logs hold the key, and the model has seen it; use a key you can rotate.
- rustain's config and data live under `/installed-agent/rustain-state`. The sessions a run saves in `./.claude/sessions` are moved to the trial logs, so the verifier sees only what the agent did to the workspace. Sessions that were there before stay. A session that cannot be copied stays too, and the run then exits 3.
- Token usage comes from the `call_usage` lines of the stream (schema 1.2, see `docs/output-schema.md`). A trial that fails or times out still reports the calls that reported usage. A stream without such a line leaves the token and cost fields empty and sets `usage_observed` to false: unknown is not zero.
- Cost uses LiteLLM's list price for `<provider>/<model>`, call by call. A model without a price reports tokens and no cost. So does a model with a call whose prompt is larger than the flat list price covers; the dearer rates above that size are not modelled.

Each trial records, besides Harbor's own token and cost fields, `provider_calls` and `first_call_input_tokens`: the prompt size of the first provider call, which is the system prompt, the tool definitions and the task instruction.

## Binary

Task images range from Debian 11 to Ubuntu 24.04, so the binary must be static:

```sh
rustup target add x86_64-unknown-linux-musl
uv tool install cargo-zigbuild
cargo zigbuild --locked --target x86_64-unknown-linux-musl
```

A binary older than schema 1.2 reports the last provider call only; the adapter then leaves the token and cost fields empty.

## Run

Task containers need Docker, which the devcontainer does not have, so the run happens on a machine with Docker.

`run-isolated.sh` keeps that machine clean. It starts one disposable Docker-in-Docker container and runs everything inside it: Harbor, its caches, the task images and the task containers. Only the jobs directory survives; the stock `docker:dind` image is removed afterwards unless it was already present. The container is privileged, which a nested Docker daemon requires.

```sh
export DEEPSEEK_API_KEY=...
evals/harbor/run-isolated.sh <static-rustain-binary> <jobs-dir>
```

The run covers five fixed Terminal-Bench 2 tasks, once each, on `deepseek/deepseek-v4-flash` (override with `RUSTAIN_EVAL_MODEL=<provider>/<model>`) and prints the cost table. Extra arguments go to `harbor run`, for example `--n-concurrent 1`.

- The key is checked before any task image is pulled. DeepSeek keys are verified against the API; for other providers only the variable being set is checked.
- The nested daemon's log is written to `<jobs-dir>/dind-dockerd.log`.
- Results are handed back to the owner of the jobs directory when the run ends or is interrupted. If the container is killed instead, they may be left owned by root.

`run-baseline.sh` is the same run without the isolation, for an environment that already has Docker and uv and may be written to. It leaves uv's cache, Harbor's task cache in `~/.cache/harbor` and the pulled task images behind. It exits with Harbor's status, or with the aggregator's when Harbor succeeded and the table could not be produced.

The table gives a job total only when every trial has the figure; `trials_with_cost` says how many do.

To aggregate an existing job again:

```sh
python -m rustain_harbor.baseline <jobs-dir>/<job-name>
```

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
- `--results` removes the job directories listed in `<dir>/.rustain-harbor/jobs`, the nested daemon's log and the record. Anything else in `<dir>` stays.
- Harbor's own task images and `~/.cache/harbor`, which `run-baseline.sh` leaves outside the container, are reported and not removed: they may serve other Harbor work. `harbor cache clean` removes them.

The uv and pip caches are shared with other projects and are not touched.

## Tests

```sh
cd evals/harbor
uv venv .venv && uv pip install --python .venv/bin/python -e '.[dev]'
.venv/bin/python -m pytest -q
```
