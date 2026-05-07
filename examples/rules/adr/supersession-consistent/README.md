# adr.supersession-consistent

If an ADR's `status` is `superseded`, a `superseded_by` key must be present
in the metadata and must be a well-formed `<NAMESPACE>-<number>` ID.

## The supersession idiom

The kernel has no built-in supersession concept — it is expressed with
primitives that already exist:

- The **new** ADR lists the old one in its `depends_on`. `core.dep-resolved`
  and `core.dep-cycle` handle existence and cycles.
- The **old** ADR gets `status: superseded` and a new key
  `superseded_by: <NEW-ID>` pointing at the replacement.

This rule enforces the second half of that idiom.

## What the rule does NOT check

- It does not check that the `superseded_by` target exists — that is
  `core.dep-resolved`'s job on the new ADR's `depends_on` entry.
- It does not warn when a document references a superseded ADR in its body.

## Activation

```toml
[ADR]
rules = [..., "adr.supersession-consistent"]
```

## Example

Old ADR (superseded):

```yaml
id: ADR-001
title: Use event sourcing for the audit trail
status: superseded
superseded_by: ADR-017
```

New ADR (superseding):

```yaml
id: ADR-017
title: Move audit trail to per-tenant partitioned streams
status: accepted
depends_on:
  - ADR-001
```

With this setup:

- `core.allowed-values` accepts `status: superseded` (listed in the allow-list).
- `core.dep-resolved` confirms ADR-017 depends on a real ADR-001.
- `core.dep-cycle` rejects any accidental cycle.
- `adr.supersession-consistent` confirms ADR-001 declares `superseded_by: ADR-017`.
