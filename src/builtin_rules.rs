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
        code: "spec.requires-prd",
        level: Level::Document,
        check: crate::agent_guide::check_spec_requires_prd,
        summary: "SPEC documents must depend on a PRD.",
        description: "Errors when a SPEC document's `depends_on` does not contain at least \
            one `PRD-<n>` entry — a SPEC without a PRD link is incomplete.",
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
        // Document-level, but auto-activated by a declared `[pipeline]`
        // table rather than a namespace `rules` list (SPEC-002 EARS-06.1):
        // run.rs invokes it for every document in a staged namespace and
        // feeds the declared stage order through params. Registering it
        // here reserves the `pipeline.` prefix (ADR-020 § ACX-010) and
        // gives it a `ctxgrd rules` description.
        code: "pipeline.conformance",
        level: Level::Document,
        check: crate::agent_guide::check_pipeline_conformance,
        summary: "Dependency edges must not skip declared pipeline stages.",
        description: "Errors when a `depends_on` edge between two namespaces listed in \
            `[pipeline].stages` skips one or more stages between them (declared distance \
            > 1) — e.g. a TASK depending directly on a PRD under a PRD → ADR → SPEC → TASK \
            pipeline. The diagnostic names the skipped stages. Edges touching a namespace \
            absent from `stages` are exempt. Active only when a `[pipeline]` table is \
            declared.",
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
            "spec.requires-prd",
            "todo.listed",
            "design.section-order",
            "design.token-ref",
            "ears.clause-syntax",
            "style.section-order",
            "style.soul-pair",
            "pipeline.conformance",
        ] {
            assert!(
                codes.contains(&expected),
                "BUILTIN_RULES missing '{expected}'"
            );
        }
        assert_eq!(codes.len(), 16, "expected exactly 16 builtin rules");
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
