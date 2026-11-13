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
- a **comment** (`#` at the start of a word) and constructs whose quoting the
  scanner does not mirror exactly: ANSI-C quoting `$'…'`, arithmetic `$[…]` /
  `((…))`, and `${…}` inside double quotes. Each can change what Bash treats as
  quoted and so hide a later redirect; `echo ok # it's` is refused, while
  `echo 'a # b'`, `echo a#b` and `$"…"` are scanned exactly. A backslash-newline
  is deleted by Bash before it recognises these openers, so a *continued*
  opener (`$\⏎'`, `$\⏎(`, `(\⏎(`, `<\⏎(`, a double-quoted `$\⏎{`) is refused
  the same way. Every construct is either modelled exactly or refused;
- a leading `NAME=value` assignment (`KUBECONFIG=/tmp/x kubectl get pods`):
  skipping it would admit `KUBECONFIG=/evil kubectl …`, where the assignment
  controls what the command *does*;
- quoted or decorated program names (`"kubectl" get`, `\kubectl get`,
  `/usr/bin/kubectl get`, `time kubectl get`) — the deliberate cost of
  literal-prefix matching without a shell parser.

Matching is intentionally textual, not a shell policy engine. `Bash(ls*)`
matches `lsof`; `/bin/kubectl`, `bash -c 'kubectl …'`, flag reordering, and
variable indirection can evade argument-shaped intent.

### Redirects under a command pattern

A prefix pattern limits the command, not the files the shell opens for it. So
where a restriction admits `Bash` **only through patterns** (an agent's or a
skill's `Bash(kubectl:*)`), a shell redirect inside an admitted command is held
to the same workspace boundary as a file tool — in **every** permission mode,
including auto-approval, and before any approval card:

- `>`, `>>`, `>|`, `&>`, `&>>`, `<>` and `>&word` write; `<` and `<&word` read.
  The target is workspace-checked exactly like a `Write`/`Read` file path
  (relative paths are the workspace, `..` and paths that resolve out of the
  workspace are refused, `/etc/`, `/usr/`, `/dev/`… are blocked). ⚠ **Known
  limitation, shared with the file tools:** a *dangling* symlink inside the
  workspace whose target does not exist yet (`link -> /outside/new-file`) is
  not recognised by the workspace check, so a redirect to `link` can create the
  file outside the workspace. Existing symlinks that resolve outside are
  refused; the dangling-leaf case is tracked as
  `DF-SECURITY-ADAPTER-DANGLING-LEAF-SYMLINK`.
- `2>&1`, `>&2`, `2>&-` and similar descriptor duplications have no file and
  are admitted. `/dev/null` (the exact string) is admitted, so `2>/dev/null`
  works; `/dev/shm/x` and every other `/dev/` path are not.
- A target that needs shell expansion is **refused, never guessed**: `$`
  (parameter or arithmetic expansion, `$'…'`), `~`, `*`, `?`, `[`, `]`, `{`,
  `}`, and an extended-glob opener `(` right after the target (`@(`, `+(`, `!(`)
  — and a backslash inside double quotes. `kubectl version > ~/.bashrc`
  and `>> $HOME/.bashrc` are refused for this reason. Quote removal follows
  bash (`> "/abs"`, `> \/abs`, `> ''/abs` are all `/abs`), so a quoted spelling
  cannot hide an absolute path.
- A **relative** target in a command that contains an unquoted `cd`, `pushd` or
  `popd` word is refused (the shell's directory may no longer be the
  workspace); an absolute in-workspace target is still admitted.
- A redirect with no target (`kubectl version >`) is refused.

The refusal is a tool error the model and the operator both see:

> Tool 'Bash' redirect refused by skill 'safe-deploy': '> /tmp/x' writes to a
> path the workspace check refuses (Workspace violation: …). Redirect to a
> literal path inside the workspace, or drop the redirect. To run it
> unrestricted, the operator can deactivate skill 'safe-deploy'.

The origin reads `agent 'x'`, `skill 'a'` / `skills 'a, b'`, or `agent 'x' and
skill 'a'`; an agent-only refusal ends "…the operator can run it without agent
'x'." Deactivate a skill with `/deactivate`.

Not changed, and still the pattern matcher's honest limits: bare `Bash` and
`Bash(*)` impose no redirect check (unrestricted Bash can already write
anywhere with `tee` or `cp`); what the admitted program does with its own
arguments (`kubectl cp pod:/x /etc/y`, `helm --output-dir`) is outside the
matcher; a symlink an earlier admitted segment of the same command creates is
not seen (the check is pre-execution); a redirect into the workspace is as
allowed as a `Write` there. Here-docs and here-strings stay refused. Use a
dedicated tool or a stronger sandbox when those boundaries matter.

This expansion is **`Bash`-only**: `Bash` is the one tool whose commands
carry an execution-time gate. A pattern on any other tool (`Read(docs/*)`,
`Write(/tmp/*)`, `mcp__db__query(SELECT:*)`) has no command gate, so it is
**not** widened to the bare tool — the item stays unoffered and is disclosed
as unmatched below. An agent's `Bash(kubectl:*)` follows the same rules as a
skill's: since Story 19.28 the one matcher is also the execution gate for an
agent's own restriction and for delegated children, so an agent pattern is
offered and command-enforced. A bare `Bash` item still grants all Bash
commands, subject to the normal blocklist and approval mode.

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
under an active skill's allowlist, a `task` call is denied unless the skill
declares `task` itself. A skill that does declare `task` may delegate, and the
child carries that skill's restriction as well as its own agent restriction
(Story 19.29): the child is offered only tools the
skill's items name, a command outside the skill's patterns is denied with
`(inherited from the delegating turn)` in the text, and a refused redirect is
denied by the same inherited restriction. The two restrictions are separate
gates — a command must satisfy both. A plan a restricted turn proposes
(`propose_plan`) carries that call's restrictions, so its children are
narrowed the same way whether the plan is auto-approved (YOLO) or approved
through the cards; approving a plan never widens a declared restriction. Plans
saved before this change carry no restriction and keep their old behaviour if
resumed. Operator-started `/fanout` waves are not model-originated and are
unchanged.

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
┃ Trust and enable this skill for this conversation?       ┃
┃ [y] Yes  [n] No  [i] Inspect                             ┃
```

- **`y`** — trust it and activate. The skill's instructions go into the system
  prompt and its `allowed-tools` starts filtering from the **next** message you
  send (see the note below).
- **`n`** — decline. The model is told
  `Skill '<name>' not trusted — activation declined.` and carries on without
  it; nothing about the skill reaches the prompt. `Esc` defers without deciding;
  use `Ctrl+X q` to reopen the decision from the Needs-you list.
- **`i`** — inspect. The SKILL.md file is shown verbatim, up to 20 lines, so
  you can read the `allowed-tools` line and the body before you answer. `Esc`
  returns to the decision; it does **not** decline for you.

Skills under `~/.agents/skills/` are treated as yours and are **not** prompted.
Only the three workspace directories are — a skill that arrived with a `git
clone` is a file someone else wrote that is about to steer your agent.

### The decision is per **conversation**

The prompt says so, and so does the behaviour: the answer is remembered in
memory, keyed on the **conversation**, for as long as the process lives. In
practice that means:

- open a second tab, or start a new conversation, and you are asked again about
  the same skill — even though you already said yes in another conversation;
- restart rustain and every answer is gone;
- nothing is written to disk, so a "yes" can never be inherited by a later run
  or by another workspace.

You are asked once per skill per conversation.

### Execution restriction is immediate; catalogue composition is per message

When a model activates a skill during a turn, rustain refreshes the live
activation set before scheduling the next tool call. A tool excluded by the
new skill is denied even if the provider emitted it beside `activate_skill` in
the same response. The provider-facing catalogue and `<skill>` prompt are
composed when the message is submitted, so their filtered form appears on the
next message; that presentation boundary does not weaken execution policy.

### Where you see the unmatched-items warning

The warning is a notice like any other: it appears as a toast, and it stays
reviewable after the toast expires in the Notification Log — `Ctrl+X` then `n`,
or `/notifications`.
