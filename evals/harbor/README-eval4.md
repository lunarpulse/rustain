# Eval-4 owner run: `cmp-benign-v1`, two DeepSeek treatments

The run executes the frozen run plan `eval4/run-plan.json`:

- **Tasks:** the five frozen `cmp-benign-v1` tasks in `eval4/cmp-benign-v1.manifest.json`, each bound to its Harbor package digest (`--task terminal-bench/<name>@sha256:…`).
- **Treatments:** `v4-flash` = `deepseek/deepseek-v4-flash` and `v4-pro` = `deepseek/deepseek-v4-pro`. The provider is fixed and only the model varies.
- **Attempts:** 3. That makes 30 slots, and each slot is its own Harbor job.
- **Binary:** a musl build of v0.6.0 (commit `512509c4`), not the signed release asset; same source and features. `build-musl.sh` builds it, and the plan pins its SHA-256. The release's glibc asset needs GLIBC_2.39, which three of the five task images (Debian 12) do not have.
- **rustain flags:** `ask --yolo --sandbox-adapter noop`, published as "sandbox off; container-isolated". `--max-cost` is set per slot. There is no `--max-turns` and no `--max-duration`. Each task keeps its own Harbor timeouts.
- **Harbor:** 0.25.0, installed from `uv.lock` with hash checking. Slots run one at a time, with `--n-attempts 1 --n-concurrent 1 --max-retries 0`.
- **Prices:** `rustain_harbor/prices-2026-11-26.json`. Both the harness cost and rustain's `--max-cost` (through a rendered `[pricing]` entry) use this table.
- **Spend:** the cap is 14 USD. Before each slot, the launcher stops when the known harness cost of the finished slots reaches the cap, or when a finished slot's cost is unknown. Otherwise it passes `--max-cost min(cap − known cost, 0.46)`. One provider call can overshoot either bound.

## What the owner supplies

- A disposable Docker host with `bash`, `realpath`, and Docker able to run a privileged container. The host also needs Python 3.12 or later for the offline steps.
- Environment variable names: `DEEPSEEK_API_KEY` only. Use a restricted, rotatable key, because the agent's shell can read it.
- The binary from `build-musl.sh`. Its `.sha256` must equal `product.binary_sha256` in `eval4/run-plan.json`.
- A new run id (`[a-z0-9-]`, up to 40 characters) for every attempt at the matrix. A used run id is refused.

## Command sequence

From the repository root at the landed commit:

```sh
# 0. Build the binary once, off the Docker host if needed.
#    The source is a clean checkout of v0.6.0.
evals/harbor/build-musl.sh <v0.6.0-checkout> 512509c4 <bin>/rustain-0.6.0-musl-512509c4

# 1. Preflight, offline. Validates the plan, manifest, price table, lock and binary hash,
#    and prints the run header and all 30 harbor argvs. Writes nothing.
evals/harbor/run-baseline.sh check <bin>/rustain-0.6.0-musl-512509c4 <jobs-dir> <run-id>

# 2. Run, on the Docker host. A disposable docker:dind container gets only DEEPSEEK_API_KEY.
#    The key is checked against the provider before the first slot.
export DEEPSEEK_API_KEY=...
evals/harbor/run-isolated.sh <bin>/rustain-0.6.0-musl-512509c4 <jobs-dir> <run-id>

# 3. Aggregate, offline, as often as needed. If any slot is missing, duplicated,
#    unexpected or unidentifiable, or if the header or a recorded argv was altered,
#    aggregation refuses: no totals and no results.json.
evals/harbor/run-baseline.sh aggregate <jobs-dir> <run-id>
```

`run-baseline.sh run <binary> <jobs-dir> <run-id>` runs step 2 without the dind isolation. It needs Docker and uv on the machine.

## What a run leaves in `<jobs-dir>/<run-id>/`

| File | Content |
|---|---|
| `header.json`, `header.sha256` | Written once, before preflight. Holds the run id, product commit, binary hash and path, manifest hash and comparator receipt, treatments, every slot in launch order with the hash of the key list, Harbor version and lock hash, price table id, date and hash, limits, spend cap, task timeouts, concurrency, and the argv template. |
| `ledger.jsonl` | One line per launcher event: `slot_started` with the exact argv it then executes and its `--max-cost`, `slot_finished` with Harbor's exit status and the slot's cost status, and `stopped` with the guard's reason. |
| `jobs/<run-id>--cmp-benign-v1--<task>--<treatment>--a<n>/` | The Harbor job of one slot. |
| `results.json` | Written only by a successful aggregation. Holds the per-slot outcome (`pass`, `fail`, `harbor_timeout`, `harbor_exception`, `limit_stop`, `verifier_absent`), whether a reward came with an exception, the cost status (`known`, `usage_unknown`, `unpriced`), and a summary per treatment. |

A stopped run, or one with a missing slot, is not resumed. Start a new run id.

## Harbor 0.25.0 behavior the launcher relies on (read in the pinned wheel)

- `harbor run` exits 0 when the job completes, whatever the trials' outcomes. Exit 1 means a configuration, preflight or dataset error. The ledger keeps the exit status only as a diagnostic.
- Retries default to 0. A retry deletes the failed trial's directory before it runs again, so the launcher passes `--max-retries 0` explicitly.
- A job directory that already exists is resumed when its config is equal, and incomplete trial directories are deleted. With a different config it fails with `FileExistsError`. The launcher therefore never reuses a job name or directory.
- The timeout exceptions are `AgentTimeoutError`, `AgentSetupTimeoutError`, `VerifierTimeoutError` and `EnvironmentStartTimeoutError`. The verifier still runs after an `AgentTimeoutError` or a non-zero agent exit, so such a result can hold a reward next to an exception.
- `result.json` records the model as `config.agent.model_name` (the `--model` string) and as `agent_info.model_info` (`name`, `provider`). It records the task as `task_name` and `task_id.ref` (the package digest).
