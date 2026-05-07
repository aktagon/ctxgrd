# Migrating body-header ADRs to frontmatter

> **Status:** Recipe stub. The full worked example is tracked as
> ADR-006 § EXT-004 and will land once the maintainer has migrated
> a real external repo end-to-end. This page exists today so the
> `ctxgrd init` advisory has a destination — not because the recipe
> is finished.

## Why migrate

ctxgrd treats YAML frontmatter as the canonical extraction surface
(ADR-006 § EXT-001). Older ADR conventions stash status, date, and
supersession state in the **body** instead — typically as a leading
table or `**Status:**`-style bullet list. Those forms aren't
extracted by ctxgrd, so the linter can't apply rules like
`core.allowed-values`, `core.dep-resolved`, or supersession checks
to them.

If you ran `ctxgrd init` and saw an advisory like:

```
[advisory] docs/adrs/ contains 7 .md files without YAML frontmatter.
  • ctxgrd extracts metadata from frontmatter only (ADR-006 § EXT-001).
  • To migrate, see docs/migration/body-headers-to-frontmatter.md in the ctxgrd repo.
```

— this is the page you were sent to.

## Approach in one sentence

Extract `id`, `title`, `status`, and `date` from the body header into
a YAML fence at the top of each file; leave the body markdown
otherwise untouched.

## Before / after (placeholder)

```markdown
# ADR-007: Use event sourcing for the audit trail

| Status   | Accepted   |
| -------- | ---------- |
| Date     | 2024-03-12 |
| Deciders | …          |

## Context

...
```

becomes

```markdown
---
id: ADR-007
title: Use event sourcing for the audit trail
status: accepted
date: 2024-03-12
---

# ADR-007: Use event sourcing for the audit trail

## Context

...
```

## Where the full recipe will live

ADR-006 § EXT-004 commits to a worked example derived from a real
external-repo migration. Until that lands, treat the before/after
above as illustrative — it shows the shape of the transform, not
every edge case.

Known cases the eventual recipe will need to address:

- Status capitalization variants (`Accepted` vs `accepted`).
- Multiple date formats in the same repo (`2024-03-12`, `Mar 12,
2024`, `12/03/2024`).
- Multi-line `**Supersedes:**` prose that doesn't reduce to a clean
  `depends_on:` list.
- Free-form deciders / context tables that don't map to single
  frontmatter keys.

If you migrate a repo before this recipe is fleshed out, the ctxgrd
maintainers would welcome the before/after pair and the transform
sequence you used — open an issue or PR against this file.

## Related

- ADR-006 § EXT-001 — frontmatter is the canonical extraction surface
- ADR-006 § EXT-003 — the `ctxgrd init` advisory that points here
- ADR-006 § EXT-004 — the requirement this page partially satisfies
