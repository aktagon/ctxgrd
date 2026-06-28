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

/// One total record for a builtin-compiled rule (REG-001). All fields
/// are required; omitting any is a compile error.
pub(crate) struct BuiltinRule {
    pub code: &'static str,
    pub level: Level,
    pub check: CheckFn,
    pub summary: &'static str,
    pub description: &'static str,
}

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
    },
    BuiltinRule {
        code: "agents.context-budget",
        level: Level::File,
        check: crate::agent_guide::check_context_budget,
        summary: "Instruction-file imports resolve and the body stays within budget.",
        description: "Warns when an `@path` import points to a missing file (a dropped \
            reference), and when the body exceeds `max_words` (default 4000) — an \
            always-loaded file taxes every request.",
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
    },
    BuiltinRule {
        code: "todo.freshness",
        level: Level::File,
        check: crate::agent_guide::check_todo_freshness,
        summary: "TODO.md carries a freshness line and is not stale.",
        description: "Errors when no parseable `Last updated: YYYY-MM-DD` line is present; \
            warns when the date is older than `stale_days` (default 30). Staleness is a \
            warning, never an error.",
    },
    BuiltinRule {
        code: "todo.structure",
        level: Level::File,
        check: crate::agent_guide::check_todo_structure,
        summary: "TODO.md has a checklist and a context section.",
        description: "Errors when there is no `### TODO` section, or a `### TODO` section \
            with no `- [ ]` item; warns when there is no `### Context` section.",
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
    },
    BuiltinRule {
        code: "skills.frontmatter",
        level: Level::File,
        check: crate::agent_guide::check_skills_frontmatter,
        summary: "SKILL.md frontmatter has non-empty `name` and `description`.",
        description: "Errors when SKILL.md is missing a `---` frontmatter fence, or when \
            `name` or `description` keys are absent or are not non-empty strings.",
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
        description: "Errors when a document in scope carries neither a `depends_on` link nor a \
            body cross-ref token — a mitigated finding must point at the implementing fix (an ADR \
            or tracker id) so the remediation is falsifiable. The `require-when-status` param \
            (e.g. `mitigated` on VULN) scopes the rule to matching documents; absent, it applies \
            unconditionally (SEC-006).",
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
            fire. Severity is the `severity` param (`error` | `warning`, default `error`). \
            Off by default, opt-in per namespace; never reads `[pipeline.gate]` (EARS-01.3).",
    },
];

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
            "ears.clause-syntax",
            "style.section-order",
            "style.soul-pair",
            "style.referenced",
            "soul.sections",
            "soul.referenced",
            "core.commit-freshness",
            "core.calendar-freshness",
            "core.requires-link",
            "security.vuln-sla",
            "security.risk-expiry",
            "security.remediation-link",
            "gdpr.processor-dpa",
            "hipaa.safeguard-evidence",
            "soc2.control-evidence",
            "iso27001.control-evidence",
            "nist.control-evidence",
            "core.acceptance-complete",
            "agent.frontmatter",
            "agent.assigned",
            "opencode.frontmatter",
            "guide.frontmatter",
            "c4.frontmatter",
        ] {
            assert!(
                codes.contains(&expected),
                "BUILTIN_RULES missing '{expected}'"
            );
        }
        assert_eq!(codes.len(), 35, "expected exactly 35 builtin rules");
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
