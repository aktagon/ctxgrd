---
id: ADR-001
title: Adopt a numbering convention
status: accepted
date: 2026-08-04
---

# ADR-001: Adopt a numbering convention

## Status

Accepted.

## Open Questions

- Should the counter ever reset per directory?
- [x] Does a rejected ADR keep its number? Yes — numbers are never reused.
  - Follow-up detail, nested deliberately: elaboration on the item above,
    not a separate question, so it must not be flagged.

An example of the config this question is about:

```toml
[ADR."core.file-name"]
- this line is YAML/TOML noise, not a question
patterns = ["NNN-*.md"]
```

- - -

## References

None.
