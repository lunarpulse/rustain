# Skills — `allowed-tools` reference

rustain loads Agent Skills (SKILL.md with YAML frontmatter) from
`.agents/skills/`, `.rustain/skills/`, `.claude/skills/` in the workspace and
`.agents/skills/` in your home directory. A skill may restrict the tools
available while it is active via the `allowed-tools` frontmatter field, using
the Agent Skills specification syntax.

This page documents the accepted forms of `allowed-tools`, what this build can
and cannot honour, and what the turn-time warning means.

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
