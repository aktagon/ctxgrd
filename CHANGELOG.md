# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0] — 2026-05-14

### Added
- Language Server Protocol (LSP) server implementation (ADR-008). Provides real-time diagnostics, go-to-definition, and completion for the document graph via the `ctxgrd lsp` subcommand.
- Neovim plugin. Includes a minimal Lua integration (`editors/nvim/lua/ctxgrd.lua`) to connect Neovim's LSP client to the `ctxgrd` server.
- Claude Code plugin integration (`.claude-plugin/marketplace.json`). Extends the agent with `lint`, `new`, and `refs` skills.
- `llms.txt` documentation index to improve discovery for LLMs and automated agents.

## [0.4.0] — 2026-05-07

### Added
- Intent-based document classification (ADR-007 § DOC-001). Markdown files are now classified as document candidates only if they contain an `id` matching `<NAMESPACE>-<number>` or if their file path matches a configured `[<NS>].paths` glob pattern. Other markdown files are skipped without producing diagnostics.
- `[<NS>].paths` configuration setting. Accepts a list of gitignore-style glob patterns to map file locations to specific namespaces.
- Auto-detection of paths in `ctxgrd init`. The command now searches for conventional directories (e.g., `docs/adrs/`) and pre-fills `[<NS>].paths` in the generated configuration, emitting a summary to `stderr`.
- `cfg.path-conflict` diagnostic (DOC-007). Emits a configuration error if multiple namespaces claim the same file via path globs and the file lacks an explicit `id` frontmatter field. The file is excluded from rule execution.
- Migration documentation stub at `docs/migration/body-headers-to-frontmatter.md` to resolve the initialization advisory link.

### Changed
- Diagnostics for missing IDs (`core.id`) and missing frontmatter (`core.frontmatter`) now only trigger for files explicitly claimed by path rules.
- Updated `DEFAULT_IGNORE_PATTERNS` to remove `**/CHANGELOG.md` and `**/README.md`, which are now naturally excluded by intent-based classification.
- Updated `DEFAULT_IGNORE_PATTERNS` to include `**/log/**`, `**/logs/**`, and `**/tmp/**` to reduce file traversal overhead.
- Updated `README.md` and `docs/namespaces.md` to prioritize documentation of the new intent-based file classification system.

[Unreleased]: https://github.com/aktagon/ctxgrd/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/aktagon/ctxgrd/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/aktagon/ctxgrd/releases/tag/v0.4.0
