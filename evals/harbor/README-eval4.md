# Eval-4 owner run: `cmp-benign-v1`, two DeepSeek treatments

The run executes the frozen run plan `eval4/run-plan.json`:

- **Tasks:** the five frozen `cmp-benign-v1` tasks in `eval4/cmp-benign-v1.manifest.json`, each bound to its Harbor package digest (`--task terminal-bench/<name>@sha256:…`).
- **Treatments:** `v4-flash` = `deepseek/deepseek-v4-flash` and `v4-pro` = `deepseek/deepseek-v4-pro`. The provider is fixed and only the model varies.
- **Attempts:** 3. That makes 30 slots, and each slot is its own Harbor job.
- **Binary:** a musl build of v0.6.0 (commit `512509c4`), not the signed release asset; same source and features. `build-musl.sh` builds it, and the plan pins its SHA-256. The release's glibc asset needs GLIBC_2.39, which three of the five task images (Debian 12) do not have.
- **rustain flags:** `ask --yolo --sandbox-adapter noop`, published as "sandbox off; container-isolated". `--max-cost` is set per slot. There is no `--max-turns` and no `--max-duration`. Each task keeps its own Harbor timeouts.
- **Harbor:** 0.25.0, installed from `uv.lock` with hash checking. Slots run one at a time, with `--n-attempts 1 --n-concurrent 1 --max-retries 0`.
- **Images:** before the first slot, the launcher pulls every task image by its manifest digest (`alexgshaw/<name>@sha256:…`) and tags it as the task's image (`alexgshaw/<name>:20251031`). Harbor runs the tag and pulls it only when it is missing, so it runs the pinned image. The tag's image id is checked before and after every slot. A pull failure refuses the run before any paid slot, and a moved tag stops the run and blocks totals. The dind container itself is `docker:29.9.3-dind@sha256:7bf70dcb…d61b`.
- **Prices:** `rustain_harbor/prices-2026-11-26.json`. Both the harness cost and rustain's `--max-cost` (through a rendered `[pricing]` entry) use this table.
- **Cost completeness:** a slot's cost is `known` only when its turn completed, without an error, and either finished normally or stopped at a settled call boundary (`max_cost`, `max_turns`). A Harbor timeout, an error, `max_duration` or `accounting_error` leaves the cost `incomplete`. The calls the stream shows are kept as `observed_cost_usd`, a lower bound and never the slot's cost.
- **Spend:** one 14 USD cap for the run plan, across every run id, kept in the spend ledger `<jobs-dir>/eval4-spend-eval-4-cmp-benign-v1.jsonl`. The ledger is append-only and `flock`-protected, and `clean-host.sh` never touches it. Only one launcher of the plan runs at a time in a jobs directory.
  - Every run id is registered when its header is written. A run id already in the ledger is refused, even after `clean-host.sh --results` removed its directory, so a cleaned run id cannot be reused.
  - Each slot identity (`<run-id>/<slot-key>`) may start once and finish once after its start. A repeated or orphaned identity, or an unknown event, is ledger corruption, and every launcher refuses until the owner repairs it. Charges are summed; none is overwritten.
  - A started slot without a known cost (unfinished, incomplete, unpriced or unknown) is charged its recorded `--max-cost` and blocks every launcher until accepted.
  - Before each slot, the launcher re-reads the ledger. It stops when the plan's spend reaches the cap, or when any slot is unresolved. Otherwise it passes `--max-cost min(cap − spent, 0.46)`.
  - One provider call can overshoot either bound.
  - Use one jobs directory for the whole authorization, because a different jobs directory has its own ledger.

## What the owner supplies

- A disposable Docker host with `bash`, `realpath`, and Docker able to run a privileged container. The host also needs Python 3.12 or later for the offline steps.
- Environment variable names: `DEEPSEEK_API_KEY` only. Use a restricted, rotatable key, because the agent's shell can read it.
- The binary from `build-musl.sh`. Its `.sha256` must equal `product.binary_sha256` in `eval4/run-plan.json`.
- A new run id (`[a-z0-9-]`, up to 40 characters) for every attempt at the matrix. A run id in the spend ledger is refused, including one whose directory `clean-host.sh` removed, and a new run id does not get a new budget.
- When a slot's cost is unknown, an explicit decision to go on. `--accept-unknown-spend <run-id>/<slot-key>[,...]` accepts the named slots. Each stays charged its recorded `--max-cost`: an assumption, not a bound. The acceptance is recorded once in the spend ledger and in the accepting run's header, and later runs keep it. Without it, every run id is refused.

## Command sequence

From the repository root at the landed commit:

```sh
# 0. Build the binary once, off the Docker host if needed.
#    The source is a clean checkout of v0.6.0.
evals/harbor/build-musl.sh <v0.6.0-checkout> 512509c4 <bin>/rustain-0.6.0-musl-512509c4

# 1. Preflight, offline. Validates the plan, manifest, price table, lock and binary hash,
#    reads the plan's spend ledger, and prints the run header and all 30 harbor argvs.
#    Writes nothing. Refuses when the plan's spend is unresolved or used up.
evals/harbor/run-baseline.sh check <bin>/rustain-0.6.0-musl-512509c4 <jobs-dir> <run-id>

# 2. Run, on the Docker host. A disposable docker:dind container gets only DEEPSEEK_API_KEY.
#    The key is checked against the provider before the first slot.
export DEEPSEEK_API_KEY=...
evals/harbor/run-isolated.sh <bin>/rustain-0.6.0-musl-512509c4 <jobs-dir> <run-id>
#    Only after inspecting a slot whose cost is unknown, and deciding to charge it its --max-cost:
evals/harbor/run-isolated.sh <bin>/rustain-0.6.0-musl-512509c4 <jobs-dir> <new-run-id> \
    --accept-unknown-spend <run-id>/<slot-key>

# 3. Aggregate, offline, as often as needed. If any slot is missing, duplicated,
#    unexpected or unidentifiable (malformed result, no package digest, another task
#    image), or if the header or a recorded argv was altered, aggregation refuses:
#    no totals, and any earlier results.json is removed. Its printed output starts and
#    ends with the labels every copy must carry: "task licences undeclared",
#    "sandbox off; container-isolated", and the binary's identity and hash.
evals/harbor/run-baseline.sh aggregate <jobs-dir> <run-id>
```

`run-baseline.sh run <binary> <jobs-dir> <run-id>` runs step 2 without the dind isolation. It needs Docker and uv on the machine.

## What a run leaves in `<jobs-dir>/<run-id>/`

| File | Content |
|---|---|
| `header.json`, `header.sha256` | Written once, after the spend check and before preflight. Holds the run id, product commit, binary hash and path, manifest hash with the comparator receipt and licence label, treatments, every slot in launch order with the hash of the key list, task images by tag and digest, Harbor version and lock hash, price table id, date and hash, limits, the spend cap with the plan's prior spend and any accepted unknown cost (`spend.state`), task timeouts, concurrency, and the argv template. |
| `ledger.jsonl` | One line per launcher event: `images_pinned` with each task image's id; `slot_started` with the exact argv it then executes, its `--max-cost` and the task image id before; `slot_finished` with Harbor's exit status, the cost status and the task image id after; and `stopped` with the guard's reason. |
| `jobs/<run-id>--cmp-benign-v1--<task>--<treatment>--a<n>/` | The Harbor job of one slot. |
| `results.json` | Written atomically, and only by a successful aggregation. Holds the labels, the per-slot outcome (`pass`, `fail`, `harbor_timeout`, `harbor_exception`, `limit_stop`, `verifier_absent`), whether a reward came with an exception, the cost status (`known`, `incomplete`, `usage_unknown`, `unpriced`), and a summary per treatment. |

A stopped run, or one with a missing slot, is not resumed. A replacement is a new run id with a full matrix, paid from what remains of the same 14 USD. If the stop left a slot's cost unknown, the replacement needs `--accept-unknown-spend`. `clean-host.sh --results` frees disk, not budget or run ids.

## Harbor 0.25.0 behavior the launcher relies on (read in the pinned wheel)

- `harbor run` exits 0 when the job completes, whatever the trials' outcomes. Exit 1 means a configuration, preflight or dataset error. The ledger keeps the exit status only as a diagnostic.
- Retries default to 0. A retry deletes the failed trial's directory before it runs again, so the launcher passes `--max-retries 0` explicitly.
- A job directory that already exists is resumed when its config is equal, and incomplete trial directories are deleted. With a different config it fails with `FileExistsError`. The launcher therefore never reuses a job name or directory.
- The timeout exceptions are `AgentTimeoutError`, `AgentSetupTimeoutError`, `VerifierTimeoutError` and `EnvironmentStartTimeoutError`. The verifier still runs after an `AgentTimeoutError` or a non-zero agent exit, so such a result can hold a reward next to an exception.
- `result.json` records the model as `config.agent.model_name` (the `--model` string) and as `agent_info.model_info` (`name`, `provider`). It records the task as `task_name` and `task_id.ref` (the package digest).
- A task with a `docker_image` runs it through `docker-compose-prebuilt.yaml` (`image: ${PREBUILT_IMAGE_NAME}`, no `pull_policy`), so `docker compose up` pulls it only when it is missing (Compose's default `missing` policy). After the trial, `down --rmi local` removes only untagged images, so the pinned tag stays. The launcher's before and after image checks catch it if this assumption ever fails.
