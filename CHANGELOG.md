# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] — 2026-05-07

> Headline: **first-touch is silent.** Drop `ctxgrd` into any repo (Hugo,
> Jekyll, design tokens, prompt files) and it will not false-fire on
> non-document markdown. A `.md` file is treated as a document only when
> it claims intent — either an `id: <NS>-<N>` frontmatter field or a
> `[<NS>].paths` glob match.

### Added

- **Intent-based document classification (ADR-007 § DOC-001).** A `.md`
  file becomes a document candidate iff it has an `id` matching
  `<NAMESPACE>-<number>` for a configured namespace OR its location
  matches one of a configured namespace's `[<NS>].paths` globs. Files
  satisfying neither are silently skipped — no `core.frontmatter`, no
  `core.id`, no diagnostic.
- **`[<NS>].paths` configuration** — list of gitignore-style globs
  declaring which files belong to a namespace by location. Set
  semantics; order-independent.
- **`ctxgrd init` paths pre-fill** — `init` sniffs conventional
  ADR/PRD directories (`docs/adrs/`, `adrs/`, …) and pre-fills
  `[<NS>].paths` for any matches found. Announces detected paths on
  stderr above the body-header advisory.
- **`cfg.path-conflict` `KernelMessage` (DOC-007).** When two
  namespaces' `[<NS>].paths` globs claim the same file, ctxgrd
  resolves via id-claim if available; otherwise emits a configuration
  error at ingest time and excludes the file from rule execution.
- **Migration recipe stub** at
  `docs/migration/body-headers-to-frontmatter.md`. Turns the EXT-003
  init advisory link from a 404 into a useful pointer; the full
  worked example tracks as ADR-006 § EXT-004.

### Changed

- **`core.id` (IdMissing) and `core.frontmatter` only fire for
  path-claimed files.** Help text drops the
  `add `<tip>`to`[ignore].patterns`` escape clause — under
  intent-based classification, these diagnostics only fire for files
  that ARE ctxgrd documents by intent.
- **`DEFAULT_IGNORE_PATTERNS`** no longer includes `**/CHANGELOG.md`
  or `**/README.md`. DOC-001 makes those workarounds obsolete; their
  presence after DOC-001 ships would have been internally
  contradictory.
- **`DEFAULT_IGNORE_PATTERNS`** now includes `**/log/**`,
  `**/logs/**`, `**/tmp/**` as walker-cost optimizations (same
  category as `target/**` and `**/dist/**`).
- **README** restructured to lead with "first-touch is silent." The
  intent-claim mechanism (id-claim or path-claim) is now the first
  user-facing concept readers encounter; rule lists come after.
- **`docs/namespaces.md`** rewritten to lead with `[<NS>].paths`.
  Documents the resolved decisions on negation (rejected — single-
  responsibility split with `[ignore]`), absolute paths (rejected),
  source-emitted documents (`[<NS>].paths` does not apply), and
  `[ignore]` precedence (`[ignore]` always wins).

### Decisions captured (ADR-007)

- OQ-1 resolved: no negation in `[<NS>].paths`.
- OQ-3 resolved: absolute paths rejected.
- OQ-4 resolved: source-emitted documents excluded from
  path-classification.
- OQ-5 resolved: `[ignore]` wins over path-claim.
- OQ-7 resolved: `init` announces pre-filled paths on stderr.
- OQ-8 resolved: `cfg.path-conflict` uses the `KernelMessage`
  channel.

OQ-2 (LSP behavior on file rename) and OQ-6 (CLI override for
`[<NS>].paths`) remain as explicitly-deferred future work.

### Verification

- 307 tests across 5 suites: 295 lib, 4 init_body_headers, 4
  fixture_smoke, 2 intent_classification (Hugo README + DESIGN.md
  silence), 2 path_conflict (cross-namespace overlap →
  `cfg.path-conflict`).
- `cargo clippy --lib --no-deps -- -D warnings` clean.
- `make check` clean (6 documents · 9 rules · 0 diagnostics).

[Unreleased]: https://github.com/aktagon/ctxgrd/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/aktagon/ctxgrd/releases/tag/v0.4.0
