# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Nothing yet.**

## [0.16.0] — 2026-06-09

### Changed

- **`ctxgrd init` namespace blocks now follow the built-in doc packs in
  full.** 0.13.0 sourced only `required-headings` from the packs;
  `required-metadata` and `allowed-values` stayed hardcoded generic
  (`id`/`title`/`status`; `draft`/`accepted`/`rejected`/`superseded`),
  so an init-generated block could be a chimera — e.g. a PMR with SRE-book
  headings but no required `incident_date` and an ADR-flavored status
  vocabulary. All three param tables now come from the owning pack
  (`project-docs` or `ops`) when one defines the namespace, making
  `ctxgrd init` and `ctxgrd pack add` produce identical shapes.
  Vocabularies follow the pack silently (no commented alternative —
  that remains headings-only). Pack-uncovered namespaces (DDR, unknown
  namespaces) keep the generic defaults.

## [0.15.0] — 2026-06-08

### Added

- **The `persona` pack (ADR-034).** Lints `STYLE.md` — the voice half of the
  [SOUL.md / STYLE.md](https://github.com/aaronjmars/soul.md) persona
  convention (MIT). Two warning-level rules: `style.section-order` flags a
  duplicate section and nudges toward the `STYLE.template.md` order (advisory —
  the spec mandates no order, so it never blocks), and `style.soul-pair` warns
  when a `STYLE.md` has no `SOUL.md` beside it. What it does not do is judge the
  voice. Whether a line is a concrete rule or a vague adjective — the thing that
  actually makes a STYLE.md work — is a semantic call that belongs to a model
  eval, not a structural linter, so ctxgrd declines it and says so in the pack
  comment. Opt in with `ctxgrd pack add persona`.
- **`ctxgrd status` (SPEC-002).** A read-only subcommand that reports where the
  project sits in its pipeline. It resolves a namespace DAG — a declared
  `[pipeline].stages` order if you have one, else the shape inferred from your
  `depends_on` edges, else the built-in `PRD → ADR → SPEC → TASK` ladder — and
  names which of the three it used. A stage is done only when its gate status is
  reached and every document under it is lint-clean, so an accepted-but-failing
  doc holds the stage instead of passing it; a join stage waits on all its
  parents. An open BUG citing a document in the active line blocks the stage it
  points at, and the block clears when the BUG leaves `open`. The output is a
  text ladder, or `--format json` for an agent that would rather route by table
  than re-read the docs every turn — current stage, blockers, and one next
  action from a fixed template, never lifted from document prose. It writes
  nothing and exits 0 at any position.
- **`pipeline.conformance` (SPEC-002).** When a `[pipeline].stages` order is
  declared — say `PRD → ADR → SPEC → TASK` — a `depends_on` edge that jumps a
  stage now errors, and the diagnostic names the stages it skipped. A TASK that
  depends straight on a PRD under that pipeline is the case it catches. Edges
  touching a namespace not listed in `stages` are exempt, and the rule stays
  silent until a `[pipeline]` table exists.

### Fixed

- **`design.section-order` and `design.token-ref` now actually fire on a
  `DESIGN.md`.** They were registered as document-level rules, but `DESIGN.md`
  is path-claimed and never becomes an id-keyed document — so they ran in their
  unit tests and nowhere else. A real `DESIGN.md` got nothing from them, and on
  top of that a spurious `core.id` error for missing an `id:` it was never meant
  to carry. Both are file-level now; the rules run, and the `design` pack no
  longer trips `core.id`. Found while wiring up the `persona` pack, which is
  path-claimed the same way.

## [0.14.0] — 2026-06-07

### Fixed

- **The walker no longer lints files the config never claimed.** A path-claim
  into an ignored directory (`.claude/skills/**/SKILL.md`) exempted the whole
  subtree from `[ignore]`, so a nested repo's ADRs were linted under the outer
  config's conventions. Now only files a claim glob actually matches escape
  the ignore set; directories stay traversable, so deep claims remain
  reachable.
- **`agents.context-headings` and `agents.context-budget` agree on nested
  instruction files.** The headings rule demanded a literal `@TODO.md`, which
  in a nested `cli/CLAUDE.md` points at a file that does not exist — and the
  budget rule warned about that. No text satisfied both rules. Imports now
  resolve relative to the importing file, so `@../TODO.md` passes, and the
  help text suggests the right relative path for the file's depth. The import
  must sit on its own line; the help says so.
- **Code examples are not imports.** Both import scanners skip `@`-tokens
  inside fenced blocks and inline code spans. A Python decorator in a
  CLAUDE.md example (`@mcp.tool(`) no longer warns as a dangling import.
- **The summary counts every file that was linted.** Path-claimed files
  (CLAUDE.md, AGENTS.md, TODO.md) now appear in the documents and rules
  counts, and the `ok: … 0 diagnostics` trailer no longer prints after a
  warning-only run. Fixing six errors in CLAUDE.md and then reading
  `ok: 3 documents` was the exact inverse of the truth.
- **Glob docs stop claiming gitignore syntax.** `[ignore].patterns` and
  `[<NS>].paths` compile as globset: anchored at the lint root, `*` crosses
  `/`, no `!` negation. The help text and `docs/namespaces.md` now say what
  the matcher does instead of what it resembles.

## [0.13.0] — 2026-06-05

### Changed

- **`ctxgrd init` defaults to the full document shapes.** For namespaces the
  built-in `project-docs` pack defines (ADR, PRD, RFC), the generated
  `required-headings` now use the pack's full requirement-driven shape; the
  conventional minimal shape (Nygard-style ADR, four-section PRD) rides along
  as a commented alternative the user can swap in. Namespaces the pack does
  not cover (DDR, RUN, PMR) keep their conventional shape unchanged.
- **`project-docs` pack: PRD headings gain `Definition of Done`.** The
  canonical PRD shape is now Context / Goals / Non-goals / User stories /
  Requirements / Definition of Done / Open Questions / References / Change log.
- **`project-docs` pack: ADR `allowed-values.status` gains `rejected`,**
  matching the status vocabulary the project's own ADRs already use.

### Added

- **New built-in `ops` pack: `RUN` and `PMR` namespaces,** grounded in
  Google's SRE Book. Runbooks (`docs/runbooks/**`: Trigger / Prerequisites /
  Steps / Rollback / Verification; status `draft`/`active`/`deprecated`) and
  postmortems (`docs/pmrs/**`: headings follow the blameless-postmortem
  template from SRE Book Appendix D — Summary / Impact / Root Causes /
  Trigger / Resolution / Detection / Action Items / Lessons Learned /
  Timeline; status `draft`/`in-review`/`complete`; `incident_date` required).
  Previously these record types existed only in the init catalogue; the
  `ops` pack now owns them. `project-docs` is unchanged from 0.12.0.
- **Drift-guard test** (`tests/dogfood_pack_drift.rs`) pinning the repo's own
  `ctxgrd.toml` ADR/PRD shape tables to the `project-docs` pack, so the
  dogfood config and the pack can no longer diverge silently.

## [0.12.0] — 2026-06-05

One consolidated release. The internal version moved through 0.6.0–0.11.1
without a tag, so this entry describes the net change since 0.5.0.

### Added

- **Rule packs (ADR-013).** A pack is a reusable bundle of namespace config
  plus the external rule scripts it needs. `ctxgrd pack add <name>` appends the
  pack's `[<NAMESPACE>]` blocks to your `ctxgrd.toml` and copies its rule
  scripts, then walks away — there is no runtime tie, so linting behaves
  identically whether or not the pack stays on disk. Each written block carries
  a `# pack: <name>` provenance comment, existing namespace blocks are never
  clobbered, and `--dry-run` prints what would change without touching a file.
- **`ctxgrd pack list` / `pack show`.** Two read-only commands that never modify
  config. `list` shows every discoverable pack and its source; `show <name>`
  prints the namespaces, rules, and scripts a pack defines before you adopt it.
- **`ctxgrd init --pack <names>`.** Sugar for the first-run case — equivalent to
  `init` followed by `pack add` for each comma-separated name.
- **Three built-in packs (ADR-023).** `project-docs` stands up the `ADR`,
  `PRD`, `RFC`, and `BUG` doc types with status vocabularies and required
  headings, plus a path-claimed `TODO` namespace for the repo-root `TODO.md`.
  `agents` bundles everything a coding agent reads, is driven by, and reuses:
  `AGENTS` (the always-loaded `CLAUDE.md` / `AGENTS.md` / `GEMINI.md`
  instruction files), `SKILLS` (`SKILL.md` files), and the id-claimed `SPEC`,
  `TASK`, and `PROMPT` doc types. `design` claims `DESIGN.md` design-token
  sheets. Path-claimed namespaces fire on adoption; id-claimed ones activate
  when you author a document.
- **Agent-context rules (ADR-020, ADR-021).** Compiled-in rules for the
  agent's own context files. Editing the always-loaded instruction files busts
  the prompt cache, so volatile state does not belong there:
  `agents.context-headings` errors on a `Current State` or `TODO` heading and
  requires an `@TODO.md` import when a root `TODO.md` exists;
  `agents.context-budget` warns on a dangling `@import` and an over-budget
  file (`max_words`); `agents.context-cache` warns, in commit context only, on
  cache-busting edits and churn (`churn_min_hours`); `todo.freshness` requires
  a `Last updated: YYYY-MM-DD` line and warns once stale (`stale_days`,
  default 30); `todo.structure` requires a `### TODO` checklist and asks for a
  `### Context` section. Opt-in `todo.sections` enforces a strict
  Now/Next/Later/Done shape instead. `ctxgrd rules` reports all of them with
  source `pack`.
- **Agent-build rules (ADR-022).** `spec.requires-prd` errors when a SPEC's
  `depends_on` names no PRD; opt-in `tasks.files-allowed` warns when a path
  under a TASK's `Files allowed` heading resolves nowhere;
  `skills.frontmatter` errors when a `SKILL.md` lacks a non-empty `name` or
  `description`.
- **`ears.clause-syntax` (ADR-031).** An EARS clause-grammar rule: each list
  item carrying an `EARS-<NN>` / `EARS-<NN>.<M>` id under a `Requirements`
  heading must parse as one of the six EARS patterns (ubiquitous,
  event-driven, unwanted-behavior, state-driven, optional-feature, complex).
  The diagnostic names the defect — missing `shall`, missing trigger comma,
  lowercase keyword — so an agent can self-correct. Self-gating: bullets
  without an `EARS-` id are skipped. Default in the `agents` pack's `[SPEC]`
  and the `project-docs` pack's `[PRD]`; fires in any namespace that lists it.
- **`core.requirement-ref` + `todo.listed` (ADR-026).** `core.requirement-ref`
  scans `**Satisfies:**` / `**Addressed by:**` list items and warns when a
  requirement-ID token does not resolve. `todo.listed` warns when a document
  whose `status` is not terminal is missing from the repo-root `TODO.md`;
  available globally or per namespace.
- **`design.section-order` + `design.token-ref` (ADR-027).**
  `design.section-order` errors when a recognized `DESIGN.md` section heading
  is out of canonical order or duplicated; `design.token-ref` warns when a
  `{path.to.token}` reference in frontmatter points at no defined token.
- **Builtin-rule registry (ADR-024).** All compiled-in rules are defined once
  in `BUILTIN_RULES`; the resolver allow-list, dispatch tables, reserved rule
  prefixes, and `ctxgrd rules` descriptions derive from it, so the registry
  and the dispatch cannot drift.
- **`ctxgrd list`.** A document-inventory command that lists ingested documents
  grouped by namespace (`rich` / `markdown` / `json` output).
- **`ctxgrd hooks install` + CI guide.** Installs a pre-commit git hook;
  `docs/ci.md` covers GitHub Actions, the pre-commit framework, and the LSP
  server.
- **Pack discovery from three sources.** Built-in (compiled into the binary),
  `~/.ctxgrd/packs/*` (per-user), and `./packs/*` (per-repo). The more local
  source wins on name collision. Committing `./packs/<name>/` to a repository is
  how a team distributes a convention — git is the channel, no registry needed.
- **`docs/packs.md`** — the pack guide, also at `ctxgrd docs packs`.

### Changed

- **Unconfigured roots fail fast.** Linting a root without a `ctxgrd.toml`
  now exits with `cfg.missing` instead of passing silently — a green run no
  longer means "nothing was checked."
- **`cfg.rule-unknown` downgraded from kernel error to warning.** An unknown
  rule code is skipped instead of aborting the run, and the message suggests
  `ctxgrd pack add <name>` when a discoverable pack provides the rule
  (ADR-025).
- **`ctxgrd init` output redesigned (ADR-025).** A gh-style summary table and
  aligned next-step hints replace the prose dump.
- **Single-pass ingest (ADR-029).** Frontmatter is parsed once per file with
  a key-to-line map, and cross-ref / requirement-ref token extraction moved
  to parse time — rules read the shared `Document`, never re-parse the body.
- **Doc filenames standardized to `NNN-slug.md`** across scaffolding and the
  `examples/` fixture.

### Fixed

- **`design.token-ref` no longer fires on composite-token references
  (ADR-027 § DES-003).** The rule previously required a `{path.to.token}`
  reference to resolve to a _scalar_, but the DESIGN.md spec lets a component
  property reference a whole composite token (e.g. `typography: "{typography.label}"`
  where `typography.label` is a `{fontFamily, fontSize, …}` map). Conformant
  files failed, and the only workaround was to flatten structured tokens into
  opaque CSS strings. Resolution is now existence-based: a path landing on any
  defined node (scalar or group) resolves; only a path that points at nothing
  warns. Diagnostics now anchor to the offending key's front-matter line
  instead of `0,0`, and the message reads "points at no defined token."

### Verification

- 455 tests (410 lib + 45 integration) pass; `cargo clippy --lib --no-deps
-- -D warnings` reports clean.
- `make check` self-lint reports clean (33 documents · 27 rules ·
  0 diagnostics).

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

[Unreleased]: https://github.com/aktagon/ctxgrd/compare/v0.16.0...HEAD
[0.16.0]: https://github.com/aktagon/ctxgrd/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/aktagon/ctxgrd/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/aktagon/ctxgrd/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/aktagon/ctxgrd/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/aktagon/ctxgrd/compare/v0.5.0...v0.12.0
[0.5.0]: https://github.com/aktagon/ctxgrd/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/aktagon/ctxgrd/releases/tag/v0.4.0
