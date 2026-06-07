# contextguard examples

A self-contained fixture for ctxgrd. Four document namespaces, two external
rules, one source stub, one intentionally broken document. Every feature of
the tool is exercised by at least one file.

Also serves as the integration test fixture for `tests/fixture_smoke.rs`.

## Layout

```
examples/
├── ctxgrd.toml
├── adrs/
│   ├── 001-use-event-sourcing-for-audit.md        valid ADR, root of the dep graph
│   └── 099-broken-demo.md                         intentionally broken; fires 5 diagnostics
├── prds/
│   └── 001-audit-trail-requirements.md
├── pmrs/
│   └── 001-audit-log-dropped-events-2026-03.md
├── rules/
│   └── adr/
│       ├── consequences-non-empty/               code: adr.consequences-non-empty
│       │   ├── run
│       │   └── README.md
│       └── supersession-consistent/              code: adr.supersession-consistent
│           ├── run
│           └── README.md
└── sources/
    └── jira-stub/                                stub source; emits two JIRA documents
        ├── run
        └── README.md
```

## Dependency graph

```
PRD-001 ──► ADR-001 ──► PMR-001

JIRA-100 ──► PRD-001     (from the jira-stub source)
JIRA-101                 (standalone, from jira-stub)

ADR-099 ──► PRD-999      (unresolved — intentional)
```

## Run it

```sh
ctxgrd --root examples
```

Expected exit code: `1`, with five diagnostics from `099-broken-demo.md`
(see `expected-output.txt`). Delete `adrs/099-broken-demo.md` and the run
is clean (exit `0`).

## What each piece demonstrates

| File                                       | Feature                                                        |
| ------------------------------------------ | -------------------------------------------------------------- |
| `ctxgrd.toml`                              | Namespace rule assignment + parameter sub-tables               |
| `adrs/001-use-event-sourcing-for-audit.md` | Valid file-based document; cross-namespace `depends_on`        |
| `prds/001-audit-trail-requirements.md`     | Different `required-headings` list from ADR                    |
| `pmrs/001-audit-log-dropped-events-…`      | Domain-specific required metadata key (`incident_date`)        |
| `adrs/099-broken-demo.md`                  | All five diagnostic types at once; code-span cross-ref opt-out |
| `rules/adr/consequences-non-empty/run`     | External rule operating on document body (file-based check)    |
| `rules/adr/supersession-consistent/run`    | External rule reading inline `context.metadata` from stdin     |
| `sources/jira-stub/run`                    | External source emitting JSONL document envelopes              |
| `[JIRA]` in `ctxgrd.toml`                  | Unified metadata: same core rules validate JIRA `extra` fields |
