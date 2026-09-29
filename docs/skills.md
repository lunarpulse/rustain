# Skills — `allowed-tools` reference

rustain loads Agent Skills (SKILL.md with YAML frontmatter) from
`.agents/skills/`, `.rustain/skills/`, `.claude/skills/` in the workspace and
`.agents/skills/` in your home directory. A skill may restrict the tools
available while it is active via the `allowed-tools` frontmatter field, using
the Agent Skills specification syntax.

This page documents the accepted forms of `allowed-tools`, what this build can
and cannot honour, what the turn-time warning means, and the **trust prompt**
the first activation of a workspace skill always raises.

## The four accepted forms

```yaml
# 1. Empty — restricts to nothing (only the carve-outs below remain)
allowed-tools: []

# 2. Inline bracket list (optionally quoted)
allowed-tools: [Read, Grep]
allowed-tools: ["Read", "Grep"]

# 3. Block list
allowed-tools:
  - Read
  - Grep
  - Glob

# 4. Scalar — space-separated, the Agent Skills standard form
allowed-tools: Bash(kubectl:*) Bash(helm:*) Read
```

Scalar-form notes:

- Items are separated by whitespace; an item's parentheses stay intact, so
  `Bash(kubectl:*)` is one item, not three. ⚠ That holds only for items
  **without spaces inside the parentheses**: a space-bearing specifier such
  as `Bash(git log *)` is **shredded** by the scalar form (`Bash(git` +
  `log:*)`) — declare those with the block list or bracket form instead,
  which is their only reachable door.
- Commas are **not** separators in a scalar (`Read,Grep` is a single item).
- A surrounding quoted scalar (`allowed-tools: "Read Grep"`) is unquoted first.
- A ` #` (space-hash) starts a YAML comment and is stripped:
  `allowed-tools: Read Grep  # only these two` → `Read Grep`.

Agents (`.claude/agents/*.md`) use the same parser for their `allowed-tools`
and `exclude-tools` fields; every form above applies to both. An agent's
`exclude-tools` removes tools in addition to any allowlist restriction.

## Bash prefix patterns

A **skill** item such as `Bash(kubectl:*)` admits the bare `Bash` tool to the
offered catalogue and gates each execution by command text. The current matcher:

- recognizes `Tool(prefix*)` (one final `*`, a non-empty prefix, no trailing
  characters after `)`), `Tool(*)` (every command), `Tool(prefix:*)` (the
  documented `:*` form, honoured only at the end — `Bash(git:* push)` keeps
  its colon as a literal and matches only the exact command `git:* push`),
  and star-free specifiers as exact literal commands. A junk specifier
  (`Bash()`, `Bash(   )`, `Bash(**)`, `Bash( *)`) is **not** a grant: the
  item stays unmatched and is disclosed below;
- is case-sensitive and normalizes whitespace in both the declared prefix and
  command before comparing;
- splits `;`, `&&`, `||`, `|`, `|&`, `&`, and newlines — but **only outside
  single and double quotes** — so `Bash(kubectl:*)` admits
  `kubectl get -o jsonpath='{.a && .b}'` as one command;
- treats `&` adjacent to `>` or `<` (`2>&1`, `>&`, `<&`, `&>`) as a redirect
  operator, never a separator — `kubectl get pods 2>&1` is one command;
- honours backslash escapes while scanning separators; and
- requires **every non-empty command segment** to match one declared prefix.

**Denied on purpose, before anything runs** (fail-closed — these can never be
expressed under a prefix pattern):

- command substitution and subshells: `$(…)`, `` `…` ``, `<(`, `>)` — even
  inside quotes, conservatively. ⚠ With no bare-`Bash` escape hatch, any
  runbook needing a **dynamic value** inside a helm/kubectl call
  (`helm upgrade --set tag=$(git rev-parse --short HEAD)`) is categorically
  unable to express it. That is a real product limit, not an inconvenience;
- heredocs (`<<`, `<<-`, `<<'W'`): every body line would otherwise be a
  non-matching segment;
- unbalanced quotes, and a dangling `&&`/`||` with nothing after it;
- a leading `NAME=value` assignment (`KUBECONFIG=/tmp/x kubectl get pods`):
  skipping it would admit `KUBECONFIG=/evil kubectl …`, where the assignment
  controls what the command *does*;
- quoted or decorated program names (`"kubectl" get`, `\kubectl get`,
  `/usr/bin/kubectl get`, `time kubectl get`) — the deliberate cost of
  literal-prefix matching without a shell parser.

Matching is intentionally textual, not a shell policy engine. `Bash(ls*)`
matches `lsof`; `/bin/kubectl`, `bash -c 'kubectl …'`, flag reordering, and
variable indirection can evade argument-shaped intent. Redirect targets are
not workspace-checked as file operations: for example,
`kubectl config view --raw > /tmp/x` still reaches the shell — including in
auto-approval modes, where no human sees the card. Use a dedicated tool or
stronger sandbox when those boundaries matter.

This expansion is **`Bash`-only**: `Bash` is the one tool whose commands
carry an execution-time gate. A pattern on any other tool (`Read(docs/*)`,
`Write(/tmp/*)`, `mcp__db__query(SELECT:*)`) has no command gate, so it is
**not** widened to the bare tool — the item stays unoffered and is disclosed
as unmatched below. An agent's `Bash(kubectl:*)` is disclosed the same way:
agent `allowed-tools` accepts the same syntax but has no command-level
execution backstop, so an agent pattern stays unoffered rather than widened
unsafely (`DF-19-11-AGENT-PATTERN-NO-EXEC-GATE`). A bare `Bash` item still
grants all Bash commands, subject to the normal blocklist and approval mode.

⚠ **Two active skills intersect their declarations as raw strings.** A skill
declaring `Bash(kubectl:*)` co-active with one declaring `Bash(helm:*)`
intersects to the empty set and fires the turn-fatal disjoint warning below —
even though expanding each side first would share `Bash`. This is fail-closed
and pre-existing; declare a common item (e.g. `Read`) in both if they must be
co-active.

## The two carve-outs

Two tools survive every allowlist at **offer** time:

- **`activate_skill`** — so the model can still activate and chain skills
  (also carved out at execution time).
- **`task`** — so an active agent that omits `task` can still delegate to
  subagents (ADR-10-5 S3).

At **execution** time the permission chain carves out only `activate_skill`:
under an active skill's allowlist, a `task` call is denied even though the tool
was offered. The result is fail-closed — a restricted skill cannot escape its
restriction through delegation — and both behaviours are pinned by tests.

## What the unmatched-items warning means

On the turn it bites, if a declared `allowed-tools` item matches no tool in
that turn's catalogue (a typo, an unavailable MCP tool, a malformed pattern,
or an agent-side pattern that cannot be execution-gated), rustain emits **one
warning** naming the unmatched items. A valid skill-side Bash prefix pattern is
matched and therefore is not named:

> Tool restriction from skills 'safe-deploy, canary-watch' cannot be honoured
> in full: [Glob] is unavailable for this turn.

This means exactly what it says:

- both skills **loaded and activated fine** — neither failed validation;
- the honourable intersection **is being enforced** (here, `Read`);
- the named item is unavailable **for this turn** — an MCP tool may match
  again once its server is back; and
- matched Bash patterns remain command-gated at execution.

If the active agent and skill tool filters share no tool at all, a separate,
louder warning fires instead: *"Active agent and skill tool filters are
disjoint — no tools available for this turn."*

## Before any of that: the trust prompt

The first time a **workspace** skill is activated, rustain stops and asks
before it loads anything:

```text
┃ New project skill detected: "safe-deploy"                ┃
┃ Trust and enable this skill for this session?            ┃
┃ [y] Yes  [n] No  [i] Inspect                             ┃
```

- **`y`** — trust it and activate. The skill's instructions go into the system
  prompt and its `allowed-tools` starts filtering from the **next** message you
  send (see the note below).
- **`n`** (or `Esc`) — decline. The model is told
  `Skill '<name>' not trusted — activation declined.` and carries on without
  it; nothing about the skill reaches the prompt.
- **`i`** — inspect. The SKILL.md file is shown verbatim, up to 20 lines, so
  you can read the `allowed-tools` line and the body before you answer. `Esc`
  returns to the prompt; it does **not** decline for you.

Skills under `~/.agents/skills/` are treated as yours and are **not** prompted.
Only the three workspace directories are — a skill that arrived with a `git
clone` is a file someone else wrote that is about to steer your agent.

### ⚠ The prompt says "session"; the decision is per **conversation**

The answer is remembered in memory, keyed on the **conversation**, for as long
as the process lives. In practice that means:

- open a second tab, or start a new conversation, and you are asked again about
  the same skill — even though you already said yes in this session;
- restart rustain and every answer is gone;
- nothing is written to disk, so a "yes" can never be inherited by a later run
  or by another workspace.

The wording is wrong in the safe direction — you are asked too often, never too
rarely — but it is wrong. Tracked as `DF-19-10-TRUST-PROMPT-SAYS-SESSION`.

### Execution restriction is immediate; catalogue composition is per message

When a model activates a skill during a turn, rustain refreshes the live
activation set before scheduling the next tool call. A tool excluded by the
new skill is denied even if the provider emitted it beside `activate_skill` in
the same response. The provider-facing catalogue and `<skill>` prompt are
composed when the message is submitted, so their filtered form appears on the
next message; that presentation boundary does not weaken execution policy.

### ⚠ You may not see the unmatched-items warning

That warning is an **advisory** notification, and in the default `Focus`
density mode advisories are queued rather than displayed. Switch density
(`Ctrl+X` then `w` for Monitor) to drain the queue and read them. Tracked as
`DF-19-10-ADVISORY-QUEUED-IN-FOCUS`.
