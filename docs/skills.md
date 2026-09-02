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
  `Bash(kubectl:*)` is one item, not three.
- Commas are **not** separators in a scalar (`Read,Grep` is a single item).
- A surrounding quoted scalar (`allowed-tools: "Read Grep"`) is unquoted first.
- A ` #` (space-hash) starts a YAML comment and is stripped:
  `allowed-tools: Read Grep  # only these two` → `Read Grep`.

Agents (`.claude/agents/*.md`) use the same parser for their `allowed-tools`
and `exclude-tools` fields; every form above applies to both. An agent's
`exclude-tools` removes tools in addition to any allowlist restriction.

## The pattern limitation (fail-closed)

This build does **not** expand patterns. An item like `Bash(kubectl:*)`:

- **is parsed and enforced** — it does not widen to `Bash`, so the `Bash` tool
  itself stays filtered out while the skill is active, and
- **matches no tool** — until pattern matching lands, the patterned tool is
  simply unavailable for the turn.

A restriction is never silently dropped and never silently widened. If you
need the patterned tool now, declare the bare tool name explicitly
(`allowed-tools: Bash Read`) — with the understanding that this grants the
whole tool, not just the patterned command.

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
that turn's catalogue (a pattern like `Bash(helm:*)`, a typo, or an MCP tool
whose server is down this turn), rustain emits **one warning** naming the
unmatched items:

> Tool restriction from skill 'safe-deploy' cannot be honoured in full:
> [Bash(helm:*), Bash(kubectl:*)] are unavailable for this turn.

This means exactly what it says:

- the skill **loaded and activated fine** — it did not fail validation;
- the honourable part of its restriction **is being enforced** (honoured items
  stay available, everything else stays filtered out);
- the named items are unavailable **for this turn** — an MCP tool may match
  again once its server is back.

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
