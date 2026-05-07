---
title: Ambiguous
---

# Ambiguous

This file lives under `docs/`, which both `[ADR].paths` and
`[PRD].paths` claim. It has no `id` to resolve the conflict, so
ctxgrd must emit a `cfg.path-conflict` `KernelMessage` and exclude
this file from rule execution for both namespaces.
