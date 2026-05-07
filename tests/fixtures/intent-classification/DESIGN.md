---
version: alpha
name: Studio
---

# Studio Design

Reproduces the canonical case that motivated ADR-007 § DOC-003: a
project-root markdown file with non-ctxgrd frontmatter (no `id`)
and no `[<NS>].paths` claim. Must produce zero diagnostics without
requiring a `[ignore]` workaround.
