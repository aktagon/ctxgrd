# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0] — 2026-05-14

### Added

- **LSP server implementation (ADR-008).** Provides real-time diagnostics and
  navigation capabilities. Integrated via the `ctxgrd lsp` subcommand.
- **Neovim plugin.** A minimal Lua integration bridging the Neovim LSP
  client to the server.
- **Claude Code plugin.** An official integration extending the agent with
  `lint`, `new`, and `refs` skills.
- **`llms.txt` index.** Provides a centralized discovery point for LLMs and automated agents.

### Changed

- **Nothing yet.**

## [0.4.0] — 2026-05-07

### Added

- **Intent-based document classification (ADR-007 § DOC-001).** A `.md`
  file is classified as a document candidate if and only if it has an `id` matching
  `<NAMESPACE>-<number>` for a configured namespace, OR its location
  matches a configured namespace's `[<NS>].paths` glob. Files
  satisfying neither condition are skipped without triggering `core.frontmatter`,
  `core.id`, or other diagnostics.
- **`[<NS>].paths` configuration.** A list of gitignore-style globs
  that declare which files belong to a namespace based on their location. This utilizes set
  semantics and is order-independent.
- **`ctxgrd init` paths pre-fill.** The `init` command identifies conventional
  directories (e.g., `docs/adrs/`, `adrs/`) and pre-fills
  `[<NS>].paths` for detected matches. It reports detected paths on
  stderr.
- **`cfg.path-conflict` `KernelMessage` (DOC-007).** When overlapping
  `[<NS>].paths` globs from multiple namespaces claim the same file, ctxgrd
  resolves the conflict using an explicit id-claim. If no id-claim is present, it emits a configuration
  error during ingestion and excludes the file from rule execution.
- **Migration recipe stub** at
  `docs/migration/body-headers-to-frontmatter.md`. Resolves the EXT-003
  initialization advisory link; the complete
  example tracks as ADR-006 § EXT-004.

### Changed

- **`core.id` (IdMissing) and `core.frontmatter` conditions.** These diagnostics now only fire for
  files claimed by path. Escape clause instructions regarding
  `[ignore].patterns` have been removed, as these diagnostics only apply to files
  identified as ctxgrd documents by intent.
- **`DEFAULT_IGNORE_PATTERNS`.** Removed `**/CHANGELOG.md`
  and `**/README.md`. Intent-based classification renders these workarounds obsolete.
- **`DEFAULT_IGNORE_PATTERNS`.** Added `**/log/**`,
  `**/logs/**`, and `**/tmp/**` to optimize walker performance (similar to
  `target/**` and `**/dist/**`).
- **README.** Restructured to prioritize the "first-touch is silent" principle. The
  intent-claim mechanisms (id-claim or path-claim) are now presented before
  rule lists.
- **`docs/namespaces.md`.** Rewritten to prioritize `[<NS>].paths`.
  It documents the finalized decisions regarding negation (rejected in favor of `[ignore]`), absolute paths (rejected),
  source-emitted documents (where `[<NS>].paths` does not apply), and
  the precedence of `[ignore]` (which always overrides path claims).

### Decisions captured (ADR-007)

- OQ-1 resolved: Negation is not supported in `[<NS>].paths`.
- OQ-3 resolved: Absolute paths are rejected.
- OQ-4 resolved: Source-emitted documents are excluded from
  path-classification.
- OQ-5 resolved: `[ignore]` patterns take precedence over path-claims.
- OQ-7 resolved: `init` announces pre-filled paths on stderr.
- OQ-8 resolved: `cfg.path-conflict` uses the `KernelMessage`
  channel.

OQ-2 (LSP behavior on file rename) and OQ-6 (CLI override for
`[<NS>].paths`) remain explicitly deferred.

### Verification

- 307 tests across 5 suites: 295 lib, 4 init_body_headers, 4
  fixture_smoke, 2 intent_classification (verifying Hugo README + DESIGN.md
  silence), 2 path_conflict (validating cross-namespace overlap →
  `cfg.path-conflict`).
- `cargo clippy --lib --no-deps -- -D warnings` reports clean.
- `make check` reports clean (6 documents · 9 rules · 0 diagnostics).

[Unreleased]: https://github.com/aktagon/ctxgrd/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/aktagon/ctxgrd/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/aktagon/ctxgrd/releases/tag/v0.4.0
