# govctl — Governance Artifacts

smedja ships a lightweight governance harness (`/gov`) for tracking work items, RFCs, and ADRs as TOML files committed to the repo. No external issue tracker required.

---

## Artifact Kinds

| Kind | Directory | ID prefix | Default status |
|------|-----------|-----------|---------------|
| Work item | `gov/work-items/` | `WI-NNN` | `planned` |
| RFC | `gov/rfc/` | `RFC-NNN` | `draft` |
| ADR | `gov/adr/` | `ADR-NNN` | `draft` |

IDs auto-increment within each kind. `WI-001`, `WI-002`, … `RFC-001`, … `ADR-001`, …

---

## TOML Format

### Work Item

```toml
id       = "WI-001"
title    = "Add role cockpit panel"
status   = "planned"
priority = "P1"         # P1–P4; optional

[detail]
description = """
Multi-line description of the work.
"""
source   = "post-v0.16.3 review"
estimate = "M"          # XS | S | M | L | XL; optional

[acceptance]
criteria = [
  "Ctrl-A toggles the role cockpit panel",
  "panel shows mode, tier, runner",
]
```

### RFC

```toml
id     = "RFC-001"
title  = "Streaming delta buffer refactor"
status = "draft"

[detail]
description = """
Proposal text.
"""
author = "morgan"

[decision]
outcome   = ""
rationale = ""
```

### ADR

```toml
id     = "ADR-001"
title  = "Use VecDeque for kill ring"
status = "accepted"

[context]
situation = "Need bounded LIFO buffer for Ctrl-K/U kills."

[decision]
choice    = "VecDeque<String> capped at 16"
rationale = "O(1) push/pop at both ends; std library, no extra dep."

[consequences]
positive = ["No external dep", "Constant memory"]
negative = ["Max 16 entries — oldest killed on overflow"]
```

---

## Valid Status Values

| Status | Applicable kinds |
|--------|-----------------|
| `planned` | WI |
| `in_progress` | WI |
| `done` | WI |
| `cancelled` | WI |
| `draft` | RFC, ADR |
| `accepted` | RFC, ADR |
| `rejected` | RFC, ADR |
| `superseded` | RFC, ADR |

---

## TUI Commands

All `/gov` commands run against the workspace root (the directory `smedja-tui` was launched from).

```
/gov list
```
Lists all artifacts from all three directories with columns: `id`, `kind`, `status`, `title`.

```
/gov show WI-001
```
Prints the full TOML content of the named artifact.

```
/gov create work-item <title>
/gov create rfc <title>
/gov create adr <title>
```
Creates a new artifact with an auto-incremented ID and the correct default status. The file is written immediately to disk.

```
/gov transition WI-001 in_progress
/gov transition RFC-001 accepted
```
Updates the `status = "..."` line in the named artifact's TOML file. The file is updated in-place; the rest of the content is preserved.

---

## Scanning

The `/gov` handler scans `gov/work-items/`, `gov/rfc/`, and `gov/adr/` recursively for `*.toml` files. Files that don't have an `id` field are silently skipped. The scan is synchronous and re-runs on every `/gov` invocation — no caching.

---

## Workflow Example

```sh
# In the TUI, start a new feature:
/gov create work-item Implement kill ring for input bar

# Track progress:
/gov transition WI-007 in_progress

# Once done:
/gov transition WI-007 done

# View all open items:
/gov list

# Full detail on one item:
/gov show WI-007
```

The TOML files are plain text — edit them directly in your editor too. The `/gov` handler just reads and writes the `status` line; all other content is preserved on transition.

---

## File Layout

```
gov/
├── work-items/
│   ├── WI-001.toml
│   ├── WI-002.toml
│   └── ...
├── rfc/
│   ├── RFC-001.toml
│   └── ...
└── adr/
    ├── ADR-001.toml
    └── ...
```

Commit the `gov/` directory to version control. The artifacts travel with the repo and are visible to every reviewer.
