# Reviewer Workflow

> Conduct a structured technical review of technologies, architecture, code,
> or design proposals. Follow these steps in order. **All findings must include
> evidence with file paths and line ranges for auditability and navigation.**

---

## When to Activate

This skill is activated when the user requests a technical review: architecture
review, design review, code review, competitive analysis, or technology
evaluation. Common triggers:

- "Review this architecture..."
- "Do a competitive analysis of..."
- "Evaluate [technology] for our stack..."
- "Audit the design of..."
- "Write a review of..."

---

## Workflow Steps

### Step 0: Clarify Scope

Before any analysis, confirm with the user:

- **Subject** — What exactly is being reviewed? (architecture, code, technology,
  design doc, API, protocol, etc.)
- **Scope** — What's in scope? What's explicitly out of scope?
- **Audience** — Who is this review for? (core team, stakeholders, public)
- **Depth** — Quick assessment (~15 min, ~5 tool calls) or deep analysis
  (~1-2 hours, ~20+ tool calls)?
- **Standards/criteria** — Any specific rubrics or evaluation criteria to apply?

Restate scope in precise, technical English before proceeding.

---

### Step 1: Gather Artifacts

Collect all source material needed for the review:

1. **Read the subject** — If it's a file, read it in full. If it's a codebase,
   identify the key entry points.
2. **Read context** — Read related files: README, architecture docs, CLAUDE.md,
   AGENTS.md, ADRs, existing reviews in `docs/competitive/`.
3. **Identify stakeholders** — Who built this? Who maintains it? (Check
   CODEOWNERS, git log, or ask the user.)
4. **Identify constraints** — What are the hard requirements (time, budget,
   compliance, compatibility)? What are the soft preferences?

**Evidence tracking during artifact gathering:**
- Record the exact **file path** for each source file consulted.
- Note the **line number ranges** for sections that inform your analysis.
- Create a findings log linking evidence to specific lines (see Step 1a below).

#### Step 1a: Evidence Log Template

Maintain a structured log as you read:

```
## Evidence Log

| Finding Topic | File Path | Line Range | Quote/Description |
|--------------|-----------|-----------|-------------------|
| Architecture: Component boundaries | `src/core/domain.rs` | 45-78 | Trait definitions for ports |
| Code: Error handling | `src/adapters/http.rs` | 120-145 | Missing error variant coverage |
| Performance: Allocation | `src/infrastructure/db.rs` | 200-215 | Loop creating Vec without capacity |
| Design: State transitions | `docs/PROTOCOL.md` | 32-48 | State machine diagram + description |
```

This log becomes your audit trail and helps cross-reference findings to sources.

---

If reviewing against competitors (competitive analysis):

5. **Identify comparison subjects** — Find 3-6 comparable implementations or
   technologies.
6. **Read each competitor's relevant code/docs** — Focus on the specific
   dimension being reviewed (architecture patterns, provider support, MCP
   integration, subagent model, etc.). **Track file paths and relevant sections for each.**
7. **Build comparison matrices** — Extract structured data per competitor.

---

### Step 2: Analyze

Apply structured analysis across these dimensions (select as appropriate):

#### 2a. Architecture Review
- **Component boundaries** — Are concerns separated correctly? Is there
  leakage across layers?
- **Dependency direction** — Does the dependency rule hold (domain → nothing,
  adapters → domain only, infrastructure → domain only)?
- **Port/trait design** — Are ports the right abstraction? Too narrow? Too fat?
- **Extensibility** — How hard is it to add a new implementation of a port?
- **Testability** — Can each layer be tested in isolation?
- **Consistency** — Does the design follow the project's established patterns
  (hexagonal ports, CPA lifecycle, ownership topology)?

#### 2b. Code Review
- **Correctness** — Does the code do what it claims? Edge cases?
- **Safety** — Panic paths, unwrap usage, unsafe blocks, error handling,
  cancellation safety.
- **Async hygiene** — Lock discipline (tokio::sync vs std::sync), guard
  lifetimes across `.await` points, cancellation token propagation.
- **Performance** — Hot paths, allocation patterns, unnecessary cloning,
  O(n²) risks.
- **Style & conventions** — Does it match the project's established patterns?
- **Completeness** — Are there missing error variants, logging, tests?

#### 2c. Technology Evaluation
- **Maturity** — Production readiness, release stability, community activity,
  maintenance status.
- **Ecosystem fit** — Does it integrate with the existing stack? License
  compatibility? Dependency weight?
- **Alternatives** — What are the main alternatives? How do they compare?
- **Risk assessment** — Bus factor, vendor lock-in, future compatibility,
  security surface.

#### 2d. Design / Protocol Review
- **Completeness** — Does the design cover all required states and transitions?
- **Consistency** — Naming, error model, versioning strategy.
- **Interoperability** — Does it work with existing protocols/systems?
- **Evolution** — How does it version? Backward compatibility story?

---

### Step 3: Synthesize Findings

Organize your analysis into:

1. **Strengths** — What's working well?
2. **Weaknesses / Risks** — What's concerning or broken?
3. **Open Questions** — What needs clarification?
4. **Recommendations** — Specific, actionable changes.

**Evidence requirement:** Every finding must include:
- **Source location** — File path and line range (e.g., `src/core/domain.rs:45-78`)
- **Quote or reference** — Code snippet or specific doc section
- **Rationale** — Why this matters to the review criteria
- **Severity/Impact** — For risks and weaknesses (High/Medium/Low)

Use the following review template:

---

## Review Template

```markdown
# Review: [Subject]

**Date:** YYYY-MM-DD
**Reviewer:** [Name]
**Scope:** [Brief scope statement]
**Audience:** [Target audience]

---

## Executive Summary

2-3 paragraphs summarizing the key findings, overall assessment, and top
recommendations. Write this last.

---

## Subject Overview

What is being reviewed? Brief description of the technology, architecture, or
code under analysis.

---

## Findings

### ✅ Strengths

- **Finding 1 title**
  - **Source:** `file/path.rs:123-145`
  - **Evidence:** [Code snippet or description]
  - **Impact:** Why this matters

- **Finding 2 title**
  - **Source:** `docs/DESIGN.md:45-67`
  - **Evidence:** [Design section quote or reference]
  - **Impact:** Why this matters

### ⚠️ Weaknesses & Risks

- **Finding 1 title** — **Severity: High**
  - **Source:** `file/path.rs:200-220`
  - **Evidence:** [Code snippet showing the issue]
  - **Issue:** Specific problem identified
  - **Impact:** Why this matters (correctness, safety, performance, etc.)

- **Finding 2 title** — **Severity: Medium**
  - **Source:** `src/adapters/http.rs:85-110`, `src/infrastructure/db.rs:156-180`
  - **Evidence:** [Multiple sources; show both]
  - **Issue:** Pattern observed in multiple places
  - **Impact:** Consistency/maintainability concern

### ❓ Open Questions

- **Question 1** — Where is [specific concern]? Source location if relevant: `file.rs:XY-Z`
- **Question 2** — Why was [design choice]? Awaiting clarification from source: `docs/ADR-005.md:15-25`

---

## Recommendations

### Must Fix (blocking)

1. **Recommendation title**
   - **Rationale:** Link to 1-2 specific findings above with source references
   - **Source findings:** `src/core/domain.rs:45-78` (weak abstraction), `src/adapters/http.rs:200-215` (leakage)
   - **Implementation sketch:** [Steps to fix with code examples, if applicable]
   - **Effort estimate:** [If applicable]

### Should Fix (important)

1. **Recommendation title**
   - **Rationale:** Link to specific findings above
   - **Source findings:** `src/utils/cache.rs:112-130` (performance concern)
   - **Implementation approach:** [Brief approach]

### Nice to Have (optional)

1. **Recommendation title**
   - **Rationale:** Quality/maintainability improvement
   - **Source findings:** Scattered across codebase (see Detailed Analysis)
   - **Notes:** Consider after blocking items resolved

---

## Detailed Analysis

### Architecture Deep Dive

[For each major finding, include:]

**Title:** [Finding name]
- **Sources:** List all relevant file paths and line ranges
- **Analysis:** [In-depth explanation]
- **Code examples:**
  ```
  [Snippet from source with line numbers if possible]
  ```
- **Comparison:** [If applicable, compare to recommendations or best practices]

### Code Quality Assessment

[Similar structure for code findings:]

**Title:** [Specific code issue]
- **Location:** `file1.rs:100-115`, `file2.rs:45-60`
- **Current behavior:** [What the code does now]
- **Issue:** [What's wrong]
- **Fix suggestion:** [How to improve]

---

## Evidence Cross-Reference

This section serves as an audit trail. Map each source file to its findings:

| File Path | Line Range | Finding(s) | Category | Severity |
|-----------|-----------|-----------|----------|----------|
| `src/core/domain.rs` | 45-78 | Weak trait boundaries | Architecture | High |
| `src/adapters/http.rs` | 200-220 | Error handling gaps | Code Quality | High |
| `src/infrastructure/db.rs` | 156-180 | Allocation in loop | Performance | Medium |
| `docs/DESIGN.md` | 45-67 | State machine clarity | Design | Low |

---

## Appendix

- **Sources consulted:** [List all files reviewed, with focus areas]
  - `src/core/domain.rs` (lines 1-200)
  - `src/adapters/http.rs` (lines 1-250)
  - `docs/DESIGN.md` (complete)
  - `Cargo.toml` (dependencies review)

- **File listing:** [If a large codebase, list structure reviewed]

- **Raw data / comparison matrices:** [If applicable]

- **Evidence log:** [Link to or embed the evidence log from Step 1a]
```

---

### Step 4: Competitive Analysis (if applicable)

When reviewing against competitors, add these sections:

#### 4a. Comparison Matrix

Include **source locations** for each cell:

| Dimension | Subject | Competitor A | Competitor B | Competitor C |
|-----------|---------|--------------|--------------|--------------|
| Pattern X | `src/core.rs:45-67` | `competing-repo/core.rs:100-120` | `their-docs.md:30-45` | ... |
| Pattern Y | `src/adapters.rs:80-110` | `their-code:200-225` | N/A | ... |

#### 4b. Per-Competitor Findings

For each competitor, document:

- **What they do well** (adopt)
  - **Source:** `competitor/file.rs:XY-Z`
  - **Evidence:** [Code or doc snippet]

- **What they do poorly** (reject)
  - **Source:** `competitor/file.rs:ABC-DEF`
  - **Evidence:** [Code or doc snippet]

- **Key insight** — The one thing to learn from them
  - **Source:** `competitor/file.rs:range` or `docs:range`
  - **Application:** How to apply this learning

#### 4c. Adopt / Reject Table

| Source | Pattern | Verdict | Reason | Your location | Competitor location |
|--------|---------|---------|--------|---------------|----------------------|
| Competitor A | Pattern X | **Adopt** | [Rationale with evidence] | `src/new/file.rs:TBD` | `their-repo/file.rs:100-120` |
| Competitor B | Pattern Y | **Reject** | [Rationale with evidence] | N/A | `their-repo/file.rs:200-225` |

#### 4d. Consensus Matrix (if multiple reviewers)

| Topic | Participant 1 | Participant 2 | Participant 3 | Consensus | Sources |
|-------|--------------|--------------|--------------|-----------|---------|
| Decision A | Yes | Yes | No | **Strong majority** | `doc.md:45-67`, `code.rs:100-120` |
| Decision B | Yes | Yes | Yes | **Unanimous** | `design.md:80-95` |

---

### Step 5: Output

Write the review to:

1. **Inline in conversation** — Present the review directly to the user with
   a clear summary at the top and all source citations embedded.

2. **File (if requested)** — Save to a path the user specifies, or propose
   one following the project's conventions:
   - `docs/reviews/<subject>-review.md` — General reviews
   - `docs/competitive/<subject>-competitive-review.md` — Competitive analyses
   - `docs/<area>/<subject>-review.md` — Area-specific reviews

When saving to a file, output a summary inline and reference the file path.

**Validation checklist before output:**
- [ ] Every finding includes file path(s) and line range(s)
- [ ] Every claim is backed by a source reference
- [ ] Evidence log cross-references all sources
- [ ] File paths are absolute or relative to project root consistently
- [ ] Line ranges are accurate and minimal (don't include entire files)
- [ ] Severity levels are assigned to all weaknesses/risks
- [ ] Recommendations link back to specific findings with source citations

---

## Tone & Style

- **Precise and technical** — Use exact terminology. Avoid vague praise or
  criticism. Every claim needs evidence with a source reference.
- **Constructive** — Frame criticism as actionable recommendations. "This is
  wrong" → "Consider X instead because Y (see `file.rs:123-145`)."
- **Evidence-based** — Quote source code, reference line numbers, cite docs.
  **Always include the file path and line range.**
- **Auditable** — A reviewer should be able to navigate directly to sources
  using the file paths and line ranges you provide.
- **Structured** — Use tables, lists, headings. Make the review scannable.
- **Balanced** — Lead with strengths before weaknesses. A review that is only
  negative loses credibility.

---

## Example Evidence Citations

Here are examples of how to cite evidence in your findings:

**Good (specific):**
```
- **Finding:** Missing error variant handling
  - **Source:** `src/adapters/http.rs:120-145`
  - **Evidence:** The `handle_response()` function matches on `Result<T>` but 
    lacks an `Err(_)` branch for timeout errors defined in `src/core/errors.rs:45-67`
```

**Poor (vague):**
```
- **Finding:** Error handling is incomplete
  - **Evidence:** There are missing error cases somewhere in the adapter layer
```

**Good (multi-source):**
```
- **Finding:** Inconsistent error model across layers
  - **Source:** `src/core/errors.rs:1-50` (domain errors), 
    `src/adapters/http.rs:85-110` (adapter errors), 
    `src/infrastructure/db.rs:156-180` (infra errors)
  - **Issue:** Domain uses `Result<T>`, adapters use custom enums, infra uses exceptions
```

---

## Example

For an example of the expected output format and depth, see the existing
reviews in this repository:

- `docs/competitive/epic-7-9-competitive-architecture-review.md`
- `docs/competitive/epic-10-14-competitive-architecture-review.md`
- `docs/competitive/sub-agent-architecture-comparison.md`
- `docs/ARCHITECTURE_REVIEW_PLAN_MODE_ORCHESTRATION.md`

---

## References

- **Project architecture docs:** `docs/`, `CLAUDE.md`, `AGENTS.md`
- **ADR documents:** (if applicable)
- **Existing review conventions:** See example files listed above
