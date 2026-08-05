//! Builtin-compiled rule registry — single source of truth (ADR-024).
//!
//! A builtin-compiled rule is defined exactly once here as a `BuiltinRule`
//! record carrying every facet the system needs. Every other list —
//! the resolver's allow-list, the file-level routing split, the dispatch
//! table, the `ctxgrd rules` descriptions — is derived from `BUILTIN_RULES`
//! at the point of use. No standalone copy of any rule's code, level, or
//! description may persist outside this registry (REG-001/002/004).
//!
//! To add a builtin-compiled rule: append one `BuiltinRule` record.
//! Omitting any field is a compile error (REG-001).

use std::path::Path;

use serde_json::Value;

use crate::diagnostic::Diagnostic;
use crate::document::Document;

/// Signature shared by every builtin-compiled rule.
pub(crate) type CheckFn = fn(&Document, Option<&Value>, &Path) -> Vec<Diagnostic>;

/// Whether a builtin-compiled rule operates on id-less path-claimed
/// singletons (File-level, e.g. CLAUDE.md/TODO.md) or on id-keyed
/// Documents (Document-level, e.g. TASK/SPEC records).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    File,
    Document,
}

/// Whether a configurable attribute is a `ctxgrd.toml` config param
/// (set under `[NS."rule.code"]`) or a frontmatter/metadata attribute
/// the rule reads from the document itself (ADR-095 § PDOC-001).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamKind {
    ConfigParam,
    FrontmatterAttribute,
}

/// One structured, machine-readable record of a configurable attribute
/// a rule accepts — the single source of truth the introspection
/// surfaces and the dogfood self-lint both read (ADR-095 § PDOC-001).
pub(crate) struct Param {
    /// The exact key: config key or frontmatter field (e.g. `headings`,
    /// `research.type`).
    pub name: &'static str,
    pub kind: ParamKind,
    pub optional: bool,
    /// Closed vocabulary, or `&[]` for an open / free-form value.
    pub values: &'static [&'static str],
    /// One short sentence describing the attribute.
    pub doc: &'static str,
}

/// One total record for a builtin-compiled rule (REG-001). All fields
/// are required; omitting any is a compile error.
pub(crate) struct BuiltinRule {
    pub code: &'static str,
    pub level: Level,
    pub check: CheckFn,
    pub summary: &'static str,
    pub description: &'static str,
    /// The rule's configurable attributes, one entry per param or
    /// frontmatter attribute it reads (ADR-095 § PDOC-001). `&[]` for a
    /// parameterless rule.
    pub params: &'static [Param],
}

/// The conditional-link rules: every builtin-compiled rule whose verdict
/// depends on whether a `depends_on` entry or body cross-ref **resolves**
/// to a document present in the run (BUG-030/BUG-031).
///
/// [`crate::run`] threads each of these the synthesized
/// [`crate::agent_guide::RESOLVED_REFS_PARAM`] — the same cross-corpus
/// channel `core.dep-shape` uses for `managed` (ADR-039 § DAG-003). The
/// list is declared here rather than inline in the dispatch so it sits
/// beside the registry it indexes; `resolution_aware_rules_are_registered`
/// pins every entry to a `Level::Document` code.
///
/// A rule added here that `run.rs` does not thread fails **closed**: its
/// candidate set is empty and it reports a gap. That is deliberate — the
/// unfixed form of every rule in this list was a false green, so a dropped
/// threading must be loud.
pub(crate) const RESOLUTION_AWARE_RULES: &[&str] = &[
    "security.risk-expiry",
    "security.remediation-link",
    "gdpr.processor-dpa",
    "hipaa.safeguard-evidence",
    "soc2.control-evidence",
    "iso27001.control-evidence",
    "nist.control-evidence",
    "core.evidence-link",
];

/// The `Level::File` rules that ALSO carry an explicit id-keyed arm in
/// [`crate::run`]'s step-6 per-document loop, and so are **not** inert on a
/// namespace that binds `core.id`.
///
/// Registration level alone cannot answer that question. `Level::File`
/// routes a rule through `Config::file_level_namespaces`, which excludes
/// `core.id` namespaces — but step 6 dispatches a few codes by name for
/// exactly this case, giving them a second, id-keyed path. ADR-078 made
/// `core.required-headings` dual-use, and ADR-109 § BDG-003 did the same
/// for `core.file-budget`. Every other `Level::File` code falls into step
/// 6's `_ => {}` and genuinely cannot fire there.
///
/// This is a second statement of a fact the dispatch owns, which is the
/// drift shape `cfg.rule-inert` exists to catch — so it is pinned from both
/// sides. `id_keyed_file_level_rules_are_registered_file_level` checks the
/// codes are real and `Level::File`;
/// `the_dual_dispatch_allow_list_actually_runs_on_an_id_keyed_document`
/// (`tests/rule_inert.rs`) checks step 6 still runs each of them. Drift the
/// other way — a new arm added to step 6 and not listed here — produces a
/// false `cfg.rule-inert`, which is loud rather than silent.
pub(crate) const ID_KEYED_FILE_LEVEL_RULES: &[&str] =
    &["core.required-headings", "core.file-budget"];

/// The authoritative registry of all builtin-compiled rules (REG-001).
/// `config`, `agent_guide`, and `introspect` derive their views from
/// this slice. No standalone parallel list may exist (REG-002, REG-004).
///
/// The `check` function bodies live in `agent_guide` (they need `Document`
/// and process-level I/O); the registry references them by function pointer
/// so it remains a leaf module that the three consumers point down onto
/// (REG-003: dependencies point toward stability).
pub(crate) const BUILTIN_RULES: &[BuiltinRule] = &[
    BuiltinRule {
        code: "agents.context-headings",
        level: Level::File,
        check: crate::agent_guide::check_context_headings,
        summary: "Instruction files stay free of volatile state and link TODO.md lazily.",
        description: "Errors on any `Current State`/`TODO` heading in CLAUDE.md/AGENTS.md \
            (volatile state churns the cached prefix), errors when a root TODO.md exists but \
            the file does not link to it, and warns when that pointer is an eager `@TODO.md` \
            import (which pays the file's tokens every session) rather than a lazy plain link.",
        params: &[],
    },
    BuiltinRule {
        code: "agents.context-budget",
        level: Level::File,
        check: crate::agent_guide::check_context_budget,
        summary: "Instruction-file imports resolve and the body stays within budget.",
        description: "Warns when an `@path` import points to a missing file (a dropped \
            reference), and when the body exceeds `max_words` (default 4000) — an \
            always-loaded file taxes every request.",
        params: &[Param {
            name: "max_words",
            kind: ParamKind::ConfigParam,
            optional: true,
            values: &[],
            doc: "Body word budget before the file is flagged (default 4000).",
        }],
    },
    BuiltinRule {
        code: "agents.context-cache",
        level: Level::File,
        check: crate::agent_guide::check_context_cache,
        summary: "Commit-context cache signals for instruction files.",
        description: "In commit context only (`CTXGRD_COMMIT_CONTEXT=1`), warns that a staged \
            edit to CLAUDE.md/AGENTS.md busts the prompt cache, and that the file churned two \
            or more times within `churn_min_hours` (opt-in, default 0). Silent in plain \
            CLI/LSP and without git.",
        params: &[Param {
            name: "churn_min_hours",
            kind: ParamKind::ConfigParam,
            optional: true,
            values: &[],
            doc: "Window (hours) within which a second edit is flagged as churn (default 0, opt-in).",
        }],
    },
    BuiltinRule {
        code: "todo.freshness",
        level: Level::File,
        check: crate::agent_guide::check_todo_freshness,
        summary: "TODO.md carries a freshness line and is not stale.",
        description: "Errors when no parseable `Last updated: YYYY-MM-DD` line is present; \
            warns when the date is older than `stale_days` (default 30). Staleness is a \
            warning, never an error.",
        params: &[Param {
            name: "stale_days",
            kind: ParamKind::ConfigParam,
            optional: true,
            values: &[],
            doc: "Age (days) past which the freshness line is considered stale (default 30).",
        }],
    },
    BuiltinRule {
        code: "todo.structure",
        level: Level::File,
        check: crate::agent_guide::check_todo_structure,
        summary: "TODO.md has a checklist and a context section.",
        description: "Errors when there is no `### TODO` section, or a `### TODO` section \
            with no `- [ ]` item; warns when there is no `### Context` section.",
        params: &[],
    },
    BuiltinRule {
        code: "todo.sections",
        level: Level::File,
        check: crate::agent_guide::check_todo_sections,
        summary: "TODO.md is shaped as Now/Next/Later/Done.",
        description: "Errors when TODO.md's H2 sections are not exactly `## Now`, `## Next`, \
            `## Later`, `## Done` in that order; when Now/Next/Later have no open `- [ ]` \
            items; or when Done contains an open `- [ ]` (Done is for completed items). \
            Opt-in — not enabled by the agent-context pack default.",
        params: &[],
    },
    BuiltinRule {
        code: "tasks.files-allowed",
        level: Level::Document,
        check: crate::agent_guide::check_task_files_allowed,
        summary: "TASK `Files allowed` paths resolve.",
        description: "Warns when a path listed under a TASK's `Files allowed` heading exists \
            neither as a file nor as a directory whose parent is present (a typo or stale \
            reference). A new file in an existing directory does not warn. Opt-in — not \
            enabled by the agent-build pack default.",
        params: &[],
    },
    BuiltinRule {
        code: "skills.frontmatter",
        level: Level::File,
        check: crate::agent_guide::check_skills_frontmatter,
        summary: "SKILL.md frontmatter has non-empty `name` and `description`.",
        description: "Errors when SKILL.md is missing a `---` frontmatter fence, or when \
            `name` or `description` keys are absent or are not non-empty strings.",
        params: &[],
    },
    BuiltinRule {
        code: "agent.frontmatter",
        level: Level::File,
        check: crate::agent_guide::check_agent_frontmatter,
        summary: "Claude Code subagent files (.claude/agents/*.md) have a well-formed `name`/`description`.",
        description: "Errors when a `.claude/agents/*.md` subagent definition is missing a \
            `---` frontmatter fence, or when `name`/`description` are absent or not non-empty \
            strings. Warns when `name` does not match the filename stem, when `description` is \
            shorter than `desc_min_chars` (default 40) or longer than an opt-in `desc_max_chars`, \
            or when `model` is outside a team-pinned `models` allowlist. The binary enumerates no \
            model names — the allowlist is config-only.",
        params: &[
            Param {
                name: "desc_min_chars",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Minimum `description` length before a warning (default 40).",
            },
            Param {
                name: "desc_max_chars",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Opt-in maximum `description` length before a warning.",
            },
            Param {
                name: "models",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Team-pinned allowlist of `model` values; the binary enumerates none.",
            },
            Param {
                name: "name",
                kind: ParamKind::FrontmatterAttribute,
                optional: false,
                values: &[],
                doc: "Agent name; must be present, non-empty, and match the filename stem.",
            },
            Param {
                name: "description",
                kind: ParamKind::FrontmatterAttribute,
                optional: false,
                values: &[],
                doc: "Agent description; must be present and non-empty.",
            },
            Param {
                name: "model",
                kind: ParamKind::FrontmatterAttribute,
                optional: true,
                values: &[],
                doc: "Optional model pin, checked against the `models` allowlist when set.",
            },
        ],
    },
    BuiltinRule {
        code: "opencode.frontmatter",
        level: Level::File,
        check: crate::agent_guide::check_opencode_frontmatter,
        summary: "opencode agent files (.opencode/agent/*.md) have a non-empty `description`.",
        description: "Errors when an `.opencode/agent/*.md` agent definition is missing a `---` \
            frontmatter fence or a non-empty `description` (opencode derives the name from the \
            filename, so there is no `name` field). Warns when `description` is shorter than \
            `desc_min_chars` (default 40) or longer than an opt-in `desc_max_chars`, or when \
            `model` is outside a team-pinned `models` allowlist. The binary enumerates no model \
            names — the allowlist is config-only.",
        params: &[
            Param {
                name: "desc_min_chars",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Minimum `description` length before a warning (default 40).",
            },
            Param {
                name: "desc_max_chars",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Opt-in maximum `description` length before a warning.",
            },
            Param {
                name: "models",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Team-pinned allowlist of `model` values; the binary enumerates none.",
            },
            Param {
                name: "description",
                kind: ParamKind::FrontmatterAttribute,
                optional: false,
                values: &[],
                doc: "Agent description; must be present and non-empty (opencode derives the name from the filename).",
            },
            Param {
                name: "model",
                kind: ParamKind::FrontmatterAttribute,
                optional: true,
                values: &[],
                doc: "Optional model pin, checked against the `models` allowlist when set.",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-057 § AOT-003/AOT-004). One generic,
        // parameterized rule carried by the `workflow` pack on TASK — harness
        // differences are params (`search_dirs`, `name_source`,
        // `builtin_agents`), not separate per-harness rules, because `pack add`
        // never merges a second pack's rule into an existing namespace.
        code: "agent.assigned",
        level: Level::Document,
        check: crate::agent_guide::check_agent_assigned,
        summary: "Every agent a TASK assigns resolves to a file agent or a configured built-in.",
        description: "Errors when a name in a TASK's `agents` metadata list resolves to neither a \
            markdown agent-definition file under the harness's agent directories nor an entry in \
            the `builtin_agents` allow-list. Params: `search_dirs` (default: Claude conventions — \
            `.claude/agents`, `~/.claude/plugins/*/agents`, `~/.claude/agents`), `name_source` \
            (`frontmatter` default, or `filename` for opencode), and `builtin_agents` (empty by \
            default; harness built-ins like `Explore` have no file and resolve only when listed). \
            The diagnostic names the searched locations, the available file agents, and a \
            nearest-match suggestion. Presence of `agents` is `core.required-metadata`'s concern \
            (ADR-057 § AOT-001).",
        params: &[
            Param {
                name: "search_dirs",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Directories searched for file agents (default: Claude conventions).",
            },
            Param {
                name: "name_source",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Where an agent's name comes from — `frontmatter` (default) or `filename` (opencode).",
            },
            Param {
                name: "builtin_agents",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Allow-list of harness built-in agent names that have no file (empty by default).",
            },
        ],
    },
    BuiltinRule {
        code: "guide.frontmatter",
        level: Level::File,
        check: crate::agent_guide::check_guide_frontmatter,
        summary: "End-user guides (docs/guides/**) have a non-empty `title` and a valid `type`.",
        description: "Errors when a guide is missing a `---` frontmatter fence, when `title` or \
            `type` are absent or not non-empty strings, or when `type` is outside a pack-supplied \
            `types` allowlist (the `guide` pack ships the Diátaxis four: tutorial, how-to, \
            reference, explanation). The binary enumerates no taxonomy — the allowlist is \
            config-only. File-level: a guide carries a title/type, not an `id`, so the filename \
            is the guide's slug (ADR-055).",
        params: &[
            Param {
                name: "types",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Allowlist of valid `type` values (the `guide` pack ships the Diátaxis four).",
            },
            Param {
                name: "title",
                kind: ParamKind::FrontmatterAttribute,
                optional: false,
                values: &[],
                doc: "Guide title; must be present and non-empty.",
            },
            Param {
                name: "diataxis.type",
                kind: ParamKind::FrontmatterAttribute,
                optional: false,
                values: &[],
                doc: "Diátaxis type; must be present and within the `types` allowlist.",
            },
        ],
    },
    BuiltinRule {
        code: "c4.frontmatter",
        level: Level::File,
        check: crate::agent_guide::check_c4_frontmatter,
        summary: "C4 architecture diagrams (docs/diagrams/**) have a non-empty `title` and a valid `c4.level`.",
        description: "Errors when a C4 diagram doc is missing a `---` frontmatter fence, when \
            `title` or `c4.level` are absent or not non-empty strings, or when `c4.level` is \
            outside a pack-supplied `levels` allowlist (the `c4` pack ships the four C4 levels — \
            context, container, component, code — plus the supplementary deployment, dynamic, \
            and landscape views). The binary enumerates no taxonomy — the allowlist is \
            config-only. The level lives under a `c4` object, not a top-level `type:`, which SSGs \
            reserve (BUG-015). File-level: a diagram carries a title/level, not an `id`, so the \
            filename is the diagram's slug. ctxgrd lints the markdown envelope only, never the \
            embedded Mermaid/DOT graph (ADR-075).",
        params: &[
            Param {
                name: "levels",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Allowlist of valid `c4.level` values (the `c4` pack ships the four C4 levels plus views).",
            },
            Param {
                name: "title",
                kind: ParamKind::FrontmatterAttribute,
                optional: false,
                values: &[],
                doc: "Diagram title; must be present and non-empty.",
            },
            Param {
                name: "c4.level",
                kind: ParamKind::FrontmatterAttribute,
                optional: false,
                values: &[],
                doc: "C4 level; must be present and within the `levels` allowlist.",
            },
        ],
    },
    BuiltinRule {
        code: "checklist.structure",
        level: Level::File,
        check: crate::agent_guide::check_checklist_structure,
        summary: "Checklists (docs/checklists/**) carry a title, a living|sealed status, a pin when sealed, and at least one checkbox.",
        description: "Errors when a `docs/checklists/**` doc is missing a `---` frontmatter \
            fence; when `title` is absent or not a non-empty string; when `status` is absent or \
            outside `{living, sealed}`; when `status: sealed` but `pinned_commit` is absent; or \
            when the body has no checkbox item (`- [ ]` / `- [x]`). File-level: a checklist \
            carries a title/status, not an `id`, so the filename is its slug. The two-state \
            lifecycle keeps a `living` template or in-flight instance un-gated while `sealed` is \
            the signed-off state the completeness and pin rules enforce (ADR-078).",
        params: &[],
    },
    BuiltinRule {
        code: "checklist.complete",
        level: Level::File,
        check: crate::agent_guide::check_checklist_complete,
        summary: "A sealed checklist has zero unchecked boxes.",
        description: "When `status: sealed`, errors once per remaining unchecked box (`- [ ]`). \
            No-op while `status: living`. Only `[x]`/`[X]` count as done; other bracket content \
            is not a task item. This is the 'all boxes checked' half of an auditable sign-off — \
            the gate fires only at seal time, never on a living template or in-flight instance \
            (ADR-078).",
        params: &[],
    },
    BuiltinRule {
        code: "checklist.pinned",
        level: Level::File,
        check: crate::agent_guide::check_checklist_pinned,
        summary: "A sealed checklist's pinned_commit is a real, in-history commit.",
        description: "When `status: sealed`, errors if `pinned_commit` is not a 40-hex SHA, does \
            not resolve to a commit in the repo, or is not an ancestor of HEAD. Degrades to a \
            warning (never a hard error) outside a usable git history — not a repo, no git, or a \
            shallow clone missing the object (fetch full history / `fetch-depth: 0` to verify). \
            No-op while `status: living`. This is the 'pinned to a commit' half of the sign-off: \
            'done' is anchored to a named integration commit that landed on this line of history \
            (ADR-078).",
        params: &[],
    },
    BuiltinRule {
        code: "core.required-headings",
        level: Level::File,
        check: crate::agent_guide::check_required_headings,
        summary: "A document contains every H2 heading named in its `headings` config param.",
        description: "Errors for each heading in the `headings` param that is absent from the \
            doc's H2 headings. Matching is normalized: a leading enumerator (`1.`, `1)`, `A.`) is \
            stripped and comparison is case-insensitive, so config `\"Plan / account structure\"` \
            matches a `## 1. Plan / account structure` heading. Presence, not order; extra \
            headings are allowed. No-op when `headings` is unset. Generic and config-driven — the \
            binary enumerates no section names; a checklist supplies its phases, an ADR its \
            sections (ADR-078).",
        params: &[Param {
            name: "headings",
            kind: ParamKind::ConfigParam,
            optional: true,
            values: &[],
            doc: "H2 headings the document must contain; no-op when unset.",
        }],
    },
    BuiltinRule {
        code: "core.required-anchors",
        level: Level::File,
        check: crate::agent_guide::check_required_anchors,
        summary: "A document's body contains every marker named in its `anchors` config param.",
        description: "Errors for each string in the `anchors` param absent from the document body \
            (substring match on the raw text, convention-agnostic — HTML-comment anchors \
            `<!-- @pack.rule -->` or any stable token). Presence only; extra anchors are allowed. \
            No-op when `anchors` is unset or empty. Generic and config-driven — the binary \
            enumerates no anchors; a stripe checklist supplies its `@stripe.*` markers, enabling \
            the vendor-specific structure rules ADR-078 deferred (ADR-078).",
        params: &[Param {
            name: "anchors",
            kind: ParamKind::ConfigParam,
            optional: true,
            values: &[],
            doc: "Marker strings the document body must contain; no-op when unset or empty.",
        }],
    },
    BuiltinRule {
        // Dual-dispatch like the two generic rules above (ADR-109 § BDG-003):
        // `Level::File` serves id-less path-claimed singletons (TODO.md), and
        // `run.rs` step 6 calls the same `check_file_budget` for id-keyed
        // documents. One function behind both paths — BUG-021 is the record of
        // what a second implementation behind one code costs.
        code: "core.file-budget",
        level: Level::File,
        check: crate::agent_guide::check_file_budget,
        summary: "A document stays under its character budget.",
        description: "Warns when the document's character count exceeds `max_chars` (default \
            150000 — Claude Code's own read-time warning threshold), counted over the full \
            document text including frontmatter. The help line names how many characters must \
            go and which H2 section is the largest candidate to move out. Warning, never an \
            error: an over-budget file is a cost, not a structural defect (ADR-109).",
        params: &[Param {
            name: "max_chars",
            kind: ParamKind::ConfigParam,
            optional: true,
            values: &[],
            doc: "Character budget before the file is flagged (default 150000).",
        }],
    },
    BuiltinRule {
        // Document-level (ADR-039 § DAG-002/DAG-003). The configurable
        // per-namespace `depends_on` edge contract that subsumes and replaces
        // both the bespoke `spec.requires-prd` (DAG-004) and
        // `pipeline.conformance` (DAG-003): a SPEC declares
        // `[SPEC."core.dep-shape"] requires = ["PRD"]` and optionally
        // `allows = ["ADR"]`. Enforces presence (`requires`) and edge
        // admissibility (`requires ∪ allows`). The managed set is threaded in
        // via the synthesized `managed` param by `run.rs`. A future `count`
        // long-form (DAG-008) stays reserved.
        code: "core.dep-shape",
        level: Level::Document,
        check: crate::agent_guide::check_dep_shape,
        summary: "A document's `depends_on` must contain each required upstream namespace and only admit allowed ones.",
        description: "For each namespace `T` in the per-namespace `requires` param, errors when \
            the document's `depends_on` has no `T-<n>` entry — presence, not cardinality, so two \
            entries of type `T` are fine (e.g. `[SPEC.\"core.dep-shape\"] requires = [\"PRD\"]` \
            requires at least one PRD link). It also errors on a `depends_on` edge to a *managed* \
            namespace (one that appears in any namespace's `core.dep-shape` requires/allows) that \
            is not in this namespace's `requires ∪ allows` — an inadmissible edge. Edges to \
            unmanaged namespaces are exempt. An absent or empty `requires` disables the presence \
            half; a future `count` long-form (DAG-008) is reserved. Replaces both \
            `spec.requires-prd` and `pipeline.conformance`.",
        params: &[
            Param {
                name: "requires",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Upstream namespaces that must each appear in `depends_on`.",
            },
            Param {
                name: "allows",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Additional managed namespaces an edge may admit beyond `requires`.",
            },
        ],
    },
    BuiltinRule {
        code: "todo.listed",
        level: Level::Document,
        check: crate::agent_guide::check_todo_listed,
        summary: "Open documents are referenced in TODO.md.",
        description: "Warns when a document's `status` is not in the terminal set \
            (accepted, superseded, done, fixed, wontfix, invalid, duplicate, \
            closed, implemented, n/a) but the document ID does not appear in the \
            repo-root TODO.md. Opt-in — not in any pack default.",
        params: &[],
    },
    BuiltinRule {
        // File-level: DESIGN.md is a path-claimed id-less singleton, so it
        // never becomes an id-keyed Document. A document-level registration
        // (the original ADR-027 wiring) made this rule dead on a real
        // DESIGN.md and also left the spurious `core.id` parse error
        // unsuppressed (BUG-007). The file-level pass builds a full AST.
        code: "design.section-order",
        level: Level::File,
        check: crate::agent_guide::check_design_section_order,
        summary: "DESIGN.md H2 sections must follow canonical order.",
        description: "Errors when a recognized section heading appears after \
            a heading with a higher canonical index, or when a recognized \
            heading appears more than once. Unrecognized sections are skipped. \
            Canonical order: Overview, Colors, Typography, Layout, \
            Elevation & Depth, Shapes, Components, Do's and Don'ts.",
        params: &[],
    },
    BuiltinRule {
        // File-level for the same reason as design.section-order (BUG-007):
        // PRODUCT.md is a path-claimed id-less singleton.
        code: "product.register",
        level: Level::File,
        check: crate::agent_guide::check_product_register,
        summary: "PRODUCT.md's `## Register` and `## Platform` values are bare and recognized.",
        description: "PRODUCT.md's register and platform are a wire contract, not prose: a \
            reader parses the first non-empty line under each heading and branches on it \
            (register picks the brand/product design reference, platform picks HIG, Material 3, \
            or neither). Errors when `## Register` is missing, empty, or holds a value outside \
            the `registers` allowlist. `## Platform` is optional — absent means `web` — and an \
            empty or unrecognized value is a warning, matching the reader's own fall back to \
            `web` rather than exceeding it. Trailing prose under either value is a warning. \
            Finally, the section named by `conditional_section` is required when the register \
            equals `conditional_on` and must be absent otherwise; that arm is skipped when the \
            register does not resolve. Section presence for `Register` and `Platform` is owned \
            here, not by `core.required-headings`, so no heading is checked twice (ADR-104).",
        params: &[
            Param {
                name: "registers",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &["brand", "product"],
                doc: "Values `## Register` may hold; defaults to brand/product.",
            },
            Param {
                name: "platforms",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &["web", "ios", "android", "adaptive"],
                doc: "Values `## Platform` may hold; defaults to web/ios/android/adaptive.",
            },
            Param {
                name: "conditional_section",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "H2 section required by exactly one register; defaults to `Conversion & Proof`.",
            },
            Param {
                name: "conditional_on",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Register value that requires `conditional_section`; defaults to `brand`.",
            },
        ],
    },
    BuiltinRule {
        code: "ears.clause-syntax",
        level: Level::Document,
        check: crate::agent_guide::check_ears_clauses,
        summary: "EARS-id'd Requirements clauses parse as one of the six EARS patterns.",
        description: "Warns when a list item carrying an `EARS-<NN>`/`EARS-<NN>.<M>` id under \
            a `Requirements` heading does not parse as one of the six EARS patterns \
            (ubiquitous, event-driven, unwanted-behavior, state-driven, optional-feature, \
            complex). The diagnostic names the defect: missing `shall`, missing trigger \
            comma, lowercase keyword. Keywords are accepted in all-caps or title case. \
            Bullets without an EARS id are skipped. Default in the agents pack's [SPEC] \
            and the project-docs pack's [PRD] — fires in any namespace that lists it.",
        params: &[],
    },
    BuiltinRule {
        // File-level for the same reason as design.section-order (BUG-007):
        // DESIGN.md is path-claimed. The synthetic file-level document parses
        // frontmatter into `metadata`, which this rule reads.
        code: "design.token-ref",
        level: Level::File,
        check: crate::agent_guide::check_design_token_ref,
        summary: "DESIGN.md {token.ref} references must resolve.",
        description: "Warns when a {path.to.token} reference in YAML \
            frontmatter string values does not resolve to a defined scalar \
            in the same file's frontmatter. Mapping and array nodes do not \
            count as resolved. Body prose is not scanned.",
        params: &[],
    },
    BuiltinRule {
        // File-level, NOT Document-level: STYLE.md is a path-claimed id-less
        // singleton (like CLAUDE.md/TODO.md/SKILL.md per ADR-020), so it never
        // becomes an id-keyed Document and the per-document loop never sees it.
        // A document-level registration would make this rule dead code on a
        // real STYLE.md (the latent state of `design.section-order`). The
        // file-level pass builds a full AST, so the heading-order check runs.
        code: "style.section-order",
        level: Level::File,
        check: crate::agent_guide::check_style_section_order,
        summary: "STYLE.md sections follow the template order; no duplicates.",
        description: "Warns on a duplicate recognized `##` section, and \
            advisorily when a recognized section appears after one with a \
            higher template index. Both warnings — the SOUL.md spec mandates \
            no order, so this nudges toward the template sequence (Voice \
            Principles, Vocabulary, Punctuation & Formatting, Platform \
            Differences, Quick Reactions, Rhetorical Moves, Anti-Patterns, \
            Examples of Right Voice). Unrecognized sections are skipped.",
        params: &[],
    },
    BuiltinRule {
        code: "style.soul-pair",
        level: Level::File,
        check: crate::agent_guide::check_style_soul_pair,
        summary: "STYLE.md has a SOUL.md sibling.",
        description: "Warns when a claimed STYLE.md has no SOUL.md in the same \
            directory — the spec's recommended persona pairing (identity + \
            voice). Warning only: the files may exist independently, so a \
            deliberately standalone STYLE.md is not blocked.",
        params: &[],
    },
    BuiltinRule {
        // File-level (ADR-047 § PRF-001). The persona-side complement to the
        // guide-side core.requires-link (ADR-046): because a file is claimed
        // by exactly one namespace, a rule on STYLE.md (claimed by [STYLE])
        // must look *outward* to the guide rather than have a persona
        // namespace claim CLAUDE.md (which would cfg.path-conflict with
        // [AGENTS]). Pack-wired so `pack add persona` activates it.
        code: "style.referenced",
        level: Level::File,
        check: crate::agent_guide::check_style_referenced,
        summary: "STYLE.md is referenced by an agent guide that exists.",
        description: "Warns when a STYLE.md exists and at least one root agent guide \
            (CLAUDE.md / AGENTS.md / GEMINI.md) exists, but none of them reference the \
            STYLE.md — by a markdown link or an `@import`, resolved file-relatively. A \
            persona file no guide loads is a dead file. Silent when no guide exists (a \
            persona may be loaded directly by a runtime). Warning only (ADR-047 § PRF-003).",
        params: &[],
    },
    BuiltinRule {
        // File-level, NOT Document-level: SOUL.md is a path-claimed id-less
        // singleton, like STYLE.md/DESIGN.md. ADR-035 § SOUL-002 specified
        // Level::Document, but that is the same latent bug ADR-034 corrected
        // for style.section-order (BUG-007) — a document-level rule never
        // fires on a path-claimed singleton through the CLI. File-level runs
        // it on the synthetic AST scan_file_level builds.
        code: "soul.sections",
        level: Level::File,
        check: crate::agent_guide::check_soul_sections,
        summary: "SOUL.md has the three high-signal sections.",
        description: "Warns once per missing high-signal section — Worldview, \
            Opinions, Boundaries — the trio the SOUL.md spec says carries the \
            most signal and to fill first. The other spec sections (Who I Am, \
            Interests, Current Focus, Influences, Vocabulary, Tensions & \
            Contradictions, Pet Peeves) are optional and unrecognized headings \
            pass silently; v1 checks presence only, not order or empty bodies.",
        params: &[],
    },
    BuiltinRule {
        // File-level (ADR-047 § PRF-001). Persona-side sibling of
        // style.referenced; fires on SOUL.md ([SOUL]) and looks outward to
        // the agent guide. See style.referenced for the path-claim rationale.
        code: "soul.referenced",
        level: Level::File,
        check: crate::agent_guide::check_soul_referenced,
        summary: "SOUL.md is referenced by an agent guide that exists.",
        description: "Warns when a SOUL.md exists and at least one root agent guide \
            (CLAUDE.md / AGENTS.md / GEMINI.md) exists, but none of them reference the \
            SOUL.md — by a markdown link or an `@import`, resolved file-relatively. A \
            persona file no guide loads is a dead file. Silent when no guide exists (a \
            persona may be loaded directly by a runtime). Warning only (ADR-047 § PRF-003).",
        params: &[],
    },
    BuiltinRule {
        // Document-level (ADR-040). The `pin` data is parsed once at
        // ingest (PIN-001, preserving ADR-029); only this check shells to
        // git, in the rule layer (PIN-006). Named `core.*` because it is a
        // namespace-agnostic core primitive, but it ships in the binary so
        // it lives in BUILTIN_RULES (ADR-024) rather than the pure
        // `core.*` set in `rules.rs`. Default-off — opt-in per namespace
        // (PIN-007).
        code: "core.commit-freshness",
        level: Level::Document,
        check: crate::agent_guide::check_commit_freshness,
        summary: "A pinned document's scoped code must not have drifted past the green commit.",
        description: "When a document carries a `pin` (a green `commit` plus a non-empty `scope` \
            of path globs), errors if that commit is not an ancestor of HEAD (PIN-002), and \
            reports the document stale when the working tree differs from the pinned commit at \
            any scoped path (PIN-003) — listing the changed paths and the commits since the pin. \
            Skips with a warning (never hard-fails) outside a usable git history — not a repo, no \
            git, or a shallow clone — naming `fetch-depth: 0` as the remedy (PIN-004). Per-namespace \
            opt-in via `require-pin` (flag pin-less documents) and `severity` (warning vs error) \
            params (PIN-007).",
        params: &[
            Param {
                name: "require-pin",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "When set, flag documents in this namespace that carry no `pin`.",
            },
            Param {
                name: "severity",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &["warning", "error"],
                doc: "Diagnostic level for a stale pin (default error).",
            },
            Param {
                name: "pin",
                kind: ParamKind::FrontmatterAttribute,
                optional: true,
                values: &[],
                doc: "A green `commit` plus a `scope` of path globs the rule tracks.",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-040 § PIN-008). Pure date arithmetic, no
        // git — the time-axis sibling of core.commit-freshness. Named
        // `core.*` as a namespace-agnostic primitive; ships compiled so it
        // lives here. `todo.freshness` is a thin preset over the same
        // staleness arithmetic.
        code: "core.calendar-freshness",
        level: Level::Document,
        check: crate::agent_guide::check_calendar_freshness,
        summary: "A dated document must be re-validated within its configured interval.",
        description: "Warns when a configured date field (`field`, default `reviewed_date`) plus \
            an interval (`stale_days`, default 30) is older than today. The namespace-agnostic \
            generalization of `todo.freshness` (PIN-008): `core.commit-freshness` is the code \
            axis, this is the time axis. A missing or unparseable date is silent (presence is \
            `core.required-metadata`'s concern); only an aged date warns.",
        params: &[
            Param {
                name: "field",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Frontmatter date field to age (default `reviewed_date`).",
            },
            Param {
                name: "stale_days",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Interval (days) after which the date warns as stale (default 30).",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-091 § FNM-001). Compares the file's leading
        // numeric prefix to the number in its `id`. Ships compiled — it
        // reads `doc.location` (a path), which the pure `rules.rs` set does
        // not touch — and is Document-level so it only ever sees id-keyed
        // documents; id-less path-claim singletons never reach it (FNM-002).
        code: "core.file-name",
        level: Level::Document,
        check: crate::agent_guide::check_file_name,
        summary: "A document's filename number matches the number in its `id`.",
        description: "Errors when the file's leading numeric prefix is absent, or when it does \
            not equal the number in the document's `id` (e.g. `docs/adrs/091-x.md` must carry \
            `id: ADR-91`). The prefix is compared as a parsed number, not a zero-padded string, \
            so `88-x.md` and `088-x.md` both satisfy `id: NS-88` — padding width is not policed. \
            Opt-in per namespace and Document-level, so it is silent on id-less path-claim \
            singletons (README/CLAUDE/GUIDE) that carry no `id`.",
        params: &[],
    },
    BuiltinRule {
        // File-level (ADR-046 § RRF-003). The generic "this file must
        // reference an existing sibling" check — the reusable form of the
        // hard-coded TODO.md linkage in agents.context-headings (ADR-020
        // § ACX-005). Named `core.*` as a namespace-agnostic primitive,
        // but ships compiled (it touches the filesystem to test target
        // existence), so it lives here rather than in the pure `core.*`
        // set in rules.rs (the core.commit-freshness precedent, ADR-040).
        // File-level because its primary target — the path-claimed agent
        // guide (CLAUDE.md / AGENTS.md) — is an id-less singleton the
        // per-document loop never sees (the BUG-007 lesson).
        code: "core.requires-link",
        level: Level::File,
        check: crate::agent_guide::check_requires_link,
        summary: "This file must reference each existing target in `targets`.",
        description: "For each path in the `targets` param, requires the linted file to \
            reference it — by a markdown link or an own-line `@import`, resolved relative to \
            the file — but only if the target exists on disk (a missing target is skipped; \
            the rule never demands a file be created). Either reference form satisfies it; \
            unlike the TODO.md check it carries no eager-vs-lazy opinion. Emits one diagnostic \
            per unreferenced existing target at the `severity` param (`error` | `warning`, \
            default `error`). The generic, parameterized form of the TODO.md lost-reference \
            check (ADR-020 § ACX-005); the completeness counterpart to core.cross-ref. Default \
            has no targets — opt-in per namespace (e.g. `[AGENTS]` with `targets = [\"SOUL.md\", \
            \"STYLE.md\"]` to lint that the agent guide loads its persona docs).",
        params: &[
            Param {
                name: "targets",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Paths the linted file must reference; a missing target is skipped.",
            },
            Param {
                name: "severity",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &["error", "warning"],
                doc: "Diagnostic level per unreferenced existing target (default error).",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-041 § SEC-004). The generic
        // core.calendar-freshness ages one date unconditionally; this
        // rule keys on `status`/`severity` and varies the window per
        // severity, which calendar-freshness cannot express.
        code: "security.vuln-sla",
        level: Level::Document,
        check: crate::agent_guide::check_vuln_sla,
        summary: "An open critical/high finding past its per-severity SLA window is flagged.",
        description: "Reads a finding's `status`, `severity`, and `discovered_date`. Acts only \
            on `status: open` findings. Looks up the finding's `severity` in the `windows` param \
            (severity name → days; default `critical = 7`, `high = 30`); a severity absent from \
            `windows` is never flagged (medium/low/info age silently). When the age exceeds the \
            window, emits one diagnostic at the configurable level (`severity` param, default \
            `error`). A missing or unparseable `discovered_date` is silent — presence is \
            `core.required-metadata`'s concern (SEC-004).",
        params: &[
            Param {
                name: "windows",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Map of severity name → SLA days (default `critical = 7`, `high = 30`).",
            },
            Param {
                name: "severity",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &["error", "warning"],
                doc: "Diagnostic level for a past-SLA finding (default error).",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-041 § SEC-005). core.required-metadata
        // cannot do the status-conditional, and core.calendar-freshness
        // warns-only and cannot enforce a *future* date or field presence.
        code: "security.risk-expiry",
        level: Level::Document,
        check: crate::agent_guide::check_risk_expiry,
        summary: "A risk acceptance must be signed, reasoned, and carry a future-dated `expires`.",
        description: "Errors on a missing/empty `approver` or `rationale`, a missing or \
            unparseable `expires`, and an `expires` that is today or in the past (re-decide the \
            risk). The `require-when-status` param (e.g. `accepted` on VULN) scopes the rule to \
            matching documents; absent, it applies unconditionally (RISK). The \
            `exempt-when-links` param names a namespace prefix (e.g. `RISK`) whose presence in \
            `depends_on` exempts the document, because the linked document carries the fields \
            canonically (SEC-005).",
        params: &[
            Param {
                name: "require-when-status",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Status that scopes the rule to matching documents; absent, it applies unconditionally.",
            },
            Param {
                name: "exempt-when-links",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Namespace prefix whose presence in `depends_on` exempts the document.",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-041 § SEC-006, mitigated-VULN case).
        // core.cross-ref only validates that links which *are* present
        // resolve — it never requires one to exist. The THREAT
        // per-mitigation control-link is deferred (needs body parsing).
        code: "security.remediation-link",
        level: Level::Document,
        check: crate::agent_guide::check_remediation_link,
        summary: "A mitigated finding must cross-ref its remediation.",
        description: "Errors when a document in scope carries neither a cross-ref resolving to \
            one of `accepted-namespaces` (default `ADR`) nor a non-empty value in one of \
            `remediation-fields` (default `remediation_link`) — a mitigated finding must point at \
            the implementing fix so the remediation is falsifiable. The link may be a \
            `depends_on` entry or a body token, must resolve to a document present in the run, \
            and may not be the document's own id (BUG-031). The `require-when-status` param \
            (e.g. `mitigated` on VULN) scopes the rule to matching documents; absent, it applies \
            unconditionally (SEC-006).",
        params: &[
            Param {
                name: "require-when-status",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Status that scopes the rule to matching documents; absent, it applies unconditionally.",
            },
            Param {
                name: "accepted-namespaces",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Namespaces a resolving cross-ref may cite as the remediation (default `ADR`).",
            },
            Param {
                name: "remediation-fields",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Frontmatter fields whose non-empty value counts as the remediation (default `remediation_link`), for a fix tracked outside the document graph.",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-066 § GDPR-002). A conditional cross-ref:
        // core.cross-ref only checks that links present resolve, never that
        // a processor-role ROPA carries one. Parameterless — the GDPR
        // semantics (controller_or_processor → processor → DPA) are
        // statutory, not configuration.
        code: "gdpr.processor-dpa",
        level: Level::Document,
        check: crate::agent_guide::check_processor_dpa,
        summary: "A processor-role ROPA must cross-ref its governing DPA.",
        description: "Acts only on a ROPA record whose `controller_or_processor` is `processor`. \
            Errors when such a record carries no `depends_on` entry and no body cross-ref token \
            resolving to the `DPA` namespace — the Art. 28 processor agreement that governs the \
            processing. A controller or joint-controller record is out of scope (GDPR-002).",
        params: &[],
    },
    BuiltinRule {
        // Document-level (ADR-066 § HIPAA-002). Encodes the Security Rule's
        // Required/Addressable distinction: the `addressable` param (the
        // Addressable id subset, emitted by the generator from
        // regulation.json) decides whether a `justification` field may stand
        // in for an evidence cross-ref.
        code: "hipaa.safeguard-evidence",
        level: Level::Document,
        check: crate::agent_guide::check_safeguard_evidence,
        summary: "An SR-MAP safeguard mapping must cite implementing evidence (or, if addressable, a justification).",
        description: "Reads a mapping's `safeguard`. Errors when it has neither a cross-ref to an \
            evidence namespace (default `POLICY`/`ADR`, overridable via `evidence-namespaces`) nor \
            — for a safeguard in the `addressable` param list — a non-empty `justification` field. \
            A Required safeguard (absent from `addressable`) has no justification escape. The \
            `addressable` list is emitted by the generator from the canonical extract's \
            Required/Addressable flag (HIPAA-002).",
        params: &[
            Param {
                name: "addressable",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Addressable safeguard ids that may substitute a `justification` for evidence.",
            },
            Param {
                name: "evidence-namespaces",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Namespaces a cross-ref may cite as evidence (default `POLICY`/`ADR`).",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-069 § SOC-002). The SOC 2 control-to-evidence
        // check, reusing the same conditional-evidence machinery as
        // hipaa.safeguard-evidence (check_control_evidence and
        // check_safeguard_evidence both delegate to evidence_gap) — not a
        // forked SOC-2-specific rule. Fires on a SOC2 control asserting a
        // `criterion`; an in-scope control with neither a POLICY/ADR cross-ref
        // nor a non-empty `evidence_link` errors. A `not-applicable` status is
        // out of scope (the `out-of-scope-status` param). SOC 2 has no
        // addressable/required split, so the catalog emits
        // `evidence-fields = ["evidence_link"]` rather than an `addressable`
        // subset.
        code: "soc2.control-evidence",
        level: Level::Document,
        check: crate::agent_guide::check_control_evidence,
        summary: "An in-scope SOC2 control must cite operating-effectiveness evidence (a POLICY/ADR cross-ref or an evidence_link).",
        description: "Reads a SOC2 control's `criterion`. Errors when an in-scope control (status not in \
            `out-of-scope-status`, default none) has neither a cross-ref to an evidence namespace (default \
            `POLICY`/`ADR`, overridable via `evidence-namespaces`) nor a non-empty value in an evidence \
            field (default `evidence_link`, via `evidence-fields`). Type II attests operating effectiveness \
            over a period, so an asserted-but-unevidenced control is the defect the register catches \
            (SOC-002).",
        params: &[
            Param {
                name: "out-of-scope-status",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Statuses that exempt a control from the evidence requirement (default none).",
            },
            Param {
                name: "evidence-namespaces",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Namespaces a cross-ref may cite as evidence (default `POLICY`/`ADR`).",
            },
            Param {
                name: "evidence-fields",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Frontmatter fields whose non-empty value counts as evidence (default `evidence_link`).",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-070 § ISO-002). The ISO 27001 control-to-evidence
        // check, reusing the same conditional-evidence machinery as
        // soc2.control-evidence / hipaa.safeguard-evidence (all delegate to
        // evidence_gap) — not a forked ISO-specific rule. Fires on an ISO27001
        // control asserting an Annex A `control`; an in-scope control with
        // neither a POLICY/ADR cross-ref nor a non-empty `evidence_link` errors.
        // A `not-applicable` status is out of scope — the Statement of
        // Applicability's applicable/not-applicable decision rides the `status`.
        // A distinct rule code only so the diagnostic speaks ISO's terms.
        code: "iso27001.control-evidence",
        level: Level::Document,
        check: crate::agent_guide::check_iso_control_evidence,
        summary: "An in-scope ISO27001 control must cite implementing evidence (a POLICY/ADR cross-ref or an evidence_link).",
        description: "Reads an ISO27001 control's `control` (an Annex A id). Errors when an in-scope control \
            (status not in `out-of-scope-status`, default none) has neither a cross-ref to an evidence \
            namespace (default `POLICY`/`ADR`, overridable via `evidence-namespaces`) nor a non-empty value \
            in an evidence field (default `evidence_link`, via `evidence-fields`). A control marked \
            applicable with no implementing evidence is an unbacked claim — the highest-value register \
            check (ISO-002).",
        params: &[
            Param {
                name: "out-of-scope-status",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Statuses that exempt a control from the evidence requirement (default none).",
            },
            Param {
                name: "evidence-namespaces",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Namespaces a cross-ref may cite as evidence (default `POLICY`/`ADR`).",
            },
            Param {
                name: "evidence-fields",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Frontmatter fields whose non-empty value counts as evidence (default `evidence_link`).",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-071 § NIST-002). The NIST 800-53 control-to-
        // evidence check, reusing the same conditional-evidence machinery as
        // soc2.control-evidence (delegating to evidence_gap) — not a forked
        // NIST-specific rule. Fires on a NIST80053 control asserting a Rev 5
        // family in `control`; an in-scope control with neither a POLICY/ADR
        // cross-ref nor a non-empty `evidence_link` errors. A `not-applicable`
        // status is out of scope. A distinct rule code only so the diagnostic
        // speaks NIST's terms.
        code: "nist.control-evidence",
        level: Level::Document,
        check: crate::agent_guide::check_nist_control_evidence,
        summary: "An in-scope NIST80053 control must cite implementing evidence (a POLICY/ADR cross-ref or an evidence_link).",
        description: "Reads a NIST80053 control's `control` (a Rev 5 control family). Errors when an in-scope \
            control (status not in `out-of-scope-status`, default none) has neither a cross-ref to an evidence \
            namespace (default `POLICY`/`ADR`, overridable via `evidence-namespaces`) nor a non-empty value \
            in an evidence field (default `evidence_link`, via `evidence-fields`). A control claimed in scope \
            with no implementing evidence is an unbacked assertion (NIST-002).",
        params: &[
            Param {
                name: "out-of-scope-status",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Statuses that exempt a control from the evidence requirement (default none).",
            },
            Param {
                name: "evidence-namespaces",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Namespaces a cross-ref may cite as evidence (default `POLICY`/`ADR`).",
            },
            Param {
                name: "evidence-fields",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Frontmatter fields whose non-empty value counts as evidence (default `evidence_link`).",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-115 § REG-001). The regime-neutral member of the
        // conditional-evidence family: one mechanism (evidence_gap), one code,
        // the trigger field named by config rather than baked into the rule
        // name. soc2/iso27001/nist each got their own code so the diagnostic
        // could speak that framework's noun; three forks was affordable, one
        // per regulation is not. New regulation packs bind this.
        code: "core.evidence-link",
        level: Level::Document,
        check: crate::agent_guide::check_evidence_link,
        summary: "An in-scope register entry must cite implementing evidence (a cross-ref or an evidence field).",
        description: "Reads the obligation identifier named by the required `field` param. Errors \
            when an in-scope entry (status not in `out-of-scope-status`, default none) has neither \
            a cross-ref resolving to an evidence namespace (default `POLICY`/`ADR`, overridable \
            via `evidence-namespaces`) nor a non-empty value in an evidence field (default \
            `evidence_link`, via `evidence-fields`). The regime-neutral sibling of \
            `soc2.control-evidence` / `iso27001.control-evidence` / `nist.control-evidence`, \
            sharing their decision core; a namespace binding it without `field` errors rather \
            than going silently inert (REG-001).",
        params: &[
            Param {
                name: "field",
                kind: ParamKind::ConfigParam,
                optional: false,
                values: &[],
                doc: "The metadata key carrying the obligation identifier (e.g. `article`, `measure`). Required — the rule has no neutral default.",
            },
            Param {
                name: "out-of-scope-status",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Statuses that exempt an entry from the evidence requirement (default none).",
            },
            Param {
                name: "evidence-namespaces",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Namespaces a cross-ref may cite as evidence (default `POLICY`/`ADR`).",
            },
            Param {
                name: "evidence-fields",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Frontmatter fields whose non-empty value counts as evidence (default `evidence_link`).",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-056 § EARS-01). The completeness counterpart
        // to the `status` done-gate: a diagnostic on an open `- [ ]` under a
        // terminal-status document's acceptance heading marks the doc dirty,
        // so it holds the stage via the existing terminal-but-dirty path
        // (SPEC-002 § EARS-02.2) — no new status logic. Opt-in, namespace-
        // agnostic (`core.` prefix); the third heading-window-scan rule, so
        // it lands the rule-of-three `h2_section_window` extraction.
        code: "core.acceptance-complete",
        level: Level::Document,
        check: crate::agent_guide::check_acceptance_complete,
        summary: "A terminal-status document must have every acceptance checkbox checked.",
        description: "WHERE a document's `status` is in the `terminal` param set (default the \
            shared terminal-status vocabulary), emits one diagnostic per unchecked `- [ ]` item \
            under each configured acceptance heading (`headings` param, default `Acceptance`, \
            `Definition of Done`) — and only under those headings, so open items under `Out of \
            scope` / `Open Questions` / `Future work` (deferred work, not unmet criteria) never \
            fire. With `require_checkboxes = true` (ADR-122 § ACC-006) the section's top-level \
            items must also BE checkboxes, closing the case where prose bullets make a section \
            unscannable and the document reports clean because nothing is checkable. Severity is \
            the `severity` param (`error` | `warning`, default `error`). \
            Off by default, opt-in per namespace; never reads `[pipeline.gate]` (EARS-01.3).",
        params: &[
            Param {
                name: "terminal",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Statuses that arm the check (default the shared terminal-status vocabulary).",
            },
            Param {
                name: "headings",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Acceptance headings whose checkboxes must be checked (default `Acceptance`, `Definition of Done`).",
            },
            Param {
                name: "require_checkboxes",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &["true", "false"],
                doc: "Also require the section's top-level items to BE checkboxes (default `false`). Without it a section written as prose bullets is invisible to the scan, so the document reports clean because nothing is checkable.",
            },
            Param {
                name: "severity",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &["error", "warning"],
                doc: "Diagnostic level per unchecked item (default error).",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-082 § DDD-003). The one check the ddd pack's
        // CONTEXTMAP namespace needs beyond the core dep-graph rules: core.dep-shape
        // asserts a BOUNDEDCONTEXT edge is *present* but cannot assert cardinality
        // ("exactly two"), and whether `upstream`/`downstream` role fields are
        // required is *conditional on `pattern`* — cross-field logic
        // core.allowed-values (single flat field) cannot express. The same if/then
        // shape as soc2.control-evidence / hipaa.safeguard-evidence.
        code: "ddd.context-map-shape",
        level: Level::Document,
        check: crate::agent_guide::check_context_map_shape,
        summary: "A CONTEXTMAP edge connects exactly two BOUNDEDCONTEXT contexts and carries upstream/downstream roles iff its pattern is asymmetric.",
        description: "Errors when a CONTEXTMAP's `depends_on` does not resolve to exactly \
            `exact_context_count` (default 2) `BOUNDEDCONTEXT` ids (`context-namespace`, default \
            `BOUNDEDCONTEXT`). Also errors when the `pattern` is asymmetric (not in \
            `symmetric_patterns`, default Partnership / Shared Kernel / Separate Ways) but the doc \
            omits an `upstream` or `downstream` role field, or when the pattern is symmetric but \
            declares either — the cardinality and cross-field direction checks core.dep-shape and \
            core.allowed-values cannot express (DDD-003). A doc with no `pattern` skips the \
            direction half; the field names are the `pattern-field`/`upstream-field`/\
            `downstream-field` params.",
        params: &[
            Param {
                name: "exact_context_count",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Number of BOUNDEDCONTEXT ids a `depends_on` must resolve to (default 2).",
            },
            Param {
                name: "context-namespace",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Namespace the edge's contexts must belong to (default `BOUNDEDCONTEXT`).",
            },
            Param {
                name: "symmetric_patterns",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Patterns that omit upstream/downstream roles (default Partnership / Shared Kernel / Separate Ways).",
            },
            Param {
                name: "pattern-field",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Frontmatter field naming the strategic pattern (default `pattern`).",
            },
            Param {
                name: "upstream-field",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Frontmatter field naming the upstream role (default `upstream`).",
            },
            Param {
                name: "downstream-field",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Frontmatter field naming the downstream role (default `downstream`).",
            },
        ],
    },
    BuiltinRule {
        // File-level (ADR-093 § RSR-002). The `research` pack's RESEARCH
        // namespace is id-less path-claim (docs/research/**), so — like
        // GUIDE/C4/CHECKLIST — only file-level rules ever dispatch on it; a
        // Level::Document registration would silently never run. A pure
        // normalized-heading walk over the H2/H3 headings (no filesystem, no
        // body re-parse, ADR-029). core.allowed-values cannot validate
        // `research.type` (it is Document-level and never dispatches on an
        // id-less file-level namespace), so the rule self-validates the field
        // exactly like c4.frontmatter self-validates `c4.level`.
        code: "research.evidence",
        level: Level::File,
        check: crate::agent_guide::check_research_evidence,
        summary: "A deep-research report (docs/research/**) has an evidence/sources section and a limitations/data-gaps section.",
        description: "Walks a RESEARCH report's normalized H2/H3 headings and checks two sections \
            by `contains`-match against configurable synonym sets. The evidence/sources half is \
            always an error when no heading contains any `evidence_headings` token (default \
            evidence, sources, references, appendix) — a sources section is standard across the \
            academic, market, and AI-report genres. The limitations/data-gaps half emits at the \
            `severity` param level (default warning, promotable to error) when no heading contains \
            any `gaps_headings` token (default data gap, limitation, assumption, caveat) — a \
            dedicated gaps heading is good practice but not an industry convention. Setting either \
            heading list to `[]` disables that half. The optional `research.type` frontmatter field \
            (nested under a `research` object, not a top-level `type:` which SSGs reserve) is a \
            monotonic opt-in: absent runs only the baseline; a value outside the closed vocabulary \
            (academic, market, deep-research) is one error; a valid value additionally warns once \
            per missing genre-skeleton heading (academic → method/result, market → methodology/\
            recommendation, deep-research → summary/conclusion). The field only ever ADDS findings — \
            it never unlocks a passing state — and, when the rule already fires without it, each \
            diagnostic carries a note advertising it. File-level: a report carries no `id`, so the \
            filename is its slug (ADR-093).",
        params: &[
            Param {
                name: "evidence_headings",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Synonym tokens a heading must contain to satisfy the evidence/sources half (default evidence, sources, references, appendix); `[]` disables it.",
            },
            Param {
                name: "gaps_headings",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Synonym tokens a heading must contain to satisfy the limitations/data-gaps half (default data gap, limitation, assumption, caveat); `[]` disables it.",
            },
            Param {
                name: "severity",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &["warning", "error"],
                doc: "Diagnostic level for the missing data-gaps section (default warning); the evidence half is always an error.",
            },
            Param {
                name: "research.type",
                kind: ParamKind::FrontmatterAttribute,
                optional: true,
                // Single-sourced from the enforcement vocabulary (ADR-095):
                // the json metadata and the rule's own validation read one const.
                values: crate::agent_guide::RESEARCH_TYPES,
                doc: "Optional genre routing; a valid value monotonically adds that genre's skeleton-heading checks, an invalid value errors.",
            },
        ],
    },
    BuiltinRule {
        // Document-level (ADR-098 § QA-003). The two sealed-record invariants a
        // Test Completion Report carries that core presence rules cannot express:
        // both are conditional cross-field checks (on `status`, on `result`),
        // bundled in one rule the way c4.frontmatter / guide.frontmatter validate
        // several frontmatter invariants at once. The commit-SHA-shape check is
        // cloned from checklist.pinned (ADR-078); like it, v1 validates shape
        // only and never shells out to git. Document-level: the [TEST] namespace
        // is id-claimed (`id: TEST-<N>`), so it runs in the per-document loop.
        code: "test.completion",
        level: Level::Document,
        check: crate::agent_guide::check_test_completion,
        summary: "A sealed Test Completion Report carries commit-SHA-shaped tested_commit/spec_commit pins, and a conditional-pass names its outstanding defects.",
        description: "Acts only on a `status: sealed` record; drafts are ignored. Errors when a \
            sealed record is missing `tested_commit` (the tree the suite ran against) or \
            `spec_commit` (the revision of the linked contract verified), or when either is not a \
            40-character hex commit SHA (shape only — it does not resolve the SHA against git). \
            Also errors when `result: conditional-pass` but the `## Outstanding Defects` section is \
            absent or empty — a waiver must name what it waives, or the honest verdict is `pass`. \
            Pin logic cloned from checklist.pinned (ADR-098).",
        params: &[],
    },
    BuiltinRule {
        // File-level, monotonic opt-in (the research.type shape, ADR-093 —
        // NOT the frontmatter-mandatory guide/c4 rules). The one custom rule
        // the `marketing` pack ships: a marketing-strategy doc MAY declare its
        // genre with a nested `marketing.type`, which core.allowed-values (top-
        // level keys only) cannot validate, and which must nest to avoid the
        // SSG-reserved top-level `type:` (BUG-015). Reads the parsed metadata,
        // so an absent/malformed frontmatter is simply "no type" — which is why
        // the frontmatter-less CAMPAIGN placeholder binds it harmlessly.
        code: "marketing.frontmatter",
        level: Level::File,
        check: crate::agent_guide::check_marketing_frontmatter,
        summary: "A marketing-strategy doc that declares a nested `marketing.type` uses a value in the pack's `types` allowlist.",
        description: "Monotonic opt-in: a CAMPAIGN/PERSONA/POSITIONING/ICP doc MAY set a nested \
            `marketing.type` (under a `marketing` object, never a top-level `type:` which SSGs \
            reserve, BUG-015). Absent → no finding. Present → the value must be one of the \
            pack-supplied `types` allowlist (the `marketing` pack ships campaign, persona, \
            positioning, icp), else one error; with no `types` pinned it is presence-only. The \
            field only ever ADDS findings — it never unlocks a passing state — so a \
            frontmatter-less doc (the live CAMPAIGN placeholder) passes untouched. The binary \
            enumerates no vocabulary; the allowlist is config-only. File-level: these namespaces \
            are id-less path-claims, so the filename is the slug (ADR-100).",
        params: &[
            Param {
                name: "types",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Allowlist of valid `marketing.type` values (the `marketing` pack ships campaign, persona, positioning, icp); absent → presence-only.",
            },
            Param {
                name: "marketing.type",
                kind: ParamKind::FrontmatterAttribute,
                optional: true,
                values: &[],
                doc: "Optional genre discriminator; when present it must be within the `types` allowlist.",
            },
        ],
    },
    BuiltinRule {
        // File-level, warn-only, monotonic (ADR-102). The deterministic half of
        // AI-writing detection: flags the *mechanical* fingerprints in an
        // document whose presence is
        // the signal — curly quotes, decorative emoji, em/en-dash density, and a
        // small config list of exact chatbot-artifact phrases. Fingerprints inside
        // fenced/inline code are masked. The semantic tells and the ambiguous
        // technical words (`seam`, `load-bearing`) stay with `writing-humanizer`.
        code: "writing.ai-fingerprints",
        level: Level::File,
        check: crate::agent_guide::check_ai_fingerprints,
        summary: "A document's prose is free of mechanical AI-writing fingerprints (curly quotes, decorative emoji, em-dash overuse, chatbot-artifact phrases).",
        description: "Warn-only, monotonic: scans prose (code spans masked) for four deterministic \
            fingerprint classes and warns per hit — curly quotes/apostrophes (`flag_curly_quotes`, \
            default on), decorative emoji (`flag_emoji`, default on), em/en-dash density above \
            `max_em_dashes_per_kwords` (default 4, `0` disables — the one soft/heuristic class), and \
            exact case-insensitive `phrases` (compiled default: \"you're absolutely right\", \"i hope \
            this helps\"; `[]` disables). Never errors. It matches only fingerprints whose presence \
            is the defect; the semantic tells and the ambiguous words `seam`/`load-bearing` stay with \
            the `writing-humanizer` judgment pass (ADR-102).",
        params: &[
            Param {
                name: "flag_curly_quotes",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Warn on curly quotes/apostrophes (default true).",
            },
            Param {
                name: "flag_emoji",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Warn on decorative emoji (default true).",
            },
            Param {
                name: "max_em_dashes_per_kwords",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Em/en-dash density (per 1000 prose words) above which the file is flagged (default 4; 0 disables — soft signal).",
            },
            Param {
                name: "phrases",
                kind: ParamKind::ConfigParam,
                optional: true,
                values: &[],
                doc: "Exact chatbot-artifact phrases, matched case-insensitively (compiled default is a two-item set; [] disables).",
            },
        ],
    },
];

/// Read-only accessor for a builtin rule's configurable attributes, as
/// `(name, is_config_param)` pairs — the minimal surface the dogfood
/// self-lint (`tests/dogfood_param_docs.rs`) needs to assert that every
/// config param used in a pack is documented (ADR-095 § PDOC-002).
/// Returns `None` when `code` is not a builtin-compiled rule.
pub fn builtin_param_names(code: &str) -> Option<Vec<(&'static str, bool)>> {
    BUILTIN_RULES.iter().find(|r| r.code == code).map(|r| {
        r.params
            .iter()
            .map(|p| (p.name, p.kind == ParamKind::ConfigParam))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REG-002 equivalence test: for every code in BUILTIN_RULES, the
    /// resolver accepts it AND exactly one of file_level_check /
    /// document_check returns Some. This test fails against any state
    /// where a rule is dispatchable but absent from the resolver (or
    /// resolver-accepted but undispatchable) — the drift that caused
    /// review finding #1 (CRITICAL).
    #[test]
    fn reg_002_every_registry_code_resolves_and_dispatches_exactly_once() {
        for rule in BUILTIN_RULES {
            assert!(
                crate::config::is_builtin_compiled(rule.code),
                "resolver does not accept '{}' — add it to BUILTIN_RULES (REG-002)",
                rule.code
            );
            let file_ok = crate::agent_guide::file_level_check(rule.code).is_some();
            let doc_ok = crate::agent_guide::document_check(rule.code).is_some();
            assert!(
                file_ok ^ doc_ok,
                "rule '{}' must dispatch via exactly one of file_level_check / document_check \
                 (got file={file_ok}, doc={doc_ok}) — check the Level field (REG-002)",
                rule.code
            );
            assert_eq!(
                rule.level == Level::File,
                file_ok,
                "rule '{}' Level::File disagrees with file_level_check dispatch (REG-002)",
                rule.code
            );
        }
    }

    #[test]
    fn all_builtin_rules_are_registered() {
        let codes: Vec<&str> = BUILTIN_RULES.iter().map(|r| r.code).collect();
        for expected in [
            "agents.context-headings",
            "agents.context-budget",
            "agents.context-cache",
            "todo.freshness",
            "todo.structure",
            "todo.sections",
            "tasks.files-allowed",
            "skills.frontmatter",
            "core.dep-shape",
            "todo.listed",
            "design.section-order",
            "design.token-ref",
            "product.register",
            "ears.clause-syntax",
            "style.section-order",
            "style.soul-pair",
            "style.referenced",
            "soul.sections",
            "soul.referenced",
            "core.commit-freshness",
            "core.calendar-freshness",
            "core.file-name",
            "core.requires-link",
            "security.vuln-sla",
            "security.risk-expiry",
            "security.remediation-link",
            "gdpr.processor-dpa",
            "hipaa.safeguard-evidence",
            "soc2.control-evidence",
            "iso27001.control-evidence",
            "nist.control-evidence",
            "core.evidence-link",
            "core.acceptance-complete",
            "agent.frontmatter",
            "agent.assigned",
            "opencode.frontmatter",
            "guide.frontmatter",
            "c4.frontmatter",
            "checklist.structure",
            "checklist.complete",
            "checklist.pinned",
            "core.required-headings",
            "core.required-anchors",
            "core.file-budget",
            "ddd.context-map-shape",
            "research.evidence",
            "test.completion",
            "marketing.frontmatter",
            "writing.ai-fingerprints",
        ] {
            assert!(
                codes.contains(&expected),
                "BUILTIN_RULES missing '{expected}'"
            );
        }
        assert_eq!(codes.len(), 49, "expected exactly 49 builtin rules");
    }

    /// Every conditional-link rule names a real `Level::Document` code, so
    /// a typo in `RESOLUTION_AWARE_RULES` cannot silently thread nothing
    /// (BUG-030/BUG-031). The reverse direction — a resolution-dependent
    /// rule *missing* from the list — is covered by the fail-closed
    /// behaviour plus the per-rule negative controls, not by this test:
    /// nothing in the type system knows which rules read a cross-ref.
    #[test]
    fn resolution_aware_rules_are_registered() {
        for code in RESOLUTION_AWARE_RULES {
            let rule = BUILTIN_RULES
                .iter()
                .find(|r| &r.code == code)
                .unwrap_or_else(|| panic!("RESOLUTION_AWARE_RULES names unknown rule '{code}'"));
            assert_eq!(
                rule.level,
                Level::Document,
                "'{code}' is threaded the resolved-refs param from the per-document loop, \
                 so it must be Level::Document"
            );
        }
    }

    #[test]
    fn id_keyed_file_level_rules_are_registered_file_level() {
        for code in ID_KEYED_FILE_LEVEL_RULES {
            let rule = BUILTIN_RULES.iter().find(|r| &r.code == code).unwrap_or_else(|| {
                panic!("ID_KEYED_FILE_LEVEL_RULES names unknown rule '{code}'")
            });
            assert_eq!(
                rule.level,
                Level::File,
                "'{code}' is only an exemption from `cfg.rule-inert` because it is \
                 registered Level::File yet dual-dispatched by id; a Level::Document \
                 rule was never inert and does not belong on this list"
            );
        }
    }

    #[test]
    fn no_rule_has_empty_summary_or_description() {
        for rule in BUILTIN_RULES {
            assert!(
                !rule.summary.is_empty(),
                "rule '{}' has empty summary",
                rule.code
            );
            assert!(
                !rule.description.is_empty(),
                "rule '{}' has empty description",
                rule.code
            );
        }
    }
}
