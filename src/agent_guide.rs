//! Builtin-compiled rule implementations and file-level scan orchestration.
//!
//! Unlike `core.*` rules (pure, no I/O — see [`crate::rules`]) and
//! external rules (subprocess scripts under `rules/<ns>/<name>/run`),
//! these rules are compiled into the binary yet live outside the `core`
//! namespace. They are the third rule category: *builtin-compiled* rules,
//! registered in [`crate::builtin_rules::BUILTIN_RULES`] and dispatched
//! from [`crate::run`]. They need filesystem, process, and env I/O — the
//! reason they cannot live in the pure `rules` module.
//!
//! Each rule does exactly its own checks; none branches on filename. The
//! namespace path-claim routes files to rules ([`scan_file_level`]), so
//! `AGENTS` (claiming `CLAUDE.md`/`AGENTS.md`) only ever feeds the three
//! `agents.*` rules and `TODO` (claiming the root `TODO.md`) only ever
//! feeds the two `todo.*` rules.
//!
//! - `agents.context-headings` — the always-loaded instruction prefix
//!   MUST NOT carry a `Current State` or `TODO` heading (volatile state
//!   there churns the cached prompt prefix on every edit), and MUST link
//!   a root `TODO.md` when one exists so the externalised state stays
//!   discoverable. The link MUST be lazy (a plain markdown link, read on
//!   demand); an eager `@TODO.md` import — which pays the file's tokens
//!   every session — is flagged as wasteful (ADR-036).
//! - `agents.context-budget` — warns on a dangling `@path` import and on
//!   an over-budget body (`max_words`, default 4000).
//! - `agents.context-cache` — in commit context only, warns that a staged
//!   edit busts the prompt cache and that the file churns
//!   (`churn_min_hours`, opt-in).
//! - `todo.freshness` — `TODO.md` MUST carry a parseable `Last updated:`
//!   line; warns when older than `stale_days` (default 30).
//! - `todo.structure` — `TODO.md` MUST have a `### TODO` section with at
//!   least one checklist item; SHOULD carry a `### Context` section.
//! - `todo.sections` (opt-in) — `TODO.md` MUST have exactly four H2
//!   sections — `## Now`, `## Next`, `## Later`, `## Done` — in that
//!   order, with no other H2s. Now/Next/Later each require ≥1 open
//!   `- [ ]` item; Done must contain only completed `- [x]` items.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::Value;

use crate::config::Config;
use crate::diagnostic::Diagnostic;
use crate::document::Document;
use crate::id::DocumentId;
use crate::path_claims::PathClaims;
use crate::source::markdown;

const HEADINGS: &str = "agents.context-headings";
const BUDGET: &str = "agents.context-budget";
const CACHE: &str = "agents.context-cache";
const FRESHNESS: &str = "todo.freshness";
const STRUCTURE: &str = "todo.structure";
const SECTIONS: &str = "todo.sections";
const FILES_ALLOWED: &str = "tasks.files-allowed";
const SKILLS_FM: &str = "skills.frontmatter";
const AGENT_FM: &str = "agent.frontmatter";
const AGENT_ASSIGNED: &str = "agent.assigned";
const OPENCODE_FM: &str = "opencode.frontmatter";
const GUIDE_FM: &str = "guide.frontmatter";
const C4_FM: &str = "c4.frontmatter";
const CHECKLIST_STRUCTURE: &str = "checklist.structure";
const CHECKLIST_COMPLETE: &str = "checklist.complete";
const CHECKLIST_PINNED: &str = "checklist.pinned";
const REQUIRED_HEADINGS: &str = "core.required-headings";
const REQUIRED_ANCHORS: &str = "core.required-anchors";
const FILE_BUDGET: &str = "core.file-budget";
/// Default `core.file-budget` ceiling (ADR-109 § BDG-002) — Claude Code's
/// own read-time warning threshold, so a bare binding reproduces the
/// warning the reader already gives rather than inventing a number.
const DEFAULT_MAX_CHARS: u64 = 150_000;
/// Below this many characters, an agent `description` is unlikely to carry
/// enough signal for reliable auto-delegation (warning). Shared default for
/// `agent.frontmatter` and `opencode.frontmatter`.
const AGENT_DESC_MIN_CHARS: usize = 40;
const DEP_SHAPE: &str = "core.dep-shape";
const FILE_NAME: &str = "core.file-name";
const TODO_LISTED: &str = "todo.listed";
const DESIGN_SECTION_ORDER: &str = "design.section-order";
const DESIGN_TOKEN_REF: &str = "design.token-ref";
const PRODUCT_REGISTER: &str = "product.register";
const EARS_SYNTAX: &str = "ears.clause-syntax";
const STYLE_SECTION_ORDER: &str = "style.section-order";
const STYLE_SOUL_PAIR: &str = "style.soul-pair";
const STYLE_REFERENCED: &str = "style.referenced";
const SOUL_SECTIONS: &str = "soul.sections";
const SOUL_REFERENCED: &str = "soul.referenced";
const COMMIT_FRESHNESS: &str = "core.commit-freshness";
const CALENDAR_FRESHNESS: &str = "core.calendar-freshness";
const REQUIRES_LINK: &str = "core.requires-link";
const VULN_SLA: &str = "security.vuln-sla";
const RISK_EXPIRY: &str = "security.risk-expiry";
const REMEDIATION_LINK: &str = "security.remediation-link";
const PROCESSOR_DPA: &str = "gdpr.processor-dpa";
const SAFEGUARD_EVIDENCE: &str = "hipaa.safeguard-evidence";
const CONTROL_EVIDENCE: &str = "soc2.control-evidence";
const ISO_CONTROL_EVIDENCE: &str = "iso27001.control-evidence";
const NIST_CONTROL_EVIDENCE: &str = "nist.control-evidence";
const EVIDENCE_LINK: &str = "core.evidence-link";
const ACCEPTANCE_COMPLETE: &str = "core.acceptance-complete";
const CONTEXT_MAP_SHAPE: &str = "ddd.context-map-shape";
const RESEARCH_EVIDENCE: &str = "research.evidence";
const TEST_COMPLETION: &str = "test.completion";
const MARKETING_FM: &str = "marketing.frontmatter";
const AI_FINGERPRINTS: &str = "writing.ai-fingerprints";

/// Default acceptance heading names the `core.acceptance-complete` scan
/// covers when the `headings` param is absent (ADR-056 § EARS-02). Matched
/// case-insensitively via [`normalize_heading`].
const DEFAULT_ACCEPTANCE_HEADINGS: &[&str] = &["acceptance", "definition of done"];

/// Default evidence/sources synonym set for `research.evidence` (ADR-093
/// § RSR-002). A section "exists" when some H2/H3 heading's normalized text
/// *contains* any of these tokens. Overridable via the `evidence_headings`
/// config param; `[]` disables the evidence half.
const DEFAULT_EVIDENCE_HEADINGS: &[&str] = &["evidence", "sources", "references", "appendix"];
/// Default limitations/data-gaps synonym set for `research.evidence`
/// (RSR-002). Overridable via `gaps_headings`; `[]` disables the gaps half.
const DEFAULT_GAPS_HEADINGS: &[&str] = &["data gap", "limitation", "assumption", "caveat"];
/// The closed `research.type` genre vocabulary (RSR-005) — a fixed taxonomy
/// tied to the field (like Diátaxis), not a volatile allowlist, so it is a
/// compiled `const` rather than a config param.
pub(crate) const RESEARCH_TYPES: &[&str] = &["academic", "market", "deep-research"];

const DEFAULT_STALE_DAYS: i64 = 30;
/// Generous default word budget for an always-loaded instruction file
/// (CLAUDE.md / AGENTS.md). Overridable via the `max_words` param.
const DEFAULT_MAX_WORDS: u64 = 4000;

/// File-level pass for the builtin-compiled namespaces (`AGENTS`, `TODO`,
/// `SKILLS`).
///
/// CLAUDE.md / AGENTS.md / TODO.md / SKILL.md files are id-less singletons
/// that never become id-keyed [`Document`]s — the per-document rule loop
/// never sees them (ADR-020 § ACX-003). So we walk the path-claimed files
/// directly, reusing the markdown walker (`sorted_markdown_files`) and AST
/// builder (`parse_ast`), and run each file-level rule the matched namespace
/// enables. Routing is by namespace path-claim + rule code — never by
/// basename (ADR-020 § ACX-004). Reads only the handful of files a
/// file-level namespace's globs match.
///
/// `path_claims` is threaded in from the caller (built once in `ingest()`)
/// to avoid the triple-rebuild per lint run (ADR-024 § review finding #7).
pub(crate) fn scan_file_level(
    root: &Path,
    config: &Config,
    path_claims: &PathClaims,
) -> std::io::Result<FileLevelScan> {
    let mut scan = FileLevelScan::default();
    let file_level = config.file_level_namespaces();
    if file_level.is_empty() {
        return Ok(scan);
    }
    for path in
        markdown::sorted_markdown_files(root, config.ignore.as_ref(), path_claims)?
    {
        let location = markdown::render_location(root, &path);
        let Some(ns) = path_claims
            .matching_namespaces(&location)
            .find(|ns| file_level.contains(ns))
            .map(str::to_owned)
        else {
            continue;
        };
        // A file we cannot read (or decode) is skipped here, not an
        // abort: the markdown scan already anchored a per-file
        // `src.markdown-read` diagnostic on it (BUG-024).
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let doc = synthetic_document(&ns, location, body);
        let ns_cfg = config.namespace_config(&ns);
        for code in &ns_cfg.rules {
            if let Some(check_fn) = file_level_check(code) {
                scan.diagnostics
                    .extend(check_fn(&doc, ns_cfg.params.get(code), root));
            }
        }
        // Coverage: this path-claimed singleton was linted but never
        // became an id-keyed document, so the caller folds it into the
        // summary counts (finding #3).
        scan.files_linted += 1;
        scan.namespaces.insert(ns);
        // Retain the synthetic document itself (ADR-103 § SRF-001):
        // `serve` renders these files from the exact set the scan linted,
        // keyed by `location`. `lint` ignores the field.
        scan.documents.push(doc);
    }
    Ok(scan)
}

/// Coverage produced by [`scan_file_level`]: the diagnostics plus the
/// file-level singletons it linted. The singletons never become id-keyed
/// [`Document`]s, so the summary counter in [`crate::run`] would miss them
/// without this (finding #3) — a misleading "0 documents" coverage signal.
#[derive(Default)]
pub(crate) struct FileLevelScan {
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// Count of path-claimed singleton files actually linted.
    pub(crate) files_linted: usize,
    /// Namespaces those singletons were claimed under — folded into the
    /// `rules_active` sum so their rules show in the summary.
    pub(crate) namespaces: std::collections::BTreeSet<String>,
    /// The synthetic [`Document`] built for each linted singleton, in
    /// walk order (ADR-103 § SRF-001). `serve` renders file-level pages
    /// from this set, keyed by `location` — the same inventory the rules
    /// above just linted, with no second walk or re-parse. Their `id` is
    /// the `<NS>-0` placeholder and `raw_id` is empty.
    pub(crate) documents: Vec<Document>,
}

/// Dispatch for *file-level* builtin-compiled rules. Derived from
/// `BUILTIN_RULES` so the dispatch and the registry cannot drift
/// (ADR-024 § REG-002).
pub(crate) fn file_level_check(code: &str) -> Option<crate::builtin_rules::CheckFn> {
    crate::builtin_rules::BUILTIN_RULES
        .iter()
        .find(|r| r.code == code && r.level == crate::builtin_rules::Level::File)
        .map(|r| r.check)
}

/// Dispatch for *document-level* builtin-compiled rules — those that lint
/// id-keyed [`Document`]s in the per-document loop ([`crate::run`] step 6)
/// rather than the id-less path-claimed singletons [`file_level_check`]
/// handles. Derived from `BUILTIN_RULES` (ADR-024 § REG-002).
pub(crate) fn document_check(code: &str) -> Option<crate::builtin_rules::CheckFn> {
    crate::builtin_rules::BUILTIN_RULES
        .iter()
        .find(|r| r.code == code && r.level == crate::builtin_rules::Level::Document)
        .map(|r| r.check)
}

/// Build a minimal [`Document`] for a file-level guide file. The `id` is
/// a placeholder — the file-level rules never read it; these files have
/// no real `<NS>-<number>` identity.
///
/// Frontmatter is parsed when present so file-level rules that read
/// `metadata` (e.g. `design.token-ref`/`style.*`) see the real values. A
/// missing or malformed fence yields empty maps — the `agents.*`/`todo.*`
/// rules ignore metadata and `skills.frontmatter` re-parses the body itself,
/// so populating these fields is purely additive. The `core.frontmatter` /
/// `core.id` parse diagnostics for these singletons are handled (suppressed)
/// in `run.rs`, not here.
fn synthetic_document(namespace: &str, location: String, body: String) -> Document {
    let ast = markdown::parse_ast(&body);
    let (metadata, frontmatter_lines) = crate::frontmatter::Frontmatter::parse_with_lines(&body)
        .map(|(fm, lines)| (fm.metadata, lines))
        .unwrap_or_default();
    Document {
        id: DocumentId::new(namespace.to_string(), 0),
        raw_id: String::new(),
        location,
        depends_on: Vec::new(),
        frontmatter_lines,
        metadata,
        pin: None,
        ast: Some(ast),
        body,
    }
}

// -- AGENTS namespace (CLAUDE.md / AGENTS.md) -------------------------

/// `agents.context-headings` (ACX-005): no volatile `Current State` /
/// `TODO` heading; a root TODO.md must be *linked* (lazily) when one
/// exists, and an eager `@TODO.md` import is flagged as token-wasteful.
pub(crate) fn check_context_headings(
    doc: &Document,
    _params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    let mut out = forbidden_heading_diags(doc);

    // A root TODO.md holds the project's volatile state. The instruction
    // file must point at it so it stays discoverable — but as a *lazy*
    // link, not an eager `@TODO.md` import. An `@import` pays TODO.md's
    // full token cost in every session's context prefix, used or not; a
    // plain link costs nothing until something opens the file. Reserve
    // `@` for content that must be present every turn — TODO.md is
    // reference state, consulted on demand (ADR-036, reversing the eager
    // form ADR-020 originally required).
    //
    // Both forms are resolved file-relatively (canonicalize) — the same
    // semantics `agents.context-budget` and Claude Code use — so a nested
    // `cli/CLAUDE.md` is satisfied by `[TODO.md](../TODO.md)` exactly as
    // it would be by `@../TODO.md`. Splitting the two invariants the rule
    // once fused: discoverability (something must point at TODO.md → an
    // error if nothing does) and loading strategy (eager vs lazy → a warn
    // when the pointer is an `@import`).
    let todo = root.join("TODO.md");
    if todo.is_file() {
        if references_root_todo_import(doc, root, &todo) {
            // An eager import resolves to TODO.md — discoverable, but
            // wasteful: it inflates every session's prefix. Warn, don't
            // error; the state is still reachable.
            out.push(
                Diagnostic::warning(
                    HEADINGS,
                    doc.location.clone(),
                    0,
                    0,
                    "this file imports TODO.md eagerly (`@TODO.md`), inflating every session's context",
                )
                .with_help(format!(
                    "replace the import with a lazy link, e.g. `{}` — the file is read on \
                     demand instead of being pulled into the always-loaded prefix",
                    suggested_link(doc, "TODO.md")
                ))
                .with_note(
                    "an `@import` pays TODO.md's full token cost every session, used or not; \
                     a plain link costs nothing until something opens it. Reserve `@` for \
                     content that must be present every turn — TODO.md is reference state.",
                ),
            );
        } else if !references_root_todo_link(doc, root, &todo) {
            // Neither an import nor a link resolves: the TODO.md is
            // orphaned — invisible to the agent and to a reader of this
            // file. Suggest the lazy form for this file's depth.
            out.push(
                Diagnostic::error(
                    HEADINGS,
                    doc.location.clone(),
                    0,
                    0,
                    "a root TODO.md exists but this file does not link to it",
                )
                .with_help(format!(
                    "add a lazy link on its own line, e.g. `{}`, so the state stays \
                     discoverable without inflating the per-session prefix",
                    suggested_link(doc, "TODO.md")
                ))
                .with_note(
                    "use a plain link (read on demand), not an `@TODO.md` import (eager — \
                     pays the file's tokens every session). The path resolves relative to \
                     this file, so a nested instruction file links `../TODO.md`.",
                ),
            );
        }
    }
    out
}

/// True when any `@<path>` import in `doc` resolves (relative to the
/// importing file's directory, mirroring Claude Code and the budget
/// rule) to the root `TODO.md` at `todo`. This is the *eager* form the
/// rule now warns on (ADR-036): the import pulls TODO.md into the
/// session prefix on every turn.
///
/// Scans the *wide* grammar ([`wide_import_paths`]), not the own-line
/// [`import_paths`] (BUG-029). The question this check asks is "does the
/// reader load TODO.md", so it must model what Claude Code resolves — and
/// Claude Code resolves a mid-sentence `@TODO.md` exactly as it resolves an
/// own-line one. Under the narrow grammar an inline import was invisible
/// here and the caller fell through to the "does not link to it" error,
/// reporting an unreachable file that was in fact loaded every turn.
fn references_root_todo_import(doc: &Document, root: &Path, todo: &Path) -> bool {
    any_path_resolves_to(wide_import_paths(doc), doc, root, todo)
}

/// True when a markdown link in `doc` resolves (file-relatively, like the
/// import check) to the root `TODO.md`. This is the *lazy* reference the
/// rule now prefers (ADR-036): the link is inert until something opens
/// the file, so TODO.md's tokens stay out of the session prefix.
fn references_root_todo_link(doc: &Document, root: &Path, todo: &Path) -> bool {
    link_resolves_to(doc, root, todo)
}

/// True when any own-line `@<path>` import in `doc` resolves
/// (file-relatively, via `canonicalize`, mirroring Claude Code and the
/// budget rule) to `target`. Shared by the `TODO.md` eager check
/// (ADR-036) and the generic `core.requires-link` rule (ADR-046 § RRF-001).
///
/// Resolution uses the filesystem: `target` is known to exist (callers
/// check `is_file()`), so an import that fails to canonicalize is dangling
/// and cannot match — the budget rule reports that separately.
fn import_resolves_to(doc: &Document, root: &Path, target: &Path) -> bool {
    any_path_resolves_to(import_paths(doc), doc, root, target)
}

/// True when any of `paths` — read from `doc` and resolved relative to
/// `doc`'s own directory — canonicalizes to `target`. The shared tail of
/// the import and link checks: they differ only in which token set they
/// hand in, never in how a path is resolved.
fn any_path_resolves_to<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    doc: &Document,
    root: &Path,
    target: &Path,
) -> bool {
    let dir = root.join(&doc.location);
    let dir = dir.parent().unwrap_or(root);
    let Ok(target) = std::fs::canonicalize(target) else {
        return false;
    };
    paths
        .into_iter()
        .filter_map(|p| std::fs::canonicalize(dir.join(p)).ok())
        .any(|resolved| resolved == target)
}

/// True when a markdown link in `doc` resolves (file-relatively) to
/// `target`. Shared by the `TODO.md` lazy check (ADR-036) and the generic
/// `core.requires-link` rule (ADR-046 § RRF-001).
///
/// Link hrefs are read from the parsed AST, so only real markdown links
/// count — a bare `TODO.md` in prose does not, mirroring the line-anchored
/// strictness `import_paths` applies to the eager form. A `#fragment` or
/// `?query` suffix is stripped before resolving.
fn link_resolves_to(doc: &Document, root: &Path, target: &Path) -> bool {
    let Some(ast) = doc.ast.as_ref() else {
        return false;
    };
    let hrefs = ast
        .links
        .iter()
        .map(|link| link.href.split(['#', '?']).next().unwrap_or(&link.href))
        .filter(|href| !href.is_empty());
    any_path_resolves_to(hrefs, doc, root, target)
}

/// `core.requires-link` (ADR-046 § RRF-001): a generic, namespace-agnostic
/// "this file must reference an existing sibling" check — the reusable form
/// of the hard-coded `TODO.md` linkage in `agents.context-headings`
/// (ADR-020 § ACX-005).
///
/// For each path in the `targets` param, resolve it relative to `root` and —
/// **only if the file exists** — require `doc` to reference it by either a
/// markdown link or an own-line `@import`. A target that does not exist is
/// skipped silently (the rule never demands a file be created). An absent or
/// empty `targets` list is a no-op.
///
/// Severity is the `severity` param (`error` | `warning`), defaulting to
/// `error` to mirror the shipped `TODO.md` precedent (RRF-002); an
/// unrecognized value falls back to the default. The eager-vs-lazy
/// distinction the `TODO.md` check makes (ADR-036) is deliberately out of
/// scope here — either reference form satisfies (RRF-005).
pub(crate) fn check_requires_link(
    doc: &Document,
    params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    let Some(targets) = params.and_then(|p| p.get("targets")).and_then(Value::as_array) else {
        return Vec::new();
    };

    let as_error = !params
        .and_then(|p| p.get("severity"))
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("warning"));

    let mut out = Vec::new();
    for target in targets.iter().filter_map(Value::as_str) {
        let path = root.join(target);
        if !path.is_file() {
            continue;
        }
        if link_resolves_to(doc, root, &path) || import_resolves_to(doc, root, &path) {
            continue;
        }
        let message =
            format!("a {target} exists but this file does not reference it");
        // `target` is root-relative; the reference forms below are resolved
        // relative to this document, so both are rendered at the document's
        // depth. Interpolating the raw `target` suggests a link that cannot
        // satisfy the rule for any document below the root (BUG-035).
        let relative = target_relative_to_doc(doc, target);
        let help = format!(
            "add a reference to {target} — a markdown link `{}` or an \
             `@{relative}` import; the path resolves relative to this file",
            suggested_link(doc, target)
        );
        let diag = if as_error {
            Diagnostic::error(REQUIRES_LINK, doc.location.clone(), 0, 0, message)
        } else {
            Diagnostic::warning(REQUIRES_LINK, doc.location.clone(), 0, 0, message)
        };
        out.push(diag.with_help(help));
    }
    out
}

/// The lazy markdown link a given document should carry to a
/// root-relative `target`. A root-level `CLAUDE.md` yields
/// `[TODO.md](TODO.md)`; a nested `cli/CLAUDE.md` yields
/// `[TODO.md](../TODO.md)`.
///
/// Shared by the `TODO.md` check (ADR-020 § ACX-005) and by
/// `core.requires-link`, the rule generalised out of it: both compare
/// targets resolved against the root against references resolved against
/// the *document*, so any suggestion they render must be re-based for the
/// document's depth or it cannot satisfy the check it is suggested for
/// (BUG-035).
fn suggested_link(doc: &Document, target: &str) -> String {
    format!("[{target}]({})", target_relative_to_doc(doc, target))
}

/// A root-relative `target` re-expressed relative to `doc`'s own
/// directory, by prefixing one `../` per directory level `doc` sits below
/// the root. Depth 0 returns `target` unchanged, so a root-level document's
/// suggestion is exactly what it was before this was depth-aware.
fn target_relative_to_doc(doc: &Document, target: &str) -> String {
    let depth = Path::new(&doc.location)
        .parent()
        .map(|p| p.components().count())
        .unwrap_or(0);
    format!("{}{target}", "../".repeat(depth))
}

/// Import paths declared on their own line — the first non-whitespace
/// token is `@<path>`. Line-anchored on purpose: a bare `@TODO.md`
/// mentioned mid-prose (e.g. documentation *about* this rule) is not an
/// import and must not satisfy `agents.context-headings` (finding #1).
/// Lines inside fenced/indented code blocks are excluded (BUG-006).
///
/// Token grammar note (BUG-004, revised by BUG-029): this is deliberately
/// a *subset* of [`wide_import_paths`], the grammar Claude Code actually
/// resolves. Two grammars, two jobs — pick by the question being asked:
///
/// - "does the reader load this file?" → [`wide_import_paths`]. Anything
///   that models the token cost or the reader's behaviour must use it.
/// - "is this written in the canonical form?" → this one. Only a
///   convention check, where own-line (visible, greppable, diff-friendly)
///   is the point, may narrow to it.
///
/// `core.requires-link` is the remaining caller, via [`import_resolves_to`].
fn import_paths(doc: &Document) -> Vec<&str> {
    doc.body
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
            if in_code_block(doc, line_no) {
                return None;
            }
            let token = line.trim_start();
            let rest = token.strip_prefix('@')?;
            let path = rest.split_whitespace().next()?;
            (!path.is_empty()).then_some(path)
        })
        .collect()
}

/// `agents.context-budget` (ACX-006): dangling `@path` imports and an
/// over-budget instruction file. Both warnings.
pub(crate) fn check_context_budget(
    doc: &Document,
    params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    let mut out = dangling_import_diags(doc, root);
    out.extend(oversize_diag(doc, params));
    out
}

/// `agents.context-cache` (ACX-007): commit-context-only cache-bust and
/// churn warnings. Silent outside commit context and without git.
pub(crate) fn check_context_cache(
    doc: &Document,
    params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if let Some(d) = cache_warning(doc, root) {
        out.push(d);
    }
    if let Some(d) = churn_warning(doc, params, root) {
        out.push(d);
    }
    out
}

/// Warn on `@<path>` imports whose target file does not exist
/// (ADR-016 § AGM-007). An import is the mechanism that pulls external
/// context back into the agent (principle 3, restorable compression); a
/// dangling one is a lost reference — the agent silently loses that
/// context.
///
/// Heuristic and deliberately conservative to avoid false positives:
/// only tokens that (a) start a word (so emails like `a@b.com` are
/// skipped), (b) contain a `.` (file-like), and (c) are repo-relative
/// (skip `~/…` and `/…`, which the linter cannot resolve) are checked.
/// Tokens inside fenced/indented code blocks or inline code spans are
/// excluded — Claude Code does not resolve those (BUG-006). Resolved
/// relative to the importing file's directory, mirroring how Claude
/// Code resolves `@imports`. Severity is warning, not error, because
/// the detection is heuristic.
///
/// Token grammar note (BUG-004): this scans the *superset* grammar
/// (any word-starting `@path`, matching what Claude Code resolves),
/// while `import_paths` — the `agents.context-headings` side — accepts
/// only the canonical own-line form. The split is intentional: a
/// dangling inline import really does lose context, but the headings
/// rule nudges toward the own-line convention.
fn dangling_import_diags(doc: &Document, root: &Path) -> Vec<Diagnostic> {
    let base = root.join(&doc.location);
    let dir = base.parent().unwrap_or(root);
    wide_import_paths(doc)
        .into_iter()
        .filter(|path| !dir.join(path).exists())
        .map(|path| {
            Diagnostic::warning(
                BUDGET,
                doc.location.clone(),
                0,
                0,
                format!("`@{path}` import points to a file that does not exist"),
            )
            .with_help(format!(
                "create `{path}` or remove the `@{path}` import — a dangling import drops that context"
            ))
        })
        .collect()
}

/// Every `@<path>` token in `doc` that Claude Code would resolve as an
/// import, deduplicated, in first-appearance order.
///
/// This is the grammar of the *reader*, and the one any rule reasoning
/// about loaded context must use (BUG-029). A token counts when it
/// (a) starts a word, so an email like `a@b.com` is skipped, (b) contains
/// a `.`, so it is file-like, and (c) is repo-relative — `~/…` and `/…`
/// are unresolvable from here. Position on the line is irrelevant: an
/// inline `see @TODO.md` is as eager as an own-line `@TODO.md`.
///
/// Fenced/indented code blocks are excluded: a fence is how documentation
/// shows a literal example, and a markdown fence containing `@TODO.md` is
/// demonstrating the syntax, not importing (BUG-006).
///
/// Inline code spans are **not** excluded (BUG-029 follow-up). BUG-006
/// masked them on the premise that Claude Code skips backticked tokens,
/// but its observed evidence was a fenced Python decorator (`@mcp.tool(`)
/// — the inline half was asserted alongside it, never measured. Treating a
/// backticked `@TODO.md` as inert is the same false negative this scanner
/// was widened to kill: if the reader loads it, the linter must see it.
/// Measured cost of unmasking, over every `CLAUDE.md` / `AGENTS.md` /
/// `GEMINI.md` in the ~90-repo fleet: one hit,
/// `` `Contributed by @username` ``, which the file-like filter drops
/// anyway for having no `.`.
///
/// Prose *about* an import stays clean through the token grammar instead:
/// a backtick sitting directly against the `@` is not whitespace, so
/// `` `@TODO.md` `` — the form real documentation writes, including this
/// repo's own `CLAUDE.md` — never matches to begin with.
///
/// Extracted from `dangling_import_diags` so the dangling-import warning
/// and the eager-import warning can never drift apart again.
fn wide_import_paths(doc: &Document) -> Vec<&str> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for (idx, line_text) in doc.body.lines().enumerate() {
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        if in_code_block(doc, line_no) {
            continue;
        }
        for caps in import_regex().captures_iter(line_text) {
            let m = caps.get(1).expect("import_regex has one capture group");
            // Strip trailing sentence punctuation that the token grabbed
            // (`@x.md.` / `@x.md,` / `(@x.md)`), so it neither corrupts the
            // path nor makes a dotless word like `@internal.` look file-like.
            //
            // The backtick is in the set because inline code spans are no
            // longer masked (BUG-029 follow-up): a token ending a span
            // arrives as `x.md\``, which resolves to nothing and would have
            // made the unmasking silently inert.
            let path = m.as_str().trim_end_matches(|c: char| {
                matches!(
                    c,
                    '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'' | '>' | '`'
                )
            });
            if path.starts_with('~') || path.starts_with('/') || !path.contains('.') {
                continue;
            }
            if seen.insert(path) {
                out.push(path);
            }
        }
    }
    out
}

/// True when `line` falls inside any fenced/indented code block of
/// `doc`. Documents without an AST (none in practice — both rule paths
/// populate it) get no masking.
fn in_code_block(doc: &Document, line: u32) -> bool {
    doc.ast.as_ref().is_some_and(|a| {
        a.code_blocks
            .iter()
            .any(|b| line >= b.line_start && line <= b.line_end)
    })
}

/// True when `(line, col)` falls inside an inline code span of `doc`.
fn in_inline_code(doc: &Document, line: u32, col: u32) -> bool {
    doc.ast.as_ref().is_some_and(|a| {
        a.inline_code_spans
            .iter()
            .any(|s| s.line == line && col >= s.col_start && col <= s.col_end)
    })
}

/// Warn when the always-loaded instruction file exceeds the word budget
/// (ADR-016 § AGM-007). CLAUDE.md / AGENTS.md ride the cached prefix on
/// every session, so size is a standing per-request token cost. The
/// limit is generous by default (`DEFAULT_MAX_WORDS`) and overridable
/// via the `max_words` param.
fn oversize_diag(doc: &Document, params: Option<&Value>) -> Option<Diagnostic> {
    let max_words = params
        .and_then(|p| p.get("max_words"))
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_MAX_WORDS);
    let words = doc.body.split_whitespace().count() as u64;
    (words > max_words).then(|| {
        Diagnostic::warning(
            BUDGET,
            doc.location.clone(),
            0,
            0,
            format!("instruction file is {words} words (limit {max_words}) — it loads into context every session"),
        )
        .with_help("move detail into referenced files (`@path`) so the always-loaded prefix stays lean")
    })
}

/// Error on any heading (at any level) whose normalised text is a
/// forbidden volatile-state marker. No-op when the document has no AST.
fn forbidden_heading_diags(doc: &Document) -> Vec<Diagnostic> {
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for h in &ast.headings {
        let normalized = normalize_heading(&h.text);
        if normalized == "current state" || normalized == "todo" {
            out.push(
                Diagnostic::error(
                    HEADINGS,
                    doc.location.clone(),
                    h.line,
                    h.col,
                    format!(
                        "instruction files must not contain a '{}' heading — volatile state belongs in TODO.md",
                        h.text.trim()
                    ),
                )
                .with_help("move this section into a root TODO.md and reference it with `@TODO.md`")
                .with_note(
                    "editing the always-loaded CLAUDE.md/AGENTS.md prefix invalidates the prompt cache; \
                     keep volatile state out of it",
                ),
            );
        }
    }
    out
}

/// Warn when a commit is staging an edit to the instruction file. Only
/// fires in commit context (`CTXGRD_COMMIT_CONTEXT=1`); silent in the
/// CLI and LSP, where "changed" is not a meaningful signal. Silent when
/// git is absent or the path is not in a repo (exit codes other than the
/// "differences found" sentinel `1`).
fn cache_warning(doc: &Document, root: &Path) -> Option<Diagnostic> {
    if std::env::var("CTXGRD_COMMIT_CONTEXT").ok().as_deref() != Some("1") {
        return None;
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--cached", "--quiet", "--"])
        .arg(&doc.location)
        .status()
        .ok()?;
    // `git diff --quiet` exits 1 when there are staged differences, 0
    // when clean, and other codes (e.g. 128) on error / not-a-repo.
    if status.code() == Some(1) {
        Some(
            Diagnostic::warning(
                CACHE,
                doc.location.clone(),
                0,
                0,
                "this commit modifies an always-loaded instruction file — edits bust the prompt cache",
            )
            .with_help(
                "keep CLAUDE.md/AGENTS.md stable; put churn-prone state in TODO.md instead",
            ),
        )
    } else {
        None
    }
}

/// Warn when the instruction file changes faster than once every
/// `churn_min_hours` (ADR-016 § AGM-007) — a meta-signal that the
/// stability the cached prefix is paying for is not real. Measured from
/// git commit history (the honest record of real edits), not a
/// persisted hash, so the rule stays stateless. Opt-in: disabled when
/// `churn_min_hours` is 0 (the default). Commit-context only, like
/// [`cache_warning`]; silent without git or history.
fn churn_warning(doc: &Document, params: Option<&Value>, root: &Path) -> Option<Diagnostic> {
    if std::env::var("CTXGRD_COMMIT_CONTEXT").ok().as_deref() != Some("1") {
        return None;
    }
    let hours = params
        .and_then(|p| p.get("churn_min_hours"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if hours == 0 {
        return None;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "log",
            &format!("--since={hours} hours ago"),
            "--format=%H",
            "--",
        ])
        .arg(&doc.location)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let changes = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    // Two or more commits touching the file inside the window means it
    // changed more often than once per `churn_min_hours`.
    (changes >= 2).then(|| {
        Diagnostic::warning(
            CACHE,
            doc.location.clone(),
            0,
            0,
            format!("instruction file changed {changes} times in the last {hours}h — frequent edits keep busting the prompt cache"),
        )
        .with_help("if this churn is expected, raise or unset `churn_min_hours`; otherwise move the moving parts into TODO.md")
    })
}

// -- TODO namespace (TODO.md) -----------------------------------------

/// `todo.freshness` (ACX-008): the freshness line is required (error when
/// absent) and warns when older than `stale_days`.
pub(crate) fn check_todo_freshness(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    match find_freshness(&doc.body) {
        Some((line, ymd)) => {
            let stale_days = params
                .and_then(|p| p.get("stale_days"))
                .and_then(|v| v.as_i64())
                .unwrap_or(DEFAULT_STALE_DAYS);
            if let Some(age) = days_since(ymd) {
                if age > stale_days {
                    out.push(
                        Diagnostic::warning(
                            FRESHNESS,
                            doc.location.clone(),
                            line,
                            0,
                            format!(
                                "state is stale — last updated {age} days ago (limit {stale_days})"
                            ),
                        )
                        .with_help("refresh the `Last updated:` date and the TODO items below it"),
                    );
                }
            }
        }
        None => out.push(
            Diagnostic::error(
                FRESHNESS,
                doc.location.clone(),
                0,
                0,
                "TODO.md must carry a `Last updated: YYYY-MM-DD` freshness line",
            )
            .with_help("add a line such as `_Last updated: 2026-05-26_` near the top"),
        ),
    }
    out
}

/// `todo.structure` (ACX-009): a `### TODO` section with at least one
/// checklist item (errors), and an advisory `### Context` section
/// (warning).
pub(crate) fn check_todo_structure(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    // `### TODO` section with at least one checklist item.
    if !has_h3(doc, "todo") {
        out.push(
            Diagnostic::error(
                STRUCTURE,
                doc.location.clone(),
                0,
                0,
                "TODO.md must have a `### TODO` section",
            )
            .with_help("add a `### TODO` heading listing the next steps as checkboxes"),
        );
    } else if !has_checklist_item(&doc.body) {
        out.push(
            Diagnostic::error(
                STRUCTURE,
                doc.location.clone(),
                0,
                0,
                "the `### TODO` section has no checklist items",
            )
            .with_help("add at least one `- [ ]` item under `### TODO`"),
        );
    }

    // `### Context` section is advisory.
    if !has_h3(doc, "context") {
        out.push(
            Diagnostic::warning(
                STRUCTURE,
                doc.location.clone(),
                0,
                0,
                "TODO.md is missing a `### Context` section",
            )
            .with_help("add a `### Context` heading capturing key facts/decisions"),
        );
    }

    out
}

/// `todo.sections` (TSE-001): TODO.md MUST have exactly four H2 sections —
/// `## Now`, `## Next`, `## Later`, `## Done` — in that order, with no
/// other H2 sections. Now/Next/Later each require at least one open
/// `- [ ]` item; Done MUST contain only completed `- [x]` items (an open
/// box in Done is an error). Opt-in — not enabled by the agent-context
/// pack default; users add the code to `[TODO].rules` to adopt the shape.
pub(crate) fn check_todo_sections(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();

    const EXPECTED_NORMALIZED: [&str; 4] = ["now", "next", "later", "done"];
    const EXPECTED_DISPLAY: [&str; 4] = ["Now", "Next", "Later", "Done"];

    let h2s: Vec<&crate::ast::Heading> = ast.headings.iter().filter(|h| h.level == 2).collect();
    let actual: Vec<String> = h2s.iter().map(|h| normalize_heading(&h.text)).collect();

    if actual
        .iter()
        .map(String::as_str)
        .ne(EXPECTED_NORMALIZED.iter().copied())
    {
        let found = if actual.is_empty() {
            "no H2 sections".to_string()
        } else {
            actual
                .iter()
                .map(|s| format!("`## {s}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push(
            Diagnostic::error(
                SECTIONS,
                doc.location.clone(),
                h2s.first().map(|h| h.line).unwrap_or(0),
                0,
                format!(
                    "TODO.md must have exactly `## Now`, `## Next`, `## Later`, `## Done` H2 sections in that order — found {found}"
                ),
            )
            .with_help(
                "structure the file as `## Now`, `## Next`, `## Later`, `## Done` — no other H2 sections",
            ),
        );
        // Without the expected shape, per-section content checks are noise.
        return out;
    }

    let lines: Vec<&str> = doc.body.lines().collect();
    for (idx, heading) in h2s.iter().enumerate() {
        // Headings carry 1-indexed line numbers; `lines[heading.line - 1]` is
        // the heading itself, so its content starts at index `heading.line`.
        let start = heading.line as usize;
        let end = h2s
            .get(idx + 1)
            .map(|next| (next.line as usize).saturating_sub(1))
            .unwrap_or(lines.len());
        let section_body = if start < end && start <= lines.len() {
            lines[start..end.min(lines.len())].join("\n")
        } else {
            String::new()
        };

        let has_open = open_checkbox_regex().is_match(&section_body);
        if EXPECTED_NORMALIZED[idx] == "done" {
            if has_open {
                out.push(
                    Diagnostic::error(
                        SECTIONS,
                        doc.location.clone(),
                        heading.line,
                        heading.col,
                        "`## Done` contains an open `- [ ]` item — Done is for completed `- [x]` items only",
                    )
                    .with_help(
                        "move open items to `## Now` / `## Next` / `## Later`, or mark them `- [x]` if they are done",
                    ),
                );
            }
        } else if !has_open {
            out.push(
                Diagnostic::error(
                    SECTIONS,
                    doc.location.clone(),
                    heading.line,
                    heading.col,
                    format!("`## {}` has no open `- [ ]` items", EXPECTED_DISPLAY[idx]),
                )
                .with_help(format!(
                    "add at least one `- [ ]` item under `## {}` (all four sections are required)",
                    EXPECTED_DISPLAY[idx]
                )),
            );
        }
    }
    out
}

// -- TASK namespace (SPEC/TASK id-claim records, ADR-022) -------------

/// `tasks.files-allowed` (ABP-005): each path listed under the `Files
/// allowed` H2 of a TASK MUST resolve — warns when neither the path nor
/// its parent directory exists relative to `root`. A file the task will
/// *create* (parent dir exists) does not warn; the parent-dir heuristic
/// catches typos and stale references without forbidding new files.
///
/// Unlike the `agents.*`/`todo.*` rules this runs on a real id-keyed TASK
/// [`Document`] (dispatched from [`document_check`]). If the document has
/// no `Files allowed` H2, the rule is silent — heading presence is
/// `core.required-headings`' concern. Opt-in: not in the agent-build pack
/// default (ABP-006).
pub(crate) fn check_task_files_allowed(
    doc: &Document,
    _params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };
    let lines: Vec<&str> = doc.body.lines().collect();
    let Some((start, limit)) = h2_section_window(ast, lines.len(), "files allowed") else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate().take(limit).skip(start) {
        let Some(caps) = list_item_regex().captures(line) else {
            continue;
        };
        let token = caps[1]
            .trim()
            .trim_matches('`')
            .trim()
            .trim_end_matches(['.', ',', ';', ':']);
        // A real path token has no internal whitespace; a bullet like
        // "All files in src" is prose, not a path.
        if token.is_empty() || token.contains(char::is_whitespace) {
            continue;
        }
        let path = root.join(token);
        if path.exists() {
            continue;
        }
        let parent_ok = path
            .parent()
            .is_some_and(|p| p.as_os_str().is_empty() || p == root || p.exists());
        if parent_ok {
            continue;
        }
        out.push(
            Diagnostic::warning(
                FILES_ALLOWED,
                doc.location.clone(),
                (i + 1) as u32,
                0,
                format!(
                    "Files-allowed path `{token}` does not exist and its parent directory is missing (typo or stale reference?)"
                ),
            )
            .with_help("fix the path, or create the directory if this slice introduces it"),
        );
    }
    out
}

// -- core.acceptance-complete (document-level, ADR-056 § EARS-01) ------

/// `core.acceptance-complete` (ADR-056 § EARS-01): WHERE a document is at
/// a status terminal for its namespace, every `- [ ]` checkbox under its
/// acceptance heading(s) MUST be checked — one diagnostic per open item.
/// The completeness counterpart to the `status` done-gate: because a
/// diagnostic anchored at the document marks it dirty, an open acceptance
/// box holds the document's stage through the existing terminal-but-dirty
/// path (SPEC-002 § EARS-02.2), with no new `status` logic (EARS-01.5).
///
/// Scans ONLY the configured acceptance heading window(s) (EARS-01.2):
/// open boxes under `Out of scope` / `Open Questions` / `Future work` are
/// deliberately-deferred work, not unmet criteria, and never fire. The
/// heading scope is the line between "criterion not met" and "thing we
/// chose not to do".
///
/// Document-level, namespace-agnostic (`core.` prefix), off-by-default and
/// opt-in per namespace (EARS-01.4) — config placement scopes it. Params
/// (EARS-01.3 — explicit, never read from `[pipeline.gate]`):
/// - `headings` (string list): acceptance heading names to scan. Default
///   `Acceptance`, `Definition of Done`. Matched case-insensitively.
/// - `terminal` (string list): the namespace's terminal status(es) that
///   activate the scan. Default: the shared terminal-status set
///   ([`DEFAULT_TERMINAL_STATUSES`]). A document whose `status` is outside
///   the set is silent — it has not yet claimed done.
/// - `severity` (`error` | `warning`): diagnostic level, default `error`.
pub(crate) fn check_acceptance_complete(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };

    // Only a terminal-status document is in scope (EARS-01.1): a doc still
    // in flight may legitimately carry open boxes.
    let status = doc
        .metadata
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let terminal: Vec<String> = params
        .and_then(|p| p.get("terminal"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_else(|| {
            DEFAULT_TERMINAL_STATUSES.iter().map(|s| s.to_string()).collect()
        });
    if !terminal.iter().any(|t| t.eq_ignore_ascii_case(status)) {
        return Vec::new();
    }

    let headings: Vec<String> = params
        .and_then(|p| p.get("headings"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_else(|| {
            DEFAULT_ACCEPTANCE_HEADINGS.iter().map(|s| s.to_string()).collect()
        });
    let severity = severity_param(params);

    let lines: Vec<&str> = doc.body.lines().collect();
    let mut out = Vec::new();
    for heading in &headings {
        let name = normalize_heading(heading);
        let Some((start, end)) = h2_section_window(ast, lines.len(), &name) else {
            continue;
        };
        // Render the heading as it appears in the document (original case),
        // not the normalized/configured form, so the diagnostic reads
        // `under `Acceptance``, not `under `acceptance``.
        let display = ast
            .headings
            .iter()
            .find(|h| h.level == 2 && normalize_heading(&h.text) == name)
            .map(|h| h.text.clone())
            .unwrap_or_else(|| heading.clone());
        for (i, line) in lines.iter().enumerate().take(end).skip(start) {
            if !open_checkbox_regex().is_match(line) {
                continue;
            }
            let mut diag = Diagnostic::error(
                ACCEPTANCE_COMPLETE,
                doc.location.clone(),
                (i + 1) as u32,
                0,
                format!(
                    "{}: unchecked acceptance item under `{display}` — a `{status}` document \
                     must have every acceptance criterion checked",
                    doc.raw_id
                ),
            )
            .with_help(
                "check the item once the criterion is met, or move deferred work out of the \
                 acceptance section (e.g. under `Out of scope` / `Future work`)",
            );
            diag.severity = severity;
            out.push(diag);
        }

        // `require_checkboxes` (ADR-122 § ACC-006). The scan above can only
        // see GFM task items, so a section written as prose bullets is
        // invisible to it — the document reports clean because nothing is
        // *checkable*, not because everything is done. That is the ADR-119
        // invariant at the item level, and it is the dominant case: 94 of this
        // repo's 119 terminal ADRs write Open Questions as prose.
        //
        // Off by default, so no existing config tightens. Where a namespace
        // turns it on, an unchecked box is still the error above; this adds
        // "and the items must be boxes in the first place".
        if !require_checkboxes(params) {
            continue;
        }
        for (i, line) in lines.iter().enumerate().take(end).skip(start) {
            let line_no = (i + 1) as u32;
            // A `- ` inside a fenced example is YAML, TOML or shell — not a
            // question anyone can answer with a checkbox. Without this the
            // diagnostic is unfixable: the author's only remedy is to delete
            // their example, and the rule ships ON by default in the
            // project-docs pack, so every consumer inherits it.
            if in_code_block(doc, line_no) {
                continue;
            }
            // Top-level list items only (no leading indentation). A nested
            // bullet under a task item is elaboration on that item, not a
            // separate question, and flagging it would punish detail.
            if !top_level_list_item_regex().is_match(line) || checklist_regex().is_match(line) {
                continue;
            }
            // `- - -` and `* * *` are CommonMark thematic breaks, not list
            // items, but they satisfy the bullet pattern. (`---` / `***` have
            // no whitespace and never matched.)
            if thematic_break_regex().is_match(line) {
                continue;
            }
            let mut diag = Diagnostic::error(
                ACCEPTANCE_COMPLETE,
                doc.location.clone(),
                (i + 1) as u32,
                0,
                format!(
                    "{}: item under `{display}` is a prose bullet, not a checkbox — a \
                     `{status}` document cannot be shown to have resolved it",
                    doc.raw_id
                ),
            )
            .with_help(
                "write the item as `- [ ]` while it is open and `- [x]` once it is resolved, \
                 so completeness is checkable rather than asserted",
            );
            diag.severity = severity;
            out.push(diag);
        }
    }
    out
}

/// The `require_checkboxes` param (ADR-122 § ACC-006), default `false`.
fn require_checkboxes(params: Option<&Value>) -> bool {
    params
        .and_then(|p| p.get("require_checkboxes"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// A list item at column 0 — no leading whitespace, so nested elaboration
/// under a task item is excluded.
///
/// Deliberately narrow: `-` and `*` only, not `+` or `1.`. Widening it
/// would make previously-clean documents fail, which the versioning policy
/// calls a MAJOR change — see BUG-055 for the hole this leaves.
fn top_level_list_item_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[-*]\s+\S").expect("valid regex"))
}

/// `- - -` / `* * *` — CommonMark thematic breaks that satisfy the
/// bullet pattern above. Three or more of the same marker separated by
/// whitespace, and nothing else on the line.
///
/// Spelled as three alternations rather than one backreferenced group:
/// the `regex` crate has no backreferences, so `([-*_])(\s*\1){2,}` is a
/// *runtime* `Regex::new` failure, not a compile error — it panicked
/// through `.expect()` on the first document that reached this scan.
fn thematic_break_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?:(?:-\s*){3,}|(?:\*\s*){3,}|(?:_\s*){3,})$").expect("valid regex")
    })
}

// -- agent.assigned (TASK ownership resolution, ADR-057) --------------

/// Where a file-defined agent's name comes from (ADR-057 § AOT-004). Claude
/// reads frontmatter `name:`; opencode derives the name from the filename
/// stem. The only harness difference the resolver needs, carried as a param.
#[derive(Clone, Copy)]
enum NameSource {
    Frontmatter,
    Filename,
}

/// `agent.assigned` (ADR-057 § AOT-003): every name in a TASK's `agents`
/// metadata list MUST resolve — as a markdown agent-definition file under the
/// harness's agent directories, or as an entry in the `builtin_agents`
/// allow-list (AOT-002). A name resolving to neither is an error, with a
/// recovery-oriented diagnostic naming the searched locations, the available
/// file agents, and a nearest-match suggestion (AOT-006).
///
/// Document-level rule on TASK, carried by the `workflow` pack (AOT-004).
/// Presence and non-emptiness of `agents` is `core.required-metadata`'s job
/// (AOT-001), so this rule is silent when the key is absent.
///
/// Params (all optional — minimal-config, AOT-005):
/// - `search_dirs`: array of directories (relative to `root`) holding agent
///   files. Default: Claude conventions — project `.claude/agents`, each
///   `~/.claude/plugins/*/agents`, and `~/.claude/agents` (local > plugin >
///   global, the order listed).
/// - `name_source`: `frontmatter` (default — each file's `name:`) or
///   `filename` (the file stem is the agent name, opencode's convention).
/// - `builtin_agents`: array of harness built-in names that have no file on
///   disk (e.g. `Explore`). Empty by default — a built-in resolves only when
///   listed (AOT-002).
pub(crate) fn check_agent_assigned(
    doc: &Document,
    params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    // Presence/non-emptiness is core.required-metadata's concern (AOT-001):
    // silent here when `agents` is absent or not a list.
    let Some(assigned) = doc.metadata.get("agents").and_then(Value::as_array) else {
        return Vec::new();
    };

    let builtins: std::collections::BTreeSet<&str> = params
        .and_then(|p| p.get("builtin_agents"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let name_source = match params
        .and_then(|p| p.get("name_source"))
        .and_then(Value::as_str)
    {
        Some(s) if s.eq_ignore_ascii_case("filename") => NameSource::Filename,
        _ => NameSource::Frontmatter,
    };

    let dirs = agent_search_dirs(params, root);
    let available = collect_agent_names(&dirs, name_source);

    // Nearest-match pool: file agents plus the allowed built-ins (AOT-006).
    let candidates: Vec<&str> = available
        .iter()
        .map(String::as_str)
        .chain(builtins.iter().copied())
        .collect();

    let line = doc.frontmatter_lines.get("agents").copied().unwrap_or(0);
    let mut out = Vec::new();
    for entry in assigned {
        let Some(name) = entry.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            // A non-string or empty list entry is a shape problem, not a
            // dangling owner — an empty list is core.required-metadata's.
            continue;
        };
        if builtins.contains(name) || available.contains(name) {
            continue;
        }

        let searched = dirs
            .iter()
            .map(|d| render_dir(root, d))
            .collect::<Vec<_>>()
            .join(", ");
        let available_list = if available.is_empty() {
            "(none)".to_string()
        } else {
            available
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let builtins_list = if builtins.is_empty() {
            "(none)".to_string()
        } else {
            builtins.iter().copied().collect::<Vec<_>>().join(", ")
        };

        let help = match nearest_match(name, &candidates) {
            Some(suggestion) => format!("did you mean `{suggestion}`?"),
            None => "add an agent-definition file for it, or list it in \
                     `[TASK.\"agent.assigned\"].builtin_agents` if it is a harness built-in"
                .to_string(),
        };

        out.push(
            Diagnostic::error(
                AGENT_ASSIGNED,
                doc.location.clone(),
                line,
                0,
                format!(
                    "{} assigns agent `{name}`, which resolves to no agent file or configured built-in",
                    doc.raw_id
                ),
            )
            .with_help(help)
            .with_note(format!(
                "searched {searched}; available file agents: {available_list}; \
                 built-ins allowed: {builtins_list}"
            )),
        );
    }
    out
}

/// The agent-definition directories `agent.assigned` searches. When the
/// `search_dirs` param is set, those (resolved relative to `root`) are used
/// verbatim; otherwise the Claude defaults (AOT-005): project `.claude/agents`,
/// each `~/.claude/plugins/*/agents`, and `~/.claude/agents`.
fn agent_search_dirs(params: Option<&Value>, root: &Path) -> Vec<std::path::PathBuf> {
    if let Some(dirs) = params
        .and_then(|p| p.get("search_dirs"))
        .and_then(Value::as_array)
    {
        return dirs
            .iter()
            .filter_map(Value::as_str)
            .map(|d| root.join(d))
            .collect();
    }
    let mut dirs = vec![root.join(".claude/agents")];
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        if let Ok(entries) = std::fs::read_dir(home.join(".claude/plugins")) {
            let mut plugin_dirs: Vec<std::path::PathBuf> = entries
                .flatten()
                .map(|e| e.path().join("agents"))
                .filter(|p| p.is_dir())
                .collect();
            plugin_dirs.sort();
            dirs.extend(plugin_dirs);
        }
        dirs.push(home.join(".claude/agents"));
    }
    dirs
}

/// Collect the names of every markdown agent-definition file under `dirs`,
/// per `name_source`. For `Filename` the name is the file stem; for
/// `Frontmatter` it is the file's `name:` field (Claude's convention, which
/// `agent.frontmatter` guarantees matches the stem, ADR-050 § PVP-002).
/// Missing directories are skipped silently.
fn collect_agent_names(
    dirs: &[std::path::PathBuf],
    name_source: NameSource,
) -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();
    for dir in dirs {
        for entry in walkdir::WalkDir::new(dir)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            match name_source {
                NameSource::Filename => {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.insert(stem.to_string());
                    }
                }
                NameSource::Frontmatter => {
                    let Ok(body) = std::fs::read_to_string(path) else {
                        continue;
                    };
                    if let Ok(fm) = crate::frontmatter::Frontmatter::parse(&body) {
                        if let Some(name) = fm
                            .metadata
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            names.insert(name.to_string());
                        }
                    }
                }
            }
        }
    }
    names
}

/// Render a search directory relative to `root` when it lives under it,
/// otherwise as an absolute path (the `~/.claude` dirs) — for the
/// "searched …" note (AOT-006).
fn render_dir(root: &Path, dir: &Path) -> String {
    dir.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| dir.display().to_string())
}

/// The closest candidate to `target` by Levenshtein distance, when one is
/// within a small threshold (a third of the longer name, floor 2) — the
/// "did you mean" suggestion for an unresolved agent (AOT-006). `None` when
/// nothing is close (a wholly unrelated name gets generic help instead).
fn nearest_match<'a>(target: &str, candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .map(|c| (*c, edit_distance(target, c)))
        .filter(|(c, d)| *d <= (target.len().max(c.len()) / 3).max(2))
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}

/// Levenshtein edit distance (insert/delete/substitute cost 1). Inputs are
/// short agent names, so the two-row O(n·m) DP is ample.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

// -- ears.clause-syntax (EARS clause grammar, ADR-031) ----------------

/// The specific way a clause failed to parse as EARS (ADR-031 § ESY-003).
/// Each variant maps to a diagnostic that names the defect — never a
/// bare "not EARS".
enum EarsDefect {
    /// No `shall` anywhere — the response is unmarked.
    MissingShall,
    /// A trigger keyword with no comma closing its segment. `keyword`
    /// is the canonical all-caps form for the message.
    MissingTriggerComma { keyword: &'static str },
    /// An all-lowercase pattern keyword (`when`, `if`, `then`, `while`,
    /// `where`) — prose case where a keyword is required.
    LowercaseKeyword { found: String },
    /// Parses as none of the six patterns for a reason the other
    /// variants do not capture (e.g. `IF` without `THEN`).
    Unrecognized,
}

/// Canonical all-caps form of a case-folded EARS keyword, for messages.
fn ears_canonical(folded: &str) -> &'static str {
    match folded {
        "when" => "WHEN",
        "while" => "WHILE",
        "where" => "WHERE",
        "if" => "IF",
        "then" => "THEN",
        _ => unreachable!("ears_canonical called with a non-keyword"),
    }
}

/// Whether a keyword token is in an accepted case: all-caps (`WHEN`) or
/// title case (`When` — the form the EARS originals use). All-lowercase
/// is a defect; any other mixed case is treated as prose, not a keyword
/// (ADR-031 § ESY-003).
fn ears_accepted_case(word: &str) -> bool {
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let rest = chars.as_str();
    first.is_uppercase()
        && (rest.chars().all(char::is_uppercase) || rest.chars().all(char::is_lowercase))
}

/// Parse one clause against the EARS grammar (ADR-031 § ESY-002). The
/// six patterns share a single shape — zero or more keyword-led segments
/// (`WHEN <trigger>,` / `WHILE <state>,` / `WHERE <feature>,` /
/// `IF <condition>, THEN`) followed by the mandatory
/// `the <system> shall <response>` tail — so "complex" (pattern 6) is
/// just more than one segment, and "ubiquitous" (pattern 1) is zero.
fn parse_ears_clause(clause: &str) -> Result<(), EarsDefect> {
    if !shall_regex().is_match(clause) {
        return Err(EarsDefect::MissingShall);
    }
    let mut rest = clause.trim();
    loop {
        let Some(word) = rest.split_whitespace().next() else {
            return Err(EarsDefect::Unrecognized);
        };
        let folded = word.to_lowercase();
        if !matches!(folded.as_str(), "when" | "while" | "where" | "if") {
            // No leading keyword: the remainder is the ubiquitous tail,
            // whose `shall` was checked up front.
            return Ok(());
        }
        if word == folded {
            return Err(EarsDefect::LowercaseKeyword { found: folded });
        }
        if !ears_accepted_case(word) {
            // Mixed case (`wHEN`) is prose, not a keyword — fall through
            // to the tail check.
            return Ok(());
        }
        // Consume `<KEYWORD> <text> ,` — the comma is the parse boundary
        // between trigger and what follows (ESY-003).
        let after_kw = rest[word.len()..].trim_start();
        let Some(comma) = after_kw.find(',') else {
            return Err(EarsDefect::MissingTriggerComma {
                keyword: ears_canonical(&folded),
            });
        };
        rest = after_kw[comma + 1..].trim_start();
        if folded == "if" {
            // The unwanted-behavior pattern requires `THEN` right after
            // the condition's comma.
            let Some(next) = rest.split_whitespace().next() else {
                return Err(EarsDefect::Unrecognized);
            };
            if next.to_lowercase() != "then" {
                return Err(EarsDefect::Unrecognized);
            }
            if next == "then" {
                return Err(EarsDefect::LowercaseKeyword {
                    found: "then".to_owned(),
                });
            }
            if !ears_accepted_case(next) {
                return Ok(());
            }
            rest = rest[next.len()..].trim_start();
        }
    }
}

/// `ears.clause-syntax` (ADR-031): each list item under a `Requirements`
/// heading carrying an `EARS-<NN>`/`EARS-<NN>.<M>` id must parse as one
/// of the six EARS patterns; a malformed clause warns with the named
/// defect (ESY-003). Bullets without an EARS id are skipped, and a
/// document with no `Requirements` heading is silent — heading presence
/// is `core.required-headings`' concern (ESY-004).
///
/// Namespace-agnostic by design: config placement scopes the rule (the
/// per-document dispatch only runs rules a namespace lists), so opting
/// in under any namespace works — no silent no-op (ESY-004 as amended).
/// Default in the agents pack's `[SPEC]` and the project-docs pack's
/// `[PRD]` (ESY-005 as amended).
pub(crate) fn check_ears_clauses(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };
    let lines: Vec<&str> = doc.body.lines().collect();
    let Some((start, limit)) = h2_section_window(ast, lines.len(), "requirements") else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut i = start;
    while i < limit {
        let Some(caps) = list_item_regex().captures(lines[i]) else {
            i += 1;
            continue;
        };
        let item_line = (i + 1) as u32;
        let mut text = caps[1].trim().to_owned();
        // Join continuation lines: a format-on-save wrap puts the tail on
        // a hanging-indent line, and CommonMark lazy continuation allows a
        // non-indented plain line too. A blank line, a new list item, a
        // heading, or a code fence ends the item.
        i += 1;
        while i < limit {
            let cont = lines[i];
            let trimmed = cont.trim_start();
            if cont.trim().is_empty()
                || trimmed.starts_with('#')
                || trimmed.starts_with("```")
                || list_item_regex().is_match(cont)
            {
                break;
            }
            text.push(' ');
            text.push_str(cont.trim());
            i += 1;
        }

        let Some(ears) = ears_item_regex().captures(&text) else {
            continue;
        };
        let id = &ears[1];
        let clause = ears[2].trim();
        let Err(defect) = parse_ears_clause(clause) else {
            continue;
        };
        let (message, help) = match defect {
            EarsDefect::MissingShall => (
                format!("EARS clause {id} has no `shall` — the response is unmarked"),
                "every EARS clause ends `the <system> shall <response>`".to_owned(),
            ),
            EarsDefect::MissingTriggerComma { keyword: "IF" } => (
                format!(
                    "EARS clause {id}: missing comma after the `IF` condition before `THEN`"
                ),
                "the unwanted-behavior pattern is `IF <condition>, THEN the <system> shall <response>`"
                    .to_owned(),
            ),
            EarsDefect::MissingTriggerComma { keyword } => (
                format!("EARS clause {id}: missing comma after the `{keyword}` trigger"),
                format!(
                    "the pattern is `{keyword} <trigger>, the <system> shall <response>`"
                ),
            ),
            EarsDefect::LowercaseKeyword { found } => (
                format!(
                    "EARS clause {id}: lowercase EARS keyword `{found}` — use `{}` or `{}{}`",
                    ears_canonical(&found),
                    found[..1].to_uppercase(),
                    &found[1..],
                ),
                "keywords are accepted in all-caps (`WHEN`) or title case (`When`)".to_owned(),
            ),
            EarsDefect::Unrecognized => (
                format!("EARS clause {id} matches none of the six EARS patterns"),
                "use ubiquitous, event-driven (WHEN), unwanted-behavior (IF/THEN), \
                 state-driven (WHILE), optional-feature (WHERE), or a combination"
                    .to_owned(),
            ),
        };
        out.push(
            Diagnostic::warning(EARS_SYNTAX, doc.location.clone(), item_line, 0, message)
                .with_help(help),
        );
    }
    out
}

// -- SKILLS namespace (SKILL.md files, ADR-023) -----------------------

/// `skills.frontmatter` (PKC-005): SKILL.md MUST have non-empty `name`
/// and `description` frontmatter keys. File-level rule — SKILL.md has
/// no `id:`, so it cannot enter the Document flow without a spurious
/// `IdMissing`; this runs via the same file-level path as `agents.*`.
pub(crate) fn check_skills_frontmatter(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    use crate::frontmatter::{Frontmatter, FrontmatterError};
    let fm = match Frontmatter::parse(&doc.body) {
        Ok(fm) => fm,
        // #3: missing fence needs a distinct message — the fence itself is
        // the problem, not the key values inside it.
        Err(FrontmatterError::MissingFence) => {
            return vec![Diagnostic::error(
                SKILLS_FM,
                doc.location.clone(),
                0,
                0,
                "SKILL.md must have a `---` frontmatter fence with `name:` and `description:`",
            )
            .with_help(
                "add a `---` frontmatter block with `name: <skill-name>` and \
                 `description: <trigger phrase>`",
            )];
        }
        Err(_) => {
            return vec![Diagnostic::error(
                SKILLS_FM,
                doc.location.clone(),
                0,
                0,
                "SKILL.md frontmatter must set a non-empty `name` and `description`",
            )
            .with_help(
                "add `name: <skill-name>` and `description: <trigger phrase>` to the frontmatter",
            )];
        }
    };

    let name_val = fm.metadata.get("name");
    let desc_val = fm.metadata.get("description");

    let name_ok = name_val
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    let desc_ok = desc_val
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());

    if name_ok && desc_ok {
        return Vec::new();
    }

    // #8: if the key exists but is not a string, emit a type-specific message
    // rather than "must set … name" (which implies the key is missing).
    let name_type_wrong = name_val.is_some_and(|v| v.as_str().is_none());
    let desc_type_wrong = desc_val.is_some_and(|v| v.as_str().is_none());
    let message = if name_type_wrong {
        "`name` must be a non-empty string".to_string()
    } else if desc_type_wrong {
        "`description` must be a non-empty string".to_string()
    } else {
        "SKILL.md frontmatter must set a non-empty `name` and `description`".to_string()
    };

    vec![
        Diagnostic::error(SKILLS_FM, doc.location.clone(), 0, 0, message).with_help(
            "add `name: <skill-name>` and `description: <trigger phrase>` to the frontmatter",
        ),
    ]
}

// -- GUIDE namespace (docs/guides/** end-user documentation) -----------

/// `guide.frontmatter`: an end-user guide (`docs/guides/**`) MUST set a
/// non-empty `title` and a `diataxis` object with a non-empty `type` field, and
/// — when the pack supplies a `types` allowlist — `diataxis.type` MUST be one of
/// those values (errors).
///
/// File-level rule: guides carry a `title`/`diataxis.type`, not an `id:`, so the
/// filename is the guide's slug and `core.*` cannot lint them (same reason as
/// [`check_skills_frontmatter`]). This is the Diátaxis-typed counterpart, ADR-055.
///
/// The class lives under a `diataxis` object rather than a top-level `type` key
/// because `type` is reserved by Hugo/Jekyll/Eleventy as a layout selector, which
/// made a lint-clean guide unpublishable as docs-as-code (BUG-015).
///
/// Params (optional):
/// - `types`: array of accepted `diataxis.type` values. Absent → presence-only,
///   no value check. The binary enumerates no taxonomy — the allowlist is
///   config-driven (the `guide` pack ships the Diátaxis four), so it never goes
///   stale against a different doc model.
pub(crate) fn check_guide_frontmatter(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    use crate::frontmatter::{Frontmatter, FrontmatterError};
    let fm = match Frontmatter::parse(&doc.body) {
        Ok(fm) => fm,
        Err(FrontmatterError::MissingFence) => {
            return vec![Diagnostic::error(
                GUIDE_FM,
                doc.location.clone(),
                0,
                0,
                "guide must have a `---` frontmatter fence with `title:` and `diataxis.type:`",
            )
            .with_help(
                "add a `---` frontmatter block with `title: <guide title>` and a \
                 `diataxis:` object whose `type:` is one of \
                 tutorial|how-to|reference|explanation",
            )];
        }
        Err(_) => {
            return vec![Diagnostic::error(
                GUIDE_FM,
                doc.location.clone(),
                0,
                0,
                "guide frontmatter must set a non-empty `title` and `diataxis.type`",
            )
            .with_help(
                "add `title: <guide title>` and a `diataxis:` object whose `type:` is \
                 one of tutorial|how-to|reference|explanation",
            )];
        }
    };

    let mut out = Vec::new();

    let title_ok = fm
        .metadata
        .get("title")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    if !title_ok {
        out.push(
            Diagnostic::error(
                GUIDE_FM,
                doc.location.clone(),
                0,
                0,
                "guide frontmatter must set a non-empty `title`",
            )
            .with_help("add `title: <guide title>` to the frontmatter"),
        );
    }

    let diataxis_type = fm
        .metadata
        .get("diataxis")
        .and_then(|d| d.get("type"))
        .and_then(Value::as_str);

    match diataxis_type {
        Some(t) if !t.trim().is_empty() => {
            // Value check only when the pack pins an allowlist.
            if let Some(allowed) = params.and_then(|p| p.get("types")).and_then(Value::as_array) {
                let allowed: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
                if !allowed.is_empty() && !allowed.contains(&t.trim()) {
                    out.push(
                        Diagnostic::error(
                            GUIDE_FM,
                            doc.location.clone(),
                            0,
                            0,
                            format!("guide `diataxis.type: {t}` is not one of the allowed types"),
                        )
                        .with_help(format!(
                            "set `diataxis.type` to one of: {}",
                            allowed.join(", ")
                        )),
                    );
                }
            }
        }
        _ => {
            out.push(
                Diagnostic::error(
                    GUIDE_FM,
                    doc.location.clone(),
                    0,
                    0,
                    "guide frontmatter must set a non-empty `diataxis.type`",
                )
                .with_help(
                    "add a `diataxis:` object whose `type:` is one of \
                     tutorial|how-to|reference|explanation",
                ),
            );
        }
    }

    out
}

/// `c4.frontmatter`: an architecture-diagram doc (path-claimed at
/// `docs/diagrams/**` by the `c4` pack) MUST set a non-empty `title` and a
/// non-empty `c4.level`, and — when the pack pins a `levels` allowlist — that
/// level MUST be one of the allowed C4 levels. File-level: a diagram doc carries
/// a title and a level, not an `id`, so the filename is its slug. The level
/// lives under a `c4` object, never a top-level `type:`, which SSGs reserve
/// (BUG-015). The binary enumerates no taxonomy — the allowlist is config-only.
/// ADR-075.
pub(crate) fn check_c4_frontmatter(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    use crate::frontmatter::{Frontmatter, FrontmatterError};
    let fm = match Frontmatter::parse(&doc.body) {
        Ok(fm) => fm,
        Err(FrontmatterError::MissingFence) => {
            return vec![Diagnostic::error(
                C4_FM,
                doc.location.clone(),
                0,
                0,
                "C4 diagram must have a `---` frontmatter fence with `title:` and `c4.level:`",
            )
            .with_help(
                "add a `---` frontmatter block with `title: <diagram title>` and a \
                 `c4:` object whose `level:` is one of \
                 context|container|component|code|deployment|dynamic|landscape",
            )];
        }
        Err(_) => {
            return vec![Diagnostic::error(
                C4_FM,
                doc.location.clone(),
                0,
                0,
                "C4 diagram frontmatter must set a non-empty `title` and `c4.level`",
            )
            .with_help(
                "add `title: <diagram title>` and a `c4:` object whose `level:` is \
                 one of context|container|component|code|deployment|dynamic|landscape",
            )];
        }
    };

    let mut out = Vec::new();

    let title_ok = fm
        .metadata
        .get("title")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    if !title_ok {
        out.push(
            Diagnostic::error(
                C4_FM,
                doc.location.clone(),
                0,
                0,
                "C4 diagram frontmatter must set a non-empty `title`",
            )
            .with_help("add `title: <diagram title>` to the frontmatter"),
        );
    }

    let c4_level = fm
        .metadata
        .get("c4")
        .and_then(|c| c.get("level"))
        .and_then(Value::as_str);

    match c4_level {
        Some(l) if !l.trim().is_empty() => {
            // Value check only when the pack pins an allowlist.
            if let Some(allowed) = params.and_then(|p| p.get("levels")).and_then(Value::as_array) {
                let allowed: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
                if !allowed.is_empty() && !allowed.contains(&l.trim()) {
                    out.push(
                        Diagnostic::error(
                            C4_FM,
                            doc.location.clone(),
                            0,
                            0,
                            format!("C4 `c4.level: {l}` is not one of the allowed levels"),
                        )
                        .with_help(format!("set `c4.level` to one of: {}", allowed.join(", "))),
                    );
                }
            }
        }
        _ => {
            out.push(
                Diagnostic::error(
                    C4_FM,
                    doc.location.clone(),
                    0,
                    0,
                    "C4 diagram frontmatter must set a non-empty `c4.level`",
                )
                .with_help(
                    "add a `c4:` object whose `level:` is one of \
                     context|container|component|code|deployment|dynamic|landscape",
                ),
            );
        }
    }

    out
}

// -- MARKETING namespaces (docs/marketing/** by default) -------------------

/// `marketing.frontmatter`: a marketing-strategy doc (CAMPAIGN / PERSONA /
/// POSITIONING / ICP, path-claimed by the `marketing` pack) MAY declare its
/// genre with a nested `marketing.type` field; when it does, that value MUST be
/// one of the pack-supplied `types` allowlist.
///
/// Monotonic opt-in (the `research.type` shape, ADR-093, not the frontmatter-
/// mandatory guide/c4 rules): the field is read from the already-parsed
/// `doc.metadata` (ingest runs once, ADR-029), so an absent or malformed
/// frontmatter is simply "no type" — no finding. This is what lets the one
/// live CAMPAIGN brief, a frontmatter-less placeholder, bind the rule
/// harmlessly. The field lives under a `marketing` object rather than a
/// top-level `type:` because `type` is a reserved layout selector in
/// Hugo/Jekyll/Eleventy (BUG-015) — a nested field a core primitive
/// (`core.allowed-values`, top-level keys only) cannot validate, which is the
/// rule's sole reason to exist.
///
/// Params (optional):
/// - `types`: array of accepted `marketing.type` values. Absent → presence-only
///   (any non-empty value passes). The binary enumerates no vocabulary — the
///   allowlist is config-driven (the `marketing` pack ships the four genres),
///   so it never goes stale against a different doc model.
pub(crate) fn check_marketing_frontmatter(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    // Read the nested discriminator from the parsed metadata (never re-parse).
    // Absent, empty, or non-string → no type declared → monotonic no-op.
    let Some(genre) = doc
        .metadata
        .get("marketing")
        .and_then(|m| m.get("type"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Vec::new();
    };

    // Value check only when the pack pins a non-empty `types` allowlist.
    let Some(allowed) = params.and_then(|p| p.get("types")).and_then(Value::as_array) else {
        return Vec::new();
    };
    let allowed: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
    if allowed.is_empty() || allowed.contains(&genre) {
        return Vec::new();
    }

    vec![Diagnostic::error(
        MARKETING_FM,
        doc.location.clone(),
        0,
        0,
        format!("marketing `marketing.type: {genre}` is not one of the allowed types"),
    )
    .with_help(format!(
        "set `marketing.type` to one of: {}",
        allowed.join(", ")
    ))]
}

// -- writing.ai-fingerprints (AGENTS/CLAUDE/GEMINI instruction files) --------
//
// The deterministic half of AI-writing detection (ADR-102). Scans an
// document's prose for the *mechanically detectable*
// tells whose presence is the signal — curly quotes/apostrophes, decorative
// emoji, em/en-dash density, and a small config list of exact chatbot-artifact
// phrases — and warns (never errors, AIF-001). The semantic tells (tone,
// significance inflation) and the ambiguous technical words (`seam`,
// `load-bearing`) stay with the `writing-humanizer` judgment pass (AIF-002).
//
// Fingerprints inside code are masked: a curly quote in a shell example or an
// emoji in a sample is code, not prose (AIF-001). Masking is a per-hit interval
// test against the parsed AST's code ranges — fenced/indented `code_blocks` mask
// whole lines, `inline_code_spans` mask byte-column ranges — not a precomputed
// flag. Columns are 1-indexed byte offsets, matching `byte_to_line_col` and the
// rest of the reporter. When the AST is absent the maskable classes are skipped
// (skip-don't-scan) so masking is never silently dropped; phrase matching still
// runs over the raw body.

const CURLY_DEFAULT: bool = true;
const EMOJI_DEFAULT: bool = true;
const DASH_DENSITY_DEFAULT: u64 = 4;
/// Compiled default phrase list (AIF-005): small and unambiguous — the two
/// clearest chatbot artifacts. Overridable via the `phrases` config param; an
/// explicit `[]` disables the phrase class. Deliberately excludes `seam` /
/// `load-bearing` (AIF-002) and borderline strings like "let me know if".
const PHRASES_DEFAULT: &[&str] = &["you're absolutely right", "i hope this helps"];

fn is_curly_quote(c: char) -> bool {
    matches!(c, '\u{2018}' | '\u{2019}' | '\u{201C}' | '\u{201D}')
}

/// True for the clearly-decorative pictograph blocks (emoji). Conservative by
/// design: excludes the arrow zones that legitimately appear in prose (dingbat
/// arrows U+2794..=U+27BF and the arrows/stars block U+2B00..=U+2BFF — so `⭐`
/// and `➕` are not flagged either, an accepted cost of dropping the arrows),
/// and never matches the dash/quote scalars handled by their own classes.
fn is_decorative_emoji(c: char) -> bool {
    matches!(c as u32,
        0x1F300..=0x1FAFF   // pictographs, emoticons, transport, supplemental, extended-A
        | 0x1F1E6..=0x1F1FF // regional-indicator flags
        | 0x2600..=0x2793)  // misc symbols + dingbats (checkmark, cross, sun), below the arrows
}

/// Zero-width joiner / emoji variation selector — the scalars that stitch a
/// single visible emoji out of several code points (flags, ZWJ sequences,
/// skin-tone/VS16 forms). Used only to coalesce an emoji run into one finding.
fn is_emoji_joiner(c: char) -> bool {
    matches!(c, '\u{200D}' | '\u{FE0F}')
}

pub(crate) fn check_ai_fingerprints(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    // Curly/emoji/dash are maskable classes: they need the parsed AST to mask
    // code spans. Without it, skip them (AIF-001 fallback — skip, don't scan
    // unmasked); phrases still run over the raw body below.
    let ast = doc.ast.as_ref();
    let flag_curly = ast.is_some() && bool_param(params, "flag_curly_quotes", CURLY_DEFAULT);
    let flag_emoji = ast.is_some() && bool_param(params, "flag_emoji", EMOJI_DEFAULT);
    let dash_threshold = if ast.is_some() {
        u64_param(params, "max_em_dashes_per_kwords", DASH_DENSITY_DEFAULT)
    } else {
        0
    };
    let phrases = phrases_param(params);
    // YAML frontmatter is metadata, not prose, and the AST does not model it —
    // skip its lines so a curly apostrophe or em-dash in a title is not flagged
    // and does not pad the density denominator.
    let frontmatter_end = frontmatter_end_line(&doc.body);

    let mut out = Vec::new();
    let mut dash_count: u64 = 0;
    let mut word_count: u64 = 0;
    let mut first_dash: Option<(u32, u32)> = None;

    for (line_idx, raw_line) in doc.body.lines().enumerate() {
        let line_no = u32::try_from(line_idx + 1).unwrap_or(u32::MAX);
        if line_no <= frontmatter_end || in_code_block(doc, line_no) {
            continue; // frontmatter or a fenced/indented code line — not prose
        }
        // Mask inline `code` by replacing its bytes with spaces (length-
        // preserving → columns stay exact) via the shared `in_inline_code`
        // predicate — a per-char test, never a byte-slice, so a multiline
        // backtick span can never invert a range and panic.
        let masked = mask_inline_code(doc, line_no, raw_line);
        let line: &str = &masked;
        // Density word count splits on whitespace *and* dashes so a no-space
        // em-dash (`word—word`, standard typography) counts as two words rather
        // than collapsing into one and inflating the density (AIF-004).
        word_count += line
            .split(|c: char| c.is_whitespace() || is_dash(c))
            .filter(|s| !s.is_empty())
            .count() as u64;
        for phrase in &phrases {
            for byte_off in find_all_ci(line, phrase) {
                let col = u32::try_from(byte_off + 1).unwrap_or(u32::MAX);
                out.push(Diagnostic::warning(
                    AI_FINGERPRINTS,
                    doc.location.clone(),
                    line_no,
                    col,
                    format!("chatbot-artifact phrase \"{phrase}\" — an AI-writing fingerprint"),
                ));
            }
        }
        // `in_emoji_run` coalesces a multi-scalar glyph (flag, ZWJ sequence,
        // skin-tone form) into one finding instead of one per code point.
        let mut in_emoji_run = false;
        for (byte_off, c) in line.char_indices() {
            let col = u32::try_from(byte_off + 1).unwrap_or(u32::MAX);
            if flag_curly && is_curly_quote(c) {
                in_emoji_run = false;
                out.push(
                    Diagnostic::warning(
                        AI_FINGERPRINTS,
                        doc.location.clone(),
                        line_no,
                        col,
                        format!(
                            "curly '{c}' (U+{:04X}) — an AI-writing fingerprint",
                            c as u32
                        ),
                    )
                    .with_help("use the straight ASCII quote/apostrophe"),
                );
            } else if flag_emoji && is_decorative_emoji(c) {
                if !in_emoji_run {
                    out.push(
                        Diagnostic::warning(
                            AI_FINGERPRINTS,
                            doc.location.clone(),
                            line_no,
                            col,
                            format!(
                                "decorative emoji '{c}' (U+{:04X}) — an AI-writing fingerprint",
                                c as u32
                            ),
                        )
                        .with_help("remove the emoji from prose"),
                    );
                }
                in_emoji_run = true;
            } else if in_emoji_run && is_emoji_joiner(c) {
                // ZWJ / VS16 between emoji scalars — stay in the run, emit nothing.
            } else {
                in_emoji_run = false;
                if dash_threshold > 0 && is_dash(c) {
                    dash_count += 1;
                    first_dash.get_or_insert((line_no, col));
                }
            }
        }
    }

    // Density is a whole-file metric (AIF-004, the one soft/heuristic class):
    // dashes per 1000 prose words, anchored at the first offending dash.
    if let Some((line_no, col)) = first_dash {
        let density = dash_count.saturating_mul(1000) / word_count.max(1);
        if density > dash_threshold {
            out.push(Diagnostic::warning(
                AI_FINGERPRINTS,
                doc.location.clone(),
                line_no,
                col,
                format!(
                    "em/en-dash density {density} per 1000 words exceeds {dash_threshold} \
                     — an AI-writing fingerprint (soft signal; set \
                     max_em_dashes_per_kwords=0 to disable)"
                ),
            ));
        }
    }
    out
}

fn is_dash(c: char) -> bool {
    matches!(c, '\u{2014}' | '\u{2013}')
}

/// Return `line` with every character that `in_inline_code` reports as inside a
/// backtick span replaced by ASCII spaces. Each character is tested at its own
/// 1-indexed byte column and replaced by `len_utf8()` spaces, so byte length
/// (and therefore every column) is preserved and the result is always valid
/// UTF-8. This is a per-character test rather than a byte-slice, so a multiline
/// inline span whose `col_end` lands on a later line can never invert a range
/// and panic — it simply masks whatever this line's columns fall in range.
fn mask_inline_code(doc: &Document, line_no: u32, line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for (byte_off, c) in line.char_indices() {
        let col = u32::try_from(byte_off + 1).unwrap_or(u32::MAX);
        if in_inline_code(doc, line_no, col) {
            for _ in 0..c.len_utf8() {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The 1-indexed line number of the closing `---` of a leading YAML frontmatter
/// block, or `0` when the body has no frontmatter (or an unterminated one).
/// Lines `1..=frontmatter_end` are metadata, not prose.
fn frontmatter_end_line(body: &str) -> u32 {
    let mut lines = body.lines();
    if lines.next().map(str::trim) != Some("---") {
        return 0;
    }
    for (idx, line) in lines.enumerate() {
        if line.trim() == "---" {
            // idx is 0-based from the *second* line, so the closing fence is at
            // line number idx + 2.
            return u32::try_from(idx + 2).unwrap_or(u32::MAX);
        }
    }
    0
}

/// Read a boolean config param, falling back to `default` when unset or the
/// wrong type.
fn bool_param(params: Option<&Value>, key: &str, default: bool) -> bool {
    params
        .and_then(|p| p.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

/// Read a non-negative integer config param, falling back to `default` when
/// unset or the wrong type.
fn u64_param(params: Option<&Value>, key: &str, default: u64) -> u64 {
    params
        .and_then(|p| p.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(default)
}

/// The lowercased phrase list: the config `phrases` array when set (an explicit
/// `[]` disables the class), else the compiled minimal default (AIF-005).
fn phrases_param(params: Option<&Value>) -> Vec<String> {
    match params.and_then(|p| p.get("phrases")).and_then(Value::as_array) {
        Some(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
        None => PHRASES_DEFAULT.iter().map(|s| s.to_string()).collect(),
    }
}

/// Byte offsets of every ASCII-case-insensitive occurrence of `needle` (already
/// lowercased) in `haystack`. Byte-exact, so the reported column stays correct
/// even when the line contains multibyte text before the match — an ASCII byte
/// never appears as a UTF-8 continuation byte, so a match can't start mid-char.
fn find_all_ci(haystack: &str, needle: &str) -> Vec<usize> {
    let (hb, nb) = (haystack.as_bytes(), needle.as_bytes());
    let mut hits = Vec::new();
    if nb.is_empty() || nb.len() > hb.len() {
        return hits;
    }
    let mut i = 0;
    while i + nb.len() <= hb.len() {
        if hb[i..i + nb.len()].eq_ignore_ascii_case(nb) {
            hits.push(i);
            i += nb.len();
        } else {
            i += 1;
        }
    }
    hits
}

// -- RESEARCH namespace (docs/research/**) ---------------------------------
//
// A deep-research report is an id-less, path-claimed synthesis document. The
// genre's trust signal is not link density (the surveyed corpus has zero
// markdown links — citations are deep-research cite-tokens) but disclosed
// provenance and, secondarily, disclosed uncertainty. `research.evidence` is a
// pure normalized-heading walk over the H2/H3 headings (no filesystem, no body
// re-parse, ADR-029): an evidence/sources section is standard across academic /
// market / AI-report genres so its absence is an error, while a dedicated
// limitations/data-gaps section is good practice but not an industry convention
// so its absence is a configurable warning. The optional `research.type`
// frontmatter field routes a report into its genre's IMRaD/market/AI skeleton —
// but only ever *adds* warnings (monotonic opt-in, RSR-005). ADR-093.

/// True when some entry in `headings` (already normalized: lowercase, trimmed,
/// trailing colon dropped) *contains* any token in `tokens` (compared
/// case-insensitively). Empty tokens are ignored, so a whitespace-only synonym
/// never matches everything.
fn heading_matches_any(headings: &[String], tokens: &[String]) -> bool {
    tokens.iter().any(|tok| {
        let t = tok.trim().to_lowercase();
        !t.is_empty() && headings.iter().any(|h| h.contains(&t))
    })
}

/// The configured synonym list for a `research.evidence` heading param, or the
/// supplied default when the param is unset. An explicit `[]` disables that
/// half (returns an empty list — never the default), per RSR-002.
fn research_headings_param(params: Option<&Value>, key: &str, default: &[&str]) -> Vec<String> {
    match params.and_then(|p| p.get(key)).and_then(Value::as_array) {
        Some(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.to_string())
            .collect(),
        None => default.iter().map(|s| s.to_string()).collect(),
    }
}

/// The skeleton heading-token groups a valid `research.type` genre additionally
/// requires (RSR-005). Each inner group is satisfied when some heading contains
/// any of its tokens; a missing group is one warning. An unknown genre yields no
/// groups (its invalidity is reported separately as an error).
fn research_genre_skeleton(genre: &str) -> &'static [&'static [&'static str]] {
    match genre {
        // IMRaD.
        "academic" => &[&["method"], &["result"]],
        "market" => &[&["methodology"], &["recommendation"]],
        "deep-research" => &[&["summary"], &["conclusion"]],
        _ => &[],
    }
}

/// `research.evidence` (ADR-093 § RSR-002/RSR-005): a `RESEARCH`-claimed report
/// (`docs/research/**`, id-less path-claim) must carry an evidence/sources
/// section (missing → error) and, by default, a limitations/data-gaps section
/// (missing → warning, promotable via `severity`). Both halves are a `contains`
/// walk over the normalized H2/H3 headings against a configurable synonym set
/// (`evidence_headings` / `gaps_headings`; `[]` disables a half). File-level: a
/// report carries no `id`, so the filename is its slug.
///
/// An optional `research.type` frontmatter field (nested under a `research`
/// object, not a top-level `type:` which SSGs reserve, BUG-015) is a monotonic
/// opt-in: absent → the baseline only; present-and-invalid → one error; present
/// and valid → the baseline *plus* one warning per missing genre-skeleton
/// heading. It only ever adds findings — it never unlocks a passing state. When
/// the rule fires and no type is set, each diagnostic carries a note advertising
/// the field (discovery only at the moment attention exists).
pub(crate) fn check_research_evidence(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };

    // The section corpus: normalized H2 *and* H3 heading texts (RSR-002).
    let headings: Vec<String> = ast
        .headings
        .iter()
        .filter(|h| h.level == 2 || h.level == 3)
        .map(|h| normalize_heading(&h.text))
        .collect();

    // Anchor every diagnostic at the report's title (first H1), falling back to
    // an unanchored (0,0) position when the report has no H1 — the same
    // (0,0)-fallback convention `core.required-headings` uses.
    let title_line = ast
        .headings
        .iter()
        .find(|h| h.level == 1)
        .map(|h| h.line)
        .unwrap_or(0);

    let evidence_tokens = research_headings_param(params, "evidence_headings", DEFAULT_EVIDENCE_HEADINGS);
    let gaps_tokens = research_headings_param(params, "gaps_headings", DEFAULT_GAPS_HEADINGS);
    // `severity` governs the gaps half only; the evidence half is always an
    // error. Default warning; only `error` promotes it.
    let gaps_is_error = params
        .and_then(|p| p.get("severity"))
        .and_then(Value::as_str)
        .map(str::trim)
        == Some("error");

    let mut out = Vec::new();

    // Evidence half — always an error when enabled and no heading matches.
    if !evidence_tokens.is_empty() && !heading_matches_any(&headings, &evidence_tokens) {
        out.push(
            Diagnostic::error(
                RESEARCH_EVIDENCE,
                doc.location.clone(),
                title_line,
                0,
                "research report has no evidence/sources section",
            )
            .with_help("add an evidence/sources section, e.g. `## Evidence appendix`"),
        );
    }

    // Gaps half — default warning, promotable to error via `severity`.
    if !gaps_tokens.is_empty() && !heading_matches_any(&headings, &gaps_tokens) {
        let ctor = if gaps_is_error {
            Diagnostic::error
        } else {
            Diagnostic::warning
        };
        out.push(
            ctor(
                RESEARCH_EVIDENCE,
                doc.location.clone(),
                title_line,
                0,
                "research report has no limitations/data-gaps section",
            )
            .with_help("add a limitations/data-gaps section, or set severity/gaps_headings to tune"),
        );
    }

    // Optional per-genre routing (RSR-005). Nested under a `research` object,
    // never a top-level `type:` (BUG-015). Read from the parsed metadata, so an
    // absent or malformed frontmatter is simply "no type" — no error, unlike the
    // frontmatter-mandatory guide/c4 rules.
    let research_type = doc
        .metadata
        .get("research")
        .and_then(|r| r.get("type"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match research_type {
        Some(genre) if RESEARCH_TYPES.contains(&genre) => {
            // Valid genre: additionally require its skeleton headings. Each
            // missing group is one warning — monotonic (only ever adds).
            for group in research_genre_skeleton(genre) {
                let group: Vec<String> = group.iter().map(|s| s.to_string()).collect();
                if !heading_matches_any(&headings, &group) {
                    let label = group.join("/");
                    out.push(
                        Diagnostic::warning(
                            RESEARCH_EVIDENCE,
                            doc.location.clone(),
                            title_line,
                            0,
                            format!(
                                "`research.type: {genre}` report is missing a `{label}` skeleton section"
                            ),
                        )
                        .with_help(format!(
                            "add a heading containing `{label}` (the {genre} report skeleton)"
                        )),
                    );
                }
            }
        }
        Some(genre) => {
            // Present but outside the closed vocabulary → one error.
            out.push(
                Diagnostic::error(
                    RESEARCH_EVIDENCE,
                    doc.location.clone(),
                    title_line,
                    0,
                    format!("`research.type: {genre}` is not a valid research genre"),
                )
                .with_help(format!(
                    "set `research.type` to one of: {}",
                    RESEARCH_TYPES.join(", ")
                )),
            );
        }
        None => {}
    }

    // On-fire discovery (RSR-005 hook 2): when the rule fires and no type is
    // set, advertise the optional field on every emitted diagnostic — teaching
    // at the moment attention exists, never on a clean report.
    if research_type.is_none() && !out.is_empty() {
        out = out
            .into_iter()
            .map(|d| {
                d.with_note(
                    "Optionally set `research.type: academic|market|deep-research` in \
                     frontmatter to also check that genre's skeleton — adding it only adds checks.",
                )
            })
            .collect();
    }

    out
}

// -- CHECKLIST namespace (docs/checklists/**) + core.required-headings -----
//
// A checklist is an id-less, path-claimed doc with a two-state lifecycle
// (`status: living` → `sealed`). `checklist.structure` is the always-on shape
// check; `checklist.complete` and `checklist.pinned` fire only once sealed, so
// a template or an in-flight instance is never gated. `core.required-headings`
// is the generic, config-driven "these H2 sections must exist" rule the
// checklist pack binds (it is not checklist-specific). ADR-078.

/// A checkbox list item — checked or unchecked — anchored to line start. The
/// three CommonMark task-list markers (`-`, `*`, `+`) are accepted so the rule
/// does not silently miss a `+ [ ]` box.
fn checklist_box_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*[-*+]\s+\[[ xX]\]").expect("valid regex"))
}

/// An *unchecked* checkbox list item (`- [ ]`). Only `[ ]` blocks a seal;
/// `[x]`/`[X]` are done and any other bracket content is not a task item.
fn checklist_unchecked_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[-*+]\s+\[ \]").expect("valid regex"))
}

/// `checklist.structure`: a `docs/checklists/**` doc MUST have a `---`
/// frontmatter fence carrying a non-empty `title`, a `status` of exactly
/// `living` or `sealed`, a `pinned_commit` when `sealed`, and at least one
/// checkbox in the body. File-level: a checklist carries a title/status, not an
/// `id`, so the filename is its slug (ADR-078 § CHK-002).
pub(crate) fn check_checklist_structure(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    use crate::frontmatter::{Frontmatter, FrontmatterError};
    let fm = match Frontmatter::parse(&doc.body) {
        Ok(fm) => fm,
        Err(FrontmatterError::MissingFence) => {
            return vec![Diagnostic::error(
                CHECKLIST_STRUCTURE,
                doc.location.clone(),
                0,
                0,
                "checklist must have a `---` frontmatter fence with `title:` and `status:`",
            )
            .with_help(
                "add a `---` frontmatter block with `title: <checklist title>` and \
                 `status: living` (or `sealed` with a `pinned_commit:`)",
            )];
        }
        Err(_) => {
            return vec![Diagnostic::error(
                CHECKLIST_STRUCTURE,
                doc.location.clone(),
                0,
                0,
                "checklist frontmatter must set a non-empty `title` and a `status`",
            )
            .with_help("set `title:` and `status: living` | `sealed`")];
        }
    };

    let mut out = Vec::new();

    let title_ok = fm
        .metadata
        .get("title")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    if !title_ok {
        out.push(
            Diagnostic::error(
                CHECKLIST_STRUCTURE,
                doc.location.clone(),
                doc.frontmatter_lines.get("title").copied().unwrap_or(0),
                0,
                "checklist frontmatter must set a non-empty `title`",
            )
            .with_help("add `title: <checklist title>` to the frontmatter"),
        );
    }

    let status = fm.metadata.get("status").and_then(Value::as_str).map(str::trim);
    let status_line = doc.frontmatter_lines.get("status").copied().unwrap_or(0);
    match status {
        Some("living") | Some("sealed") => {}
        Some(other) => out.push(
            Diagnostic::error(
                CHECKLIST_STRUCTURE,
                doc.location.clone(),
                status_line,
                0,
                format!("checklist `status: {other}` must be `living` or `sealed`"),
            )
            .with_help("set `status: living` (in progress) or `status: sealed` (signed off)"),
        ),
        None => out.push(
            Diagnostic::error(
                CHECKLIST_STRUCTURE,
                doc.location.clone(),
                0,
                0,
                "checklist frontmatter must set `status: living` or `status: sealed`",
            )
            .with_help("add `status: living` while filling it in; `sealed` at sign-off"),
        ),
    }

    if status == Some("sealed") {
        let pin_ok = fm
            .metadata
            .get("pinned_commit")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !pin_ok {
            out.push(
                Diagnostic::error(
                    CHECKLIST_STRUCTURE,
                    doc.location.clone(),
                    status_line,
                    0,
                    "a `sealed` checklist must set `pinned_commit` to the integration commit",
                )
                .with_help("add `pinned_commit: <40-hex commit SHA>` or revert to `status: living`"),
            );
        }
    }

    if !checklist_box_regex().is_match(&doc.body) {
        out.push(
            Diagnostic::error(
                CHECKLIST_STRUCTURE,
                doc.location.clone(),
                0,
                0,
                "checklist body has no checkbox items (`- [ ]` / `- [x]`)",
            )
            .with_help("a checklist must list at least one `- [ ]` item"),
        );
    }

    out
}

/// `checklist.complete`: when (and only when) `status: sealed`, every remaining
/// unchecked box (`- [ ]`) is an error. No-op while `living`. Only `[x]`/`[X]`
/// count as done (ADR-078 § CHK-003).
pub(crate) fn check_checklist_complete(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    use crate::frontmatter::Frontmatter;
    let Ok(fm) = Frontmatter::parse(&doc.body) else {
        return Vec::new();
    };
    if fm.metadata.get("status").and_then(Value::as_str).map(str::trim) != Some("sealed") {
        return Vec::new();
    }

    let mut out = Vec::new();
    for (i, line) in doc.body.lines().enumerate() {
        if checklist_unchecked_regex().is_match(line) {
            out.push(
                Diagnostic::error(
                    CHECKLIST_COMPLETE,
                    doc.location.clone(),
                    (i + 1) as u32,
                    0,
                    "unchecked item in a `sealed` checklist",
                )
                .with_help("check it (`- [x]`) or set `status: living` until the work lands"),
            );
        }
    }
    out
}

/// `checklist.pinned`: when (and only when) `status: sealed`, the
/// `pinned_commit` frontmatter SHA must be 40-hex, resolve to a commit in the
/// repo, and be an ancestor of `HEAD`. Degrades to a warning (never a hard
/// error) outside a usable git history — not a repo, no git, or a shallow clone
/// missing the object (ADR-078 § CHK-004).
pub(crate) fn check_checklist_pinned(
    doc: &Document,
    _params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    use crate::frontmatter::Frontmatter;
    let Ok(fm) = Frontmatter::parse(&doc.body) else {
        return Vec::new();
    };
    if fm.metadata.get("status").and_then(Value::as_str).map(str::trim) != Some("sealed") {
        return Vec::new();
    }
    // A missing/empty pin is `checklist.structure`'s concern, not this rule's.
    let sha = match fm.metadata.get("pinned_commit").and_then(Value::as_str).map(str::trim) {
        Some(s) if !s.is_empty() => s,
        _ => return Vec::new(),
    };
    let line = doc.frontmatter_lines.get("pinned_commit").copied().unwrap_or(0);

    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return vec![Diagnostic::error(
            CHECKLIST_PINNED,
            doc.location.clone(),
            line,
            0,
            format!("`pinned_commit: {sha}` is not a 40-character hex commit SHA"),
        )
        .with_help("pin to the full 40-character SHA of the integration commit")];
    }

    if !git_repo_available(root) {
        return vec![Diagnostic::warning(
            CHECKLIST_PINNED,
            doc.location.clone(),
            line,
            0,
            "cannot verify `pinned_commit`: not a git repository or git unavailable",
        )
        .with_help("run ctxgrd inside the repo so the pin can be checked")];
    }

    if !git_object_exists(root, sha) {
        if git_is_shallow(root) {
            return vec![Diagnostic::warning(
                CHECKLIST_PINNED,
                doc.location.clone(),
                line,
                0,
                format!("cannot verify `pinned_commit` {sha} in a shallow clone"),
            )
            .with_help("fetch full history (`fetch-depth: 0` in CI) so the pin can be checked")];
        }
        return vec![Diagnostic::error(
            CHECKLIST_PINNED,
            doc.location.clone(),
            line,
            0,
            format!("`pinned_commit` {sha} does not resolve to a commit in this repository"),
        )
        .with_help("pin to a commit that exists in this repo")];
    }

    match git_is_ancestor(root, sha) {
        Some(Some(0)) => Vec::new(),
        Some(Some(1)) => vec![Diagnostic::error(
            CHECKLIST_PINNED,
            doc.location.clone(),
            line,
            0,
            format!("`pinned_commit` {sha} is not an ancestor of HEAD — the integration has not landed"),
        )
        .with_help("seal to a commit that is merged into this line of history, or re-seal after it lands")],
        _ => vec![Diagnostic::warning(
            CHECKLIST_PINNED,
            doc.location.clone(),
            line,
            0,
            format!("could not determine whether `pinned_commit` {sha} is an ancestor of HEAD"),
        )
        .with_help("ensure git history is available so the pin can be checked")],
    }
}

// -- test.completion (TEST completion report, ADR-098 § QA-003) --------

/// A commit-SHA *shape*: exactly 40 hex digits (a full SHA-1), the same shape
/// `checklist.pinned` requires — ADR-098 clones its pin logic. Shape only;
/// reachability against git is out of scope in v1.
fn is_commit_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether the H2 section named `heading` exists and holds at least one
/// non-blank content line before the next H2. Reuses the shared
/// `h2_section_window` idiom (the acceptance-complete / tasks / ears
/// rule-of-three). A missing section counts as empty.
fn section_is_nonempty(doc: &Document, heading: &str) -> bool {
    let Some(ast) = doc.ast.as_ref() else {
        return false;
    };
    let name = normalize_heading(heading);
    let lines: Vec<&str> = doc.body.lines().collect();
    let Some((start, end)) = h2_section_window(ast, lines.len(), &name) else {
        return false;
    };
    lines[start..end].iter().any(|l| !l.trim().is_empty())
}

/// `test.completion` (ADR-098 § QA-003): the two invariants a *sealed* Test
/// Completion Report must satisfy that the core presence rules cannot express.
/// Acts only on `status: sealed`; a draft carries no pins yet and is silent.
///
/// 1. A sealed record carries `tested_commit` (the tree the suite ran against)
///    and `spec_commit` (the revision of the linked contract verified), each a
///    40-hex commit SHA. Shape only — like `checklist.pinned` it does not
///    resolve the SHA against git (ADR-098 rejects git-verified pins in v1).
/// 2. When `result: conditional-pass`, the `## Outstanding Defects` section
///    must be non-empty — a waiver of open defects must name what it waives, or
///    the honest verdict is `pass`.
///
/// Document-level: the `[TEST]` namespace is id-claimed (`id: TEST-<N>`), so
/// the report is an id-keyed [`Document`] linted in the per-document loop.
pub(crate) fn check_test_completion(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    // Only a sealed report is gated; a draft is still being written and carries
    // no pins by design (ADR-098 § QA-003).
    if doc.metadata.get("status").and_then(Value::as_str).map(str::trim) != Some("sealed") {
        return Vec::new();
    }

    let mut out = Vec::new();

    // (1) The two commit-SHA pins — presence AND shape, per pin.
    for key in ["tested_commit", "spec_commit"] {
        let line = doc
            .frontmatter_lines
            .get(key)
            .or_else(|| doc.frontmatter_lines.get("status"))
            .copied()
            .unwrap_or(0);
        match doc.metadata.get(key).and_then(Value::as_str).map(str::trim) {
            Some(sha) if !sha.is_empty() => {
                if !is_commit_sha(sha) {
                    out.push(
                        Diagnostic::error(
                            TEST_COMPLETION,
                            doc.location.clone(),
                            line,
                            0,
                            format!(
                                "{}: `{key}: {sha}` is not a 40-character hex commit SHA",
                                doc.raw_id
                            ),
                        )
                        .with_help(format!(
                            "pin `{key}` to the full 40-character SHA the sealed report was taken at"
                        )),
                    );
                }
            }
            _ => out.push(
                Diagnostic::error(
                    TEST_COMPLETION,
                    doc.location.clone(),
                    line,
                    0,
                    format!(
                        "{}: a sealed completion report must set `{key}` to a commit SHA",
                        doc.raw_id
                    ),
                )
                .with_help(if key == "tested_commit" {
                    "add `tested_commit: <40-hex SHA>` — the tree the suite ran against"
                } else {
                    "add `spec_commit: <40-hex SHA>` — the revision of the contract it verified"
                }),
            ),
        }
    }

    // (2) A conditional-pass verdict is a waiver — it must name its defects.
    let result = doc.metadata.get("result").and_then(Value::as_str).map(str::trim);
    if result == Some("conditional-pass") && !section_is_nonempty(doc, "Outstanding Defects") {
        let line = doc.frontmatter_lines.get("result").copied().unwrap_or(0);
        out.push(
            Diagnostic::error(
                TEST_COMPLETION,
                doc.location.clone(),
                line,
                0,
                format!(
                    "{}: `result: conditional-pass` but the `Outstanding Defects` section is \
                     empty — a waiver must name what it waives",
                    doc.raw_id
                ),
            )
            .with_help(
                "list the waived defects under `## Outstanding Defects`, or set `result: pass` \
                 if there are none",
            ),
        );
    }

    out
}

/// `core.required-headings`: each heading named in the `headings` config param
/// must appear as an H2. Matching is normalized — a leading enumerator (`1.`,
/// `1)`, `A.`) is stripped and comparison is case-insensitive — so config
/// `"Plan / account structure"` matches a `## 1. Plan / account structure`
/// heading. Presence, not order; extra headings allowed. No-op when `headings`
/// is unset (ADR-078 § CHK-005).
pub(crate) fn check_required_headings(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(required) = params.and_then(|p| p.get("headings")).and_then(Value::as_array) else {
        return Vec::new();
    };
    let required: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
    if required.is_empty() {
        return Vec::new();
    }
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };
    let present: Vec<String> = ast
        .headings
        .iter()
        .filter(|h| h.level == 2)
        .map(|h| normalize_required_heading(&h.text))
        .collect();

    let mut out = Vec::new();
    for want in required {
        let want_norm = normalize_required_heading(want);
        if want_norm.is_empty() || present.iter().any(|p| p == &want_norm) {
            continue;
        }
        out.push(
            Diagnostic::error(
                REQUIRED_HEADINGS,
                doc.location.clone(),
                0,
                0,
                format!("required H2 heading `{want}` is missing"),
            )
            .with_help(format!("add a `## {want}` section")),
        );
    }
    out
}

/// `core.required-anchors`: a document's body must contain every marker string
/// named in its `anchors` config param. Generic and config-driven — the binary
/// enumerates no anchors; a checklist supplies its `@stripe.*` anchors, another
/// namespace its own markers. Substring match on the raw body, so it is
/// convention-agnostic (HTML-comment anchors `<!-- @pack.rule -->`, or any
/// stable token). File-level so it runs on id-less path-claimed docs like
/// docs/checklists/** (ADR-078: the generic enabler for the deferred
/// vendor-specific structure rules).
pub(crate) fn check_required_anchors(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(required) = params.and_then(|p| p.get("anchors")).and_then(Value::as_array) else {
        return Vec::new();
    };
    let required: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
    if required.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for want in required {
        if want.is_empty() || doc.body.contains(want) {
            continue;
        }
        out.push(
            Diagnostic::error(
                REQUIRED_ANCHORS,
                doc.location.clone(),
                0,
                0,
                format!("required anchor `{want}` is missing"),
            )
            .with_help(format!("add the `{want}` anchor to the item it governs")),
        );
    }
    out
}

/// `core.file-budget` (ADR-109 § BDG-001): the document stays under its
/// character budget.
///
/// Generic and namespace-agnostic — a TODO.md, an ADR, a runbook. The unit
/// is characters because that is the unit the readers imposing a ceiling
/// report in (Claude Code warns at 150 000 characters on a file it loads).
/// Counted over the full document text the source handed the kernel,
/// frontmatter included: the rule measures the file a reader loads, not the
/// prose that survives parsing.
///
/// Warning, never an error — an over-budget file is a cost, not a
/// structural defect.
///
/// Dual-dispatch (BDG-003): registered `Level::File` so it runs on id-less
/// path-claimed singletons, and dispatched again from `run.rs` step 6 for
/// id-keyed documents. Both paths call *this* function, so one rule code
/// never means two semantics — the defect BUG-021 records.
pub(crate) fn check_file_budget(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let max_chars = params
        .and_then(|p| p.get("max_chars"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_CHARS);
    let chars = doc.body.chars().count() as u64;
    if chars <= max_chars {
        return Vec::new();
    }
    vec![
        Diagnostic::warning(
            FILE_BUDGET,
            doc.location.clone(),
            0,
            0,
            format!("file is {chars} characters (budget {max_chars})"),
        )
        .with_help(budget_help(doc, chars - max_chars, chars))
        .with_note(format!(
            "raise the ceiling with `[{}.\"{FILE_BUDGET}\"] max_chars = <n>` if this size is intended",
            doc.id.namespace
        )),
    ]
}

/// The `help:` line for an over-budget file: how much has to go, and where
/// the bulk is. The candidate is whichever weighs more — the largest H2
/// section, or the preamble above the first H2 — so the suggestion names
/// the actual bulk rather than the biggest *named* thing. Naming it turns
/// "this file is too big" into one concrete edit, which is the whole point
/// of the diagnostic.
fn budget_help(doc: &Document, over: u64, total: u64) -> String {
    let pct = |n: u64| n.saturating_mul(100) / total.max(1);
    let preamble = chars_before_first_h2(doc);
    let section = largest_section(doc);

    // The preamble wins when it outweighs every section (a state file whose
    // bulk is dated narrative above the first `##`). It has no heading to
    // move, so the advice is structural, not "move this section".
    if section.is_none_or(|(_, size)| preamble > size) && preamble > 0 {
        return format!(
            "trim {over} characters — {preamble} characters ({pct}% of the file) sit above the \
             first `## ` heading, where no section owns them: give that text sections and split \
             the settled parts out, or archive it",
            pct = pct(preamble),
        );
    }
    match section {
        Some((title, size)) => format!(
            "trim {over} characters — the largest section is `## {title}` ({size} characters, \
             {pct}% of the file): move it into a linked file, or archive it if it is settled",
            pct = pct(size),
        ),
        None => format!(
            "trim {over} characters — move settled detail into a linked file and reference it, \
             or archive the parts that are done"
        ),
    }
}

/// Characters from the top of the document to its first H2 (the preamble
/// no `## ` section owns). `0` when the document has no AST or opens on an
/// H2. Line-summed the same way [`largest_section`] measures a section, so
/// the two figures are comparable.
fn chars_before_first_h2(doc: &Document) -> u64 {
    let Some(ast) = doc.ast.as_ref() else {
        return 0;
    };
    let Some(first) = ast.headings.iter().find(|h| h.level == 2) else {
        return 0;
    };
    doc.body
        .lines()
        .take((first.line as usize).saturating_sub(1))
        .map(|line| line.chars().count() as u64 + 1)
        .sum()
}

/// The largest H2 section of `doc` as `(heading text, characters)`.
///
/// A section runs from its own `## ` line to the next H2 (nested H3s count
/// inside it, which is what makes the answer useful — the fix is to move
/// the whole subtree). `None` when the document has no AST or no H2, in
/// which case the caller falls back to generic advice.
fn largest_section(doc: &Document) -> Option<(&str, u64)> {
    let ast = doc.ast.as_ref()?;
    let lines: Vec<&str> = doc.body.lines().collect();
    let sections: Vec<(usize, &str)> = ast
        .headings
        .iter()
        .filter(|h| h.level == 2)
        .map(|h| (h.line as usize, h.text.trim()))
        .collect();
    let mut best: Option<(&str, u64)> = None;
    for (idx, (start, text)) in sections.iter().enumerate() {
        let end = sections
            .get(idx + 1)
            .map_or(lines.len() + 1, |(next, _)| *next);
        // `+ 1` per line for the newline the `lines()` split dropped, so
        // the section sizes sum to roughly the file's own character count.
        let size: u64 = lines
            .iter()
            .skip(start.saturating_sub(1))
            .take(end.saturating_sub(*start))
            .map(|line| line.chars().count() as u64 + 1)
            .sum();
        if best.is_none_or(|(_, largest)| size > largest) {
            best = Some((text, size));
        }
    }
    best
}

/// A leading heading enumerator (`1.`, `1)`, `A.`) to strip before comparing a
/// config heading against a document heading, so authors can number their
/// sections without the config mirroring the numbers.
fn required_enumerator_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(?:\d+|[A-Za-z])[.)]\s+").expect("valid regex"))
}

/// Normalize a heading for `core.required-headings`: strip a leading enumerator,
/// then trim, drop a trailing colon, and case-fold (matching `normalize_heading`
/// plus the enumerator strip). Shared with the id-pipeline dispatch in
/// `rules.rs` so both paths of the dual-use rule agree (BUG-021).
pub(crate) fn normalize_required_heading(text: &str) -> String {
    let stripped = required_enumerator_regex().replace(text.trim(), "");
    normalize_heading(&stripped)
}

/// `git cat-file -e <sha>^{commit}` — the pinned SHA resolves to a commit
/// object present in this repository.
fn git_object_exists(root: &Path, commit: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// -- AGENT namespace (.claude/agents/*.md subagent definitions) --------

/// `agent.frontmatter`: a Claude Code subagent definition file
/// (`.claude/agents/*.md`) MUST set non-empty `name` and `description`
/// frontmatter keys (errors). It SHOULD use a `name` matching its filename,
/// a `description` in a useful length band, and — when the team pins a list —
/// a known `model` (warnings).
///
/// File-level rule: subagent files carry `name:`, not `id:`, so `core.*`
/// cannot lint them (same reason as [`check_skills_frontmatter`]).
///
/// Params (all optional; the binary enumerates no model names — the allowlist
/// is config-driven so it never goes stale against new aliases):
/// - `models`: array of accepted `model` values. Absent → no value check.
/// - `desc_min_chars`: floor for `description` length (default
///   [`AGENT_DESC_MIN_CHARS`]). A subagent `description` is always loaded into
///   the orchestrator's routing context, so too-short under-triggers.
/// - `desc_max_chars`: ceiling for `description` length. Absent → no ceiling
///   (opt-in, like `agents.context-cache`). Too-long taxes every session's
///   routing context — the per-field analog of `agents.context-budget`.
pub(crate) fn check_agent_frontmatter(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    use crate::frontmatter::{Frontmatter, FrontmatterError};
    let fm = match Frontmatter::parse(&doc.body) {
        Ok(fm) => fm,
        Err(FrontmatterError::MissingFence) => {
            return vec![Diagnostic::error(
                AGENT_FM,
                doc.location.clone(),
                0,
                0,
                "subagent file must have a `---` frontmatter fence with `name:` and `description:`",
            )
            .with_help(
                "add a `---` frontmatter block with `name: <agent-name>` and \
                 `description: <when to delegate to this agent>`",
            )];
        }
        Err(_) => {
            return vec![Diagnostic::error(
                AGENT_FM,
                doc.location.clone(),
                0,
                0,
                "subagent frontmatter must set a non-empty `name` and `description`",
            )
            .with_help(
                "add `name: <agent-name>` and `description: <when to delegate>` to the frontmatter",
            )];
        }
    };

    let name_val = fm.metadata.get("name");
    let desc_val = fm.metadata.get("description");

    let name = name_val
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let desc = desc_val
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Hard requirement: name + description present, non-empty, string-typed.
    let (Some(name), Some(desc)) = (name, desc) else {
        let name_type_wrong = name_val.is_some_and(|v| v.as_str().is_none());
        let desc_type_wrong = desc_val.is_some_and(|v| v.as_str().is_none());
        let message = if name_type_wrong {
            "`name` must be a non-empty string"
        } else if desc_type_wrong {
            "`description` must be a non-empty string"
        } else {
            "subagent frontmatter must set a non-empty `name` and `description`"
        };
        return vec![
            Diagnostic::error(AGENT_FM, doc.location.clone(), 0, 0, message).with_help(
                "add `name: <agent-name>` and `description: <when to delegate>` to the frontmatter",
            ),
        ];
    };

    let mut diags = Vec::new();

    // Warning: name should match the filename stem. Routing uses the `name`
    // field, but a mismatch is a maintenance smell (rename drift).
    if let Some(stem) = Path::new(&doc.location)
        .file_stem()
        .and_then(|s| s.to_str())
    {
        if stem != name {
            diags.push(
                Diagnostic::warning(
                    AGENT_FM,
                    doc.location.clone(),
                    0,
                    0,
                    format!("`name: {name}` does not match filename `{stem}`"),
                )
                .with_help("Claude Code convention: a subagent's `name` matches its filename stem"),
            );
        }
    }

    // Warning (opt-in): `model` must be one of the team-pinned values. The
    // binary enumerates none — the allowlist lives in config so it never goes
    // stale against new model names.
    if let Some(allowed) = params.and_then(|p| p.get("models")).and_then(Value::as_array) {
        if let Some(model) = fm.metadata.get("model").and_then(Value::as_str) {
            let model = model.trim();
            let ok = allowed.iter().filter_map(Value::as_str).any(|a| a == model);
            if !ok {
                let list = allowed
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                diags.push(
                    Diagnostic::warning(
                        AGENT_FM,
                        doc.location.clone(),
                        0,
                        0,
                        format!("`model: {model}` is not in the allowed set"),
                    )
                    .with_help(format!("allowed models (from config): {list}")),
                );
            }
        }
    }

    // Warning: description length band. Floor defaults on; ceiling opt-in.
    let n = desc.chars().count();
    let min_chars = params
        .and_then(|p| p.get("desc_min_chars"))
        .and_then(Value::as_u64)
        .map_or(AGENT_DESC_MIN_CHARS, |v| v as usize);
    if n < min_chars {
        diags.push(
            Diagnostic::warning(
                AGENT_FM,
                doc.location.clone(),
                0,
                0,
                format!("`description` is short ({n} chars) — it may not trigger reliable delegation"),
            )
            .with_help(
                "describe when to use this agent in enough detail for auto-delegation \
                 (what tasks, what triggers)",
            ),
        );
    }
    if let Some(max_chars) = params
        .and_then(|p| p.get("desc_max_chars"))
        .and_then(Value::as_u64)
        .map(|v| v as usize)
    {
        if n > max_chars {
            diags.push(
                Diagnostic::warning(
                    AGENT_FM,
                    doc.location.clone(),
                    0,
                    0,
                    format!(
                        "`description` is long ({n} chars) — every session loads it into routing context"
                    ),
                )
                .with_help(format!(
                    "trim `description` toward the routing essentials (max {max_chars} chars)"
                )),
            );
        }
    }

    diags
}

// -- OPENCODEAGENTS namespace (.opencode/agent/*.md agent definitions) -------

/// `opencode.frontmatter`: an opencode agent definition file
/// (`.opencode/agent/*.md`) MUST set a non-empty `description` (errors). It
/// SHOULD keep `description` in a useful length band and — when the team pins a
/// list — use a known `model` (warnings).
///
/// Unlike Claude Code (`agent.frontmatter`), opencode has **no `name` field** —
/// the agent name is its filename — so there is no name presence or
/// name↔filename check. `description` is the only required frontmatter key.
/// File-level rule: agent files carry no `id:`, so `core.*` cannot lint them
/// (same reason as [`check_skills_frontmatter`]).
///
/// Params (all optional; the binary enumerates no model names):
/// - `models`: array of accepted `model` values. Absent → no value check.
/// - `desc_min_chars`: floor for `description` length (default
///   [`AGENT_DESC_MIN_CHARS`]).
/// - `desc_max_chars`: ceiling for `description` length. Absent → no ceiling.
pub(crate) fn check_opencode_frontmatter(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    use crate::frontmatter::{Frontmatter, FrontmatterError};
    let fm = match Frontmatter::parse(&doc.body) {
        Ok(fm) => fm,
        Err(FrontmatterError::MissingFence) => {
            return vec![Diagnostic::error(
                OPENCODE_FM,
                doc.location.clone(),
                0,
                0,
                "opencode agent file must have a `---` frontmatter fence with `description:`",
            )
            .with_help(
                "add a `---` frontmatter block with `description: <when to use this agent>`",
            )];
        }
        Err(_) => {
            return vec![Diagnostic::error(
                OPENCODE_FM,
                doc.location.clone(),
                0,
                0,
                "opencode agent frontmatter must set a non-empty `description`",
            )
            .with_help("add `description: <when to use this agent>` to the frontmatter")];
        }
    };

    let desc_val = fm.metadata.get("description");
    let desc = desc_val
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Hard requirement: a non-empty, string-typed `description`.
    let Some(desc) = desc else {
        let message = if desc_val.is_some_and(|v| v.as_str().is_none()) {
            "`description` must be a non-empty string"
        } else {
            "opencode agent frontmatter must set a non-empty `description`"
        };
        return vec![
            Diagnostic::error(OPENCODE_FM, doc.location.clone(), 0, 0, message)
                .with_help("add `description: <when to use this agent>` to the frontmatter"),
        ];
    };

    let mut diags = Vec::new();

    // Warning (opt-in): `model` must be one of the team-pinned values. The
    // binary enumerates none — the allowlist lives in config so it never goes
    // stale against new model names.
    if let Some(allowed) = params.and_then(|p| p.get("models")).and_then(Value::as_array) {
        if let Some(model) = fm.metadata.get("model").and_then(Value::as_str) {
            let model = model.trim();
            let ok = allowed.iter().filter_map(Value::as_str).any(|a| a == model);
            if !ok {
                let list = allowed
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                diags.push(
                    Diagnostic::warning(
                        OPENCODE_FM,
                        doc.location.clone(),
                        0,
                        0,
                        format!("`model: {model}` is not in the allowed set"),
                    )
                    .with_help(format!("allowed models (from config): {list}")),
                );
            }
        }
    }

    // Warning: description length band. Floor defaults on; ceiling opt-in.
    let n = desc.chars().count();
    let min_chars = params
        .and_then(|p| p.get("desc_min_chars"))
        .and_then(Value::as_u64)
        .map_or(AGENT_DESC_MIN_CHARS, |v| v as usize);
    if n < min_chars {
        diags.push(
            Diagnostic::warning(
                OPENCODE_FM,
                doc.location.clone(),
                0,
                0,
                format!("`description` is short ({n} chars) — it may not trigger reliable delegation"),
            )
            .with_help(
                "describe when to use this agent in enough detail for auto-delegation \
                 (what tasks, what triggers)",
            ),
        );
    }
    if let Some(max_chars) = params
        .and_then(|p| p.get("desc_max_chars"))
        .and_then(Value::as_u64)
        .map(|v| v as usize)
    {
        if n > max_chars {
            diags.push(
                Diagnostic::warning(
                    OPENCODE_FM,
                    doc.location.clone(),
                    0,
                    0,
                    format!(
                        "`description` is long ({n} chars) — every session loads it into routing context"
                    ),
                )
                .with_help(format!(
                    "trim `description` toward the routing essentials (max {max_chars} chars)"
                )),
            );
        }
    }

    diags
}

// -- core.dep-shape (depends_on edge contract, ADR-039) ---------------

/// `core.dep-shape` (ADR-039 § DAG-002/DAG-003): a namespace's
/// `depends_on` edge contract. Two halves, both keyed off the same
/// per-namespace `requires`/`allows` params:
///
/// 1. **Presence** (DAG-002): each namespace `T` in `requires` MUST
///    appear at least once in `depends_on` as a `T-<n>` entry. Presence,
///    not cardinality — two `PRD-<n>` entries satisfy `requires = ["PRD"]`
///    (DAG-008). Generalizes the deleted `spec.requires-prd` (DAG-004).
/// 2. **Admissibility / conformance** (DAG-003): each `depends_on` entry
///    whose namespace `U` is *managed* (appears in SOME namespace's
///    `core.dep-shape` `requires`/`allows` anywhere in the config) MUST be
///    in *this* namespace's `requires ∪ allows`. An edge to an UNMANAGED
///    namespace is exempt (the unstaged-endpoint exemption inherited from
///    `pipeline.conformance`, EARS-06.3). This replaces the linear
///    "skipping stage" check: in a DAG a `PRD → SPEC` edge is admissible
///    iff SPEC admits PRD, never a "skip" — killing the BUG-008 Catch-22.
///
/// The managed set arrives via the synthesized `managed` param (an array
/// of namespace names), threaded in by [`crate::run`] the same way
/// `pipeline.conformance` received `stages` — the rule is edge-level, so
/// it needs config the per-document `CheckFn` channel would not otherwise
/// carry. An absent `managed` (e.g. a direct unit-test call) disables the
/// admissibility half; the presence half still runs.
pub(crate) fn check_dep_shape(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let str_set = |key: &str| -> std::collections::BTreeSet<String> {
        params
            .and_then(|p| p.get(key))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let requires = str_set("requires");
    let allows = str_set("allows");

    let line = doc
        .frontmatter_lines
        .get("depends_on")
        .copied()
        .unwrap_or(0);
    let mut out = Vec::new();

    // Half 1 — presence (DAG-002/DAG-008).
    for required_ns in &requires {
        let present = doc
            .depends_on
            .iter()
            .any(|dep| dep.split_once('-').is_some_and(|(ns, _)| ns == required_ns));
        if present {
            continue;
        }
        out.push(
            Diagnostic::error(
                DEP_SHAPE,
                doc.location.clone(),
                line,
                0,
                format!(
                    "{} must depend on a {required_ns}; add a {required_ns}-<n> to depends_on",
                    doc.id.namespace
                ),
            )
            .with_help(format!(
                "add `- {required_ns}-<n>` to the `depends_on:` frontmatter list — a {} without a {required_ns} link is incomplete",
                doc.id.namespace
            )),
        );
    }

    // Half 2 — admissibility / conformance (DAG-003). Runs only when the
    // managed set is supplied (the cross-config channel).
    let Some(managed) = params.and_then(|p| p.get("managed")).and_then(Value::as_array) else {
        return out;
    };
    let managed: std::collections::BTreeSet<&str> =
        managed.iter().filter_map(Value::as_str).collect();
    let this_ns = doc.id.namespace.as_str();
    for entry in &doc.depends_on {
        let Some((upstream, _)) = entry.split_once('-') else {
            continue;
        };
        // An edge to an unmanaged namespace is exempt — nothing declares a
        // shape for it (the old unstaged-endpoint exemption, EARS-06.3).
        if !managed.contains(upstream) {
            continue;
        }
        if requires.contains(upstream) || allows.contains(upstream) {
            continue;
        }
        out.push(
            Diagnostic::error(
                DEP_SHAPE,
                doc.location.clone(),
                line,
                0,
                format!(
                    "{} depends on {entry}, but {upstream} is not an allowed dependency of {this_ns}",
                    doc.raw_id
                ),
            )
            .with_help(format!(
                "add it to [{this_ns}.\"core.dep-shape\"].allows or requires"
            )),
        );
    }
    out
}

// -- core.commit-freshness (document-level, ADR-040) -----------------

/// The outcome of the git ancestry probe (ADR-040 § PIN-002), kept as a
/// pure value so the exit-code interpretation is testable without a repo.
#[derive(Debug, PartialEq, Eq)]
enum Ancestry {
    /// `git merge-base --is-ancestor` exited 0 — the commit is an
    /// ancestor of HEAD; proceed to the drift check.
    IsAncestor,
    /// Exit 1 — the object is known but is NOT an ancestor (history
    /// diverged or was rewritten). The hard error of PIN-002.
    NotAncestor,
    /// Exit 128 / unknown object (shallow clone, GC'd commit) — the
    /// question is unanswerable, route to skip-with-warning (PIN-004).
    Unanswerable,
}

/// Interpret the exit code of `git merge-base --is-ancestor` (PIN-002).
/// Exit 0 = ancestor, 1 = definitive non-ancestor, anything else
/// (notably 128 for a missing object) = unanswerable.
fn interpret_ancestry(exit_code: Option<i32>) -> Ancestry {
    match exit_code {
        Some(0) => Ancestry::IsAncestor,
        Some(1) => Ancestry::NotAncestor,
        _ => Ancestry::Unanswerable,
    }
}

/// `core.commit-freshness` (ADR-040 § PIN-002/003/004/007): flag a
/// document whose pinned commit's scoped paths have drifted in the
/// working tree. Default-off — silent unless a namespace opts in and the
/// document carries a `pin` (or `require-pin = true`).
///
/// All git access lives here in the rule layer (PIN-006); the parse
/// pipeline never shells to git. The diagnostic family:
///
/// - no `pin` + `require-pin = true` → error (PIN-007);
/// - pinned commit not an ancestor of HEAD → error (PIN-002);
/// - scoped working-tree drift → stale at the configured severity
///   (PIN-003, PIN-007);
/// - not a repo / no git / shallow / any unexpected git failure →
///   skip-with-warning naming `fetch-depth: 0` (PIN-004).
pub(crate) fn check_commit_freshness(
    doc: &Document,
    params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    let require_pin = params
        .and_then(|p| p.get("require-pin"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let stale_severity = params
        .and_then(|p| p.get("severity"))
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("warning"))
        .map(|is_warning| {
            if is_warning {
                crate::diagnostic::Severity::Warning
            } else {
                crate::diagnostic::Severity::Error
            }
        })
        .unwrap_or(crate::diagnostic::Severity::Error);

    let Some(pin) = doc.pin.as_ref() else {
        if require_pin {
            let line = doc.frontmatter_lines.get("id").copied().unwrap_or(0);
            return vec![Diagnostic::error(
                COMMIT_FRESHNESS,
                doc.location.clone(),
                line,
                0,
                format!(
                    "{} has no `pin` block but this namespace requires one (require-pin)",
                    doc.raw_id
                ),
            )
            .with_help(
                "add a `pin:` block with `commit:` (the green commit) and a non-empty `scope:` \
                 list of the path globs this document covers",
            )];
        }
        return Vec::new();
    };

    // Compile the scope globs — this is the authoritative scope matcher
    // (PIN-001/003); git is never asked to interpret the globs.
    let scope = match compile_scope(&pin.scope) {
        Ok(set) => set,
        Err(invalid) => {
            let line = pin_commit_line(doc);
            return vec![Diagnostic::error(
                COMMIT_FRESHNESS,
                doc.location.clone(),
                line,
                0,
                format!("{}: `pin.scope` glob `{invalid}` is invalid", doc.raw_id),
            )
            .with_help("fix the glob in `pin.scope` (same syntax as `[NS].paths`)")];
        }
    };

    // PIN-004: degrade gracefully when git history is unavailable.
    if !git_repo_available(root) {
        return vec![skip_warning(
            doc,
            "not a git repository or `git` is unavailable — cannot verify the commit pin",
        )];
    }
    if git_is_shallow(root) {
        return vec![skip_warning(
            doc,
            "shallow clone — history does not reach the pinned commit",
        )];
    }

    // PIN-002: the pinned commit must be an ancestor of HEAD.
    let Some(ancestry_code) = git_is_ancestor(root, &pin.commit) else {
        return vec![skip_warning(
            doc,
            "could not run `git merge-base` to verify the commit pin",
        )];
    };
    match interpret_ancestry(ancestry_code) {
        Ancestry::IsAncestor => {}
        Ancestry::Unanswerable => {
            // Missing object (exit 128) — shallow/GC'd. Skip, do not error.
            return vec![skip_warning(
                doc,
                &format!(
                    "pinned commit `{}` is not in the available history (shallow clone or \
                     pruned object) — cannot verify",
                    pin.commit
                ),
            )];
        }
        Ancestry::NotAncestor => {
            let line = pin_commit_line(doc);
            return vec![Diagnostic::error(
                COMMIT_FRESHNESS,
                doc.location.clone(),
                line,
                0,
                format!(
                    "{}: pinned commit `{}` is not an ancestor of HEAD — history was rewritten \
                     or the SHA is wrong",
                    doc.raw_id, pin.commit
                ),
            )
            .with_help(format!(
                "re-validate against the current code and run `ctxgrd pin --bless {}` to record \
                 the new commit",
                doc.raw_id
            ))];
        }
    }

    // PIN-003: scoped drift against the WORKING TREE (committed and
    // uncommitted), via an endpoint diff. The compiled globset decides
    // scope membership; git lists every changed path.
    let Some(changed) = git_diff_name_only(root, &pin.commit) else {
        return vec![skip_warning(
            doc,
            "could not run `git diff` to compare against the pinned commit",
        )];
    };
    let mut stale_paths: Vec<String> = changed
        .into_iter()
        .filter(|path| scope.is_match(path))
        .collect();
    if stale_paths.is_empty() {
        return Vec::new();
    }
    stale_paths.sort();

    let line = pin_commit_line(doc);
    let mut message = format!(
        "{}: scoped code changed since the pinned commit `{}` — {} stale path(s): {}",
        doc.raw_id,
        pin.commit,
        stale_paths.len(),
        stale_paths.join(", ")
    );
    // Enumerate the offending commits for the message (explanation only,
    // not the verdict — the working-tree diff already decided staleness).
    if let Some(commits) = git_log_oneline(root, &pin.commit, &pin.scope) {
        if !commits.is_empty() {
            message.push_str("; commits since the pin: ");
            message.push_str(&commits.join("; "));
        }
    }

    let mut diag = Diagnostic::error(COMMIT_FRESHNESS, doc.location.clone(), line, 0, message)
        .with_help(format!(
            "re-validate this document against the changed code, then run \
             `ctxgrd pin --bless {}` to record the new green commit",
            doc.raw_id
        ));
    diag.severity = stale_severity;
    vec![diag]
}

/// Compile a list of scope globs into a `GlobSet`, matching how
/// `[NS].paths` globs are handled (root-anchored, gitignore-style).
/// Returns the first offending glob string on failure.
fn compile_scope(scope: &[String]) -> Result<globset::GlobSet, String> {
    let mut builder = globset::GlobSetBuilder::new();
    for raw in scope {
        let glob = globset::Glob::new(raw).map_err(|_| raw.clone())?;
        builder.add(glob);
    }
    builder.build().map_err(|_| scope.join(", "))
}

/// A `skip-with-warning` diagnostic (PIN-004): names the remedy and never
/// escalates the exit code.
fn skip_warning(doc: &Document, reason: &str) -> Diagnostic {
    Diagnostic::warning(
        COMMIT_FRESHNESS,
        doc.location.clone(),
        pin_commit_line(doc),
        0,
        format!("commit-freshness skipped: {reason}"),
    )
    .with_help(
        "deepen the checkout to verify the pin — set `fetch-depth: 0` in CI \
         (see docs/ci.md); the lint exit code is unaffected by this skip",
    )
}

/// The 1-indexed line of the `pin` key for diagnostic anchoring, falling
/// back to the `id` line then 0.
fn pin_commit_line(doc: &Document) -> u32 {
    doc.frontmatter_lines
        .get("pin")
        .or_else(|| doc.frontmatter_lines.get("id"))
        .copied()
        .unwrap_or(0)
}

/// True when `root` is inside a git work tree and `git` is on PATH.
fn git_repo_available(root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// True when the repository is a shallow clone (PIN-004).
fn git_is_shallow(root: &Path) -> bool {
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-shallow-repository"])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout).trim() == "true"
}

/// Run `git merge-base --is-ancestor <commit> HEAD` and return its exit
/// code (PIN-002). `None` when the process could not be spawned at all.
fn git_is_ancestor(root: &Path, commit: &str) -> Option<Option<i32>> {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "--is-ancestor", commit, "HEAD"])
        .status()
        .ok()?;
    Some(status.code())
}

/// Run `git diff --name-only <commit> --` against the working tree
/// (PIN-003): every path that differs between the pinned commit's tree
/// and the on-disk working tree, committed or not. `None` on any
/// unexpected git failure (routes to skip-with-warning).
fn git_diff_name_only(root: &Path, commit: &str) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--name-only", commit, "--"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// Enumerate the commits since the pin touching the scoped paths, for the
/// diagnostic message only (PIN-003 — explanation, not the verdict). The
/// scope globs are passed as a coarse pathspec prefilter; the verdict was
/// already decided by the globset in [`check_commit_freshness`]. `None`
/// on failure — the message simply omits the commit list.
fn git_log_oneline(root: &Path, commit: &str, scope: &[String]) -> Option<Vec<String>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(["log", "--oneline", &format!("{commit}..HEAD"), "--"]);
    for glob in scope {
        cmd.arg(glob);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

// -- core.calendar-freshness (document-level, ADR-040 § PIN-008) ------

/// `core.calendar-freshness` (ADR-040 § PIN-008): the namespace-agnostic
/// time-axis sibling of `core.commit-freshness`. Flags a document whose
/// configured date field plus an interval is older than today. Pure date
/// arithmetic, no git. `todo.freshness` is a thin preset over this (it
/// fixes `field`/`stale_days` and keeps its required-line behaviour).
///
/// Params:
/// - `field` (string, default `reviewed_date`): the frontmatter metadata
///   key carrying the `YYYY-MM-DD` date to age.
/// - `stale_days` (integer, default 30): the staleness interval.
///
/// A missing or unparseable date is silent here (presence is
/// `core.required-metadata`'s concern); only an aged date warns.
pub(crate) fn check_calendar_freshness(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let field = params
        .and_then(|p| p.get("field"))
        .and_then(Value::as_str)
        .unwrap_or("reviewed_date");
    let stale_days = params
        .and_then(|p| p.get("stale_days"))
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_STALE_DAYS);

    let Some(raw) = doc.metadata.get(field).and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(ymd) = parse_ymd(raw.trim()) else {
        return Vec::new();
    };
    let Some(age) = days_since(ymd) else {
        return Vec::new();
    };
    if age <= stale_days {
        return Vec::new();
    }

    let line = doc.frontmatter_lines.get(field).copied().unwrap_or(0);
    vec![Diagnostic::warning(
        CALENDAR_FRESHNESS,
        doc.location.clone(),
        line,
        0,
        format!(
            "{} is stale — `{field}` is {age} days old (limit {stale_days})",
            doc.raw_id
        ),
    )
    .with_help(format!(
        "re-validate the document and refresh the `{field}:` date"
    ))]
}

// -- core.file-name (document-level, ADR-091 § FNM-001) --------------

/// `core.file-name` — the file's leading numeric prefix must equal the
/// number carried in its `id` (FNM-001). Opt-in and Document-level, so it
/// is only ever dispatched on id-keyed documents; id-less path-claim
/// singletons (README/CLAUDE/GUIDE) are `Level::File` and never reach it
/// (FNM-002). The prefix is compared as a parsed `u32`, not as a
/// zero-padded string, so both `88-slug.md` and `088-slug.md` satisfy
/// `id: NS-88` (FNM-003) — padding width is a cosmetic convention this
/// rule deliberately does not police.
pub(crate) fn check_file_name(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    // `doc.location` is the root-relative path (e.g. `docs/adrs/091-x.md`);
    // the convention constrains the final path component only.
    let file_name = Path::new(&doc.location)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(doc.location.as_str());
    let digits: String = file_name
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let line = doc.frontmatter_lines.get("id").copied().unwrap_or(0);
    let want = doc.id.number;

    if digits.is_empty() {
        return vec![Diagnostic::error(
            FILE_NAME,
            doc.location.clone(),
            line,
            0,
            format!(
                "{}: filename `{file_name}` has no numeric prefix — expected it to start with `{want}` to match the id",
                doc.raw_id
            ),
        )
        .with_help(format!(
            "rename the file so it starts with the id number, e.g. `{want:03}-<slug>.md` (any padding width is accepted)"
        ))];
    }

    // `u32::from_str` collapses leading zeros exactly like `core.id`
    // parses its number; an overflowing prefix can never equal a valid id
    // number, so it falls through to the mismatch arm.
    match digits.parse::<u32>() {
        Ok(n) if n == want => Vec::new(),
        _ => vec![Diagnostic::error(
            FILE_NAME,
            doc.location.clone(),
            line,
            0,
            format!("{}: filename prefix `{digits}` does not match the id number {want}", doc.raw_id),
        )
        .with_help(format!(
            "rename the file to start with `{want}` (e.g. `{want:03}-<slug>.md`), or change `id:` to match the filename"
        ))],
    }
}

// -- security.vuln-sla (document-level, ADR-041 § SEC-004) -----------

/// Map the `severity` param to a diagnostic [`Severity`], mirroring
/// `check_commit_freshness`: `"warning"` → Warning, anything else
/// (including absent) → Error.
fn severity_param(params: Option<&Value>) -> crate::diagnostic::Severity {
    params
        .and_then(|p| p.get("severity"))
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("warning"))
        .map(|is_warning| {
            if is_warning {
                crate::diagnostic::Severity::Warning
            } else {
                crate::diagnostic::Severity::Error
            }
        })
        .unwrap_or(crate::diagnostic::Severity::Error)
}

/// `security.vuln-sla` (ADR-041 § SEC-004): an `open` finding whose
/// `severity` has a configured SLA window and whose `discovered_date` is
/// older than that window is flagged at the configured diagnostic level.
///
/// Params:
/// - `windows` (object): severity name → integer days. Default when
///   absent: `{ critical = 7, high = 30 }`.
/// - `severity` (string): the diagnostic level, `"error"` (default) or
///   `"warning"` — the same mapping `core.commit-freshness` uses.
///
/// Only `status: open` findings are considered. A severity absent from
/// `windows` is never flagged (medium/low/info age silently). A missing,
/// unparseable, or future `discovered_date` is silent — presence is
/// `core.required-metadata`'s concern.
pub(crate) fn check_vuln_sla(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let status = doc
        .metadata
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !status.eq_ignore_ascii_case("open") {
        return Vec::new();
    }

    let Some(severity) = doc.metadata.get("severity").and_then(Value::as_str) else {
        return Vec::new();
    };
    let severity = severity.to_lowercase();

    // Look up the window for this severity. The default (no `windows`
    // param) is critical=7, high=30; a severity absent from the map is
    // never flagged.
    let window_days = match params.and_then(|p| p.get("windows")) {
        Some(windows) => windows.get(&severity).and_then(Value::as_i64),
        None => match severity.as_str() {
            "critical" => Some(7),
            "high" => Some(30),
            _ => None,
        },
    };
    let Some(window_days) = window_days else {
        return Vec::new();
    };

    let Some(raw) = doc.metadata.get("discovered_date").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(ymd) = parse_ymd(raw.trim()) else {
        return Vec::new();
    };
    let Some(age) = days_since(ymd) else {
        return Vec::new();
    };
    if age <= window_days {
        return Vec::new();
    }

    let line = doc
        .frontmatter_lines
        .get("status")
        .or_else(|| doc.frontmatter_lines.get("severity"))
        .or_else(|| doc.frontmatter_lines.get("discovered_date"))
        .or_else(|| doc.frontmatter_lines.get("id"))
        .copied()
        .unwrap_or(0);
    let mut diag = Diagnostic::error(
        VULN_SLA,
        doc.location.clone(),
        line,
        0,
        format!(
            "{}: open `{severity}` finding is {age} days old, past its {window_days}-day SLA",
            doc.raw_id
        ),
    )
    .with_help(format!(
        "remediate the finding, change its `status`, or widen the window in \
         `[{}.\"security.vuln-sla\"]`",
        doc.id.namespace
    ));
    diag.severity = severity_param(params);
    vec![diag]
}

// -- security.risk-expiry (document-level, ADR-041 § SEC-005) --------

/// `security.risk-expiry` (ADR-041 § SEC-005): a risk acceptance must be
/// signed (`approver`) and reasoned (`rationale`) and carry a future-dated
/// `expires`, forcing the risk back up for re-decision on a date.
///
/// Params:
/// - `require-when-status` (string, optional): when set, the rule acts
///   only on documents whose `status` matches it (case-insensitive) —
///   `"accepted"` on `VULN`. When absent, the rule always acts (the
///   `RISK` case).
/// - `exempt-when-links` (string, optional): a namespace prefix. When any
///   `depends_on` entry resolves to that namespace, the document is exempt
///   — the linked document carries the fields canonically (`"RISK"`).
///
/// Each problem emits one `error`.
pub(crate) fn check_risk_expiry(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    // Scope: only act when the document's status matches the configured
    // gate, or unconditionally when no gate is set.
    if let Some(want) = params
        .and_then(|p| p.get("require-when-status"))
        .and_then(Value::as_str)
    {
        let status = doc
            .metadata
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !status.eq_ignore_ascii_case(want) {
            return Vec::new();
        }
    }

    // Exemption: a linked document in the configured namespace is
    // canonical, so the fields are not required inline. The link must
    // *resolve* — this is the one place in the family where a phantom
    // reference did not merely satisfy a requirement but granted an
    // exemption, so an unsigned, un-time-boxed acceptance passed by citing
    // a RISK nobody wrote (BUG-030).
    if let Some(prefix) = params
        .and_then(|p| p.get("exempt-when-links"))
        .and_then(Value::as_str)
    {
        if links_namespace(params, prefix) {
            return Vec::new();
        }
    }

    let line_for = |key: &str| -> u32 {
        doc.frontmatter_lines
            .get(key)
            .or_else(|| doc.frontmatter_lines.get("id"))
            .copied()
            .unwrap_or(0)
    };
    let nonempty = |key: &str| -> bool {
        doc.metadata
            .get(key)
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    };

    let mut diags = Vec::new();

    if !nonempty("approver") {
        diags.push(
            Diagnostic::error(
                RISK_EXPIRY,
                doc.location.clone(),
                line_for("approver"),
                0,
                format!("{}: a risk acceptance must name an `approver`", doc.raw_id),
            )
            .with_help("add an `approver:` field naming who signed off on accepting this risk"),
        );
    }

    if !nonempty("rationale") {
        diags.push(
            Diagnostic::error(
                RISK_EXPIRY,
                doc.location.clone(),
                line_for("rationale"),
                0,
                format!(
                    "{}: a risk acceptance must record a `rationale`",
                    doc.raw_id
                ),
            )
            .with_help("add a `rationale:` field explaining why this risk is being accepted"),
        );
    }

    match doc.metadata.get("expires").and_then(Value::as_str) {
        None => diags.push(
            Diagnostic::error(
                RISK_EXPIRY,
                doc.location.clone(),
                line_for("expires"),
                0,
                format!(
                    "{}: a risk acceptance must carry a future-dated `expires`",
                    doc.raw_id
                ),
            )
            .with_help("add an `expires: YYYY-MM-DD` date when this acceptance must be re-decided"),
        ),
        Some(raw) => {
            let trimmed = raw.trim();
            match parse_ymd(trimmed) {
                None => diags.push(
                    Diagnostic::error(
                        RISK_EXPIRY,
                        doc.location.clone(),
                        line_for("expires"),
                        0,
                        format!(
                            "{}: `expires` (`{trimmed}`) is not a valid `YYYY-MM-DD` date",
                            doc.raw_id
                        ),
                    )
                    .with_help("set `expires` to a future `YYYY-MM-DD` date"),
                ),
                Some(ymd) => {
                    // days_since returns today - then, so a future date is
                    // negative; today or past is >= 0.
                    if days_since(ymd).is_some_and(|age| age >= 0) {
                        diags.push(
                            Diagnostic::error(
                                RISK_EXPIRY,
                                doc.location.clone(),
                                line_for("expires"),
                                0,
                                format!(
                                    "{}: `expires` ({trimmed}) is not in the future — re-decide the risk",
                                    doc.raw_id
                                ),
                            )
                            .with_help(
                                "re-decide the risk and set `expires` to a new future date, \
                                 or close it",
                            ),
                        );
                    }
                }
            }
        }
    }

    diags
}

// -- security.remediation-link (document-level, ADR-041 § SEC-006) ---

/// `security.remediation-link` (ADR-041 § SEC-006, mitigated-VULN case):
/// a finding in scope must cross-ref its remediation — the implementing
/// decision — so the fix is falsifiable, not "trust me".
///
/// Params:
/// - `require-when-status` (string, optional): when set, the rule acts
///   only on documents whose `status` matches it (case-insensitive) —
///   `"mitigated"` on `VULN`. When absent, the rule always acts.
/// - `accepted-namespaces` (string list, optional, default `["ADR"]`): the
///   namespaces a resolving cross-ref may cite as the remediation.
/// - `remediation-fields` (string list, optional, default
///   `["remediation_link"]`): metadata fields whose non-empty value counts
///   as the remediation, for a fix tracked outside the document graph.
///
/// The document satisfies the rule when it carries a **resolving**
/// cross-ref (`depends_on` or body token, own id excluded — see
/// [`resolved_refs`]) into one of `accepted-namespaces`, or a non-empty
/// value in one of `remediation-fields`. Otherwise one `error`.
///
/// Both halves are BUG-031's fix, and both are contract breaks by design
/// (MAJOR). Previously *any* token satisfied the rule, including the
/// document's own id — which `ctxgrd new VULN` scaffolds into the body H1,
/// so every scaffolded finding self-satisfied from birth and marking one
/// `mitigated` was enough to pass. The `accepted-namespaces` /
/// `remediation-fields` split is the same shape `soc2.control-evidence`
/// already uses for `evidence-namespaces` / `evidence-fields`: a resolvable
/// cross-ref, or an explicit opaque external pointer — never an
/// unconstrained token that cannot be told apart from a typo.
pub(crate) fn check_remediation_link(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    if let Some(want) = params
        .and_then(|p| p.get("require-when-status"))
        .and_then(Value::as_str)
    {
        let status = doc
            .metadata
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !status.eq_ignore_ascii_case(want) {
            return Vec::new();
        }
    }

    let accepted: Vec<String> = params
        .and_then(|p| p.get("accepted-namespaces"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| vec!["ADR".to_owned()]);
    if accepted.iter().any(|ns| links_namespace(params, ns)) {
        return Vec::new();
    }

    let fields: Vec<&str> = params
        .and_then(|p| p.get("remediation-fields"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_else(|| vec!["remediation_link"]);
    let field_satisfied = fields.iter().any(|f| {
        doc.metadata
            .get(*f)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    });
    if field_satisfied {
        return Vec::new();
    }

    let line = doc
        .frontmatter_lines
        .get("status")
        .or_else(|| doc.frontmatter_lines.get("id"))
        .copied()
        .unwrap_or(0);
    let accepted_list = accepted.join("/");
    let field = fields.first().copied().unwrap_or("remediation_link");
    vec![Diagnostic::error(
        REMEDIATION_LINK,
        doc.location.clone(),
        line,
        0,
        format!(
            "{}: mitigated finding must cross-ref its remediation — a resolving \
             {accepted_list} link or a `{field}`",
            doc.raw_id
        ),
    )
    .with_help(format!(
        "add the id of the implementing {accepted_list} to `depends_on` (or cite it in the \
         body), or add a `{field}:` pointing at the fix in an external tracker — the \
         document's own id does not count"
    ))]
}

// -- shared conditional-link matcher (BUG-030 / BUG-031) -------------

/// The synthesized param carrying a document's *resolved* outbound
/// references. Threaded by [`crate::run`] for every code in
/// [`crate::builtin_rules::RESOLUTION_AWARE_RULES`]; never a config key,
/// so it is absent from those rules' declared `params` (the same channel
/// `core.dep-shape` uses for `managed`, ADR-039 § DAG-003).
pub(crate) const RESOLVED_REFS_PARAM: &str = "resolved-refs";

/// Every reference `doc` carries that **resolves** to a document present
/// in the run, as canonical `NS-<n>` strings — the candidate set the
/// conditional link rules are allowed to count as evidence (BUG-030).
///
/// Sources, matching what the rules previously scanned by prefix alone:
/// `depends_on` entries, and body cross-ref tokens outside code spans and
/// strikethrough (a token in backticks is a literal example, not a
/// reference).
///
/// Two exclusions, both load-bearing:
///
/// 1. **Unresolvable targets are dropped** (BUG-030). A well-formed id for
///    a document nobody wrote is not evidence; before this, it satisfied
///    every conditional evidence rule exactly as a real one did, and
///    `security.risk-expiry` went further and granted an *exemption* on
///    one.
/// 2. **The host's own id is dropped** (BUG-031). `ctxgrd new VULN`
///    scaffolds `# VULN-001: <title>` as the body H1, so an open-target
///    rule like `security.remediation-link` self-satisfied from birth.
///    The exclusion lives here rather than in that one rule because the
///    sibling rules are immune only by coincidence — each happens to
///    demand a foreign namespace — so the next open-target rule would
///    reproduce the defect.
pub(crate) fn resolved_refs(doc: &Document, known: &BTreeSet<DocumentId>) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut admit = |id: DocumentId| {
        if id != doc.id && known.contains(&id) {
            out.insert(id.to_string());
        }
    };
    for entry in &doc.depends_on {
        if let Ok(id) = entry.parse::<DocumentId>() {
            admit(id);
        }
    }
    if let Some(ast) = doc.ast.as_ref() {
        for t in &ast.cross_ref_tokens {
            if t.in_code || t.in_strikethrough {
                continue;
            }
            admit(DocumentId::new(t.namespace.clone(), t.number));
        }
    }
    out.into_iter().collect()
}

/// The resolved-reference candidate set for the document under check, read
/// from the synthesized [`RESOLVED_REFS_PARAM`].
///
/// **Fails closed.** An absent param yields an empty set, so a rule sees
/// "no evidence" rather than "evidence unverified". Every one of these
/// rules is a false-green defect in its unfixed form, so the failure mode
/// of a dropped threading must be a loud diagnostic, not silence — a
/// permissive fallback here would restore BUG-030 the moment the dispatch
/// changed, and nothing would say so.
fn resolved_candidates(params: Option<&Value>) -> Vec<&str> {
    params
        .and_then(|p| p.get(RESOLVED_REFS_PARAM))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Whether the document under check links a **resolvable** document in
/// `namespace`. Namespace match is case-insensitive. Shared by the
/// conditional cross-ref rules (`gdpr.processor-dpa`,
/// `hipaa.safeguard-evidence`, the three `*.control-evidence` rules, and
/// `security.risk-expiry`'s exemption).
fn links_namespace(params: Option<&Value>, namespace: &str) -> bool {
    resolved_candidates(params).iter().any(|id| {
        id.split_once('-')
            .map(|(ns, _)| ns.eq_ignore_ascii_case(namespace))
            .unwrap_or(false)
    })
}

/// The configured evidence namespaces, rendered for a diagnostic.
///
/// The default **must** stay in step with [`evidence_gap`]'s own default,
/// which is what the rules actually enforce: a message naming `POLICY`/`ADR`
/// while the pack accepts only `ADR` tells the author to do something the
/// rule will reject. Every `evidence_gap` caller renders its namespaces
/// through here rather than hardcoding the pair, so narrowing
/// `evidence-namespaces` in a pack narrows the diagnostic with it.
/// Returns `(list, article)` — the rendered namespaces and the indefinite
/// article that fits the first one, so a narrowed list reads "an ADR
/// cross-ref" rather than "a ADR cross-ref". Namespaces are acronyms, so the
/// written-letter test is the right one here.
fn evidence_namespace_list(params: Option<&Value>) -> (String, &'static str) {
    let ns = product_list_param(params, "evidence-namespaces", &["POLICY", "ADR"]);
    let article = match ns.first().and_then(|n| n.chars().next()) {
        Some(c) if "AEIOU".contains(c.to_ascii_uppercase()) => "an",
        _ => "a",
    };
    (ns.join(" or "), article)
}

/// The first configured evidence field — the one a diagnostic names when it
/// tells an author which key to add. Shared by all five [`evidence_gap`]
/// callers, which otherwise repeated this read verbatim.
fn first_evidence_field(params: Option<&Value>) -> &str {
    product_list_param(params, "evidence-fields", &["evidence_link"])
        .first()
        .copied()
        .unwrap_or("evidence_link")
}

// -- gdpr.processor-dpa (document-level, ADR-066 § GDPR-002) ---------

/// `gdpr.processor-dpa` (ADR-066 § GDPR-002): a `ROPA` record whose
/// `controller_or_processor` role is `processor` MUST cross-ref its
/// governing `DPA` (the Art. 28 agreement). A controller or
/// joint-controller record is out of scope.
///
/// No *config* params: the GDPR semantics are statutory, not configuration
/// — the trigger field (`controller_or_processor`), the trigger value
/// (`processor`), and the target namespace (`DPA`) are fixed. The link may
/// be a `depends_on` entry or a body cross-ref token, and must resolve to a
/// `DPA` document present in the run ([`resolved_refs`], BUG-030); the
/// candidate set arrives through the synthesized [`RESOLVED_REFS_PARAM`].
/// `core.cross-ref` only checks that links which *are* present resolve; it
/// never requires a processor record to carry one. Emits one `error`.
pub(crate) fn check_processor_dpa(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let role = doc
        .metadata
        .get("controller_or_processor")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !role.eq_ignore_ascii_case("processor") {
        return Vec::new();
    }

    if links_namespace(params, "DPA") {
        return Vec::new();
    }

    let line = doc
        .frontmatter_lines
        .get("controller_or_processor")
        .or_else(|| doc.frontmatter_lines.get("id"))
        .copied()
        .unwrap_or(0);
    vec![Diagnostic::error(
        PROCESSOR_DPA,
        doc.location.clone(),
        line,
        0,
        format!(
            "{}: a processor-role ROPA must cross-ref its governing `DPA` (the Art. 28 agreement)",
            doc.raw_id
        ),
    )
    .with_help("add the governing DPA id to `depends_on`, or cross-ref it in the body")]
}

// -- shared conditional-evidence machinery (ADR-066 § HIPAA-002, --
// -- ADR-069 § SOC-002) ----------------------------------------------

/// A failing conditional-evidence check: the document asserts a control
/// `value` (a safeguard or a TSC criterion) at `line` but cites no
/// implementing evidence. `addressable` carries whether the item is in the
/// rule's `addressable` subset, so each caller can render its own message.
struct EvidenceGap {
    value: String,
    line: u32,
    addressable: bool,
}

/// The decision core shared by the conditional compliance evidence rules
/// (`hipaa.safeguard-evidence`, `soc2.control-evidence`). One mechanism,
/// reused — not forked per pack (ADR-069 § SOC-002). Returns `None` when
/// the document satisfies the rule; otherwise the gap.
///
/// A document satisfies the rule when any of these hold:
/// - it carries no trigger field (presence is `core.required-metadata`'s
///   concern, not this rule's);
/// - its `status` is in `out-of-scope-status` (e.g. `not-applicable`) — an
///   out-of-scope control owes no operating-effectiveness evidence;
/// - it cross-refs an evidence namespace (`evidence-namespaces`, default
///   `POLICY`/`ADR`) — a `depends_on` entry or a body token, which must
///   *resolve* to a document present in the run ([`resolved_refs`],
///   BUG-030): evidence that does not exist is not evidence;
/// - a metadata field named in `evidence-fields` carries a non-empty value
///   (SOC 2's `evidence_link`); or
/// - the asserted value is in `addressable` AND a `justification-field`
///   (default `justification`) carries a non-empty value (HIPAA's
///   addressable escape).
///
/// Params (all optional, defaults preserve the original
/// `hipaa.safeguard-evidence` behavior):
/// - `field` (string): the trigger metadata field. Defaults to `default_field`.
/// - `addressable` (string list): ids whose justification may stand in for
///   evidence (the HIPAA Addressable subset; empty for SOC 2).
/// - `evidence-namespaces` (string list): namespaces whose cross-ref counts
///   as evidence (default `POLICY`, `ADR`).
/// - `evidence-fields` (string list): metadata fields whose non-empty value
///   counts as evidence (SOC 2's `evidence_link`; empty for HIPAA).
/// - `justification-field` (string): the addressable escape field (default
///   `justification`).
/// - `out-of-scope-status` (string list): `status` values that exempt the
///   document (empty for HIPAA; `not-applicable` for SOC 2).
fn evidence_gap(doc: &Document, params: Option<&Value>, default_field: &str) -> Option<EvidenceGap> {
    let field = params
        .and_then(|p| p.get("field"))
        .and_then(Value::as_str)
        .unwrap_or(default_field);
    let value = doc.metadata.get(field).and_then(Value::as_str)?;

    // Out-of-scope gate: a status the pack marks out of scope exempts the
    // document (SOC 2 `not-applicable`). HIPAA passes an empty list.
    let status = doc
        .metadata
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let out_of_scope = params
        .and_then(|p| p.get("out-of-scope-status"))
        .and_then(Value::as_array)
        .is_some_and(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .any(|s| s.eq_ignore_ascii_case(status))
        });
    if out_of_scope {
        return None;
    }

    // Evidence 1: a cross-ref to an evidence namespace.
    let evidence_ns: Vec<String> = params
        .and_then(|p| p.get("evidence-namespaces"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| vec!["POLICY".to_owned(), "ADR".to_owned()]);
    if evidence_ns.iter().any(|ns| links_namespace(params, ns)) {
        return None;
    }

    // Evidence 2: a non-empty value in a configured evidence field
    // (SOC 2's `evidence_link`). HIPAA configures none.
    let evidence_field_satisfied = params
        .and_then(|p| p.get("evidence-fields"))
        .and_then(Value::as_array)
        .is_some_and(|a| {
            a.iter().filter_map(Value::as_str).any(|f| {
                doc.metadata
                    .get(f)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty())
            })
        });
    if evidence_field_satisfied {
        return None;
    }

    // Addressable escape: an addressable item may stand on a justification.
    let addressable = params
        .and_then(|p| p.get("addressable"))
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().filter_map(Value::as_str).any(|s| s == value));
    let just_field = params
        .and_then(|p| p.get("justification-field"))
        .and_then(Value::as_str)
        .unwrap_or("justification");
    let has_justification = doc
        .metadata
        .get(just_field)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    if addressable && has_justification {
        return None;
    }

    let line = doc
        .frontmatter_lines
        .get(field)
        .or_else(|| doc.frontmatter_lines.get("id"))
        .copied()
        .unwrap_or(0);
    Some(EvidenceGap {
        value: value.to_owned(),
        line,
        addressable,
    })
}

// -- hipaa.safeguard-evidence (document-level, ADR-066 § HIPAA-002) --

/// `hipaa.safeguard-evidence` (ADR-066 § HIPAA-002): a `SAFEGUARD` safeguard
/// mapping MUST point at implementing evidence. Every in-scope safeguard
/// must cross-ref a `POLICY` or an ADR; an `addressable` safeguard MAY
/// instead carry a `justification` field recording why an equivalent (or
/// no) control is reasonable. A `required` safeguard has no justification
/// escape — "addressable" is the only Security Rule category that admits a
/// documented rationale in lieu of implementation.
///
/// Trigger field `safeguard`. The `addressable`, `evidence-namespaces`, and
/// `justification-field` params are documented on [`evidence_gap`], the
/// shared decision core. Emits one `error` when an in-scope safeguard has
/// neither evidence nor a permitted justification.
pub(crate) fn check_safeguard_evidence(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(gap) = evidence_gap(doc, params, "safeguard") else {
        return Vec::new();
    };
    let safeguard = &gap.value;
    let just_field = params
        .and_then(|p| p.get("justification-field"))
        .and_then(Value::as_str)
        .unwrap_or("justification");
    let (ns_list, ns_article) = evidence_namespace_list(params);
    let (message, help) = if gap.addressable {
        (
            format!(
                "{}: addressable safeguard `{safeguard}` needs implementing evidence \
                 ({ns_article} {ns_list} cross-ref) or a `{just_field}` field",
                doc.raw_id
            ),
            format!(
                "cross-ref the implementing {ns_list}, or add a `{just_field}:` field \
                 recording why an equivalent (or no) control is reasonable"
            ),
        )
    } else {
        (
            format!(
                "{}: required safeguard `{safeguard}` needs implementing evidence \
                 ({ns_article} {ns_list} cross-ref)",
                doc.raw_id
            ),
            format!(
                "cross-ref the implementing {ns_list} — a required safeguard has no \
                 justification escape"
            ),
        )
    };
    vec![
        Diagnostic::error(SAFEGUARD_EVIDENCE, doc.location.clone(), gap.line, 0, message)
            .with_help(help),
    ]
}

// -- soc2.control-evidence (document-level, ADR-069 § SOC-002) -------

/// `soc2.control-evidence` (ADR-069 § SOC-002): an in-scope `SOC2` control
/// asserting a TSC `criterion` MUST point at operating-effectiveness
/// evidence — a `POLICY`/`ADR` cross-ref or a non-empty `evidence_link`.
/// A control whose `status` is `not-applicable` is out of scope and exempt.
/// Reuses the shared [`evidence_gap`] machinery (not a forked SOC-2-specific
/// rule); SOC 2 has no addressable/required split, so the catalog's
/// `evidence-fields = ["evidence_link"]` carries the evidence escape and the
/// `addressable` list stays empty.
///
/// Trigger field `criterion`. Params are documented on [`evidence_gap`].
/// Emits one `error` when an in-scope control cites a criterion with no
/// evidence.
pub(crate) fn check_control_evidence(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(gap) = evidence_gap(doc, params, "criterion") else {
        return Vec::new();
    };
    let criterion = &gap.value;
    let evidence_field = first_evidence_field(params);
    let (ns_list, ns_article) = evidence_namespace_list(params);
    let message = format!(
        "{}: in-scope control for criterion `{criterion}` needs operating-effectiveness evidence \
         ({ns_article} {ns_list} cross-ref) or an `{evidence_field}`",
        doc.raw_id
    );
    let help = format!(
        "cross-ref the implementing {ns_list}, or add an `{evidence_field}:` pointing at the \
         operating-effectiveness evidence (an access-review log, change ticket, or config export)"
    );
    vec![
        Diagnostic::error(CONTROL_EVIDENCE, doc.location.clone(), gap.line, 0, message)
            .with_help(help),
    ]
}

// -- iso27001.control-evidence (document-level, ADR-070 § ISO-002) ---

/// `iso27001.control-evidence` (ADR-070 § ISO-002): an in-scope `ISO27001`
/// control asserting an Annex A `control` MUST point at implementing
/// evidence — a `POLICY`/`ADR` cross-ref or a non-empty `evidence_link`. A
/// control whose `status` is `not-applicable` is out of scope and exempt
/// (the Statement of Applicability's not-applicable decision rides the
/// `status`). Reuses the shared [`evidence_gap`] machinery (not a forked
/// ISO-specific rule); ISO 27001 has no addressable/required split, so the
/// catalog's `evidence-fields = ["evidence_link"]` carries the evidence
/// escape and the `addressable` list stays empty.
///
/// Trigger field `control`. Params are documented on [`evidence_gap`]. Emits
/// one `error` when an in-scope control cites an Annex A control with no
/// evidence.
pub(crate) fn check_iso_control_evidence(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(gap) = evidence_gap(doc, params, "control") else {
        return Vec::new();
    };
    let control = &gap.value;
    let evidence_field = first_evidence_field(params);
    let (ns_list, ns_article) = evidence_namespace_list(params);
    let message = format!(
        "{}: in-scope control `{control}` needs implementing evidence \
         ({ns_article} {ns_list} cross-ref) or an `{evidence_field}`",
        doc.raw_id
    );
    let help = format!(
        "cross-ref the implementing {ns_list}, or add an `{evidence_field}:` pointing at the \
         implementing evidence — or mark the control `not-applicable` if it is out of scope"
    );
    vec![
        Diagnostic::error(ISO_CONTROL_EVIDENCE, doc.location.clone(), gap.line, 0, message)
            .with_help(help),
    ]
}

// -- nist.control-evidence (document-level, ADR-071 § NIST-002) ------

/// `nist.control-evidence` (ADR-071 § NIST-002): an in-scope `NIST80053`
/// control asserting a Rev 5 control family in `control` MUST point at
/// implementing evidence — a `POLICY`/`ADR` cross-ref or a non-empty
/// `evidence_link`. A control whose `status` is `not-applicable` is out of
/// scope and exempt. Reuses the shared [`evidence_gap`] machinery (not a
/// forked NIST-specific rule); NIST 800-53 has no addressable/required
/// split, so the catalog's `evidence-fields = ["evidence_link"]` carries the
/// evidence escape and the `addressable` list stays empty.
///
/// Trigger field `control`. Params are documented on [`evidence_gap`]. Emits
/// one `error` when an in-scope control cites a family with no evidence.
pub(crate) fn check_nist_control_evidence(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(gap) = evidence_gap(doc, params, "control") else {
        return Vec::new();
    };
    let control = &gap.value;
    let evidence_field = first_evidence_field(params);
    let (ns_list, ns_article) = evidence_namespace_list(params);
    let message = format!(
        "{}: in-scope control for family `{control}` needs implementing evidence \
         ({ns_article} {ns_list} cross-ref) or an `{evidence_field}`",
        doc.raw_id
    );
    let help = format!(
        "cross-ref the implementing {ns_list}, or add an `{evidence_field}:` pointing at the \
         implementing evidence (an SSP narrative, assessment record, or config export) — or mark \
         the control `not-applicable` if it is out of scope"
    );
    vec![
        Diagnostic::error(NIST_CONTROL_EVIDENCE, doc.location.clone(), gap.line, 0, message)
            .with_help(help),
    ]
}

// -- core.evidence-link (document-level, ADR-115 § REG-001) ----------

/// `core.evidence-link` (ADR-115 § REG-001): the **regime-neutral**
/// conditional-evidence rule. An in-scope register entry asserting an
/// obligation identifier MUST point at implementing evidence — a resolving
/// cross-ref into an evidence namespace, or a non-empty evidence field.
///
/// The fifth caller of the shared [`evidence_gap`] core, and the first that
/// is not named after one regime. `soc2.control-evidence`,
/// `iso27001.control-evidence` and `nist.control-evidence` are three codes
/// over one mechanism, forked purely so each diagnostic speaks its
/// framework's noun ("criterion", "Annex A control", "family"). That was
/// affordable for three; it does not scale to every regulation with a
/// register, so the packs added by ADR-115/116/117 share this code and name
/// their own trigger field through `field` instead.
///
/// Params: `field` (the trigger metadata key — no default, since there is no
/// neutral one), plus everything [`evidence_gap`] documents
/// (`evidence-namespaces`, `evidence-fields`, `out-of-scope-status`,
/// `addressable`, `justification-field`). Emits one `error`.
pub(crate) fn check_evidence_link(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    // No default trigger field: a namespace binding this rule must say what
    // its obligation identifier is called. `evidence_gap` returns None when
    // the field is absent from the document, so a missing `field` param makes
    // the rule inert — silence for a misconfiguration is the wrong failure,
    // so the sentinel below cannot match a real key.
    let Some(field) = params.and_then(|p| p.get("field")).and_then(Value::as_str) else {
        let line = doc.frontmatter_lines.get("id").copied().unwrap_or(0);
        return vec![Diagnostic::error(
            EVIDENCE_LINK,
            doc.location.clone(),
            line,
            0,
            format!(
                "{}: [{}.\"core.evidence-link\"] must declare `field` — the metadata key \
                 carrying the obligation identifier",
                doc.raw_id, doc.id.namespace
            ),
        )
        .with_help(
            "add `field = \"<key>\"` (e.g. `article`, `measure`, `purpose`) to the rule's \
             params block — the rule cannot guess which key is the obligation id",
        )];
    };
    let Some(gap) = evidence_gap(doc, params, field) else {
        return Vec::new();
    };
    let value = &gap.value;
    let evidence_field = first_evidence_field(params);
    let (ns_list, ns_article) = evidence_namespace_list(params);
    vec![Diagnostic::error(
        EVIDENCE_LINK,
        doc.location.clone(),
        gap.line,
        0,
        format!(
            "{}: in-scope entry for `{field}` `{value}` needs implementing evidence \
             ({ns_article} {ns_list} cross-ref) or an `{evidence_field}`",
            doc.raw_id
        ),
    )
    .with_help(format!(
        "cross-ref the implementing {ns_list}, or add an `{evidence_field}:` pointing at \
         the evidence — or mark the entry out of scope if it does not apply"
    ))]
}

// -- ddd.context-map-shape (document-level, ADR-082 § DDD-003) --------

/// `ddd.context-map-shape` (ADR-082 § DDD-003): a `CONTEXTMAP` edge doc MUST
/// connect exactly `exact_context_count` (default 2) `BOUNDEDCONTEXT` contexts
/// through `depends_on`, and its `pattern` MUST carry `upstream`/`downstream`
/// role fields when the pattern is asymmetric while omitting them when it is
/// symmetric. This is the cross-field, cardinality-aware check that
/// `core.dep-shape` (presence-only, single field) cannot express — whether the
/// direction fields are required is *conditional on `pattern`* — so it is the
/// same if/then compiled-rule shape as `soc2.control-evidence` /
/// `hipaa.safeguard-evidence`, not a new pattern for the codebase.
///
/// Params (all optional; defaults encode the Evans vocabulary the pack ships):
/// - `exact_context_count` (int, default 2): the number of `BOUNDEDCONTEXT`
///   endpoints one relationship edge connects.
/// - `context-namespace` (string, default `BOUNDEDCONTEXT`): the namespace an
///   endpoint's `depends_on` entry must be **prefixed with** to count as a
///   context.
/// - `symmetric_patterns` (string list, default `Partnership` / `Shared Kernel`
///   / `Separate Ways`): patterns that forbid the direction fields; every other
///   pattern is asymmetric and requires them.
/// - `pattern-field` (default `pattern`), `upstream-field` (default `upstream`),
///   `downstream-field` (default `downstream`): the metadata keys read.
///
/// A doc with no `pattern` field skips the direction half (presence of
/// `pattern` is `core.allowed-values`/`core.required-metadata`'s concern).
///
/// **This counts namespace prefixes; it does not resolve endpoints** (BUG-032,
/// wontfix). An edge naming `BOUNDEDCONTEXT-999` counts as a context whether or
/// not that document exists — the cardinality question ("exactly two contexts")
/// and the existence question are separate, and existence is
/// `core.dep-resolved`'s, which the `ddd` pack binds on the same namespace.
/// A consumer who materialises the pack and then prunes `core.dep-resolved` from
/// `[CONTEXTMAP].rules` restores a silent false green; nothing warns about that
/// today.
pub(crate) fn check_context_map_shape(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let str_param = |key: &str, default: &'static str| -> String {
        params
            .and_then(|p| p.get(key))
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_owned()
    };
    let ctx_ns = str_param("context-namespace", "BOUNDEDCONTEXT");
    let pattern_field = str_param("pattern-field", "pattern");
    let upstream_field = str_param("upstream-field", "upstream");
    let downstream_field = str_param("downstream-field", "downstream");
    let exact = params
        .and_then(|p| p.get("exact_context_count"))
        .and_then(Value::as_u64)
        .unwrap_or(2);
    let symmetric: Vec<String> = params
        .and_then(|p| p.get("symmetric_patterns"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "Partnership".to_owned(),
                "Shared Kernel".to_owned(),
                "Separate Ways".to_owned(),
            ]
        });

    let mut out = Vec::new();

    // Half 1 — cardinality: exactly `exact` BOUNDEDCONTEXT endpoints. This is
    // what core.dep-shape's presence-only `requires` cannot assert (DDD-003).
    let count = doc
        .depends_on
        .iter()
        .filter(|dep| dep.split_once('-').is_some_and(|(ns, _)| ns == ctx_ns))
        .count() as u64;
    let dep_line = doc
        .frontmatter_lines
        .get("depends_on")
        .or_else(|| doc.frontmatter_lines.get("id"))
        .copied()
        .unwrap_or(0);
    if count != exact {
        out.push(
            Diagnostic::error(
                CONTEXT_MAP_SHAPE,
                doc.location.clone(),
                dep_line,
                0,
                format!(
                    "{}: a context map must connect exactly {exact} {ctx_ns} contexts, found {count}",
                    doc.raw_id
                ),
            )
            .with_help(format!(
                "list exactly {exact} {ctx_ns}-<n> ids in `depends_on` — one CONTEXTMAP file is a single relationship edge between two contexts"
            )),
        );
    }

    // Half 2 — direction fields conditional on `pattern` symmetry (DDD-003).
    if let Some(pattern) = doc.metadata.get(&pattern_field).and_then(Value::as_str) {
        let is_symmetric = symmetric.iter().any(|p| p == pattern);
        let non_empty = |field: &str| {
            doc.metadata
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
        };
        let has_up = non_empty(&upstream_field);
        let has_down = non_empty(&downstream_field);
        let pattern_line = doc
            .frontmatter_lines
            .get(&pattern_field)
            .or_else(|| doc.frontmatter_lines.get("id"))
            .copied()
            .unwrap_or(0);
        if is_symmetric {
            if has_up || has_down {
                out.push(
                    Diagnostic::error(
                        CONTEXT_MAP_SHAPE,
                        doc.location.clone(),
                        pattern_line,
                        0,
                        format!(
                            "{}: symmetric pattern `{pattern}` must not declare `{upstream_field}`/`{downstream_field}` roles",
                            doc.raw_id
                        ),
                    )
                    .with_help(format!(
                        "remove the `{upstream_field}:`/`{downstream_field}:` fields — {pattern} is a symmetric relationship with no upstream/downstream direction"
                    )),
                );
            }
        } else if !has_up || !has_down {
            out.push(
                Diagnostic::error(
                    CONTEXT_MAP_SHAPE,
                    doc.location.clone(),
                    pattern_line,
                    0,
                    format!(
                        "{}: asymmetric pattern `{pattern}` must declare both `{upstream_field}` and `{downstream_field}` roles",
                        doc.raw_id
                    ),
                )
                .with_help(format!(
                    "add `{upstream_field}:` and `{downstream_field}:` fields naming which context is upstream and which is downstream"
                )),
            );
        }
    }

    out
}

// -- todo.listed (document-level) ------------------------------------

/// The shared **arming** vocabulary: the statuses that mean a document has
/// stopped moving *and settles what rests on it*. Compared case-insensitively
/// against the document's `status` field, and the default of every rule that
/// takes a `terminal` param — `todo.listed` (the exemption set), and
/// `core.acceptance-complete` / `core.dep-status` (the arming set) — so a
/// project declaring its terminal vocabulary once does not redeclare it
/// differently per rule (ADR-106 § DPS-003).
///
/// This is **not** the set that answers "does this document still have work
/// left?". That is [`is_settled_status`], which is strictly wider. The two
/// questions were unified by DPS-003 and split by `ADR-121` once `BUG-037`
/// measured the cost; see [`SETTLED_ONLY_STATUSES`] for the statuses where
/// the answers differ. Reading this list for a census question is the defect
/// `BUG-037` records.
///
/// `rejected` is deliberately absent: a rejected document has stopped
/// moving but settles nothing, so nothing may rest on it. Widening the set
/// to cover it would make it legal to depend on a decision the project
/// declined — the one edge this vocabulary exists to keep visible.
///
/// `consumed` was added 2026-08-02 (`HANDOFF-037` § A3) for the `[HANDOFF]`
/// claim protocol (`ADR-105`): a consumed handoff has been *executed*, so
/// it has stopped moving and resting on it is safe. Until then, 29 finished
/// handoffs sat in `ctxgrd status`'s ready queue.
///
/// `deferred` was proposed alongside it and **declined**, for `rejected`'s
/// reason. `ADR-105` calls it an "unexecuted terminal", and it is terminal
/// for the claim protocol — no agent should pick a deferred handoff up. But
/// this set answers a second question too, which DPS-003 deliberately
/// unified with the first: *may a finished document rest on this one?* A
/// deferred document is paused, not done; the work has not happened, and a
/// terminal document depending on it is exactly what `core.dep-status`
/// exists to surface. `rules::tests::dep_status_wording_never_infers_what_a_status_means`
/// pins that. That split shipped in `ADR-121`: `deferred` now sits in
/// [`SETTLED_ONLY_STATUSES`], settled for the census and non-arming here —
/// which is what TRM-003 predicted and why it stays out of this list.
pub(crate) const DEFAULT_TERMINAL_STATUSES: &[&str] = &[
    "accepted",
    "superseded",
    "done",
    "fixed",
    "wontfix",
    "invalid",
    "duplicate",
    "closed",
    "implemented",
    "consumed",
    "n/a",
];

/// The statuses that are **settled but not arming**: a document carrying one
/// has stopped moving and needs no further work, yet nothing may rest on it.
///
/// This is the second half of the split `ADR-120` § TRM-003 named and `ADR-121`
/// executed. The list above answers *may a document rest on this one?* — the
/// question `core.dep-status` and `core.acceptance-complete` ask. The census in
/// [`crate::status`] asks a different one: *does this still have work left?*
/// The two answers diverge on exactly the statuses here.
///
/// `rejected` is the motivating case (`BUG-037`). A rejected decision is
/// finished — there is nothing left to do and it does not belong in a work
/// queue — but it settles nothing, so an edge into it must stay reportable.
/// Adding it to `DEFAULT_TERMINAL_STATUSES` would have silenced the one edge
/// that vocabulary exists to keep visible (`ADR-106` § DPS-003).
///
/// `deferred` is the same shape, arriving from the other direction: `ADR-105`
/// calls it an "unexecuted terminal", so no agent should pick a deferred
/// handoff up (settled), but the work has not happened, so a finished document
/// depending on one is exactly what `core.dep-status` should surface (not
/// arming). It was declined from the list above in `ADR-120` § TRM-002 for
/// precisely this reason, with the split recorded as the real fix.
///
/// Never read this alone — read [`is_settled_status`], which is defined as the
/// union with `DEFAULT_TERMINAL_STATUSES` so the settled set cannot drift from
/// the arming set it extends.
pub(crate) const SETTLED_ONLY_STATUSES: &[&str] = &["rejected", "deferred"];

/// Is `status` **settled** — finished, with no work left — for the readiness
/// census? True for every arming status ([`DEFAULT_TERMINAL_STATUSES`]) plus
/// the settled-but-not-arming ones ([`SETTLED_ONLY_STATUSES`]).
///
/// The union is computed, never transcribed: a status added to the arming
/// vocabulary is settled by construction, which is the drift `BUG-037`'s fix
/// (a) warned about. Compared case-insensitively, like every other read of a
/// `status` field.
///
/// Callers wanting *may a document rest on this one?* must ask
/// `DEFAULT_TERMINAL_STATUSES` directly — this predicate is deliberately wider
/// and answers only the census question.
pub(crate) fn is_settled_status(status: &str) -> bool {
    let s = status.to_lowercase();
    DEFAULT_TERMINAL_STATUSES.contains(&s.as_str()) || SETTLED_ONLY_STATUSES.contains(&s.as_str())
}

/// `todo.listed`: a document with a non-terminal `status` MUST be mentioned
/// in the repo-root `TODO.md`. Opt-in — not in any pack default.
///
/// Terminal statuses (hardcoded) exempt a document from the check.
/// The set covers the built-in pack vocabulary; see `DEFAULT_TERMINAL_STATUSES`.
///
/// Silent when:
/// - the document has no `status` field,
/// - `TODO.md` does not exist at the lint root, or
/// - the document's status is terminal.
pub(crate) fn check_todo_listed(
    doc: &Document,
    _params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    let status = match doc.metadata.get("status").and_then(|v| v.as_str()) {
        Some(s) => s.to_lowercase(),
        None => return Vec::new(),
    };

    if DEFAULT_TERMINAL_STATUSES.contains(&status.as_str()) {
        return Vec::new();
    }

    let todo_path = root.join("TODO.md");
    let todo_content = match std::fs::read_to_string(&todo_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    if todo_content.contains(doc.raw_id.as_str()) {
        return Vec::new();
    }

    let line = doc.frontmatter_lines.get("status").copied().unwrap_or(0);
    vec![Diagnostic::warning(
        TODO_LISTED,
        doc.location.clone(),
        line,
        0,
        format!(
            "{} has status `{status}` but is not mentioned in TODO.md",
            doc.raw_id
        ),
    )
    .with_help("add a TODO.md entry for this document, or advance it to a terminal status")]
}

// -- DESIGN namespace (design.section-order, design.token-ref) --------

/// Maps a normalized heading string to its canonical 0-indexed position
/// in the DESIGN.md section order (ADR-027 § DES-002). Returns `None`
/// for unrecognized headings, which are silently skipped.
fn design_canonical_index(normalized: &str) -> Option<usize> {
    match normalized {
        "overview" | "brand & style" => Some(0),
        "colors" => Some(1),
        "typography" => Some(2),
        "layout" | "layout & spacing" => Some(3),
        "elevation & depth" | "elevation" => Some(4),
        "shapes" => Some(5),
        "components" => Some(6),
        "do's and don'ts" => Some(7),
        _ => None,
    }
}

/// `design.section-order` (DES-002): DESIGN.md H2 sections must appear
/// in the canonical order defined by the spec. Duplicate recognized
/// headings are also an error. Unrecognized headings are silently skipped.
pub(crate) fn check_design_section_order(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut last_seen: Option<(usize, String)> = None;
    let mut seen_indices = [false; 8];

    for h in ast.headings.iter().filter(|h| h.level == 2) {
        let normalized = h.text.trim().to_lowercase();
        let Some(idx) = design_canonical_index(&normalized) else {
            continue;
        };

        if seen_indices[idx] {
            out.push(
                Diagnostic::error(
                    DESIGN_SECTION_ORDER,
                    doc.location.clone(),
                    h.line,
                    0,
                    format!("duplicate section '{}'", h.text.trim()),
                )
                .with_help("see DESIGN.md spec for canonical section order"),
            );
        } else if last_seen
            .as_ref()
            .is_some_and(|(last_idx, _)| idx < *last_idx)
        {
            let last_text = last_seen.as_ref().map(|(_, t)| t.as_str()).unwrap_or("");
            out.push(
                Diagnostic::error(
                    DESIGN_SECTION_ORDER,
                    doc.location.clone(),
                    h.line,
                    0,
                    format!(
                        "section '{}' appears after '{}' — canonical order is Overview, \
                         Colors, Typography, Layout, Elevation & Depth, Shapes, Components, \
                         Do's and Don'ts",
                        h.text.trim(),
                        last_text
                    ),
                )
                .with_help("see DESIGN.md spec for canonical section order"),
            );
        } else {
            seen_indices[idx] = true;
            last_seen = Some((idx, h.text.trim().to_owned()));
        }
    }

    out
}

fn token_ref_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\{([a-zA-Z0-9_.-]+)\}").expect("valid regex"))
}

/// Recursively collect all string values from a JSON Value tree.
fn collect_strings<'a>(val: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    match val {
        serde_json::Value::String(s) => out.push(s.as_str()),
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_strings(v, out);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

/// Resolve a dotted-path token reference against the document's metadata map.
/// Returns `true` iff the path lands on a defined node — scalar OR group.
///
/// Existence is resolution: the DESIGN.md spec permits a component property
/// to reference a composite token (e.g. `typography: "{typography.label}"`,
/// where `typography.label` is a `{fontFamily, fontSize, …}` map), so a
/// reference to a mapping node is legal, not an error (ADR-027 § DES-003
/// amendment). The only genuine break — the one that makes an exported
/// Tailwind/DTCG config silently drop a value — is a path that points at
/// nothing, which returns `false` below.
fn resolve_token_path(metadata: &std::collections::BTreeMap<String, Value>, path: &str) -> bool {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.is_empty() {
        return false;
    }
    let Some(mut current) = metadata.get(parts[0]) else {
        return false;
    };
    for part in &parts[1..] {
        match current {
            Value::Object(map) => {
                let Some(next) = map.get(*part) else {
                    return false;
                };
                current = next;
            }
            // Trying to traverse into a scalar (path is deeper than the
            // tree) is a genuine broken reference.
            _ => return false,
        }
    }
    true
}

/// `design.token-ref` (DES-003): every `{path.to.token}` reference in
/// YAML frontmatter string values must point at a defined token (scalar or
/// group) at that dotted path in the same file's frontmatter. One warning
/// per unresolved token, deduplicated within a document.
///
/// Diagnostics are anchored to the line of the top-level frontmatter key
/// under which the offending reference appears ([`Document::frontmatter_lines`]),
/// falling back to line 0 when that key is unmapped — far better than a flat
/// 0,0 for the LSP squiggle and CLI caret.
pub(crate) fn check_design_token_ref(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut reported: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (key, val) in &doc.metadata {
        let mut strings: Vec<&str> = Vec::new();
        collect_strings(val, &mut strings);
        let line = doc.frontmatter_lines.get(key).copied().unwrap_or(0);

        for s in strings {
            for caps in token_ref_regex().captures_iter(s) {
                let token = caps[1].to_owned();
                if reported.contains(&token) {
                    continue;
                }
                if !resolve_token_path(&doc.metadata, &token) {
                    reported.insert(token.clone());
                    out.push(
                        Diagnostic::warning(
                            DESIGN_TOKEN_REF,
                            doc.location.clone(),
                            line,
                            0,
                            format!("token reference '{{{}}}' points at no defined token", token),
                        )
                        .with_help(format!(
                            "define '{}' in the frontmatter, or fix the reference",
                            token
                        )),
                    );
                }
            }
        }
    }

    out
}

// -- PRODUCT namespace (product.register) -----------------------------

/// Default `registers` allowlist: the two design registers PRODUCT.md may
/// declare (ADR-104 § PMD-001).
const DEFAULT_PRODUCT_REGISTERS: &[&str] = &["brand", "product"];
/// Default `platforms` allowlist. An absent `## Platform` section means
/// `web`, so the value set is only consulted when the section is present.
const DEFAULT_PRODUCT_PLATFORMS: &[&str] = &["web", "ios", "android", "adaptive"];
/// Default section required by (and only by) the `brand` register.
const DEFAULT_CONDITIONAL_SECTION: &str = "Conversion & Proof";
/// Default register value that turns the conditional section on.
const DEFAULT_CONDITIONAL_ON: &str = "brand";

/// Read a string config param, falling back to `default`.
fn product_str_param<'a>(params: Option<&'a Value>, key: &str, default: &'a str) -> &'a str {
    params
        .and_then(|p| p.get(key))
        .and_then(Value::as_str)
        .unwrap_or(default)
}

/// Read a string-array config param, falling back to `default`.
fn product_list_param<'a>(params: Option<&'a Value>, key: &str, default: &[&'a str]) -> Vec<&'a str> {
    params
        .and_then(|p| p.get(key))
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_else(|| default.to_vec())
}

/// The 1-indexed line of the first H2 whose normalized text equals `name`.
fn h2_heading_line(ast: &crate::ast::Ast, name: &str) -> u32 {
    ast.headings
        .iter()
        .filter(|h| h.level == 2)
        .find(|h| normalize_heading(&h.text) == name)
        .map(|h| h.line)
        .unwrap_or(0)
}

/// The non-empty content lines of an H2 section, trimmed.
fn h2_section_values<'a>(body_lines: &[&'a str], start: usize, end: usize) -> Vec<&'a str> {
    body_lines[start.min(body_lines.len())..end.min(body_lines.len())]
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect()
}

/// `product.register` (ADR-104): PRODUCT.md's machine-read fields.
///
/// `## Register` and `## Platform` are not prose — the impeccable skill's
/// `context.mjs` parses the first non-empty line under each and branches the
/// whole run on it (register picks the `brand.md` / `product.md` reference,
/// platform picks HIG / Material 3 / neither). This rule guards that wire
/// contract and the one structural consequence of it: `Conversion & Proof` is
/// required by the `brand` register and must be absent under `product`.
///
/// Severity tracks the consumer, never exceeding it. A value outside the
/// allowlist is an error (the consumer cannot resolve it), but trailing prose
/// under an otherwise-valid value is a warning — the spec says "no prose, no
/// commentary", yet the extractor reads the first line and carries on.
/// An absent `## Platform` is legal and means `web`.
///
/// File-level, for the same reason as `design.section-order`: PRODUCT.md is a
/// path-claimed id-less singleton and never becomes an id-keyed `Document`
/// (BUG-007). Section presence for `Register` and `Platform` is owned here
/// rather than by `core.required-headings`, so no heading is checked twice.
pub(crate) fn check_product_register(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };
    let body_lines: Vec<&str> = doc.body.lines().collect();

    let registers = product_list_param(params, "registers", DEFAULT_PRODUCT_REGISTERS);
    let platforms = product_list_param(params, "platforms", DEFAULT_PRODUCT_PLATFORMS);
    let conditional_section =
        product_str_param(params, "conditional_section", DEFAULT_CONDITIONAL_SECTION);
    let conditional_on = product_str_param(params, "conditional_on", DEFAULT_CONDITIONAL_ON);

    let mut out = Vec::new();

    // -- Register: required, bare, allowlisted.
    let mut register: Option<String> = None;
    match h2_section_window(ast, body_lines.len(), "register") {
        None => out.push(
            Diagnostic::error(
                PRODUCT_REGISTER,
                doc.location.clone(),
                0,
                0,
                "`## Register` section is missing".to_string(),
            )
            .with_help(format!(
                "add a `## Register` section holding a bare `{}`",
                registers.join("` or `")
            )),
        ),
        Some((start, end)) => {
            let line = h2_heading_line(ast, "register");
            let values = h2_section_values(&body_lines, start, end);
            match values.split_first() {
                None => out.push(
                    Diagnostic::error(
                        PRODUCT_REGISTER,
                        doc.location.clone(),
                        line,
                        0,
                        "`## Register` is empty".to_string(),
                    )
                    .with_help(format!("write a bare `{}`", registers.join("` or `"))),
                ),
                Some((first, rest)) => {
                    let value = first.to_lowercase();
                    if registers.contains(&value.as_str()) {
                        register = Some(value);
                    } else {
                        out.push(
                            Diagnostic::error(
                                PRODUCT_REGISTER,
                                doc.location.clone(),
                                line,
                                0,
                                format!("register `{first}` is not a recognized value"),
                            )
                            .with_help(format!(
                                "use a bare `{}` — the impeccable skill reads this line to \
                                 pick the register reference",
                                registers.join("` or `")
                            )),
                        );
                    }
                    if !rest.is_empty() {
                        out.push(
                            Diagnostic::warning(
                                PRODUCT_REGISTER,
                                doc.location.clone(),
                                line,
                                0,
                                "`## Register` carries prose beyond its value".to_string(),
                            )
                            .with_help(
                                "the value is a bare word — readers take the first line and \
                                 ignore the rest",
                            ),
                        );
                    }
                }
            }
        }
    }

    // -- Platform: optional (absent means `web`), bare, allowlisted.
    if let Some((start, end)) = h2_section_window(ast, body_lines.len(), "platform") {
        let line = h2_heading_line(ast, "platform");
        let values = h2_section_values(&body_lines, start, end);
        match values.split_first() {
            None => out.push(
                Diagnostic::warning(
                    PRODUCT_REGISTER,
                    doc.location.clone(),
                    line,
                    0,
                    "`## Platform` is empty — readers will treat this project as `web`"
                        .to_string(),
                )
                .with_help(format!("write a bare `{}`", platforms.join("` or `"))),
            ),
            Some((first, rest)) => {
                if !platforms.contains(&first.to_lowercase().as_str()) {
                    out.push(
                        Diagnostic::warning(
                            PRODUCT_REGISTER,
                            doc.location.clone(),
                            line,
                            0,
                            format!(
                                "platform `{first}` is not recognized — readers will treat \
                                 this project as `web`"
                            ),
                        )
                        .with_help(format!(
                            "use a bare `{}`, naming the design language the app renders, \
                             not the toolchain",
                            platforms.join("` or `")
                        )),
                    );
                }
                if !rest.is_empty() {
                    out.push(
                        Diagnostic::warning(
                            PRODUCT_REGISTER,
                            doc.location.clone(),
                            line,
                            0,
                            "`## Platform` carries prose beyond its value".to_string(),
                        )
                        .with_help(
                            "the value is a bare word — readers take the first line and \
                             ignore the rest",
                        ),
                    );
                }
            }
        }
    }

    // -- The register-conditional section. Skipped when the register did not
    // resolve: there is no decision to enforce against.
    if let Some(register) = register {
        let normalized = normalize_heading(conditional_section);
        let present = h2_section_window(ast, body_lines.len(), &normalized).is_some();
        let wanted = register == conditional_on.to_lowercase();
        if wanted && !present {
            out.push(
                Diagnostic::error(
                    PRODUCT_REGISTER,
                    doc.location.clone(),
                    0,
                    0,
                    format!(
                        "the `{register}` register requires a `## {conditional_section}` section"
                    ),
                )
                .with_help(format!("add a `## {conditional_section}` section")),
            );
        } else if !wanted && present {
            out.push(
                Diagnostic::error(
                    PRODUCT_REGISTER,
                    doc.location.clone(),
                    h2_heading_line(ast, &normalized),
                    0,
                    format!(
                        "`## {conditional_section}` belongs to the `{conditional_on}` register, \
                         but this file declares `{register}`"
                    ),
                )
                .with_help(format!(
                    "drop the section, heading included, or change the register to \
                     `{conditional_on}`"
                )),
            );
        }
    }

    out
}

// -- STYLE namespace (style.section-order, style.soul-pair) -----------

/// Maps a normalized heading string to its 0-indexed position in the
/// `STYLE.template.md` section sequence (ADR-034 § STY-002). Returns
/// `None` for unrecognized headings, which are silently skipped — STYLE.md
/// sections are optional ("cut any that do not apply").
fn style_canonical_index(normalized: &str) -> Option<usize> {
    match normalized {
        "voice principles" => Some(0),
        "vocabulary" => Some(1),
        "punctuation & formatting" => Some(2),
        "platform differences" => Some(3),
        "quick reactions" => Some(4),
        "rhetorical moves" => Some(5),
        "anti-patterns" => Some(6),
        "examples of right voice" => Some(7),
        _ => None,
    }
}

/// `style.section-order` (STY-002): duplicate recognized `##` headings are
/// an authoring mistake; recognized sections appearing after a
/// higher-template-index section get an advisory nudge toward the template
/// sequence. Both are **warnings** — unlike `design.section-order`, the
/// SOUL.md spec mandates no order (verified 2026-06-08), so the order arm
/// must never fail a spec-conformant file that reordered optional sections.
/// Unrecognized headings are silently skipped.
pub(crate) fn check_style_section_order(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut last_seen: Option<(usize, String)> = None;
    let mut seen_indices = [false; 8];

    for h in ast.headings.iter().filter(|h| h.level == 2) {
        let normalized = h.text.trim().to_lowercase();
        let Some(idx) = style_canonical_index(&normalized) else {
            continue;
        };

        if seen_indices[idx] {
            out.push(
                Diagnostic::warning(
                    STYLE_SECTION_ORDER,
                    doc.location.clone(),
                    h.line,
                    0,
                    format!("duplicate section '{}'", h.text.trim()),
                )
                .with_help("each STYLE.md section should appear at most once"),
            );
        } else if last_seen
            .as_ref()
            .is_some_and(|(last_idx, _)| idx < *last_idx)
        {
            let last_text = last_seen.as_ref().map(|(_, t)| t.as_str()).unwrap_or("");
            out.push(
                Diagnostic::warning(
                    STYLE_SECTION_ORDER,
                    doc.location.clone(),
                    h.line,
                    0,
                    format!(
                        "section '{}' appears after '{}' — STYLE.template.md order is Voice \
                         Principles, Vocabulary, Punctuation & Formatting, Platform Differences, \
                         Quick Reactions, Rhetorical Moves, Anti-Patterns, Examples of Right Voice",
                        h.text.trim(),
                        last_text
                    ),
                )
                .with_help(
                    "advisory only — the spec mandates no order; reorder to match the template \
                     sequence or ignore",
                ),
            );
        } else {
            seen_indices[idx] = true;
            last_seen = Some((idx, h.text.trim().to_owned()));
        }
    }

    out
}

/// `style.soul-pair` (STY-003): a claimed `STYLE.md` should have a `SOUL.md`
/// beside it — the spec's recommended persona-folder pairing (identity +
/// voice). A **warning** only: the spec confirms the files "can exist
/// independently" (verified 2026-06-08), so a deliberately standalone
/// STYLE.md must not be blocked. Only the sibling's existence is checked;
/// its contents are not inspected.
pub(crate) fn check_style_soul_pair(
    doc: &Document,
    _params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    let style_path = root.join(&doc.location);
    let dir = style_path.parent().unwrap_or(root);
    if dir.join("SOUL.md").is_file() {
        return Vec::new();
    }
    vec![
        Diagnostic::warning(
            STYLE_SOUL_PAIR,
            doc.location.clone(),
            0,
            0,
            "STYLE.md has no SOUL.md beside it — voice with no documented identity to deliver",
        )
        .with_help(
            "add a SOUL.md (identity) in the same folder, or ignore if this STYLE.md is \
             deliberately standalone (the spec permits it)",
        ),
    ]
}

/// `soul.sections` (SOUL-002): enforces the structural floor of the
/// [soul.md](https://github.com/aaronjmars/soul.md) template — a single `#`
/// title followed by `##` sections. The three high-signal sections the spec
/// says to fill first — Worldview, Opinions, Boundaries — must be present
/// **as `##` headings**, under an H1 title. The remaining spec sections (Who
/// I Am, Interests, Current Focus, Influences, Vocabulary, Tensions &
/// Contradictions, Pet Peeves) are optional and unrecognized `##` headings
/// pass silently — the spec instructs authors to delete sections that do not
/// apply, so v1 checks presence only (order and empty-body checks are
/// deferred, SOUL-003).
///
/// Three diagnostic shapes, all **warning** (the persona pack is advisory and
/// the sibling `style.*` rules are warnings too):
/// 1. A required section absent at any level → "missing".
/// 2. A required section present but not at `##` (the `# Worldview` mistake
///    when the author skips the title) → an actionable wrong-level message,
///    not "missing" (BUG-009: the old code reported these as missing, then
///    0.23.1 over-corrected by matching any level, which diverged from the
///    template; this is the correct fix).
/// 3. No `#` title at all → the template opens with `# [Your Name]`.
///
/// Matching is case-insensitive and trims surrounding whitespace.
pub(crate) fn check_soul_sections(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };

    // The spec's high-signal trio: "Fill Worldview, Opinions, and
    // Boundaries first; they carry the most signal."
    const REQUIRED: [&str; 3] = ["Worldview", "Opinions", "Boundaries"];

    let mut out = Vec::new();
    for name in REQUIRED {
        // Satisfied only at `##` — the template's section level.
        if ast
            .headings
            .iter()
            .any(|h| h.level == 2 && h.text.trim().eq_ignore_ascii_case(name))
        {
            continue;
        }
        // Present at the wrong level vs. absent entirely: distinguish, so the
        // author of a `# Worldview` file is told to demote it, not to add a
        // section that is plainly there.
        match ast
            .headings
            .iter()
            .find(|h| h.text.trim().eq_ignore_ascii_case(name))
        {
            Some(h) => out.push(
                Diagnostic::warning(
                    SOUL_SECTIONS,
                    doc.location.clone(),
                    h.line,
                    h.col,
                    format!(
                        "SOUL.md section '{name}' is an H{} — sections must be `##` \
                         under a single `#` title (per the soul.md template)",
                        h.level
                    ),
                )
                .with_help(
                    "demote this heading to `## ` and give the file one `# Title` \
                     heading at the top",
                ),
            ),
            None => out.push(
                Diagnostic::warning(
                    SOUL_SECTIONS,
                    doc.location.clone(),
                    0,
                    0,
                    format!("SOUL.md is missing the high-signal section '{name}'"),
                )
                .with_help(format!(
                    "the spec says fill Worldview, Opinions, and Boundaries first — they \
                     carry the most signal; add `## {name}`, or drop soul.sections if this \
                     persona deliberately omits it"
                )),
            ),
        }
    }

    // The template opens with a single `#` title (`# [Your Name]`). Frontmatter
    // `name:` is not a substitute — the spec mandates no required frontmatter.
    if !ast.headings.iter().any(|h| h.level == 1) {
        out.push(
            Diagnostic::warning(
                SOUL_SECTIONS,
                doc.location.clone(),
                0,
                0,
                "SOUL.md should open with an H1 title (`# Your Name`)".to_string(),
            )
            .with_help(
                "add a single `# ` title heading at the top of the file, as the \
                 soul.md template does",
            ),
        );
    }

    out
}

/// `soul.referenced` (ADR-047 § PRF-001) and `style.referenced`: the
/// persona-side reference check. Fires on the persona file (`SOUL.md` /
/// `STYLE.md`) and warns when an agent guide exists but none of them
/// reference it — a persona the agent guide never loads is a dead file.
/// Silent when no guide exists at all (ADR-035's standalone case, PRF-002):
/// a `SOUL.md` may be loaded directly by a runtime with no `CLAUDE.md`.
///
/// This is the persona-side complement to ADR-046's guide-side
/// `core.requires-link`. Because each file is claimed by exactly one
/// namespace (`cfg.path-conflict` on overlap; first-match in
/// [`scan_file_level`]), a rule that runs *on* `CLAUDE.md` must live under
/// `[AGENTS]` — so a persona-pack-owned check must instead fire on the
/// persona file and look outward, resolving the reference in each guide it
/// reads from disk (ADR-047 § PRF-005). Reuses the ADR-046 resolvers with
/// the guide as the link-bearing document and the persona file as target.
fn persona_referenced(
    doc: &Document,
    root: &Path,
    code: &'static str,
    name: &str,
) -> Vec<Diagnostic> {
    const GUIDES: [&str; 3] = ["CLAUDE.md", "AGENTS.md", "GEMINI.md"];
    let target = root.join(&doc.location);
    let mut any_guide = false;
    for guide in GUIDES {
        let guide_path = root.join(guide);
        if !guide_path.is_file() {
            continue;
        }
        any_guide = true;
        let Ok(body) = std::fs::read_to_string(&guide_path) else {
            continue;
        };
        let guide_doc = synthetic_document("AGENTS", guide.to_string(), body);
        if link_resolves_to(&guide_doc, root, &target)
            || import_resolves_to(&guide_doc, root, &target)
        {
            return Vec::new();
        }
    }
    if !any_guide {
        return Vec::new();
    }
    let loc = &doc.location;
    vec![
        Diagnostic::warning(
            code,
            doc.location.clone(),
            0,
            0,
            format!(
                "{name} is not referenced by any agent guide (CLAUDE.md / AGENTS.md / \
                 GEMINI.md) — the agent will not load this persona"
            ),
        )
        .with_help(format!(
            "add a reference in your CLAUDE.md or AGENTS.md — a link `[{name}]({loc})` or an \
             `@{loc}` import — or ignore if this persona is loaded directly by a runtime"
        )),
    ]
}

/// `soul.referenced` (ADR-047 § PRF-001): warn when a `SOUL.md` exists but
/// no agent guide references it. See [`persona_referenced`].
pub(crate) fn check_soul_referenced(
    doc: &Document,
    _params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    persona_referenced(doc, root, SOUL_REFERENCED, "SOUL.md")
}

/// `style.referenced` (ADR-047 § PRF-001): warn when a `STYLE.md` exists but
/// no agent guide references it. See [`persona_referenced`].
pub(crate) fn check_style_referenced(
    doc: &Document,
    _params: Option<&Value>,
    root: &Path,
) -> Vec<Diagnostic> {
    persona_referenced(doc, root, STYLE_REFERENCED, "STYLE.md")
}

fn has_h3(doc: &Document, normalized_text: &str) -> bool {
    doc.ast.as_ref().is_some_and(|ast| {
        ast.headings
            .iter()
            .any(|h| h.level == 3 && normalize_heading(&h.text) == normalized_text)
    })
}

// -- shared helpers ---------------------------------------------------

/// Normalise a heading for matching: trim, lowercase, drop a trailing
/// colon. Keeps `TODO:` and ` Current State ` matching their canonical
/// forms without over-broadening (`TODOs` stays distinct).
fn normalize_heading(text: &str) -> String {
    text.trim().trim_end_matches(':').trim().to_lowercase()
}

/// The body-line window (`start`..`end`, both 0-indexed into
/// `doc.body.lines()`, `end` exclusive) of the section under the first H2
/// whose normalized text equals `name`, or `None` when no such H2 exists.
/// `start` is the first content line after the heading; `end` is just
/// before the next H2, or the body end for the last section.
///
/// The shared heading-window-slice idiom behind the three document-level
/// section-scanning rules — `tasks.files-allowed`, `ears.clause-syntax`,
/// and `core.acceptance-complete` (ADR-056 § rule-of-three, the
/// extraction flagged in TODO.md). Each rule scans the window its own way;
/// only the window computation is shared. Headings carry 1-indexed line
/// numbers, so `heading.line` is already the 0-indexed first content line
/// (the heading itself sits at `heading.line - 1`).
fn h2_section_window(ast: &crate::ast::Ast, body_lines: usize, name: &str) -> Option<(usize, usize)> {
    let h2s: Vec<&crate::ast::Heading> = ast.headings.iter().filter(|h| h.level == 2).collect();
    let idx = h2s.iter().position(|h| normalize_heading(&h.text) == name)?;
    let start = h2s[idx].line as usize;
    let end = h2s
        .get(idx + 1)
        .map(|next| (next.line as usize).saturating_sub(1))
        .unwrap_or(body_lines);
    Some((start, end.min(body_lines)))
}

fn freshness_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?im)^[ \t]*_?[ \t]*last updated:[ \t]*(\d{4})-(\d{2})-(\d{2})")
            .expect("valid regex")
    })
}

/// Matches an `@<path>` import token: `@` at a word boundary (so emails
/// like `user@host` are skipped) followed by a non-whitespace path.
fn import_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|\s)@([^\s]+)").expect("valid regex"))
}

/// Matches a markdown list item, capturing its text (group 1). Used by
/// `tasks.files-allowed` to pull each `Files allowed` bullet.
fn list_item_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[-*]\s+(.+?)\s*$").expect("valid regex"))
}

/// Matches the `shall` keyword as a word, any case. Its absence is the
/// `MissingShall` defect (ADR-031 § ESY-003) — `shall` is the parse
/// boundary between the system name and the response.
fn shall_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bshall\b").expect("valid regex"))
}

/// Matches a list item that carries an EARS clause id, capturing the id
/// (group 1, `EARS-<NN>` or `EARS-<NN>.<M>`) and the clause text
/// (group 2). Local to `ears.clause-syntax` (ADR-031 § ESY-003): the
/// repo-wide requirement-id shape (`[A-Z]{2,}-\d{3,}`) recognises
/// neither the two-digit form nor the dotted refinement. Tolerates
/// emphasis markers around the id and a `:`/dash separator.
fn ears_item_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[*_`]*(EARS-\d+(?:\.\d+)?)[*_`]*\s*[:.—–-]?\s*(.+)$").expect("valid regex")
    })
}

fn checklist_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*[-*]\s+\[[ xX]\]").expect("valid regex"))
}

/// Matches an open `- [ ]` (unchecked) checkbox item. Used by
/// `todo.sections` to distinguish open work from completed `- [x]`.
fn open_checkbox_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*[-*]\s+\[ \]").expect("valid regex"))
}

/// Find the freshness date and the 1-indexed line it sits on.
fn find_freshness(body: &str) -> Option<(u32, (i64, i64, i64))> {
    let caps = freshness_regex().captures(body)?;
    let m = caps.get(0)?;
    let line = body[..m.start()].bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    let y = caps[1].parse().ok()?;
    let mo = caps[2].parse().ok()?;
    let d = caps[3].parse().ok()?;
    Some((line, (y, mo, d)))
}

fn has_checklist_item(body: &str) -> bool {
    checklist_regex().is_match(body)
}

/// Parse a bare `YYYY-MM-DD` date into a `(year, month, day)` triple.
/// `None` when the string is not exactly that shape. Shared by
/// `core.calendar-freshness` (PIN-008); the `todo.freshness` preset keeps
/// its own `Last updated:` body-line scanner ([`find_freshness`]).
fn parse_ymd(s: &str) -> Option<(i64, i64, i64)> {
    let caps = ymd_regex().captures(s)?;
    let y = caps[1].parse().ok()?;
    let mo = caps[2].parse().ok()?;
    let d = caps[3].parse().ok()?;
    Some((y, mo, d))
}

fn ymd_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d{4})-(\d{2})-(\d{2})").expect("valid regex"))
}

/// Whole days between the given calendar date and today (UTC). `None`
/// when the system clock is before the Unix epoch (not expected).
fn days_since(ymd: (i64, i64, i64)) -> Option<i64> {
    let (y, m, d) = ymd;
    let then = days_from_civil(y, m, d);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let today = (now.as_secs() / 86_400) as i64;
    Some(today - then)
}

/// Days since the Unix epoch for a proleptic-Gregorian civil date.
/// Howard Hinnant's `days_from_civil` algorithm — stdlib-only, no
/// calendar crate.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Ast, Heading, Link};
    use std::collections::BTreeMap;

    /// Prose-drift guard for the third copy of the `research.type` vocabulary
    /// (ADR-095). The enforcement const `RESEARCH_TYPES` and the `research.type`
    /// json metadata now share one source; the pack.toml comment documents the
    /// same vocabulary in prose. Deriving that comment from the metadata (full
    /// PDOC-003) would need the config generator to inject it — out of scope
    /// here — so this cheaply asserts every enforced genre is still named in the
    /// pack comment, catching drift without wiring generation.
    #[test]
    fn research_pack_comment_lists_every_research_type() {
        let pack = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packs/research/pack.toml"
        ))
        .expect("packs/research/pack.toml is readable");
        for genre in RESEARCH_TYPES {
            assert!(
                pack.contains(genre),
                "packs/research/pack.toml comment must mention the `{genre}` research.type genre \
                 (prose drift from RESEARCH_TYPES)"
            );
        }
    }

    fn doc(location: &str, body: &str, headings: Vec<(u8, &str)>) -> Document {
        Document {
            id: "AGENTS-1"
                .parse()
                .unwrap_or_else(|_| "ADR-1".parse().unwrap()),
            raw_id: String::new(),
            location: location.to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast {
                headings: headings
                    .into_iter()
                    .map(|(level, text)| Heading {
                        level,
                        text: text.to_owned(),
                        line: 1,
                        col: 1,
                    })
                    .collect(),
                ..Ast::default()
            }),
            body: body.to_owned(),
        }
    }

    /// A `doc` whose parsed AST carries one markdown link with `href` —
    /// the shape `references_root_todo_link` reads for the lazy form.
    fn doc_with_link(location: &str, body: &str, href: &str) -> Document {
        let mut d = doc(location, body, vec![]);
        if let Some(ast) = d.ast.as_mut() {
            ast.links.push(Link {
                href: href.to_owned(),
                text: "TODO.md".to_owned(),
                line: 1,
                col: 1,
            });
        }
        d
    }

    #[test]
    fn forbidden_current_state_heading_errors() {
        let d = doc(
            "CLAUDE.md",
            "## Current State\n",
            vec![(2, "Current State")],
        );
        let diags = forbidden_heading_diags(&d);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "agents.context-headings");
        assert_eq!(
            diags[0].message,
            "instruction files must not contain a 'Current State' heading — volatile state belongs in TODO.md"
        );
    }

    #[test]
    fn forbidden_todo_heading_matches_case_and_colon() {
        let d = doc("AGENTS.md", "### todo:\n", vec![(3, "todo:")]);
        assert_eq!(forbidden_heading_diags(&d).len(), 1);
    }

    #[test]
    fn unrelated_headings_pass() {
        let d = doc(
            "CLAUDE.md",
            "## Build\n## Conventions\n",
            vec![(2, "Build"), (2, "Conventions"), (2, "TODOs")],
        );
        assert_eq!(forbidden_heading_diags(&d).len(), 0);
    }

    #[test]
    fn freshness_parses_italic_and_line() {
        let body = "# TODO\n\n_Last updated: 2026-05-26 16:45_\n\n### TODO\n- [ ] ship it\n";
        let (line, ymd) = find_freshness(body).expect("freshness present");
        assert_eq!(line, 3);
        assert_eq!(ymd, (2026, 5, 26));
    }

    #[test]
    fn freshness_absent_is_none() {
        assert_eq!(find_freshness("# TODO\n\n### TODO\n- [ ] x\n"), None);
    }

    #[test]
    fn checklist_detection() {
        assert!(has_checklist_item("- [ ] open\n"));
        assert!(has_checklist_item("  - [x] done\n"));
        assert!(!has_checklist_item("- plain bullet\n"));
    }

    #[test]
    fn days_from_civil_matches_known_epoch_anchors() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1), 10_957);
        assert_eq!(days_from_civil(2026, 5, 26), 20_599);
    }

    #[test]
    fn state_file_missing_freshness_and_todo_errors() {
        let d = doc("TODO.md", "# Notes\n\nsome prose\n", vec![(1, "Notes")]);
        let root = Path::new(".");
        let mut diags = check_todo_freshness(&d, None, root);
        diags.extend(check_todo_structure(&d, None, root));
        // missing freshness (error), missing ### TODO (error), missing ### Context (warning)
        let errors = diags
            .iter()
            .filter(|x| x.message.contains("freshness") || x.message.contains("`### TODO` section"))
            .count();
        assert_eq!(errors, 2);
        assert!(diags.iter().any(|x| x.message.contains("`### Context`")));
    }

    #[test]
    fn state_file_well_formed_passes() {
        let body = "# TODO\n\n_Last updated: 2026-05-26_\n\n### Context\n- fact\n\n### TODO\n- [ ] do it\n";
        let d = doc(
            "TODO.md",
            body,
            vec![(1, "TODO"), (3, "Context"), (3, "TODO")],
        );
        // stale_days huge so the recent date never warns.
        let params = serde_json::json!({"stale_days": 100000});
        let root = Path::new(".");
        let mut diags = check_todo_freshness(&d, Some(&params), root);
        diags.extend(check_todo_structure(&d, Some(&params), root));
        assert_eq!(diags.len(), 0, "unexpected diagnostics: {diags:?}");
    }

    #[test]
    fn state_file_todo_without_checklist_errors() {
        let body = "_Last updated: 2026-05-26_\n\n### Context\n- x\n\n### TODO\nprose, no boxes\n";
        let d = doc("TODO.md", body, vec![(3, "Context"), (3, "TODO")]);
        let params = serde_json::json!({"stale_days": 100000});
        let root = Path::new(".");
        let mut diags = check_todo_freshness(&d, Some(&params), root);
        diags.extend(check_todo_structure(&d, Some(&params), root));
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("no checklist items"));
    }

    #[test]
    fn oversize_warns_over_budget_and_passes_under() {
        let d = doc("CLAUDE.md", "one two three four five", vec![]);
        let tight = serde_json::json!({"max_words": 3});
        let warn = oversize_diag(&d, Some(&tight)).expect("over budget warns");
        assert_eq!(warn.severity, crate::diagnostic::Severity::Warning);
        assert!(warn.message.contains("5 words (limit 3)"));
        let loose = serde_json::json!({"max_words": 100});
        assert!(oversize_diag(&d, Some(&loose)).is_none());
    }

    #[test]
    fn dangling_import_warns_only_for_missing_repo_relative_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/architecture.md"), "# arch\n").unwrap();

        let body = "See @docs/architecture.md and @docs/missing.md.\n\
                    Email chris@aktagon.com, ping @teamlead, scope @internal.\n";
        let d = doc("CLAUDE.md", body, vec![]);
        let diags = dangling_import_diags(&d, root);

        // Only the missing repo-relative file warns: the existing file, the
        // email, the dotless `@teamlead`/`@internal` are all skipped.
        assert_eq!(diags.len(), 1, "unexpected: {diags:?}");
        assert!(diags[0].message.contains("`@docs/missing.md`"));
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
    }

    #[test]
    fn import_paths_are_line_anchored() {
        // Only lines whose first non-whitespace token is `@<path>` count.
        let body =
            "@TODO.md\n  @../TODO.md\nSee @TODO.md mid-prose, not an import.\nEmail a@b.com\n";
        let d = synthetic_document("AGENTS", "CLAUDE.md".to_string(), body.to_string());
        assert_eq!(import_paths(&d), vec!["TODO.md", "../TODO.md"]);
    }

    #[test]
    fn scan_file_level_retains_synthetic_documents() {
        // ADR-103 § SRF-001: a path-claimed id-less file yields one entry
        // in `scan.documents` with its location set, an empty `raw_id`,
        // and a populated AST + frontmatter metadata.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(
            root.join("ctxgrd.toml"),
            "[AGENTS]\npaths = [\"CLAUDE.md\"]\nrules = [\"agents.context-budget\"]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("CLAUDE.md"),
            "---\ntitle: Build guide\n---\n# Build guide\n\nRun make check.\n",
        )
        .unwrap();

        let config = crate::config::load_with_global(root, None).expect("config loads");
        let claims = PathClaims::from_config(&config);
        let scan = scan_file_level(root, &config, &claims).expect("scan succeeds");

        assert_eq!(scan.files_linted, 1);
        assert_eq!(scan.documents.len(), 1, "synthetic document retained");
        let doc = &scan.documents[0];
        assert_eq!(doc.location, "CLAUDE.md");
        assert_eq!(doc.raw_id, "");
        assert_eq!(doc.id.namespace, "AGENTS");
        assert_eq!(
            doc.metadata.get("title"),
            Some(&serde_json::Value::String("Build guide".to_string()))
        );
        let ast = doc.ast.as_ref().expect("ast populated");
        assert_eq!(ast.headings[0].text, "Build guide");
    }

    #[test]
    fn dangling_import_ignores_tokens_in_fenced_code() {
        // BUG-006: a decorator in a fenced example is not an import —
        // Claude Code does not resolve tokens inside code blocks.
        let tmp = tempfile::tempdir().expect("tempdir");
        let body = "# Test\n\n```python\n@mcp.tool(\n    description=\"x\"\n)\ndef f(): pass\n```\n";
        let d = synthetic_document("AGENTS", "CLAUDE.md".to_string(), body.to_string());
        let diags = dangling_import_diags(&d, tmp.path());
        assert!(diags.is_empty(), "fenced code must not warn: {diags:?}");
    }

    #[test]
    fn dangling_import_reads_tokens_in_inline_code() {
        // Reverses BUG-006's inline half (BUG-029 follow-up). Claude Code
        // resolves a backticked `@path`, so a dangling one inside a span is
        // a real lost reference and must warn. BUG-006's masking was right
        // about fenced blocks — see `dangling_import_ignores_tokens_in_
        // fenced_code`, still passing — and unmeasured about spans.
        let tmp = tempfile::tempdir().expect("tempdir");
        let body = "Write `import @docs/missing.md` style imports.\n";
        let d = synthetic_document("AGENTS", "CLAUDE.md".to_string(), body.to_string());
        let diags = dangling_import_diags(&d, tmp.path());
        assert_eq!(diags.len(), 1, "inline-code import must warn: {diags:?}");
        assert!(diags[0].message.contains("`@docs/missing.md`"));
    }

    #[test]
    fn dangling_import_skips_backtick_adjacent_token() {
        // The guard that replaces span masking: a backtick directly against
        // the `@` is not whitespace, so the token never matches. This is the
        // form documentation actually writes, and it is what keeps a
        // CLAUDE.md describing this rule from flagging itself.
        let tmp = tempfile::tempdir().expect("tempdir");
        let body = "Never write `@docs/missing.md` as an import.\n";
        let d = synthetic_document("AGENTS", "CLAUDE.md".to_string(), body.to_string());
        let diags = dangling_import_diags(&d, tmp.path());
        assert!(
            diags.is_empty(),
            "a backtick-adjacent token is not an import: {diags:?}"
        );
    }

    #[test]
    fn headings_rule_rejects_fenced_todo_import() {
        // A fenced example containing an own-line `@TODO.md` is not a
        // real import; the rule must still error.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("TODO.md"), "# TODO\n").unwrap();
        let body = "# Project\n\n```markdown\n@TODO.md\n```\n";
        let d = synthetic_document("AGENTS", "CLAUDE.md".to_string(), body.to_string());
        let diags = check_context_headings(&d, None, root);
        assert_eq!(
            diags.len(),
            1,
            "fenced import must not satisfy the rule: {diags:?}"
        );
    }

    #[test]
    fn headings_rule_passes_root_claude_with_todo_link() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("TODO.md"), "# TODO\n").unwrap();
        let d = doc_with_link("CLAUDE.md", "# Project\n\n[TODO.md](TODO.md)\n", "TODO.md");
        assert!(
            check_context_headings(&d, None, root).is_empty(),
            "root CLAUDE.md with a lazy TODO.md link must pass clean"
        );
    }

    #[test]
    fn headings_rule_passes_nested_claude_with_parent_relative_link() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("TODO.md"), "# TODO\n").unwrap();
        std::fs::create_dir_all(root.join("cli")).unwrap();
        // `[TODO.md](../TODO.md)` resolves from cli/ to the root TODO.md.
        let d = doc_with_link("cli/CLAUDE.md", "# CLI\n\n[TODO.md](../TODO.md)\n", "../TODO.md");
        let diags = check_context_headings(&d, None, root);
        assert!(
            diags.is_empty(),
            "nested CLAUDE.md with a file-relative link must pass: {diags:?}"
        );
    }

    #[test]
    fn headings_rule_warns_on_eager_import() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("TODO.md"), "# TODO\n").unwrap();
        // `@TODO.md` keeps TODO.md discoverable but eagerly — warn, don't
        // error, and suggest the lazy link.
        let d = doc("CLAUDE.md", "# Project\n\n@TODO.md\n", vec![]);
        let diags = check_context_headings(&d, None, root);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].code, "agents.context-headings");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
        assert!(
            diags[0].help.as_deref().unwrap().contains("[TODO.md](TODO.md)"),
            "warning should suggest the lazy link: {:?}",
            diags[0].help
        );
    }

    #[test]
    fn headings_rule_warns_on_inline_eager_import() {
        // BUG-029 case A. Claude Code resolves a mid-sentence `@TODO.md`
        // exactly as it resolves an own-line one, so this is eager. Under
        // the old own-line grammar the import was invisible here and the
        // rule fell through to "does not link to it" — an orphan error
        // about a file the agent loads on every turn.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("TODO.md"), "# TODO\n").unwrap();
        let d = synthetic_document(
            "AGENTS",
            "CLAUDE.md".to_string(),
            "# Project\n\nVolatile state lives in @TODO.md, read it first.\n".to_string(),
        );
        let diags = check_context_headings(&d, None, root);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
        assert!(
            diags[0].message.contains("eagerly"),
            "inline import must raise the eager warning, not the orphan error: {}",
            diags[0].message
        );
    }

    #[test]
    fn headings_rule_warns_on_inline_import_despite_lazy_link() {
        // BUG-029 case C — the live `llmkit-bug042` shape, and the worst of
        // the three: an inline import plus the lazy link the help line
        // suggests. The link satisfied the discoverability half and nothing
        // was left to notice the import, so the file linted clean while the
        // reader loaded a 209k-char TODO.md into every session prefix.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("TODO.md"), "# TODO\n").unwrap();
        let d = synthetic_document(
            "AGENTS",
            "CLAUDE.md".to_string(),
            "# Project\n\nState lives in @TODO.md.\n\n[TODO.md](TODO.md)\n".to_string(),
        );
        let diags = check_context_headings(&d, None, root);
        assert_eq!(
            diags.len(),
            1,
            "a lazy link must not cancel an eager import: {diags:?}"
        );
        assert!(diags[0].message.contains("eagerly"), "{}", diags[0].message);
    }

    #[test]
    fn headings_rule_warns_on_import_inside_inline_code() {
        // BUG-029 follow-up. A code span does not stop Claude Code from
        // resolving the token, so it must not stop the rule either — the
        // file is loaded eagerly and the lazy link alongside it does not
        // undo that. Was clean before this change; the owner's call.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("TODO.md"), "# TODO\n").unwrap();
        let d = synthetic_document(
            "AGENTS",
            "CLAUDE.md".to_string(),
            "# Project\n\nDo not write `state in @TODO.md` inline.\n\n[TODO.md](TODO.md)\n"
                .to_string(),
        );
        let diags = check_context_headings(&d, None, root);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("eagerly"), "{}", diags[0].message);
    }

    #[test]
    fn headings_rule_ignores_backtick_adjacent_todo_mention() {
        // The surviving self-reference guard, and the one this repo's own
        // CLAUDE.md relies on: `@TODO.md` written with the backtick against
        // the `@` is prose about the syntax, not an import.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("TODO.md"), "# TODO\n").unwrap();
        let d = synthetic_document(
            "AGENTS",
            "CLAUDE.md".to_string(),
            "# Project\n\nLinked, not `@TODO.md`-imported.\n\n[TODO.md](TODO.md)\n".to_string(),
        );
        assert!(
            check_context_headings(&d, None, root).is_empty(),
            "a backtick-adjacent mention must stay clean"
        );
    }

    #[test]
    fn wide_import_paths_are_not_line_anchored() {
        // Counterpart to `import_paths_are_line_anchored`. The reader's
        // grammar takes the token anywhere on the line, while keeping the
        // conservative filters: emails are skipped (no whitespace before
        // `@`) and a dotless word is not file-like once its trailing
        // sentence punctuation is stripped.
        let body = "@TODO.md\nSee @docs/plan.md mid-prose.\nEmail a@b.com\n@internal.\n";
        let d = synthetic_document("AGENTS", "CLAUDE.md".to_string(), body.to_string());
        assert_eq!(wide_import_paths(&d), vec!["TODO.md", "docs/plan.md"]);
    }

    #[test]
    fn headings_rule_errors_when_no_reference_resolves_and_suggests_relative_link() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("TODO.md"), "# TODO\n").unwrap();
        std::fs::create_dir_all(root.join("cli")).unwrap();
        // `@TODO.md` in cli/ resolves to nonexistent cli/TODO.md, and there
        // is no link — neither form references the root TODO.md, so the rule
        // errors and the help suggests the file-relative lazy link.
        let d = doc("cli/CLAUDE.md", "# CLI\n\n@TODO.md\n", vec![]);
        let diags = check_context_headings(&d, None, root);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].code, "agents.context-headings");
        assert!(
            diags[0]
                .help
                .as_deref()
                .unwrap()
                .contains("[TODO.md](../TODO.md)"),
            "help should suggest the parent-relative lazy link: {:?}",
            diags[0].help
        );
    }

    #[test]
    fn headings_rule_prose_mention_does_not_satisfy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("TODO.md"), "# TODO\n").unwrap();
        // A mid-prose mention is not an import — the rule must still error.
        let d = doc(
            "CLAUDE.md",
            "# Project\n\nState lives in @TODO.md, see the docs.\n",
            vec![],
        );
        let diags = check_context_headings(&d, None, root);
        assert_eq!(diags.len(), 1, "prose mention must not satisfy: {diags:?}");
        assert_eq!(diags[0].code, "agents.context-headings");
    }

    /// Build a TODO.md-shaped doc whose AST headings carry the real line
    /// numbers of the `## …` lines in `body`. `todo.sections` slices the
    /// body by heading line, so the headings cannot all sit on line 1.
    fn todo_doc(body: &str) -> Document {
        let headings: Vec<Heading> = body
            .lines()
            .enumerate()
            .filter_map(|(idx, line)| {
                let trimmed = line.trim_start();
                let level = trimmed.bytes().take_while(|&b| b == b'#').count();
                if !(1..=6).contains(&level) {
                    return None;
                }
                let text = trimmed[level..].trim().to_owned();
                if text.is_empty() {
                    return None;
                }
                Some(Heading {
                    level: level as u8,
                    text,
                    line: (idx + 1) as u32,
                    col: 1,
                })
            })
            .collect();
        Document {
            id: "ADR-1".parse().unwrap(),
            raw_id: String::new(),
            location: "TODO.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast {
                headings,
                ..Ast::default()
            }),
            body: body.to_owned(),
        }
    }

    #[test]
    fn sections_well_formed_passes() {
        let body = "\
# TODO

_Last updated: 2026-05-29_

## Now

- [ ] ship it

## Next

- [ ] follow-up

## Later

- [ ] benchmarks

## Done

- [x] CI green
";
        let diags = check_todo_sections(&todo_doc(body), None, Path::new("."));
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    }

    #[test]
    fn sections_missing_one_errors_with_shape_message() {
        // Now/Next/Done — Later omitted.
        let body = "\
# TODO

## Now
- [ ] a

## Next
- [ ] b

## Done
- [x] c
";
        let diags = check_todo_sections(&todo_doc(body), None, Path::new("."));
        assert_eq!(diags.len(), 1, "unexpected: {diags:?}");
        assert!(
            diags[0]
                .message
                .contains("exactly `## Now`, `## Next`, `## Later`, `## Done`"),
            "{}",
            diags[0].message
        );
    }

    #[test]
    fn sections_out_of_order_errors() {
        let body = "\
# TODO

## Now
- [ ] a

## Later
- [ ] b

## Next
- [ ] c

## Done
- [x] d
";
        let diags = check_todo_sections(&todo_doc(body), None, Path::new("."));
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("in that order"));
    }

    #[test]
    fn sections_extra_h2_errors() {
        let body = "\
# TODO

## Now
- [ ] a

## Next
- [ ] b

## Later
- [ ] c

## Notes
extra section

## Done
- [x] d
";
        let diags = check_todo_sections(&todo_doc(body), None, Path::new("."));
        assert_eq!(diags.len(), 1, "extra H2 must be a shape error");
        assert!(diags[0].message.contains("`## notes`"));
    }

    #[test]
    fn sections_now_empty_errors() {
        let body = "\
# TODO

## Now

## Next
- [ ] b

## Later
- [ ] c

## Done
- [x] d
";
        let diags = check_todo_sections(&todo_doc(body), None, Path::new("."));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("`## Now` has no open"));
    }

    #[test]
    fn sections_done_with_open_item_errors() {
        let body = "\
# TODO

## Now
- [ ] a

## Next
- [ ] b

## Later
- [ ] c

## Done
- [x] shipped
- [ ] stragglers
";
        let diags = check_todo_sections(&todo_doc(body), None, Path::new("."));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("`## Done` contains an open"));
    }

    #[test]
    fn sections_done_empty_passes() {
        // Fresh project: nothing finished yet, but the shape is still
        // required. Done with zero items must not error.
        let body = "\
# TODO

## Now
- [ ] a

## Next
- [ ] b

## Later
- [ ] c

## Done
";
        let diags = check_todo_sections(&todo_doc(body), None, Path::new("."));
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    // -- tasks.files-allowed (ADR-022) --------------------------------

    /// A TASK-shaped doc whose AST headings carry the real line numbers of
    /// the `## …` lines in `body` — `tasks.files-allowed` slices the
    /// `Files allowed` section by heading line.
    fn task_doc(body: &str) -> Document {
        let headings: Vec<Heading> = body
            .lines()
            .enumerate()
            .filter_map(|(idx, line)| {
                let trimmed = line.trim_start();
                let level = trimmed.bytes().take_while(|&b| b == b'#').count();
                if !(1..=6).contains(&level) {
                    return None;
                }
                let text = trimmed[level..].trim().to_owned();
                if text.is_empty() {
                    return None;
                }
                Some(Heading {
                    level: level as u8,
                    text,
                    line: (idx + 1) as u32,
                    col: 1,
                })
            })
            .collect();
        Document {
            id: "ADR-1".parse().unwrap(),
            raw_id: String::new(),
            location: "docs/tasks/TASK-1.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast {
                headings,
                ..Ast::default()
            }),
            body: body.to_owned(),
        }
    }

    #[test]
    fn files_allowed_existing_file_and_new_file_in_existing_dir_pass() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/routes")).unwrap();
        std::fs::write(root.join("src/routes/projects.ts"), "").unwrap();

        // `projects.ts` exists; `project.ts` is new but its dir exists;
        // `top.ts` is a new root-level file (parent == root, exists).
        let body = "\
# TASK

## Goal
ship it

## Files allowed
- `src/routes/projects.ts`
- `src/routes/project.ts`
- top.ts

## Acceptance
tests pass
";
        let diags = check_task_files_allowed(&task_doc(body), None, root);
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    }

    #[test]
    fn files_allowed_missing_parent_dir_warns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();

        let body = "\
# TASK

## Files allowed
- src/routes/projects.ts
";
        let diags = check_task_files_allowed(&task_doc(body), None, root);
        assert_eq!(diags.len(), 1, "unexpected: {diags:?}");
        assert_eq!(diags[0].code, "tasks.files-allowed");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
        assert!(diags[0].message.contains("`src/routes/projects.ts`"));
        // Pointed at the bullet's line (line 4 of the body).
        assert_eq!(diags[0].line, Some(4));
    }

    #[test]
    fn files_allowed_prose_bullet_is_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        let body = "\
# TASK

## Files allowed
- All files in src
";
        let diags = check_task_files_allowed(&task_doc(body), None, root);
        assert!(diags.is_empty(), "prose bullet must not warn: {diags:?}");
    }

    #[test]
    fn files_allowed_absent_section_is_silent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let body = "# TASK\n\n## Goal\nship it\n";
        let diags = check_task_files_allowed(&task_doc(body), None, root);
        assert!(diags.is_empty(), "no section means silent: {diags:?}");
    }

    // -- agent.assigned (ADR-057) --------------------------------------

    /// A TASK document carrying an `agents` metadata list. `frontmatter_lines`
    /// pins `agents` to line 5 so the diagnostic's line is asserted exactly.
    fn assigned_doc(agents: &[&str]) -> Document {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "agents".to_owned(),
            Value::Array(agents.iter().map(|s| Value::String((*s).to_owned())).collect()),
        );
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("agents".to_owned(), 5);
        Document {
            id: "TASK-900".parse().unwrap(),
            raw_id: "TASK-900".to_owned(),
            location: "docs/tasks/TASK-900-probe.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: Some(Ast::default()),
            body: String::new(),
        }
    }

    /// Write a Claude-style agent file (frontmatter `name:`) under `dir`.
    fn write_claude_agent(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.md")),
            format!(
                "---\nname: {name}\ndescription: Review code for quality, architecture, and project standards.\n---\n# {name}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn agent_assigned_resolves_a_file_agent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_claude_agent(&root.join(".claude/agents"), "code-reviewer");

        let params = serde_json::json!({ "search_dirs": [".claude/agents"] });
        let diags = check_agent_assigned(&assigned_doc(&["code-reviewer"]), Some(&params), root);
        assert!(diags.is_empty(), "a real file agent must resolve: {diags:?}");
    }

    #[test]
    fn agent_assigned_silent_when_agents_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let doc = Document {
            id: "TASK-901".parse().unwrap(),
            raw_id: "TASK-901".to_owned(),
            location: "docs/tasks/TASK-901.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast::default()),
            body: String::new(),
        };
        let diags = check_agent_assigned(&doc, None, tmp.path());
        assert!(
            diags.is_empty(),
            "presence is core.required-metadata's job, not this rule's: {diags:?}"
        );
    }

    #[test]
    fn agent_assigned_unresolved_suggests_nearest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_claude_agent(&root.join(".claude/agents"), "code-reviewer");

        let params = serde_json::json!({ "search_dirs": [".claude/agents"] });
        let diags = check_agent_assigned(&assigned_doc(&["code-viewer"]), Some(&params), root);
        assert_eq!(diags.len(), 1, "unexpected: {diags:?}");
        assert_eq!(diags[0].code, "agent.assigned");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Error);
        assert_eq!(diags[0].line, Some(5), "points at the `agents` frontmatter line");
        assert!(
            diags[0].message.contains("code-viewer"),
            "names the unresolved agent: {:?}",
            diags[0].message
        );
        assert_eq!(
            diags[0].help.as_deref(),
            Some("did you mean `code-reviewer`?"),
            "help must suggest the nearest match"
        );
    }

    #[test]
    fn agent_assigned_unrelated_name_gets_generic_help() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_claude_agent(&root.join(".claude/agents"), "code-reviewer");

        let params = serde_json::json!({ "search_dirs": [".claude/agents"] });
        let diags = check_agent_assigned(&assigned_doc(&["nonexistent-owner"]), Some(&params), root);
        assert_eq!(diags.len(), 1, "unexpected: {diags:?}");
        assert!(
            diags[0]
                .help
                .as_deref()
                .unwrap()
                .contains("builtin_agents"),
            "no near match → generic help naming the allow-list: {:?}",
            diags[0].help
        );
    }

    #[test]
    fn agent_assigned_builtin_requires_allowlist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_claude_agent(&root.join(".claude/agents"), "code-reviewer");

        // `Explore` is a harness built-in with no file: unresolved when the
        // allow-list is empty (AOT-002).
        let without = serde_json::json!({ "search_dirs": [".claude/agents"] });
        let diags = check_agent_assigned(&assigned_doc(&["Explore"]), Some(&without), root);
        assert_eq!(diags.len(), 1, "built-in must fail without allow-list: {diags:?}");

        // Listed in `builtin_agents`, it resolves clean.
        let with = serde_json::json!({
            "search_dirs": [".claude/agents"],
            "builtin_agents": ["Explore", "general-purpose"]
        });
        let clean = check_agent_assigned(&assigned_doc(&["Explore"]), Some(&with), root);
        assert!(clean.is_empty(), "allow-listed built-in resolves: {clean:?}");
    }

    #[test]
    fn agent_assigned_filename_name_source_resolves_by_stem() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // opencode agent files have no `name:` field — the stem is the name.
        let dir = root.join(".opencode/agent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("reviewer.md"),
            "---\ndescription: Reviews changes before merge.\n---\n# reviewer\n",
        )
        .unwrap();

        let params = serde_json::json!({
            "search_dirs": [".opencode/agent"],
            "name_source": "filename"
        });
        let diags = check_agent_assigned(&assigned_doc(&["reviewer"]), Some(&params), root);
        assert!(diags.is_empty(), "filename stem must resolve: {diags:?}");
    }

    #[test]
    fn edit_distance_counts_single_edits() {
        assert_eq!(edit_distance("code-reviewer", "code-reviewer"), 0);
        assert_eq!(edit_distance("code-viewer", "code-reviewer"), 2);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    // -- ears.clause-syntax (ADR-031) ----------------------------------

    /// EARS tests reuse the heading-aware doc builder. The rule is
    /// namespace-agnostic — config placement scopes it (ADR-031 § ESY-004
    /// as amended), so the builder's TASK/ADR identity is irrelevant.
    fn ears_diags(body: &str) -> Vec<Diagnostic> {
        check_ears_clauses(&task_doc(body), None, Path::new("."))
    }

    #[test]
    fn ears_six_well_formed_patterns_pass() {
        // One clause per ESY-002 pattern: ubiquitous, event-driven,
        // unwanted-behavior, state-driven, optional-feature, and two
        // complex compositions (WHILE+WHEN, WHILE+IF+THEN).
        let body = "\
# SPEC

## Requirements
- EARS-01: The linter shall exit non-zero when any error diagnostic is emitted.
- EARS-02: WHEN a watched file changes, the linter shall re-lint the file.
- EARS-03: IF the frontmatter fence is missing, THEN the linter shall emit core.frontmatter.
- EARS-04: WHILE watch mode is active, the linter shall keep the diagnostics panel current.
- EARS-05: WHERE LSP support is enabled, the linter shall publish diagnostics on save.
- EARS-06: WHILE watch mode is active, WHEN ctxgrd.toml changes, the linter shall reload the config.
- EARS-07: WHILE watch mode is active, IF the config fails to parse, THEN the linter shall keep the last good config.
";
        let diags = ears_diags(body);
        assert!(diags.is_empty(), "all six patterns must pass: {diags:?}");
    }

    #[test]
    fn ears_title_case_keywords_pass() {
        // ESY-003: keywords accepted in all-caps or title case — the
        // title-case form is what the EARS originals use.
        let body = "\
# SPEC

## Requirements
- EARS-01: When a watched file changes, the linter shall re-lint the file.
- EARS-02: If the frontmatter fence is missing, Then the linter shall emit core.frontmatter.
- EARS-03: While watch mode is active, the linter shall keep the diagnostics panel current.
- EARS-04: Where LSP support is enabled, the linter shall publish diagnostics on save.
";
        let diags = ears_diags(body);
        assert!(diags.is_empty(), "title-case keywords must pass: {diags:?}");
    }

    #[test]
    fn ears_missing_shall_warns() {
        let body = "\
# SPEC

## Requirements
- EARS-01: WHEN a watched file changes, the linter re-lints the file.
";
        let diags = ears_diags(body);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].code, "ears.clause-syntax");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
        assert!(
            diags[0].message.contains("no `shall`"),
            "names the defect: {}",
            diags[0].message
        );
        assert_eq!(diags[0].line, Some(4));
    }

    #[test]
    fn ears_missing_trigger_comma_warns() {
        let body = "\
# SPEC

## Requirements
- EARS-02: WHEN a watched file changes the linter shall re-lint the file.
";
        let diags = ears_diags(body);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(
            diags[0]
                .message
                .contains("missing comma after the `WHEN` trigger"),
            "names the defect: {}",
            diags[0].message
        );
    }

    #[test]
    fn ears_if_missing_comma_before_then_warns() {
        let body = "\
# SPEC

## Requirements
- EARS-03: IF the frontmatter fence is missing THEN the linter shall emit core.frontmatter.
";
        let diags = ears_diags(body);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(
            diags[0]
                .message
                .contains("missing comma after the `IF` condition before `THEN`"),
            "names the defect: {}",
            diags[0].message
        );
    }

    #[test]
    fn ears_lowercase_keyword_warns() {
        let body = "\
# SPEC

## Requirements
- EARS-04: when a watched file changes, the linter shall re-lint the file.
";
        let diags = ears_diags(body);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(
            diags[0].message.contains("lowercase EARS keyword `when`"),
            "names the defect: {}",
            diags[0].message
        );
    }

    #[test]
    fn ears_if_without_then_is_unrecognized() {
        let body = "\
# SPEC

## Requirements
- EARS-05: IF the frontmatter fence is missing, the linter shall emit core.frontmatter.
";
        let diags = ears_diags(body);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(
            diags[0].message.contains("none of the six EARS patterns"),
            "unrecognized clause names the pattern set: {}",
            diags[0].message
        );
    }

    #[test]
    fn ears_no_id_bullet_is_skipped() {
        // ESY-003: a bullet with no EARS- id is not a clause this rule
        // owns — even though it has no `shall`.
        let body = "\
# SPEC

## Requirements
- The linter re-checks every document on startup.
- EARS-01: The linter shall exit non-zero when any error diagnostic is emitted.
";
        let diags = ears_diags(body);
        assert!(diags.is_empty(), "no-id bullet must be skipped: {diags:?}");
    }

    #[test]
    fn ears_dotted_refinement_id_is_linted() {
        // SPEC refinements carry EARS-NN.M ids (ADR-031 § ESY-003) —
        // the dotted form must be matched, not truncated to EARS-NN.
        let body = "\
# SPEC

## Requirements
- EARS-02.1: WHEN a watched file changes the linter shall re-lint the file.
";
        let diags = ears_diags(body);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(
            diags[0].message.contains("EARS-02.1"),
            "names the dotted id: {}",
            diags[0].message
        );
    }

    #[test]
    fn ears_wrapped_clause_continuation_lines_join() {
        // A format-on-save wrap puts the tail on a hanging-indent line;
        // the clause must still parse as one sentence.
        let body = "\
# SPEC

## Requirements
- EARS-02: WHEN a watched file changes,
  the linter shall re-lint the file.
";
        let diags = ears_diags(body);
        assert!(diags.is_empty(), "wrapped clause must pass: {diags:?}");
    }

    #[test]
    fn ears_lazy_continuation_line_joins() {
        // CommonMark lazy continuation: a non-indented plain line directly
        // after a bullet is still part of the item. Dropping it would
        // truncate the clause and false-positive MissingShall.
        let body = "\
# SPEC

## Requirements
- EARS-02: WHEN a watched file changes,
the linter shall re-lint the file.
";
        let diags = ears_diags(body);
        assert!(diags.is_empty(), "lazy continuation must join: {diags:?}");
    }

    #[test]
    fn ears_blank_line_ends_the_clause() {
        // A blank line closes the list item (CommonMark); following prose
        // is a separate paragraph, not clause text — the truncated clause
        // still warns.
        let body = "\
# SPEC

## Requirements
- EARS-01: WHEN a watched file changes, the linter

shall is discussed in the EARS paper.
";
        let diags = ears_diags(body);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(
            diags[0].message.contains("no `shall`"),
            "paragraph after blank line is not clause text: {}",
            diags[0].message
        );
    }

    #[test]
    fn ears_outside_requirements_heading_is_silent() {
        // ESY-004: only the Requirements section is parsed — the same
        // malformed clause under another heading is not this rule's text.
        let body = "\
# SPEC

## Goal
- EARS-01: when a watched file changes, the linter re-lints the file.

## Requirements
- EARS-02: The linter shall exit non-zero when any error diagnostic is emitted.
";
        let diags = ears_diags(body);
        assert!(
            diags.is_empty(),
            "non-Requirements sections are silent: {diags:?}"
        );
    }

    #[test]
    fn ears_no_requirements_heading_is_silent() {
        // Heading presence is core.required-headings' concern (ESY-004).
        let body = "# SPEC\n\n## Goal\nship it\n";
        let diags = ears_diags(body);
        assert!(diags.is_empty(), "no heading means silent: {diags:?}");
    }

    // -- skills.frontmatter: fence and type errors (findings #3, #8) ----

    fn skills_doc(location: &str, body: &str) -> Document {
        Document {
            id: "ADR-1".parse().unwrap(),
            raw_id: String::new(),
            location: location.to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: None,
            body: body.to_owned(),
        }
    }

    #[test]
    fn skills_fm_no_fence_emits_fence_message() {
        let body = "# My Skill\n\nsome content\n";
        let d = skills_doc(".claude/skills/my-skill/SKILL.md", body);
        let diags = check_skills_frontmatter(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "expected one diagnostic: {diags:?}");
        assert_eq!(diags[0].code, "skills.frontmatter");
        assert!(
            diags[0].message.contains("frontmatter fence"),
            "missing-fence message expected, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn skills_fm_non_string_name_emits_type_message() {
        let body = "---\nname: 42\ndescription: triggers on foo\n---\n# My Skill\n";
        let d = skills_doc(".claude/skills/my-skill/SKILL.md", body);
        let diags = check_skills_frontmatter(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "expected one diagnostic: {diags:?}");
        assert!(
            diags[0]
                .message
                .contains("`name` must be a non-empty string"),
            "type-error message expected, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn skills_fm_valid_frontmatter_passes() {
        let body = "---\nname: my-skill\ndescription: triggers on foo\n---\n# My Skill\n";
        let d = skills_doc(".claude/skills/my-skill/SKILL.md", body);
        let diags = check_skills_frontmatter(&d, None, Path::new("."));
        assert!(diags.is_empty(), "valid SKILL.md must pass: {diags:?}");
    }

    // -- guide.frontmatter tests --------------------------------------

    fn guide_fm_doc(body: &str) -> Document {
        skills_doc("docs/guides/getting-started.md", body)
    }

    fn diataxis_types() -> Value {
        serde_json::json!({"types": ["tutorial", "how-to", "reference", "explanation"]})
    }

    #[test]
    fn guide_fm_valid_title_and_type_passes() {
        let body = "---\ntitle: Getting started with ctxgrd\ndiataxis:\n  type: how-to\n---\n# Getting started\n";
        let d = guide_fm_doc(body);
        let diags = check_guide_frontmatter(&d, Some(&diataxis_types()), Path::new("."));
        assert!(diags.is_empty(), "valid guide must pass: {diags:?}");
    }

    #[test]
    fn guide_fm_legacy_top_level_type_rejected() {
        // BUG-015: the class moved off the top-level `type:` key (which collides
        // with Hugo/Jekyll/Eleventy reserved keys) into a `diataxis` object. A
        // guide using the old top-level `type:` and no `diataxis` must now fail.
        let body = "---\ntitle: Getting started\ntype: how-to\n---\n# Getting started\n";
        let d = guide_fm_doc(body);
        let diags = check_guide_frontmatter(&d, Some(&diataxis_types()), Path::new("."));
        assert_eq!(diags.len(), 1, "legacy top-level type must not satisfy: {diags:?}");
        assert!(
            diags[0].message.contains("non-empty `diataxis.type`"),
            "expected diataxis.type message, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn guide_fm_no_fence_emits_fence_message() {
        let d = guide_fm_doc("# Getting started\n\nsome content\n");
        let diags = check_guide_frontmatter(&d, Some(&diataxis_types()), Path::new("."));
        assert_eq!(diags.len(), 1, "expected one diagnostic: {diags:?}");
        assert_eq!(diags[0].code, "guide.frontmatter");
        assert!(
            diags[0].message.contains("frontmatter fence"),
            "missing-fence message expected, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn guide_fm_missing_type_errors() {
        let body = "---\ntitle: Getting started\n---\n# Getting started\n";
        let d = guide_fm_doc(body);
        let diags = check_guide_frontmatter(&d, Some(&diataxis_types()), Path::new("."));
        assert_eq!(diags.len(), 1, "expected one diagnostic: {diags:?}");
        assert!(
            diags[0].message.contains("non-empty `diataxis.type`"),
            "missing-type message expected, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn guide_fm_type_outside_allowlist_errors() {
        let body = "---\ntitle: Getting started\ndiataxis:\n  type: walkthrough\n---\n# Getting started\n";
        let d = guide_fm_doc(body);
        let diags = check_guide_frontmatter(&d, Some(&diataxis_types()), Path::new("."));
        assert_eq!(diags.len(), 1, "expected one diagnostic: {diags:?}");
        assert!(
            diags[0].message.contains("not one of the allowed types"),
            "unknown-type message expected, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn guide_fm_unknown_type_passes_without_allowlist() {
        // Absent `types` param → presence-only, no value check (config-driven taxonomy).
        let body = "---\ntitle: Getting started\ndiataxis:\n  type: walkthrough\n---\n# Getting started\n";
        let d = guide_fm_doc(body);
        let diags = check_guide_frontmatter(&d, None, Path::new("."));
        assert!(
            diags.is_empty(),
            "without a types allowlist, any non-empty type passes: {diags:?}"
        );
    }

    // -- c4.frontmatter tests -----------------------------------------

    fn c4_fm_doc(body: &str) -> Document {
        skills_doc("docs/diagrams/00-system-context.md", body)
    }

    fn c4_levels() -> Value {
        serde_json::json!({"levels": [
            "context", "container", "component", "code",
            "deployment", "dynamic", "landscape"
        ]})
    }

    #[test]
    fn c4_fm_valid_title_and_level_passes() {
        let body = "---\ntitle: claude-box system context\nc4:\n  level: container\n---\n# Container view\n";
        let d = c4_fm_doc(body);
        let diags = check_c4_frontmatter(&d, Some(&c4_levels()), Path::new("."));
        assert!(diags.is_empty(), "valid C4 diagram must pass: {diags:?}");
    }

    #[test]
    fn c4_fm_legacy_top_level_type_rejected() {
        // BUG-015: the level lives under a `c4` object, never a top-level
        // `type:` (an SSG-reserved layout key). A diagram using the old
        // top-level `type:` and no `c4` object must fail on the level.
        let body = "---\ntitle: System context\ntype: context\n---\n# Context\n";
        let d = c4_fm_doc(body);
        let diags = check_c4_frontmatter(&d, Some(&c4_levels()), Path::new("."));
        assert_eq!(diags.len(), 1, "legacy top-level type must not satisfy: {diags:?}");
        assert!(
            diags[0].message.contains("non-empty `c4.level`"),
            "expected c4.level message, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn c4_fm_no_fence_emits_fence_message() {
        let d = c4_fm_doc("# System context\n\nsome content\n");
        let diags = check_c4_frontmatter(&d, Some(&c4_levels()), Path::new("."));
        assert_eq!(diags.len(), 1, "expected one diagnostic: {diags:?}");
        assert_eq!(diags[0].code, "c4.frontmatter");
        assert!(
            diags[0].message.contains("frontmatter fence"),
            "missing-fence message expected, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn c4_fm_missing_level_errors() {
        let body = "---\ntitle: System context\n---\n# Context\n";
        let d = c4_fm_doc(body);
        let diags = check_c4_frontmatter(&d, Some(&c4_levels()), Path::new("."));
        assert_eq!(diags.len(), 1, "expected one diagnostic: {diags:?}");
        assert!(
            diags[0].message.contains("non-empty `c4.level`"),
            "missing-level message expected, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn c4_fm_level_outside_allowlist_errors() {
        let body = "---\ntitle: System context\nc4:\n  level: sequence\n---\n# Context\n";
        let d = c4_fm_doc(body);
        let diags = check_c4_frontmatter(&d, Some(&c4_levels()), Path::new("."));
        assert_eq!(diags.len(), 1, "expected one diagnostic: {diags:?}");
        assert!(
            diags[0].message.contains("not one of the allowed levels"),
            "unknown-level message expected, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn c4_fm_unknown_level_passes_without_allowlist() {
        // Absent `levels` param → presence-only, no value check (config-driven).
        let body = "---\ntitle: System context\nc4:\n  level: sequence\n---\n# Context\n";
        let d = c4_fm_doc(body);
        let diags = check_c4_frontmatter(&d, None, Path::new("."));
        assert!(
            diags.is_empty(),
            "without a levels allowlist, any non-empty level passes: {diags:?}"
        );
    }

    // -- todo.listed tests --------------------------------------------

    fn listed_doc(raw_id: &str, status: &str) -> Document {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "status".to_owned(),
            serde_json::Value::String(status.to_owned()),
        );
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("status".to_owned(), 3u32);
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/adrs/{raw_id}.md"),
            depends_on: Vec::new(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: None,
            body: String::new(),
        }
    }

    #[test]
    fn todo_listed_terminal_status_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let d = listed_doc("ADR-027", "accepted");
        let diags = check_todo_listed(&d, None, tmp.path());
        assert!(diags.is_empty(), "terminal status must pass: {diags:?}");
    }

    #[test]
    fn todo_listed_no_status_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut d = listed_doc("ADR-027", "draft");
        d.metadata.remove("status");
        let diags = check_todo_listed(&d, None, tmp.path());
        assert!(diags.is_empty(), "no status field must pass: {diags:?}");
    }

    #[test]
    fn todo_listed_no_todo_md_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let d = listed_doc("ADR-027", "draft");
        // no TODO.md written → silent
        let diags = check_todo_listed(&d, None, tmp.path());
        assert!(diags.is_empty(), "missing TODO.md must not fire: {diags:?}");
    }

    #[test]
    fn todo_listed_id_in_todo_passes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("TODO.md"),
            "- [ ] ADR-027 implement design rules\n",
        )
        .unwrap();
        let d = listed_doc("ADR-027", "draft");
        let diags = check_todo_listed(&d, None, tmp.path());
        assert!(diags.is_empty(), "mentioned ID must pass: {diags:?}");
    }

    #[test]
    fn todo_listed_id_absent_fires() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("TODO.md"),
            "- [ ] ADR-001 some other task\n",
        )
        .unwrap();
        let d = listed_doc("ADR-027", "draft");
        let diags = check_todo_listed(&d, None, tmp.path());
        assert_eq!(diags.len(), 1, "unlisted open doc must warn: {diags:?}");
        assert_eq!(diags[0].code, "todo.listed");
        assert!(diags[0].message.contains("ADR-027"));
    }

    // -- design.section-order (ADR-027 § DES-002) -------------------------

    fn design_section_doc(headings: &[&str]) -> Document {
        let ast_headings: Vec<Heading> = headings
            .iter()
            .enumerate()
            .map(|(i, text)| Heading {
                level: 2,
                text: text.to_string(),
                line: (i + 1) as u32,
                col: 1,
            })
            .collect();
        Document {
            id: "ADR-1".parse().unwrap(),
            raw_id: String::new(),
            location: "DESIGN.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast {
                headings: ast_headings,
                ..Ast::default()
            }),
            body: String::new(),
        }
    }

    #[test]
    fn design_section_order_all_in_order_passes() {
        let d = design_section_doc(&[
            "Overview",
            "Colors",
            "Typography",
            "Layout",
            "Elevation & Depth",
            "Shapes",
            "Components",
            "Do's and Don'ts",
        ]);
        let diags = check_design_section_order(&d, None, Path::new("."));
        assert!(diags.is_empty(), "canonical order must pass: {diags:?}");
    }

    #[test]
    fn design_section_order_colors_before_overview_fires() {
        let d = design_section_doc(&["Colors", "Overview"]);
        let diags = check_design_section_order(&d, None, Path::new("."));
        assert_eq!(
            diags.len(),
            1,
            "expected one out-of-order diagnostic: {diags:?}"
        );
        assert_eq!(diags[0].code, "design.section-order");
        assert!(
            diags[0].message.contains("Overview"),
            "message: {}",
            diags[0].message
        );
        assert!(
            diags[0].message.contains("Colors"),
            "message: {}",
            diags[0].message
        );
        assert_eq!(diags[0].line, Some(2), "diagnostic anchored at Overview heading");
    }

    #[test]
    fn design_section_order_unknown_section_between_known_skipped() {
        let d = design_section_doc(&["Overview", "Brand Guidelines", "Typography"]);
        let diags = check_design_section_order(&d, None, Path::new("."));
        assert!(
            diags.is_empty(),
            "unknown section must be skipped: {diags:?}"
        );
    }

    #[test]
    fn design_section_order_duplicate_colors_fires() {
        let d = design_section_doc(&["Overview", "Colors", "Colors"]);
        let diags = check_design_section_order(&d, None, Path::new("."));
        assert_eq!(
            diags.len(),
            1,
            "expected one duplicate diagnostic: {diags:?}"
        );
        assert_eq!(diags[0].code, "design.section-order");
        assert!(
            diags[0].message.contains("duplicate"),
            "message: {}",
            diags[0].message
        );
        assert!(
            diags[0].message.contains("Colors"),
            "message: {}",
            diags[0].message
        );
    }

    #[test]
    fn design_section_order_no_recognized_sections_passes() {
        let d = design_section_doc(&["Introduction", "Guidelines", "Appendix"]);
        let diags = check_design_section_order(&d, None, Path::new("."));
        assert!(
            diags.is_empty(),
            "no recognized sections must pass: {diags:?}"
        );
    }

    // -- product.register (ADR-104 § PMD-001..003) -----------------------

    /// A PRODUCT.md built from real markdown — `product.register` reads the
    /// body lines under each heading, so a headings-only synthetic AST would
    /// not exercise it.
    fn product_doc(register: &str, platform: Option<&str>, conversion: bool) -> Document {
        let mut body = format!("# Product\n\n## Register\n\n{register}\n");
        if let Some(platform) = platform {
            body.push_str(&format!("\n## Platform\n\n{platform}\n"));
        }
        body.push_str(
            "\n## Users\n\nA Mac knowledge worker who wants healthy work rhythms.\n\
             \n## Product Purpose\n\nAn ambient reminder companion for macOS.\n",
        );
        if conversion {
            body.push_str(
                "\n## Conversion & Proof\n\n- Primary CTA: Download the app.\n\
                 - Secondary CTA: See how it works.\n",
            );
        }
        body.push_str("\n## Design Principles\n\n- Keep the reminder calm until it matters.\n");
        Document {
            id: "PRODUCT-0".parse().unwrap(),
            raw_id: String::new(),
            location: "PRODUCT.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(markdown::parse_ast(&body)),
            body,
        }
    }

    #[test]
    fn product_register_brand_with_conversion_passes() {
        let d = product_doc("brand", Some("web"), true);
        let diags = check_product_register(&d, None, Path::new("."));
        assert!(diags.is_empty(), "a conformant brand file must pass: {diags:?}");
    }

    #[test]
    fn product_register_product_without_conversion_passes() {
        let d = product_doc("product", Some("ios"), false);
        let diags = check_product_register(&d, None, Path::new("."));
        assert!(diags.is_empty(), "a conformant product file must pass: {diags:?}");
    }

    #[test]
    fn product_register_absent_platform_passes() {
        // An absent `## Platform` is legal and means `web` (init.md Step 4).
        let d = product_doc("brand", None, true);
        let diags = check_product_register(&d, None, Path::new("."));
        assert!(diags.is_empty(), "absent platform must be silent: {diags:?}");
    }

    #[test]
    fn product_register_unknown_value_errors() {
        let d = product_doc("marketing", Some("web"), true);
        let diags = check_product_register(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "expected one register error: {diags:?}");
        assert_eq!(diags[0].code, "product.register");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Error);
        assert!(diags[0].message.contains("marketing"), "{}", diags[0].message);
    }

    #[test]
    fn product_register_unresolved_skips_the_conditional_arm() {
        // `marketing` is not a register, so there is no decision to enforce
        // `Conversion & Proof` against — one error, not two.
        let d = product_doc("marketing", Some("web"), false);
        let diags = check_product_register(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "conditional arm must be skipped: {diags:?}");
    }

    #[test]
    fn product_register_prose_instead_of_bare_value_errors() {
        let d = product_doc("This is a brand surface, mostly.", Some("web"), true);
        let diags = check_product_register(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "expected one register error: {diags:?}");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Error);
    }

    #[test]
    fn product_register_trailing_commentary_warns_but_resolves() {
        let d = product_doc("brand\n\nWe may revisit this once the app ships.", Some("web"), true);
        let diags = check_product_register(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "expected one prose warning: {diags:?}");
        assert_eq!(
            diags[0].severity,
            crate::diagnostic::Severity::Warning,
            "the reader takes the first line and carries on, so this never errors"
        );
    }

    #[test]
    fn product_register_missing_section_errors() {
        let mut d = product_doc("brand", Some("web"), true);
        let body = d.body.replace("## Register\n\nbrand\n", "");
        d.ast = Some(markdown::parse_ast(&body));
        d.body = body;
        let diags = check_product_register(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "expected one missing-section error: {diags:?}");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Error);
        assert!(diags[0].message.contains("missing"), "{}", diags[0].message);
    }

    #[test]
    fn product_register_unknown_platform_warns_matching_the_reader() {
        // The consumer falls back to `web` on an unrecognized platform rather
        // than failing, so the linter must not be stricter than it.
        let d = product_doc("brand", Some("macOS"), true);
        let diags = check_product_register(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "expected one platform warning: {diags:?}");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
        assert!(diags[0].message.contains("macOS"), "{}", diags[0].message);
    }

    #[test]
    fn product_register_brand_missing_conversion_errors() {
        let d = product_doc("brand", Some("web"), false);
        let diags = check_product_register(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "expected one conditional error: {diags:?}");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Error);
        assert!(
            diags[0].message.contains("Conversion & Proof"),
            "{}",
            diags[0].message
        );
    }

    #[test]
    fn product_register_product_carrying_conversion_errors() {
        let d = product_doc("product", Some("web"), true);
        let diags = check_product_register(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "expected one conditional error: {diags:?}");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Error);
        assert!(
            diags[0].message.contains("belongs to the `brand` register"),
            "{}",
            diags[0].message
        );
    }

    #[test]
    fn product_register_conditional_heading_matched_case_insensitively() {
        // init.md's template writes `## Conversion & proof`; the wild writes
        // `## Conversion & Proof`. Capitalization must not be load-bearing.
        let mut d = product_doc("brand", Some("web"), true);
        let body = d.body.replace("## Conversion & Proof", "## Conversion & proof");
        d.ast = Some(markdown::parse_ast(&body));
        d.body = body;
        let diags = check_product_register(&d, None, Path::new("."));
        assert!(diags.is_empty(), "capitalization must not matter: {diags:?}");
    }

    #[test]
    fn product_register_values_are_config_driven() {
        // Nothing about impeccable is baked into the binary.
        let d = product_doc("editorial", Some("print"), false);
        let params = serde_json::json!({
            "registers": ["editorial", "commerce"],
            "platforms": ["print", "web"],
            "conditional_section": "Circulation",
            "conditional_on": "commerce",
        });
        let diags = check_product_register(&d, Some(&params), Path::new("."));
        assert!(diags.is_empty(), "config allowlists must govern: {diags:?}");
    }

    // -- design.token-ref (ADR-027 § DES-003) ----------------------------

    fn design_token_doc(metadata: BTreeMap<String, serde_json::Value>) -> Document {
        Document {
            id: "ADR-1".parse().unwrap(),
            raw_id: String::new(),
            location: "DESIGN.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata,
            pin: None,
            ast: None,
            body: String::new(),
        }
    }

    #[test]
    fn design_token_ref_resolved_scalar_passes() {
        let mut meta = BTreeMap::new();
        meta.insert(
            "colors".to_owned(),
            serde_json::json!({"primary": "#0055ff"}),
        );
        meta.insert(
            "components".to_owned(),
            serde_json::json!({"button": {"bg": "{colors.primary}"}}),
        );
        let d = design_token_doc(meta);
        let diags = check_design_token_ref(&d, None, Path::new("."));
        assert!(diags.is_empty(), "resolved scalar must pass: {diags:?}");
    }

    #[test]
    fn design_token_ref_unresolved_key_fires() {
        let mut meta = BTreeMap::new();
        meta.insert(
            "colors".to_owned(),
            serde_json::json!({"primary": "#0055ff"}),
        );
        meta.insert(
            "components".to_owned(),
            serde_json::json!({"button": {"bg": "{colors.brand}"}}),
        );
        let d = design_token_doc(meta);
        let diags = check_design_token_ref(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "unresolved token must warn: {diags:?}");
        assert_eq!(diags[0].code, "design.token-ref");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
        assert!(
            diags[0].message.contains("colors.brand"),
            "message: {}",
            diags[0].message
        );
    }

    #[test]
    fn design_token_ref_three_level_path_resolves() {
        let mut meta = BTreeMap::new();
        meta.insert(
            "components".to_owned(),
            serde_json::json!({"button": {"backgroundColor": "#fff"}}),
        );
        meta.insert(
            "theme".to_owned(),
            serde_json::json!({"primary": "{components.button.backgroundColor}"}),
        );
        let d = design_token_doc(meta);
        let diags = check_design_token_ref(&d, None, Path::new("."));
        assert!(
            diags.is_empty(),
            "three-level resolved path must pass: {diags:?}"
        );
    }

    #[test]
    fn design_token_ref_composite_typography_resolves() {
        // The DESIGN.md spec permits a component property to reference a whole
        // composite token, so `{typography.label}` pointing at a map is legal
        // (ADR-027 § DES-003 amendment) — existence is resolution.
        let mut meta = BTreeMap::new();
        meta.insert(
            "typography".to_owned(),
            serde_json::json!({
                "label": {"fontFamily": "Space Grotesk", "fontSize": "11px", "fontWeight": 500}
            }),
        );
        meta.insert(
            "components".to_owned(),
            serde_json::json!({"chip": {"typography": "{typography.label}"}}),
        );
        let d = design_token_doc(meta);
        let diags = check_design_token_ref(&d, None, Path::new("."));
        assert!(
            diags.is_empty(),
            "composite-token reference must resolve: {diags:?}"
        );
    }

    #[test]
    fn design_token_ref_path_into_scalar_fires() {
        // `{colors.primary.hex}` traverses past a scalar (`primary` is a
        // string) — a genuine broken reference, must still warn.
        let mut meta = BTreeMap::new();
        meta.insert("colors".to_owned(), serde_json::json!({"primary": "#fff"}));
        meta.insert(
            "theme".to_owned(),
            serde_json::json!({"bg": "{colors.primary.hex}"}),
        );
        let d = design_token_doc(meta);
        let diags = check_design_token_ref(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "over-deep path must warn: {diags:?}");
        assert_eq!(diags[0].code, "design.token-ref");
        assert!(
            diags[0].message.contains("colors.primary.hex"),
            "message: {}",
            diags[0].message
        );
    }

    #[test]
    fn design_token_ref_no_token_references_passes() {
        let mut meta = BTreeMap::new();
        meta.insert("name".to_owned(), serde_json::json!("Acme Design System"));
        meta.insert("version".to_owned(), serde_json::json!("1.0.0"));
        let d = design_token_doc(meta);
        let diags = check_design_token_ref(&d, None, Path::new("."));
        assert!(diags.is_empty(), "no token refs must pass: {diags:?}");
    }

    // -- style.section-order (ADR-034 § STY-002) --------------------------

    fn style_section_doc(headings: &[&str]) -> Document {
        let ast_headings: Vec<Heading> = headings
            .iter()
            .enumerate()
            .map(|(i, text)| Heading {
                level: 2,
                text: text.to_string(),
                line: (i + 1) as u32,
                col: 1,
            })
            .collect();
        Document {
            id: "STYLE-0".parse().unwrap(),
            raw_id: String::new(),
            location: "STYLE.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast {
                headings: ast_headings,
                ..Ast::default()
            }),
            body: String::new(),
        }
    }

    #[test]
    fn style_section_order_all_in_order_passes() {
        let d = style_section_doc(&[
            "Voice Principles",
            "Vocabulary",
            "Punctuation & Formatting",
            "Platform Differences",
            "Quick Reactions",
            "Rhetorical Moves",
            "Anti-Patterns",
            "Examples of Right Voice",
        ]);
        let diags = check_style_section_order(&d, None, Path::new("."));
        assert!(diags.is_empty(), "template order must pass: {diags:?}");
    }

    #[test]
    fn style_section_order_vocabulary_before_voice_principles_warns() {
        let d = style_section_doc(&["Vocabulary", "Voice Principles"]);
        let diags = check_style_section_order(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "expected one out-of-order warning: {diags:?}");
        assert_eq!(diags[0].code, "style.section-order");
        assert_eq!(
            diags[0].severity,
            crate::diagnostic::Severity::Warning,
            "order arm must be a warning, never an error"
        );
        assert!(diags[0].message.contains("Voice Principles"), "{}", diags[0].message);
        assert_eq!(diags[0].line, Some(2), "anchored at the out-of-order heading");
    }

    #[test]
    fn style_section_order_unknown_section_between_known_skipped() {
        let d = style_section_doc(&["Voice Principles", "Catchphrases", "Vocabulary"]);
        let diags = check_style_section_order(&d, None, Path::new("."));
        assert!(diags.is_empty(), "unknown section must be skipped: {diags:?}");
    }

    #[test]
    fn style_section_order_duplicate_anti_patterns_warns() {
        let d = style_section_doc(&["Voice Principles", "Anti-Patterns", "Anti-Patterns"]);
        let diags = check_style_section_order(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "expected one duplicate warning: {diags:?}");
        assert_eq!(diags[0].code, "style.section-order");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
        assert!(diags[0].message.contains("duplicate"), "{}", diags[0].message);
        assert!(diags[0].message.contains("Anti-Patterns"), "{}", diags[0].message);
    }

    #[test]
    fn style_section_order_no_recognized_sections_passes() {
        let d = style_section_doc(&["Intro", "Tone", "Notes"]);
        let diags = check_style_section_order(&d, None, Path::new("."));
        assert!(diags.is_empty(), "no recognized sections must pass: {diags:?}");
    }

    // -- style.soul-pair (ADR-034 § STY-003) ------------------------------

    fn style_doc_at(location: &str) -> Document {
        Document {
            id: "STYLE-0".parse().unwrap(),
            raw_id: String::new(),
            location: location.to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast::default()),
            body: String::new(),
        }
    }

    #[test]
    fn style_soul_pair_with_sibling_passes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("STYLE.md"), "# Style\n").expect("write STYLE.md");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").expect("write SOUL.md");
        let d = style_doc_at("STYLE.md");
        let diags = check_style_soul_pair(&d, None, tmp.path());
        assert!(diags.is_empty(), "sibling SOUL.md must pass: {diags:?}");
    }

    #[test]
    fn style_soul_pair_without_sibling_warns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("STYLE.md"), "# Style\n").expect("write STYLE.md");
        let d = style_doc_at("STYLE.md");
        let diags = check_style_soul_pair(&d, None, tmp.path());
        assert_eq!(diags.len(), 1, "missing SOUL.md must warn: {diags:?}");
        assert_eq!(diags[0].code, "style.soul-pair");
        assert_eq!(
            diags[0].severity,
            crate::diagnostic::Severity::Warning,
            "must never be an error — files may exist independently"
        );
        assert!(diags[0].message.contains("SOUL.md"), "{}", diags[0].message);
    }

    // -- soul.sections (ADR-035 § SOUL-002) -------------------------------

    // Template-conformant fixture: a `#` title followed by the given sections
    // at `##`, the shape the soul.md template prescribes.
    fn soul_section_doc(sections: &[&str]) -> Document {
        soul_doc(Some("Acme Persona"), sections, 2)
    }

    // No title; sections forced to `level`. For wrong-level / missing-title cases.
    fn soul_section_doc_at(sections: &[&str], level: u8) -> Document {
        soul_doc(None, sections, level)
    }

    fn soul_doc(title: Option<&str>, sections: &[&str], level: u8) -> Document {
        let mut ast_headings: Vec<Heading> = Vec::new();
        let mut line = 1u32;
        if let Some(t) = title {
            ast_headings.push(Heading {
                level: 1,
                text: t.to_string(),
                line,
                col: 1,
            });
            line += 1;
        }
        for text in sections {
            ast_headings.push(Heading {
                level,
                text: text.to_string(),
                line,
                col: 1,
            });
            line += 1;
        }
        Document {
            id: "SOUL-0".parse().unwrap(),
            raw_id: String::new(),
            location: "SOUL.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast {
                headings: ast_headings,
                ..Ast::default()
            }),
            body: String::new(),
        }
    }

    #[test]
    fn soul_sections_trio_as_h1_fires_wrong_level() {
        // BUG-009 reversed (ADR-035 amended to the soul.md template): the trio
        // authored as `#` is the wrong level — sections must be `##` under a
        // single `#` title. Each fires one actionable wrong-level warning that
        // names the level, not a misleading "missing".
        let d = soul_section_doc_at(&["Worldview", "Opinions", "Boundaries"], 1);
        let diags = check_soul_sections(&d, None, Path::new("."));
        assert_eq!(diags.len(), 3, "one wrong-level warning per H1 section: {diags:?}");
        for diag in &diags {
            assert!(
                diag.message.contains("H1") && diag.message.contains("`##`"),
                "message must point at the wrong level: {}",
                diag.message
            );
            assert!(
                !diag.message.contains("missing"),
                "present-but-wrong-level must not say missing: {}",
                diag.message
            );
        }
    }

    #[test]
    fn soul_sections_missing_title_fires() {
        // The soul.md template opens with a single `#` title. A file whose trio
        // is correctly at `##` but that has no H1 title fires exactly one title
        // warning (frontmatter `name:` is not a substitute).
        let d = soul_section_doc_at(&["Worldview", "Opinions", "Boundaries"], 2);
        let diags = check_soul_sections(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "missing title fires exactly once: {diags:?}");
        assert_eq!(diags[0].code, "soul.sections");
        assert!(
            diags[0].message.contains("H1 title"),
            "the diagnostic must name the missing title: {}",
            diags[0].message
        );
    }

    #[test]
    fn soul_sections_all_three_present_passes() {
        let d = soul_section_doc(&["Who I Am", "Worldview", "Opinions", "Boundaries"]);
        let diags = check_soul_sections(&d, None, Path::new("."));
        assert!(diags.is_empty(), "the high-signal trio present must pass: {diags:?}");
    }

    #[test]
    fn soul_sections_missing_opinions_fires_once() {
        let d = soul_section_doc(&["Worldview", "Boundaries"]);
        let diags = check_soul_sections(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "exactly one missing-section warning: {diags:?}");
        assert_eq!(diags[0].code, "soul.sections");
        assert_eq!(
            diags[0].severity,
            crate::diagnostic::Severity::Warning,
            "soul.sections must be a warning, never an error"
        );
        assert!(diags[0].message.contains("Opinions"), "{}", diags[0].message);
    }

    #[test]
    fn soul_sections_optional_section_absent_passes() {
        // Pet Peeves is an optional section — its absence must not fire,
        // only the three high-signal sections are required.
        let d = soul_section_doc(&["Worldview", "Opinions", "Boundaries"]);
        let diags = check_soul_sections(&d, None, Path::new("."));
        assert!(diags.is_empty(), "absent optional section must pass: {diags:?}");
    }

    #[test]
    fn soul_sections_unknown_heading_skipped() {
        // An unrecognized heading neither satisfies nor fires a requirement;
        // the three required sections are still all present here.
        let d = soul_section_doc(&["Worldview", "Catchphrases", "Opinions", "Boundaries"]);
        let diags = check_soul_sections(&d, None, Path::new("."));
        assert!(diags.is_empty(), "unknown heading must be skipped: {diags:?}");
    }

    #[test]
    fn soul_sections_tensions_alias_recognized() {
        // "Tensions and Contradictions" (the & alias spelled out) is an
        // optional section — it must pass silently, not be penalized, when
        // the required trio is present.
        let d = soul_section_doc(&[
            "Worldview",
            "Opinions",
            "Boundaries",
            "Tensions and Contradictions",
        ]);
        let diags = check_soul_sections(&d, None, Path::new("."));
        assert!(diags.is_empty(), "optional alias section must pass: {diags:?}");
    }

    #[test]
    fn soul_sections_case_insensitive_match() {
        // SOUL-002: matching is case-insensitive and trims whitespace.
        let d = soul_section_doc(&["  worldview  ", "OPINIONS", "Boundaries"]);
        let diags = check_soul_sections(&d, None, Path::new("."));
        assert!(diags.is_empty(), "case-insensitive trimmed match must pass: {diags:?}");
    }

    #[test]
    fn soul_sections_all_three_missing_fires_three() {
        let d = soul_section_doc(&["Who I Am", "Interests"]);
        let diags = check_soul_sections(&d, None, Path::new("."));
        assert_eq!(diags.len(), 3, "one warning per missing required section: {diags:?}");
    }

    // -- core.requires-link (ADR-046 § RRF-001/002) ---------------------

    /// A guide file (CLAUDE.md-shaped) whose body parses to a real AST, so
    /// link hrefs and `@import` lines resolve exactly as they do in the CLI.
    fn guide_doc(location: &str, body: &str) -> Document {
        synthetic_document("AGENTS", location.to_string(), body.to_string())
    }

    #[test]
    fn requires_link_satisfied_by_markdown_link() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").expect("write SOUL.md");
        let d = guide_doc("CLAUDE.md", "# Project\n\nPersona: [SOUL.md](SOUL.md)\n");
        let params = serde_json::json!({"targets": ["SOUL.md"]});
        let diags = check_requires_link(&d, Some(&params), tmp.path());
        assert!(diags.is_empty(), "a markdown link must satisfy: {diags:?}");
    }

    #[test]
    fn requires_link_satisfied_by_import() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").expect("write SOUL.md");
        let d = guide_doc("CLAUDE.md", "# Project\n\n@SOUL.md\n");
        let params = serde_json::json!({"targets": ["SOUL.md"]});
        let diags = check_requires_link(&d, Some(&params), tmp.path());
        assert!(diags.is_empty(), "an @import must satisfy: {diags:?}");
    }

    #[test]
    fn requires_link_unreferenced_existing_target_errors() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").expect("write SOUL.md");
        let d = guide_doc("CLAUDE.md", "# Project\n\nNo persona reference here.\n");
        let params = serde_json::json!({"targets": ["SOUL.md"]});
        let diags = check_requires_link(&d, Some(&params), tmp.path());
        assert_eq!(diags.len(), 1, "an unreferenced existing target must fire once: {diags:?}");
        assert_eq!(diags[0].code, "core.requires-link");
        assert_eq!(
            diags[0].severity,
            crate::diagnostic::Severity::Error,
            "default severity is error (mirrors the TODO.md precedent)"
        );
        assert!(diags[0].message.contains("SOUL.md"), "{}", diags[0].message);
    }

    #[test]
    fn requires_link_nonexistent_target_skipped() {
        // STYLE.md does not exist on disk — the rule must never demand a
        // file be created, only that an existing one be referenced.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").expect("write SOUL.md");
        let d = guide_doc("CLAUDE.md", "# Project\n\n[SOUL.md](SOUL.md)\n");
        let params = serde_json::json!({"targets": ["SOUL.md", "STYLE.md"]});
        let diags = check_requires_link(&d, Some(&params), tmp.path());
        assert!(diags.is_empty(), "a nonexistent target must be skipped: {diags:?}");
    }

    #[test]
    fn requires_link_absent_targets_is_noop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").expect("write SOUL.md");
        let d = guide_doc("CLAUDE.md", "# Project\n");
        assert!(
            check_requires_link(&d, None, tmp.path()).is_empty(),
            "no params must be a no-op"
        );
        let empty = serde_json::json!({"targets": []});
        assert!(
            check_requires_link(&d, Some(&empty), tmp.path()).is_empty(),
            "empty targets must be a no-op"
        );
    }

    #[test]
    fn requires_link_severity_param_downgrades_to_warning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").expect("write SOUL.md");
        let d = guide_doc("CLAUDE.md", "# Project\n\nNo reference.\n");
        let params = serde_json::json!({"targets": ["SOUL.md"], "severity": "warning"});
        let diags = check_requires_link(&d, Some(&params), tmp.path());
        assert_eq!(diags.len(), 1, "still fires, at the configured severity: {diags:?}");
        assert_eq!(
            diags[0].severity,
            crate::diagnostic::Severity::Warning,
            "severity = warning must downgrade the diagnostic"
        );
    }

    #[test]
    fn requires_link_resolves_relative_to_nested_guide() {
        // A nested guide references the root target with `../` — resolution
        // is file-relative, the BUG-004 lesson the TODO check encodes.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").expect("write SOUL.md");
        std::fs::create_dir(tmp.path().join("cli")).expect("mkdir cli");
        let d = guide_doc("cli/CLAUDE.md", "# CLI\n\n[SOUL.md](../SOUL.md)\n");
        let params = serde_json::json!({"targets": ["SOUL.md"]});
        let diags = check_requires_link(&d, Some(&params), tmp.path());
        assert!(diags.is_empty(), "file-relative `../SOUL.md` must satisfy: {diags:?}");
    }

    #[test]
    fn requires_link_help_suggests_a_reference_that_satisfies_it() {
        // BUG-035: `targets` resolve against the lint root, references
        // resolve against the document. The help renders references, so it
        // must render them at the document's depth — otherwise pasting the
        // suggestion verbatim leaves the diagnostic standing for every
        // document below the root.
        //
        // Asserted as the property (the suggestion, inserted verbatim,
        // clears the diagnostic) rather than as a string, and at depth 0, 1
        // and 2: a test written only at the root reproduces the bug instead
        // of catching it, since depth 0 is the case that already worked.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").expect("write SOUL.md");
        std::fs::create_dir_all(tmp.path().join("cli/nested")).expect("mkdir cli/nested");
        let params = serde_json::json!({"targets": ["SOUL.md"]});

        for location in ["CLAUDE.md", "cli/CLAUDE.md", "cli/nested/CLAUDE.md"] {
            let d = guide_doc(location, "# Guide\n\nNo persona reference here.\n");
            let diags = check_requires_link(&d, Some(&params), tmp.path());
            assert_eq!(diags.len(), 1, "{location} must fire unreferenced: {diags:?}");
            let help = diags[0].help.as_deref().expect("the diagnostic carries help");
            // The help quotes the markdown link and the `@import` in
            // backticks, in that order.
            let quoted: Vec<&str> = help.split('`').skip(1).step_by(2).collect();
            assert_eq!(quoted.len(), 2, "help quotes both reference forms: {help}");

            for suggestion in quoted {
                let fixed = guide_doc(location, &format!("# Guide\n\n{suggestion}\n"));
                let after = check_requires_link(&fixed, Some(&params), tmp.path());
                assert!(
                    after.is_empty(),
                    "the help for {location} suggested `{suggestion}`, \
                     which does not satisfy the rule it was suggested for: {after:?}",
                );
            }
        }
    }

    // -- soul.referenced / style.referenced (ADR-047 § PRF-001/002) -----

    /// A persona file at `location`; only its location is read by the rule
    /// (it looks outward to the guide), so the AST/body are inert.
    fn persona_doc(location: &str) -> Document {
        Document {
            id: "SOUL-0".parse().unwrap(),
            raw_id: String::new(),
            location: location.to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast::default()),
            body: String::new(),
        }
    }

    #[test]
    fn soul_referenced_linked_by_guide_passes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").unwrap();
        std::fs::write(
            tmp.path().join("CLAUDE.md"),
            "# Project\n\nPersona: [SOUL.md](SOUL.md)\n",
        )
        .unwrap();
        let d = persona_doc("SOUL.md");
        let diags = check_soul_referenced(&d, None, tmp.path());
        assert!(diags.is_empty(), "a guide link must satisfy: {diags:?}");
    }

    #[test]
    fn soul_referenced_imported_by_guide_passes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Agents\n\n@SOUL.md\n").unwrap();
        let d = persona_doc("SOUL.md");
        let diags = check_soul_referenced(&d, None, tmp.path());
        assert!(diags.is_empty(), "an @import in AGENTS.md must satisfy: {diags:?}");
    }

    #[test]
    fn soul_referenced_guide_present_but_unlinked_warns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Project\n\nNo persona.\n").unwrap();
        let d = persona_doc("SOUL.md");
        let diags = check_soul_referenced(&d, None, tmp.path());
        assert_eq!(diags.len(), 1, "guide exists but does not link → one warning: {diags:?}");
        assert_eq!(diags[0].code, "soul.referenced");
        assert_eq!(
            diags[0].severity,
            crate::diagnostic::Severity::Warning,
            "persona-side reference rule is advisory"
        );
        assert!(diags[0].message.contains("SOUL.md"), "{}", diags[0].message);
    }

    #[test]
    fn soul_referenced_no_guide_is_silent() {
        // The standalone case (ADR-035): a SOUL.md loaded directly by a
        // runtime, no CLAUDE.md/AGENTS.md/GEMINI.md in the repo.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\n").unwrap();
        let d = persona_doc("SOUL.md");
        let diags = check_soul_referenced(&d, None, tmp.path());
        assert!(diags.is_empty(), "no guide present → silent: {diags:?}");
    }

    #[test]
    fn soul_referenced_nested_persona_path_resolves() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join("soul")).unwrap();
        std::fs::write(tmp.path().join("soul/SOUL.md"), "# Soul\n").unwrap();
        std::fs::write(
            tmp.path().join("CLAUDE.md"),
            "# Project\n\n[SOUL.md](soul/SOUL.md)\n",
        )
        .unwrap();
        let d = persona_doc("soul/SOUL.md");
        let diags = check_soul_referenced(&d, None, tmp.path());
        assert!(diags.is_empty(), "a link to soul/SOUL.md must resolve: {diags:?}");
    }

    #[test]
    fn style_referenced_warns_when_guide_does_not_link() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("STYLE.md"), "# Style\n").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Project\n").unwrap();
        let d = persona_doc("STYLE.md");
        let diags = check_style_referenced(&d, None, tmp.path());
        assert_eq!(diags.len(), 1, "unreferenced STYLE.md warns: {diags:?}");
        assert_eq!(diags[0].code, "style.referenced");
        assert!(diags[0].message.contains("STYLE.md"), "{}", diags[0].message);
    }

    // -- core.dep-shape (ADR-039 § DAG-002, presence half) ---------------

    fn dep_shape_doc(raw_id: &str, depends_on: Vec<&str>) -> Document {
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("depends_on".to_owned(), 5u32);
        Document {
            id: raw_id.parse().expect("valid id"),
            raw_id: raw_id.to_string(),
            location: format!("docs/specs/{}.md", raw_id.to_lowercase()),
            depends_on: depends_on.into_iter().map(String::from).collect(),
            frontmatter_lines,
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast::default()),
            body: String::new(),
        }
    }

    fn requires_params(types: &[&str]) -> Value {
        serde_json::json!({ "requires": types })
    }

    #[test]
    fn dep_shape_passes_when_required_prd_present() {
        // A SPEC depending on a PRD satisfies requires = ["PRD"].
        let d = dep_shape_doc("SPEC-014", vec!["PRD-7", "ADR-39"]);
        let params = requires_params(&["PRD"]);
        assert!(check_dep_shape(&d, Some(&params), Path::new(".")).is_empty());
    }

    #[test]
    fn dep_shape_errors_once_when_required_prd_missing() {
        // No PRD link → exactly one core.dep-shape error, anchored at the
        // depends_on frontmatter line.
        let d = dep_shape_doc("SPEC-014", vec!["ADR-39"]);
        let params = requires_params(&["PRD"]);
        let diags = check_dep_shape(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.dep-shape");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Error);
        assert_eq!(diags[0].line, Some(5));
        assert!(
            diags[0].message.contains("SPEC") && diags[0].message.contains("PRD"),
            "{}",
            diags[0].message
        );
        assert!(
            diags[0].help.as_deref().unwrap_or("").contains("PRD-<n>"),
            "{:?}",
            diags[0].help
        );
    }

    #[test]
    fn dep_shape_passes_with_two_prds_presence_not_cardinality() {
        // DAG-008: presence, not cardinality — two PRD links lint clean.
        let d = dep_shape_doc("SPEC-014", vec!["PRD-7", "PRD-8"]);
        let params = requires_params(&["PRD"]);
        assert!(check_dep_shape(&d, Some(&params), Path::new(".")).is_empty());
    }

    #[test]
    fn dep_shape_noop_without_requires_param() {
        let d = dep_shape_doc("SPEC-014", vec!["ADR-39"]);
        assert!(check_dep_shape(&d, None, Path::new(".")).is_empty());
        let empty = serde_json::json!({ "requires": [] });
        assert!(check_dep_shape(&d, Some(&empty), Path::new(".")).is_empty());
    }

    // -- core.dep-shape (ADR-039 § DAG-003, admissibility half) ----------

    #[test]
    fn dep_shape_flags_inadmissible_edge_to_managed_namespace() {
        // DAG-003: SPEC admits only PRD, but depends on a TASK, which is
        // managed (it appears in the managed set) → one error naming TASK.
        let d = dep_shape_doc("SPEC-014", vec!["PRD-7", "TASK-3"]);
        let params = serde_json::json!({
            "requires": ["PRD"],
            "managed": ["PRD", "ADR", "SPEC", "TASK"],
        });
        let diags = check_dep_shape(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].code, "core.dep-shape");
        assert!(diags[0].message.contains("TASK-3"), "{}", diags[0].message);
        assert!(
            diags[0].message.contains("TASK") && diags[0].message.contains("SPEC"),
            "{}",
            diags[0].message
        );
        assert!(
            diags[0]
                .help
                .as_deref()
                .unwrap_or("")
                .contains("[SPEC.\"core.dep-shape\"].allows"),
            "{:?}",
            diags[0].help
        );
    }

    #[test]
    fn dep_shape_admits_edge_in_allows() {
        // DAG-003 + BUG-008 Catch-22: SPEC requires PRD, allows ADR; an
        // edge to ADR is admissible — no error (the dead Catch-22).
        let d = dep_shape_doc("SPEC-014", vec!["PRD-7", "ADR-39"]);
        let params = serde_json::json!({
            "requires": ["PRD"],
            "allows": ["ADR"],
            "managed": ["PRD", "ADR", "SPEC", "TASK"],
        });
        assert!(
            check_dep_shape(&d, Some(&params), Path::new(".")).is_empty(),
            "an edge in `allows` is admissible"
        );
    }

    #[test]
    fn dep_shape_exempts_edge_to_unmanaged_namespace() {
        // DAG-003: an edge to a namespace not in the managed set is exempt
        // (the old unstaged-endpoint exemption). BUG is unmanaged here.
        let d = dep_shape_doc("SPEC-014", vec!["PRD-7", "BUG-1"]);
        let params = serde_json::json!({
            "requires": ["PRD"],
            "managed": ["PRD", "ADR", "SPEC", "TASK"],
        });
        assert!(
            check_dep_shape(&d, Some(&params), Path::new(".")).is_empty(),
            "an edge to an unmanaged namespace is exempt"
        );
    }

    #[test]
    fn dep_shape_admissibility_off_without_managed_param() {
        // Without the synthesized `managed` param the admissibility half is
        // inert; only the presence half runs (a direct unit call).
        let d = dep_shape_doc("SPEC-014", vec!["PRD-7", "TASK-3"]);
        let params = serde_json::json!({ "requires": ["PRD"] });
        assert!(check_dep_shape(&d, Some(&params), Path::new(".")).is_empty());
    }

    // -- core.commit-freshness (ADR-040) pure helpers --------------------

    #[test]
    fn interpret_ancestry_maps_exit_codes() {
        assert_eq!(interpret_ancestry(Some(0)), Ancestry::IsAncestor);
        assert_eq!(interpret_ancestry(Some(1)), Ancestry::NotAncestor);
        assert_eq!(interpret_ancestry(Some(128)), Ancestry::Unanswerable);
        assert_eq!(interpret_ancestry(None), Ancestry::Unanswerable);
    }

    #[test]
    fn compile_scope_matches_globs() {
        let set = compile_scope(&["src/auth/**".to_owned(), "Cargo.lock".to_owned()])
            .expect("valid globs");
        assert!(set.is_match("src/auth/token.rs"));
        assert!(set.is_match("Cargo.lock"));
        assert!(!set.is_match("src/main.rs"));
    }

    #[test]
    fn compile_scope_reports_invalid_glob() {
        let err = compile_scope(&["src/[unterminated".to_owned()]).unwrap_err();
        assert_eq!(err, "src/[unterminated");
    }

    fn pinned_doc(raw_id: &str, pin: Option<crate::frontmatter::Pin>) -> Document {
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("pin".to_owned(), 4u32);
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/secrevs/{raw_id}.md"),
            depends_on: Vec::new(),
            frontmatter_lines,
            metadata: BTreeMap::new(),
            pin,
            ast: None,
            body: String::new(),
        }
    }

    #[test]
    fn commit_freshness_silent_without_pin_and_without_require() {
        let d = pinned_doc("SECREV-001", None);
        assert!(check_commit_freshness(&d, None, Path::new(".")).is_empty());
    }

    #[test]
    fn commit_freshness_requires_pin_when_configured() {
        let d = pinned_doc("SECREV-001", None);
        let params = serde_json::json!({ "require-pin": true });
        let diags = check_commit_freshness(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.commit-freshness");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Error);
        assert_eq!(
            diags[0].message,
            "SECREV-001 has no `pin` block but this namespace requires one (require-pin)"
        );
    }

    // -- core.file-name (ADR-091 § FNM-001) ------------------------------

    fn fname_doc(raw_id: &str, location: &str) -> Document {
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("id".to_owned(), 2u32);
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: location.to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines,
            metadata: BTreeMap::new(),
            pin: None,
            ast: None,
            body: String::new(),
        }
    }

    #[test]
    fn file_name_clean_when_zero_padded_prefix_matches_id() {
        let d = fname_doc("ADR-91", "docs/adrs/091-file-name-rule.md");
        assert!(check_file_name(&d, None, Path::new(".")).is_empty());
    }

    #[test]
    fn file_name_clean_when_unpadded_prefix_matches_id_numerically() {
        // FNM-003: compared as numbers, so `88-` satisfies `id: ADR-88`
        // even though the padding width differs from the `088-` convention.
        let d = fname_doc("ADR-88", "docs/adrs/88-checklist-pack.md");
        assert!(check_file_name(&d, None, Path::new(".")).is_empty());
    }

    #[test]
    fn file_name_errors_when_prefix_number_differs_from_id() {
        let d = fname_doc("ADR-88", "docs/adrs/087-roadmap.md");
        let diags = check_file_name(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.file-name");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Error);
        assert_eq!(diags[0].line, Some(2));
        assert_eq!(
            diags[0].message,
            "ADR-88: filename prefix `087` does not match the id number 88"
        );
    }

    #[test]
    fn file_name_errors_when_no_numeric_prefix() {
        let d = fname_doc("ADR-88", "docs/adrs/roadmap-namespace.md");
        let diags = check_file_name(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.file-name");
        assert_eq!(
            diags[0].message,
            "ADR-88: filename `roadmap-namespace.md` has no numeric prefix — \
             expected it to start with `88` to match the id"
        );
    }

    #[test]
    fn file_name_errors_when_prefix_overflows_u32() {
        // A prefix too large to be any real id number is a mismatch, not a panic.
        let d = fname_doc("ADR-88", "docs/adrs/99999999999-x.md");
        let diags = check_file_name(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].message,
            "ADR-88: filename prefix `99999999999` does not match the id number 88"
        );
    }

    // -- core.calendar-freshness (ADR-040 § PIN-008) ---------------------

    #[test]
    fn parse_ymd_round_trips() {
        assert_eq!(parse_ymd("2026-06-13"), Some((2026, 6, 13)));
        assert_eq!(parse_ymd("not-a-date"), None);
    }

    fn dated_doc(raw_id: &str, field: &str, date: &str) -> Document {
        let mut metadata = BTreeMap::new();
        metadata.insert(field.to_owned(), serde_json::Value::String(date.to_owned()));
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert(field.to_owned(), 5u32);
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/policies/{raw_id}.md"),
            depends_on: Vec::new(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: None,
            body: String::new(),
        }
    }

    #[test]
    fn calendar_freshness_flags_aged_date() {
        // A date well past today minus the default 30-day interval.
        let d = dated_doc("POLICY-001", "reviewed_date", "2020-01-01");
        let diags = check_calendar_freshness(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.calendar-freshness");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
        assert_eq!(diags[0].line, Some(5));
        assert!(
            diags[0].message.contains("POLICY-001 is stale")
                && diags[0].message.contains("`reviewed_date`"),
            "unexpected message: {}",
            diags[0].message
        );
    }

    #[test]
    fn calendar_freshness_green_for_recent_date() {
        // Tomorrow is never stale; verify the green path with a future date.
        let d = dated_doc("POLICY-001", "reviewed_date", "2999-01-01");
        assert!(check_calendar_freshness(&d, None, Path::new(".")).is_empty());
    }

    #[test]
    fn calendar_freshness_honors_custom_field_and_interval() {
        // `audited_on` is 100 days old; with stale_days=365 it is green,
        // with stale_days=10 it is stale — proving the params are read.
        let d = dated_doc("ISO-001", "audited_on", "2020-01-01");
        let lenient = serde_json::json!({ "field": "audited_on", "stale_days": 100000 });
        assert!(check_calendar_freshness(&d, Some(&lenient), Path::new(".")).is_empty());
        let strict = serde_json::json!({ "field": "audited_on", "stale_days": 1 });
        let diags = check_calendar_freshness(&d, Some(&strict), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("`audited_on`"));
    }

    #[test]
    fn calendar_freshness_silent_when_field_absent() {
        let d = dated_doc("POLICY-001", "reviewed_date", "2020-01-01");
        // Look for a field the document does not carry.
        let params = serde_json::json!({ "field": "ratified_on" });
        assert!(check_calendar_freshness(&d, Some(&params), Path::new(".")).is_empty());
    }

    // -- security.vuln-sla (ADR-041 § SEC-004) ---------------------------

    /// A VULN finding with the given metadata fields. `frontmatter_lines`
    /// anchors `status` at line 3 (the rule's preferred anchor).
    fn vuln_doc(raw_id: &str, fields: &[(&str, &str)]) -> Document {
        let mut metadata = BTreeMap::new();
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("status".to_owned(), 3u32);
        for (k, v) in fields {
            metadata.insert((*k).to_owned(), serde_json::Value::String((*v).to_owned()));
        }
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/security/findings/{raw_id}.md"),
            depends_on: Vec::new(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: None,
            body: String::new(),
        }
    }

    #[test]
    fn vuln_sla_flags_open_high_finding_past_default_window() {
        // status: open, severity: high, discovered 2020 → far past the
        // default 30-day high SLA. Default windows: critical=7, high=30.
        let d = vuln_doc(
            "VULN-014",
            &[
                ("status", "open"),
                ("severity", "high"),
                ("discovered_date", "2020-01-01"),
            ],
        );
        let diags = check_vuln_sla(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "security.vuln-sla");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Error);
        assert_eq!(diags[0].line, Some(3));
        assert!(
            diags[0].message.starts_with("VULN-014: open `high` finding is")
                && diags[0].message.ends_with("past its 30-day SLA"),
            "unexpected message: {}",
            diags[0].message
        );
    }

    #[test]
    fn vuln_sla_silent_for_mitigated_finding() {
        // Same aged finding, but mitigated — only `open` findings are aged.
        let d = vuln_doc(
            "VULN-014",
            &[
                ("status", "mitigated"),
                ("severity", "high"),
                ("discovered_date", "2020-01-01"),
            ],
        );
        assert!(check_vuln_sla(&d, None, Path::new(".")).is_empty());
    }

    #[test]
    fn vuln_sla_silent_for_severity_without_a_window() {
        // medium has no default window, so it ages silently.
        let d = vuln_doc(
            "VULN-021",
            &[
                ("status", "open"),
                ("severity", "medium"),
                ("discovered_date", "2020-01-01"),
            ],
        );
        assert!(check_vuln_sla(&d, None, Path::new(".")).is_empty());
    }

    #[test]
    fn vuln_sla_silent_when_within_window() {
        // A future discovered_date yields a negative age, never past SLA.
        let d = vuln_doc(
            "VULN-030",
            &[
                ("status", "open"),
                ("severity", "critical"),
                ("discovered_date", "2999-01-01"),
            ],
        );
        assert!(check_vuln_sla(&d, None, Path::new(".")).is_empty());
    }

    #[test]
    fn vuln_sla_silent_when_discovered_date_missing() {
        let d = vuln_doc("VULN-040", &[("status", "open"), ("severity", "critical")]);
        assert!(check_vuln_sla(&d, None, Path::new(".")).is_empty());
    }

    #[test]
    fn vuln_sla_honours_custom_windows_and_warning_severity() {
        // Custom window of 5 days for `low`, emitted as a warning.
        let d = vuln_doc(
            "VULN-050",
            &[
                ("status", "open"),
                ("severity", "low"),
                ("discovered_date", "2020-01-01"),
            ],
        );
        let params = serde_json::json!({ "windows": { "low": 5 }, "severity": "warning" });
        let diags = check_vuln_sla(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
        assert!(diags[0].message.contains("past its 5-day SLA"));
    }

    #[test]
    fn vuln_sla_severity_compared_case_insensitively() {
        let d = vuln_doc(
            "VULN-060",
            &[
                ("status", "Open"),
                ("severity", "Critical"),
                ("discovered_date", "2020-01-01"),
            ],
        );
        let diags = check_vuln_sla(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("open `critical` finding"));
    }

    // -- the conditional-link matcher (BUG-030 / BUG-031) ----------------

    /// Config params plus the synthesized resolved-refs set, exactly as
    /// `run.rs` assembles them — `present` names the ids that exist in the
    /// run besides `doc`'s own.
    ///
    /// The refs are computed by the **production** [`resolved_refs`] rather
    /// than hand-written, so these tests exercise the real matcher: the
    /// resolution lookup, the own-id exclusion, and the code-span/
    /// strikethrough filter. A hand-written array would let the matcher
    /// regress while every test still passed.
    ///
    /// A test that omits this and passes bare config params is asserting
    /// the *fail-closed* path, not the rule's logic — see
    /// `conditional_link_rules_fail_closed_without_resolution`.
    fn with_refs(params: Option<serde_json::Value>, doc: &Document, present: &[&str]) -> serde_json::Value {
        let mut known: BTreeSet<DocumentId> = present
            .iter()
            .map(|s| s.parse().expect("test fixture id parses"))
            .collect();
        known.insert(doc.id.clone());
        let refs = resolved_refs(doc, &known);
        let mut merged = params.unwrap_or_else(|| serde_json::json!({}));
        merged[RESOLVED_REFS_PARAM] = serde_json::Value::Array(
            refs.into_iter().map(serde_json::Value::String).collect(),
        );
        merged
    }

    // -- security.risk-expiry (ADR-041 § SEC-005) ------------------------

    /// A RISK/VULN document with metadata fields and `depends_on` links.
    fn risk_doc(raw_id: &str, fields: &[(&str, &str)], depends_on: &[&str]) -> Document {
        let mut metadata = BTreeMap::new();
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("expires".to_owned(), 6u32);
        for (k, v) in fields {
            metadata.insert((*k).to_owned(), serde_json::Value::String((*v).to_owned()));
        }
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/security/risk-acceptances/{raw_id}.md"),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: None,
            body: String::new(),
        }
    }

    #[test]
    fn risk_expiry_green_when_signed_reasoned_and_future_dated() {
        let d = risk_doc(
            "RISK-003",
            &[
                ("approver", "Dana Whitfield, CISO"),
                ("rationale", "Compensating WAF rule blocks the exploit path"),
                ("expires", "2999-01-01"),
            ],
            &[],
        );
        assert!(check_risk_expiry(&d, None, Path::new(".")).is_empty());
    }

    #[test]
    fn risk_expiry_flags_each_missing_field() {
        // No approver, no rationale, no expires → three errors.
        let d = risk_doc("RISK-004", &[], &[]);
        let diags = check_risk_expiry(&d, None, Path::new("."));
        assert_eq!(diags.len(), 3);
        assert!(diags.iter().all(|d| d.code == "security.risk-expiry"));
        assert!(diags.iter().any(|d| d.message.contains("`approver`")));
        assert!(diags.iter().any(|d| d.message.contains("`rationale`")));
        assert!(diags
            .iter()
            .any(|d| d.message.contains("future-dated `expires`")));
    }

    #[test]
    fn risk_expiry_flags_past_expires() {
        let d = risk_doc(
            "RISK-005",
            &[
                ("approver", "Dana Whitfield, CISO"),
                ("rationale", "Accepted pending vendor patch"),
                ("expires", "2020-01-01"),
            ],
            &[],
        );
        let diags = check_risk_expiry(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, Some(6));
        assert_eq!(
            diags[0].message,
            "RISK-005: `expires` (2020-01-01) is not in the future — re-decide the risk"
        );
    }

    #[test]
    fn risk_expiry_flags_unparseable_expires() {
        let d = risk_doc(
            "RISK-006",
            &[
                ("approver", "Dana Whitfield, CISO"),
                ("rationale", "Accepted"),
                ("expires", "next quarter"),
            ],
            &[],
        );
        let diags = check_risk_expiry(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1);
        assert!(diags[0]
            .message
            .contains("is not a valid `YYYY-MM-DD` date"));
    }

    #[test]
    fn risk_expiry_scoped_by_require_when_status() {
        // On a VULN, the rule only acts when status == accepted. An `open`
        // finding (missing every field) is out of scope and silent.
        let d = vuln_doc("VULN-070", &[("status", "open")]);
        let params = serde_json::json!({ "require-when-status": "accepted" });
        assert!(check_risk_expiry(&d, Some(&params), Path::new(".")).is_empty());

        // The same finding, accepted, with no fields → flagged.
        let accepted = vuln_doc("VULN-071", &[("status", "accepted")]);
        let diags = check_risk_expiry(&accepted, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 3);
    }

    #[test]
    fn risk_expiry_exempt_when_links_canonical_risk() {
        // An accepted VULN that links a RISK is exempt — the RISK carries
        // the fields canonically, so the inline fields are not required.
        let cfg =
            serde_json::json!({ "require-when-status": "accepted", "exempt-when-links": "RISK" });
        let d = risk_doc("VULN-080", &[("status", "accepted")], &["RISK-003"]);
        let params = with_refs(Some(cfg.clone()), &d, &["RISK-003"]);
        assert!(check_risk_expiry(&d, Some(&params), Path::new(".")).is_empty());

        // Linking some other namespace does not exempt it.
        let d2 = risk_doc("VULN-081", &[("status", "accepted")], &["ADR-040"]);
        let params2 = with_refs(Some(cfg), &d2, &["ADR-040"]);
        let diags = check_risk_expiry(&d2, Some(&params2), Path::new("."));
        assert_eq!(diags.len(), 3);
    }

    /// BUG-030, the exemption case and the most consequential one: a
    /// phantom `RISK` link did not merely satisfy a requirement, it
    /// *discharged* the signing and time-boxing requirement entirely. An
    /// unsigned, un-time-boxed risk acceptance passed by citing a risk
    /// document that was never written.
    ///
    /// The pair is the test. Identical documents, identical params; the only
    /// difference is whether `RISK-003` exists in the run.
    #[test]
    fn risk_expiry_phantom_risk_link_grants_no_exemption() {
        let cfg =
            serde_json::json!({ "require-when-status": "accepted", "exempt-when-links": "RISK" });
        let d = risk_doc("VULN-082", &[("status", "accepted")], &["RISK-003"]);

        let exists = with_refs(Some(cfg.clone()), &d, &["RISK-003"]);
        assert!(
            check_risk_expiry(&d, Some(&exists), Path::new(".")).is_empty(),
            "a resolving RISK link must still exempt"
        );

        let phantom = with_refs(Some(cfg), &d, &[]);
        assert_eq!(
            check_risk_expiry(&d, Some(&phantom), Path::new(".")).len(),
            3,
            "a RISK that does not exist must not discharge approver/rationale/expires"
        );
    }

    // -- security.remediation-link (ADR-041 § SEC-006) -------------------

    /// A VULN document with `status`, `depends_on`, and an optional body
    /// (parsed into an AST so cross_ref_tokens are populated).
    fn remediation_doc(raw_id: &str, status: &str, depends_on: &[&str], body: &str) -> Document {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "status".to_owned(),
            serde_json::Value::String(status.to_owned()),
        );
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("status".to_owned(), 3u32);
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/security/findings/{raw_id}.md"),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: Some(markdown::parse_ast(body)),
            body: body.to_owned(),
        }
    }

    #[test]
    fn remediation_link_green_with_depends_on() {
        let d = remediation_doc("VULN-014", "mitigated", &["ADR-040"], "Fixed.");
        let params = with_refs(
            Some(serde_json::json!({ "require-when-status": "mitigated" })),
            &d,
            &["ADR-040"],
        );
        assert!(check_remediation_link(&d, Some(&params), Path::new(".")).is_empty());
    }

    #[test]
    fn remediation_link_green_with_body_cross_ref() {
        let d = remediation_doc(
            "VULN-015",
            "mitigated",
            &[],
            "Remediated by ADR-040, which rewrote the auth boundary.",
        );
        let params = with_refs(
            Some(serde_json::json!({ "require-when-status": "mitigated" })),
            &d,
            &["ADR-040"],
        );
        assert!(check_remediation_link(&d, Some(&params), Path::new(".")).is_empty());
    }

    #[test]
    fn remediation_link_flags_mitigated_without_any_link() {
        let d = remediation_doc(
            "VULN-016",
            "mitigated",
            &[],
            "We fixed it, trust me — no link.",
        );
        let params = with_refs(
            Some(serde_json::json!({ "require-when-status": "mitigated" })),
            &d,
            &[],
        );
        let diags = check_remediation_link(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "security.remediation-link");
        assert_eq!(diags[0].line, Some(3));
        assert_eq!(
            diags[0].message,
            "VULN-016: mitigated finding must cross-ref its remediation — a resolving ADR link \
             or a `remediation_link`"
        );
    }

    /// BUG-031: `ctxgrd new VULN` scaffolds `# VULN-001: <title>` as the
    /// body H1, and the rule accepted a token of *any* namespace — so every
    /// scaffolded finding self-satisfied from birth and marking one
    /// `mitigated` was enough to pass. Nothing had to be fixed.
    ///
    /// The pair differs only in whether the H1 carries the host's own id.
    /// After the fix neither passes; before it, the first one did.
    #[test]
    fn remediation_link_own_id_in_scaffolded_h1_is_not_a_remediation() {
        let cfg = serde_json::json!({ "require-when-status": "mitigated" });

        let scaffolded = remediation_doc(
            "VULN-019",
            "mitigated",
            &[],
            "# VULN-019: Unauthenticated admin endpoint\n\nFixed it.",
        );
        let params = with_refs(Some(cfg.clone()), &scaffolded, &[]);
        assert_eq!(
            check_remediation_link(&scaffolded, Some(&params), Path::new(".")).len(),
            1,
            "the document's own id is not evidence that it was remediated"
        );

        let retitled = remediation_doc(
            "VULN-019",
            "mitigated",
            &[],
            "# Unauthenticated admin endpoint\n\nFixed it.",
        );
        let params = with_refs(Some(cfg.clone()), &retitled, &[]);
        assert_eq!(
            check_remediation_link(&retitled, Some(&params), Path::new(".")).len(),
            1,
            "removing the scaffolded H1 must not change the verdict — otherwise the test \
             cannot tell a fix from a rename"
        );

        // And the fix does not simply ban everything: the same scaffolded
        // H1 plus a real ADR link passes.
        let fixed = remediation_doc(
            "VULN-019",
            "mitigated",
            &["ADR-040"],
            "# VULN-019: Unauthenticated admin endpoint\n\nFixed it.",
        );
        let params = with_refs(Some(cfg), &fixed, &["ADR-040"]);
        assert!(check_remediation_link(&fixed, Some(&params), Path::new(".")).is_empty());
    }

    /// BUG-031 option 2: an unresolvable token is not a remediation either,
    /// and an id outside `accepted-namespaces` does not stand in for one.
    #[test]
    fn remediation_link_rejects_phantom_and_off_vocabulary_links() {
        let cfg = serde_json::json!({ "require-when-status": "mitigated" });

        // A well-formed ADR id for an ADR nobody wrote (BUG-030).
        let phantom = remediation_doc("VULN-020", "mitigated", &["ADR-999"], "Fixed.");
        let params = with_refs(Some(cfg.clone()), &phantom, &[]);
        assert_eq!(
            check_remediation_link(&phantom, Some(&params), Path::new(".")).len(),
            1
        );

        // A resolving link into a namespace that is not the remediation
        // vocabulary. `accepted-namespaces` defaults to ADR.
        let off_vocab = remediation_doc("VULN-021", "mitigated", &["POLICY-002"], "Fixed.");
        let params = with_refs(Some(cfg.clone()), &off_vocab, &["POLICY-002"]);
        assert_eq!(
            check_remediation_link(&off_vocab, Some(&params), Path::new(".")).len(),
            1
        );

        // Widening the vocabulary admits it — the param is the knob, not
        // the absence of one.
        let widened = serde_json::json!({
            "require-when-status": "mitigated",
            "accepted-namespaces": ["ADR", "POLICY"],
        });
        let params = with_refs(Some(widened), &off_vocab, &["POLICY-002"]);
        assert!(check_remediation_link(&off_vocab, Some(&params), Path::new(".")).is_empty());
    }

    /// The external-tracker escape (BUG-031 option 2): a fix tracked outside
    /// the document graph goes in an explicit field, so it is legible as
    /// "opaque external pointer" rather than masquerading as a cross-ref
    /// nothing can resolve. An empty value is not an escape.
    #[test]
    fn remediation_link_accepts_explicit_field_but_not_an_empty_one() {
        let cfg = serde_json::json!({ "require-when-status": "mitigated" });

        let mut d = remediation_doc("VULN-022", "mitigated", &[], "Fixed.");
        d.metadata.insert(
            "remediation_link".to_owned(),
            serde_json::Value::String("JIRA-421".to_owned()),
        );
        let params = with_refs(Some(cfg.clone()), &d, &[]);
        assert!(check_remediation_link(&d, Some(&params), Path::new(".")).is_empty());

        let mut empty = remediation_doc("VULN-023", "mitigated", &[], "Fixed.");
        empty.metadata.insert(
            "remediation_link".to_owned(),
            serde_json::Value::String("   ".to_owned()),
        );
        let params = with_refs(Some(cfg), &empty, &[]);
        assert_eq!(
            check_remediation_link(&empty, Some(&params), Path::new(".")).len(),
            1
        );
    }

    #[test]
    fn remediation_link_silent_for_open_finding_out_of_scope() {
        let d = remediation_doc("VULN-017", "open", &[], "Still investigating.");
        let params = with_refs(
            Some(serde_json::json!({ "require-when-status": "mitigated" })),
            &d,
            &[],
        );
        assert!(check_remediation_link(&d, Some(&params), Path::new(".")).is_empty());
    }

    #[test]
    fn remediation_link_acts_unconditionally_without_status_param() {
        // No require-when-status: the rule always acts, so an unlinked
        // finding is flagged regardless of status.
        let d = remediation_doc("VULN-018", "accepted", &[], "No link here.");
        let params = with_refs(None, &d, &[]);
        let diags = check_remediation_link(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1);
    }

    /// The fail-closed contract (BUG-030/BUG-031). Every rule in
    /// `RESOLUTION_AWARE_RULES` is a false green in its unfixed form, so a
    /// dropped param threading must produce a loud diagnostic rather than
    /// silence. This test pins that: identical documents that would pass
    /// with the resolution view fail without it.
    ///
    /// It is the test that would catch a future "simplification" of the
    /// `run.rs` dispatch — and the reason the fallback is not permissive.
    #[test]
    fn conditional_link_rules_fail_closed_without_resolution() {
        let cfg = serde_json::json!({ "require-when-status": "mitigated" });
        let d = remediation_doc("VULN-024", "mitigated", &["ADR-040"], "Fixed.");
        let threaded = with_refs(Some(cfg.clone()), &d, &["ADR-040"]);
        assert!(check_remediation_link(&d, Some(&threaded), Path::new(".")).is_empty());
        assert_eq!(
            check_remediation_link(&d, Some(&cfg), Path::new(".")).len(),
            1,
            "without the resolved-refs param the rule must report a gap, not stay silent"
        );

        let ropa = ropa_doc("ROPA-020", "processor", &["DPA-003"], "Payroll processing.");
        let threaded = with_refs(None, &ropa, &["DPA-003"]);
        assert!(check_processor_dpa(&ropa, Some(&threaded), Path::new(".")).is_empty());
        assert_eq!(
            check_processor_dpa(&ropa, None, Path::new(".")).len(),
            1,
            "a parameterless rule must fail closed too — `None` params means no resolution"
        );
    }

    // -- gdpr.processor-dpa (ADR-066 § GDPR-002) ------------------------

    /// A ROPA record with a `controller_or_processor` role, `depends_on`
    /// links, and an optional body (parsed so cross_ref_tokens populate).
    fn ropa_doc(raw_id: &str, role: &str, depends_on: &[&str], body: &str) -> Document {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "controller_or_processor".to_owned(),
            serde_json::Value::String(role.to_owned()),
        );
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("controller_or_processor".to_owned(), 4u32);
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/compliance/gdpr/ropa/{raw_id}.md"),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: Some(markdown::parse_ast(body)),
            body: body.to_owned(),
        }
    }

    #[test]
    fn processor_dpa_green_with_dpa_depends_on() {
        let d = ropa_doc("ROPA-007", "processor", &["DPA-003"], "Payroll processing.");
        let params = with_refs(None, &d, &["DPA-003"]);
        assert!(check_processor_dpa(&d, Some(&params), Path::new(".")).is_empty());
    }

    #[test]
    fn processor_dpa_green_with_dpa_body_cross_ref() {
        let d = ropa_doc(
            "ROPA-008",
            "processor",
            &[],
            "Governed by DPA-003, the executed processor agreement.",
        );
        let params = with_refs(None, &d, &["DPA-003"]);
        assert!(check_processor_dpa(&d, Some(&params), Path::new(".")).is_empty());
    }

    /// BUG-030: a processor-role ROPA discharged its Art. 28 obligation by
    /// citing a DPA that does not exist. The pair differs only in whether
    /// `DPA-003` is in the run.
    #[test]
    fn processor_dpa_phantom_dpa_does_not_satisfy() {
        let d = ropa_doc("ROPA-010", "processor", &["DPA-003"], "Payroll processing.");
        let exists = with_refs(None, &d, &["DPA-003"]);
        assert!(check_processor_dpa(&d, Some(&exists), Path::new(".")).is_empty());
        let phantom = with_refs(None, &d, &[]);
        assert_eq!(
            check_processor_dpa(&d, Some(&phantom), Path::new(".")).len(),
            1
        );
    }

    #[test]
    fn processor_dpa_flags_processor_without_dpa_link() {
        let d = ropa_doc("ROPA-009", "processor", &["ADR-040"], "Linked to an ADR, not a DPA.");
        let diags = check_processor_dpa(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "gdpr.processor-dpa");
        assert_eq!(diags[0].line, Some(4));
        assert_eq!(
            diags[0].message,
            "ROPA-009: a processor-role ROPA must cross-ref its governing `DPA` \
             (the Art. 28 agreement)"
        );
    }

    #[test]
    fn processor_dpa_silent_for_controller_out_of_scope() {
        let d = ropa_doc("ROPA-010", "controller", &[], "We decide the means.");
        assert!(check_processor_dpa(&d, None, Path::new(".")).is_empty());
    }

    // -- hipaa.safeguard-evidence (ADR-066 § HIPAA-002) -----------------

    /// A SAFEGUARD mapping carrying a `safeguard`, optional `justification`,
    /// `depends_on` links, and an optional body.
    fn srmap_doc(
        raw_id: &str,
        safeguard: &str,
        justification: Option<&str>,
        depends_on: &[&str],
        body: &str,
    ) -> Document {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "safeguard".to_owned(),
            serde_json::Value::String(safeguard.to_owned()),
        );
        if let Some(j) = justification {
            metadata.insert(
                "justification".to_owned(),
                serde_json::Value::String(j.to_owned()),
            );
        }
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("safeguard".to_owned(), 5u32);
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/compliance/hipaa/safeguards/{raw_id}.md"),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: Some(markdown::parse_ast(body)),
            body: body.to_owned(),
        }
    }

    /// The addressable list as the generator emits it (a representative
    /// subset is enough for the rule's branching).
    fn addressable_params() -> Value {
        serde_json::json!({
            "addressable": ["164.308.security_reminders", "164.312.encryption_decryption"]
        })
    }

    #[test]
    fn safeguard_evidence_green_with_policy_link() {
        // A required safeguard with a POLICY cross-ref lints clean.
        let d = srmap_doc(
            "SAFEGUARD-001",
            "164.308.risk_analysis",
            None,
            &["POLICY-002"],
            "Implemented by our risk-analysis policy.",
        );
        let params = with_refs(Some(addressable_params()), &d, &["POLICY-002"]);
        assert!(check_safeguard_evidence(&d, Some(&params), Path::new(".")).is_empty());
    }

    /// BUG-030: a required safeguard cited a POLICY that does not exist and
    /// the register reported clean. The pair differs only in whether
    /// `POLICY-002` is in the run — the message that appears is the
    /// "needs implementing evidence" one, which is the accurate statement:
    /// evidence that does not exist is not evidence.
    #[test]
    fn safeguard_evidence_phantom_policy_is_not_evidence() {
        let d = srmap_doc(
            "SAFEGUARD-010",
            "164.308.risk_analysis",
            None,
            &["POLICY-002"],
            "Implemented by our risk-analysis policy.",
        );
        let exists = with_refs(Some(addressable_params()), &d, &["POLICY-002"]);
        assert!(check_safeguard_evidence(&d, Some(&exists), Path::new(".")).is_empty());
        let phantom = with_refs(Some(addressable_params()), &d, &[]);
        let diags = check_safeguard_evidence(&d, Some(&phantom), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("needs implementing evidence"));
    }

    #[test]
    fn safeguard_evidence_flags_required_without_evidence() {
        let d = srmap_doc("SAFEGUARD-002", "164.308.risk_analysis", None, &[], "No evidence yet.");
        let diags = check_safeguard_evidence(&d, Some(&addressable_params()), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "hipaa.safeguard-evidence");
        assert_eq!(diags[0].line, Some(5));
        assert_eq!(
            diags[0].message,
            "SAFEGUARD-002: required safeguard `164.308.risk_analysis` needs implementing evidence \
             (a POLICY or ADR cross-ref)"
        );
    }

    #[test]
    fn safeguard_evidence_required_justification_does_not_excuse() {
        // A required safeguard cannot substitute a justification for evidence.
        let d = srmap_doc(
            "SAFEGUARD-003",
            "164.308.risk_analysis",
            Some("We think it is fine."),
            &[],
            "Required, but no link.",
        );
        let diags = check_safeguard_evidence(&d, Some(&addressable_params()), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "hipaa.safeguard-evidence");
    }

    #[test]
    fn safeguard_evidence_addressable_justification_is_accepted() {
        // An addressable safeguard may stand on a documented justification.
        let d = srmap_doc(
            "SAFEGUARD-004",
            "164.308.security_reminders",
            Some("Equivalent control: monthly all-hands security briefing."),
            &[],
            "Addressable, justified.",
        );
        assert!(check_safeguard_evidence(&d, Some(&addressable_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn safeguard_evidence_flags_addressable_without_evidence_or_justification() {
        let d = srmap_doc("SAFEGUARD-005", "164.308.security_reminders", None, &[], "Nothing here.");
        let diags = check_safeguard_evidence(&d, Some(&addressable_params()), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].message,
            "SAFEGUARD-005: addressable safeguard `164.308.security_reminders` needs implementing \
             evidence (a POLICY or ADR cross-ref) or a `justification` field"
        );
    }

    #[test]
    fn safeguard_evidence_green_with_adr_body_cross_ref() {
        let d = srmap_doc(
            "SAFEGUARD-006",
            "164.312.encryption_decryption",
            None,
            &[],
            "Implemented per ADR-040, which mandates at-rest encryption.",
        );
        let params = with_refs(Some(addressable_params()), &d, &["ADR-040"]);
        assert!(check_safeguard_evidence(&d, Some(&params), Path::new(".")).is_empty());
    }

    // -- soc2.control-evidence (ADR-069 § SOC-002) ----------------------

    /// A SOC2 control carrying a `criterion`, `status`, optional
    /// `evidence_link`, `depends_on` links, and an optional body.
    fn soc2_doc(
        raw_id: &str,
        criterion: &str,
        status: &str,
        evidence_link: Option<&str>,
        depends_on: &[&str],
        body: &str,
    ) -> Document {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "criterion".to_owned(),
            serde_json::Value::String(criterion.to_owned()),
        );
        metadata.insert(
            "status".to_owned(),
            serde_json::Value::String(status.to_owned()),
        );
        if let Some(link) = evidence_link {
            metadata.insert(
                "evidence_link".to_owned(),
                serde_json::Value::String(link.to_owned()),
            );
        }
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("criterion".to_owned(), 7u32);
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/compliance/soc2/controls/{raw_id}.md"),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: Some(markdown::parse_ast(body)),
            body: body.to_owned(),
        }
    }

    /// The soc2.control-evidence params as the generator emits them.
    fn control_evidence_params() -> Value {
        serde_json::json!({
            "evidence-fields": ["evidence_link"],
            "out-of-scope-status": ["not-applicable"]
        })
    }

    #[test]
    fn control_evidence_flags_in_scope_without_evidence() {
        // SOC-002: an in-scope control citing a criterion with no evidence
        // link and no cross-ref is flagged.
        let d = soc2_doc("SOC2-001", "CC6.1", "implemented", None, &[], "No evidence yet.");
        let diags = check_control_evidence(&d, Some(&control_evidence_params()), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "soc2.control-evidence");
        assert_eq!(diags[0].line, Some(7));
        assert_eq!(
            diags[0].message,
            "SOC2-001: in-scope control for criterion `CC6.1` needs operating-effectiveness evidence \
             (a POLICY or ADR cross-ref) or an `evidence_link`"
        );
    }

    #[test]
    fn control_evidence_green_with_evidence_link() {
        // SOC-002: the same control with a non-empty evidence_link lints clean.
        let d = soc2_doc(
            "SOC2-002",
            "CC6.1",
            "implemented",
            Some("https://wiki.example.com/access-reviews/2026-Q2"),
            &[],
            "Quarterly access review.",
        );
        assert!(check_control_evidence(&d, Some(&control_evidence_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn control_evidence_green_with_policy_cross_ref() {
        // A POLICY cross-ref is operating-effectiveness evidence too.
        let d = soc2_doc(
            "SOC2-003",
            "CC6.2",
            "implemented",
            None,
            &["POLICY-004"],
            "Governed by our access-provisioning policy.",
        );
        let params = with_refs(Some(control_evidence_params()), &d, &["POLICY-004"]);
        assert!(check_control_evidence(&d, Some(&params), Path::new(".")).is_empty());
    }

    /// BUG-030 on the rule whose stated purpose the phantom reference
    /// defeats (ADR-069 § SOC-002): a Type II attestation asserts operating
    /// effectiveness over a period, and a control naming a POLICY nobody
    /// wrote reported clean. The pair differs only in whether `POLICY-004`
    /// is in the run.
    #[test]
    fn control_evidence_phantom_policy_is_not_evidence() {
        let d = soc2_doc(
            "SOC2-010",
            "CC6.2",
            "implemented",
            None,
            &["POLICY-004"],
            "Governed by our access-provisioning policy.",
        );
        let exists = with_refs(Some(control_evidence_params()), &d, &["POLICY-004"]);
        assert!(check_control_evidence(&d, Some(&exists), Path::new(".")).is_empty());
        let phantom = with_refs(Some(control_evidence_params()), &d, &[]);
        assert_eq!(
            check_control_evidence(&d, Some(&phantom), Path::new(".")).len(),
            1
        );
    }

    #[test]
    fn control_evidence_exempts_not_applicable() {
        // An out-of-scope (not-applicable) control owes no evidence.
        let d = soc2_doc("SOC2-004", "PI1.1", "not-applicable", None, &[], "We process no transactions.");
        assert!(check_control_evidence(&d, Some(&control_evidence_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn control_evidence_silent_without_criterion() {
        // Presence of `criterion` is core.required-metadata's concern; the
        // rule is silent when none is declared.
        let mut metadata = BTreeMap::new();
        metadata.insert("status".to_owned(), serde_json::Value::String("implemented".to_owned()));
        let d = Document {
            id: "SOC2-005".parse().unwrap(),
            raw_id: "SOC2-005".to_owned(),
            location: "docs/compliance/soc2/controls/SOC2-005.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata,
            pin: None,
            ast: Some(markdown::parse_ast("")),
            body: String::new(),
        };
        assert!(check_control_evidence(&d, Some(&control_evidence_params()), Path::new(".")).is_empty());
    }

    // -- iso27001.control-evidence (ADR-070 § ISO-002) ------------------
    // -- nist.control-evidence (ADR-071 § NIST-002) ---------------------

    /// A control register entry (ISO27001 or NIST80053) carrying a `control`,
    /// `status`, optional `evidence_link`, `depends_on` links, and a body. The
    /// trigger field is `control` for both rules.
    fn control_doc(
        raw_id: &str,
        location: &str,
        control: &str,
        status: &str,
        evidence_link: Option<&str>,
        depends_on: &[&str],
        body: &str,
    ) -> Document {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "control".to_owned(),
            serde_json::Value::String(control.to_owned()),
        );
        metadata.insert(
            "status".to_owned(),
            serde_json::Value::String(status.to_owned()),
        );
        if let Some(link) = evidence_link {
            metadata.insert(
                "evidence_link".to_owned(),
                serde_json::Value::String(link.to_owned()),
            );
        }
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("control".to_owned(), 6u32);
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: location.to_owned(),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: Some(markdown::parse_ast(body)),
            body: body.to_owned(),
        }
    }

    /// The control-evidence params as the generator emits them for iso/nist.
    fn iso_nist_params() -> Value {
        serde_json::json!({
            "evidence-fields": ["evidence_link"],
            "out-of-scope-status": ["not-applicable"]
        })
    }

    #[test]
    fn iso_control_evidence_flags_in_scope_without_evidence() {
        // ISO-002: an in-scope control citing an Annex A id with no evidence
        // link and no cross-ref is flagged.
        let d = control_doc(
            "ISO27001-001",
            "docs/compliance/iso-27001/controls/ISO27001-001.md",
            "A.5.15",
            "implemented",
            None,
            &[],
            "Access control policy in progress.",
        );
        let diags = check_iso_control_evidence(&d, Some(&iso_nist_params()), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "iso27001.control-evidence");
        assert_eq!(diags[0].line, Some(6));
        assert_eq!(
            diags[0].message,
            "ISO27001-001: in-scope control `A.5.15` needs implementing evidence \
             (a POLICY or ADR cross-ref) or an `evidence_link`"
        );
    }

    #[test]
    fn iso_control_evidence_green_with_evidence_link() {
        // ISO-002: the same control with a non-empty evidence_link lints clean.
        let d = control_doc(
            "ISO27001-002",
            "docs/compliance/iso-27001/controls/ISO27001-002.md",
            "A.8.9",
            "implemented",
            Some("https://wiki.example.com/config-baseline/2026-Q2"),
            &[],
            "Configuration management baseline.",
        );
        assert!(check_iso_control_evidence(&d, Some(&iso_nist_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn iso_control_evidence_green_with_policy_cross_ref() {
        // A POLICY cross-ref is implementing evidence too.
        let d = control_doc(
            "ISO27001-003",
            "docs/compliance/iso-27001/controls/ISO27001-003.md",
            "A.5.1",
            "implemented",
            None,
            &["POLICY-007"],
            "Governed by our information security policy.",
        );
        let params = with_refs(Some(iso_nist_params()), &d, &["POLICY-007"]);
        assert!(check_iso_control_evidence(&d, Some(&params), Path::new(".")).is_empty());
    }

    #[test]
    fn iso_control_evidence_exempts_not_applicable() {
        // An out-of-scope (not-applicable) control owes no evidence — the SoA
        // not-applicable decision rides the control `status`.
        let d = control_doc(
            "ISO27001-004",
            "docs/compliance/iso-27001/controls/ISO27001-004.md",
            "A.7.4",
            "not-applicable",
            None,
            &[],
            "We operate no physical premises.",
        );
        assert!(check_iso_control_evidence(&d, Some(&iso_nist_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn iso_control_evidence_silent_without_control() {
        // Presence of `control` is core.required-metadata's concern; the rule
        // is silent when none is declared.
        let mut metadata = BTreeMap::new();
        metadata.insert("status".to_owned(), serde_json::Value::String("implemented".to_owned()));
        let d = Document {
            id: "ISO27001-005".parse().unwrap(),
            raw_id: "ISO27001-005".to_owned(),
            location: "docs/compliance/iso-27001/controls/ISO27001-005.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata,
            pin: None,
            ast: Some(markdown::parse_ast("")),
            body: String::new(),
        };
        assert!(check_iso_control_evidence(&d, Some(&iso_nist_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn nist_control_evidence_flags_in_scope_without_evidence() {
        // NIST-002: an in-scope control citing a family with no evidence link
        // and no cross-ref is flagged, with NIST-specific wording.
        let d = control_doc(
            "NIST80053-001",
            "docs/compliance/nist-800-53/controls/NIST80053-001.md",
            "AC",
            "implemented",
            None,
            &[],
            "Access control narrative pending.",
        );
        let diags = check_nist_control_evidence(&d, Some(&iso_nist_params()), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "nist.control-evidence");
        assert_eq!(diags[0].line, Some(6));
        assert_eq!(
            diags[0].message,
            "NIST80053-001: in-scope control for family `AC` needs implementing evidence \
             (a POLICY or ADR cross-ref) or an `evidence_link`"
        );
    }

    #[test]
    fn nist_control_evidence_green_with_evidence_link() {
        let d = control_doc(
            "NIST80053-002",
            "docs/compliance/nist-800-53/controls/NIST80053-002.md",
            "AU",
            "implemented",
            Some("https://wiki.example.com/ssp/audit-and-accountability"),
            &[],
            "Audit and accountability SSP narrative.",
        );
        assert!(check_nist_control_evidence(&d, Some(&iso_nist_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn nist_control_evidence_exempts_not_applicable() {
        let d = control_doc(
            "NIST80053-003",
            "docs/compliance/nist-800-53/controls/NIST80053-003.md",
            "PT",
            "not-applicable",
            None,
            &[],
            "We process no PII.",
        );
        assert!(check_nist_control_evidence(&d, Some(&iso_nist_params()), Path::new(".")).is_empty());
    }

    // -- core.evidence-link (ADR-115 § REG-001) -------------------------

    /// A register entry over an arbitrary trigger field — the shape
    /// `core.evidence-link` reads, with `field` naming the key rather than
    /// the rule hardcoding one.
    fn register_doc(
        raw_id: &str,
        field: &str,
        value: &str,
        status: &str,
        evidence_link: Option<&str>,
        depends_on: &[&str],
        body: &str,
    ) -> Document {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            field.to_owned(),
            serde_json::Value::String(value.to_owned()),
        );
        metadata.insert(
            "status".to_owned(),
            serde_json::Value::String(status.to_owned()),
        );
        if let Some(link) = evidence_link {
            metadata.insert(
                "evidence_link".to_owned(),
                serde_json::Value::String(link.to_owned()),
            );
        }
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert(field.to_owned(), 6u32);
        frontmatter_lines.insert("id".to_owned(), 2u32);
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/compliance/nis2/measures/{raw_id}.md"),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: Some(markdown::parse_ast(body)),
            body: body.to_owned(),
        }
    }

    /// `core.evidence-link` params as the `nis2` pack ships them.
    fn nis2_params() -> Value {
        serde_json::json!({
            "field": "measure",
            "evidence-fields": ["evidence_link"],
            "out-of-scope-status": ["not-applicable"]
        })
    }

    #[test]
    fn evidence_link_errors_when_field_param_absent() {
        // REG-001: a namespace binding this rule must say which key carries
        // the obligation id. The failure mode for a misconfiguration must be
        // a diagnostic, never silence — a rule advertising an evidence gate
        // while enforcing none is the defect this whole family fixes. Note
        // the document itself is *fine*; only the config is wrong.
        let d = register_doc(
            "NIS2-001",
            "measure",
            "21(2)(b)",
            "implemented",
            Some("https://grc.example/controls/ir-01"),
            &[],
            "Incident handling is implemented.",
        );
        let params = serde_json::json!({ "evidence-fields": ["evidence_link"] });
        let diags = check_evidence_link(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.evidence-link");
        assert_eq!(
            diags[0].message,
            "NIS2-001: [NIS2.\"core.evidence-link\"] must declare `field` — the metadata key \
             carrying the obligation identifier"
        );
    }

    #[test]
    fn evidence_link_field_param_present_is_the_positive_control() {
        // The pair for the test above: the same document, the same evidence,
        // with `field` declared — silent. Without this, "errors when the
        // param is absent" is indistinguishable from "always errors".
        let d = register_doc(
            "NIS2-001",
            "measure",
            "21(2)(b)",
            "implemented",
            Some("https://grc.example/controls/ir-01"),
            &[],
            "Incident handling is implemented.",
        );
        assert!(check_evidence_link(&d, Some(&nis2_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn evidence_link_phantom_cross_ref_is_not_evidence() {
        // BUG-030 for the regime-neutral rule: citing a POLICY nobody wrote
        // must not satisfy the gate. Asserted as a pair with the test below,
        // which cites a POLICY that does resolve.
        let d = register_doc(
            "NIS2-002",
            "measure",
            "21(2)(a)",
            "implemented",
            None,
            &["POLICY-999"],
            "Risk analysis policy.",
        );
        let params = with_refs(Some(nis2_params()), &d, &[]);
        let diags = check_evidence_link(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.evidence-link");
        assert_eq!(
            diags[0].message,
            "NIS2-002: in-scope entry for `measure` `21(2)(a)` needs implementing evidence \
             (a POLICY or ADR cross-ref) or an `evidence_link`"
        );
    }

    #[test]
    fn evidence_link_resolving_cross_ref_satisfies() {
        let d = register_doc(
            "NIS2-002",
            "measure",
            "21(2)(a)",
            "implemented",
            None,
            &["POLICY-001"],
            "Risk analysis policy.",
        );
        let params = with_refs(Some(nis2_params()), &d, &["POLICY-001"]);
        assert!(check_evidence_link(&d, Some(&params), Path::new(".")).is_empty());
    }

    #[test]
    fn evidence_link_own_id_is_not_evidence() {
        // BUG-031's shape, for a namespace whose own id shares the evidence
        // vocabulary: the scaffolded H1 must not satisfy the gate.
        let d = register_doc(
            "POLICY-007",
            "measure",
            "21(2)(c)",
            "implemented",
            None,
            &[],
            "# POLICY-007: Business continuity\n\nDrafted.",
        );
        let params = with_refs(Some(nis2_params()), &d, &[]);
        let diags = check_evidence_link(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1, "own id must not count as its own evidence");
    }

    #[test]
    fn evidence_link_message_names_the_configured_namespaces() {
        // The diagnostic must name the namespaces the rule actually accepts.
        // A pack narrowing `evidence-namespaces` to ADR previously still read
        // "a POLICY or ADR cross-ref", telling the author to do something the
        // rule would reject.
        let d = register_doc(
            "NIS2-003",
            "measure",
            "21(2)(d)",
            "implemented",
            None,
            &[],
            "Supply chain security.",
        );
        let mut params = nis2_params();
        params["evidence-namespaces"] = serde_json::json!(["ADR"]);
        let diags = check_evidence_link(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert!(
            diags[0].message.contains("(an ADR cross-ref)"),
            "message must name the configured namespace: {}",
            diags[0].message
        );
        assert!(
            !diags[0].message.contains("POLICY"),
            "message must not name a namespace the rule rejects: {}",
            diags[0].message
        );
    }

    // -- ddd.context-map-shape (ADR-082 § DDD-003) ----------------------

    /// A CONTEXTMAP edge doc carrying `depends_on` endpoints, a `pattern`, and
    /// optional `upstream`/`downstream` role fields — the shape
    /// `ddd.context-map-shape` reads.
    fn ctxmap_doc(
        raw_id: &str,
        pattern: &str,
        depends_on: &[&str],
        upstream: Option<&str>,
        downstream: Option<&str>,
    ) -> Document {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "pattern".to_owned(),
            serde_json::Value::String(pattern.to_owned()),
        );
        if let Some(up) = upstream {
            metadata.insert("upstream".to_owned(), serde_json::Value::String(up.to_owned()));
        }
        if let Some(down) = downstream {
            metadata.insert(
                "downstream".to_owned(),
                serde_json::Value::String(down.to_owned()),
            );
        }
        let mut frontmatter_lines = BTreeMap::new();
        frontmatter_lines.insert("depends_on".to_owned(), 5u32);
        frontmatter_lines.insert("pattern".to_owned(), 8u32);
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/ddd/context-maps/{raw_id}.md"),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: Some(markdown::parse_ast("")),
            body: String::new(),
        }
    }

    /// The pack's `ddd.context-map-shape` params (defaults, made explicit).
    fn context_map_shape_params() -> Value {
        serde_json::json!({
            "exact_context_count": 2,
            "symmetric_patterns": ["Partnership", "Shared Kernel", "Separate Ways"]
        })
    }

    #[test]
    fn context_map_shape_clean_asymmetric_edge() {
        // A well-formed Customer-Supplier edge: exactly two BOUNDEDCONTEXT
        // endpoints and both direction roles named.
        let d = ctxmap_doc(
            "CONTEXTMAP-001",
            "Customer-Supplier",
            &["BOUNDEDCONTEXT-1", "BOUNDEDCONTEXT-2"],
            Some("BOUNDEDCONTEXT-1"),
            Some("BOUNDEDCONTEXT-2"),
        );
        assert!(check_context_map_shape(&d, Some(&context_map_shape_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn context_map_shape_clean_symmetric_edge() {
        // A Partnership edge: two endpoints, no direction roles.
        let d = ctxmap_doc(
            "CONTEXTMAP-002",
            "Partnership",
            &["BOUNDEDCONTEXT-3", "BOUNDEDCONTEXT-4"],
            None,
            None,
        );
        assert!(check_context_map_shape(&d, Some(&context_map_shape_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn context_map_shape_flags_one_endpoint() {
        // DDD-003: a map resolving to fewer than two BOUNDEDCONTEXT ids fails
        // the cardinality half.
        let d = ctxmap_doc(
            "CONTEXTMAP-003",
            "Partnership",
            &["BOUNDEDCONTEXT-5"],
            None,
            None,
        );
        let diags = check_context_map_shape(&d, Some(&context_map_shape_params()), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "ddd.context-map-shape");
        assert_eq!(diags[0].line, Some(5));
        assert_eq!(
            diags[0].message,
            "CONTEXTMAP-003: a context map must connect exactly 2 BOUNDEDCONTEXT contexts, found 1"
        );
    }

    #[test]
    fn context_map_shape_flags_three_endpoints() {
        // DDD-003: more than two endpoints is also a cardinality error.
        let d = ctxmap_doc(
            "CONTEXTMAP-004",
            "Shared Kernel",
            &["BOUNDEDCONTEXT-6", "BOUNDEDCONTEXT-7", "BOUNDEDCONTEXT-8"],
            None,
            None,
        );
        let diags = check_context_map_shape(&d, Some(&context_map_shape_params()), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "ddd.context-map-shape");
        assert_eq!(
            diags[0].message,
            "CONTEXTMAP-004: a context map must connect exactly 2 BOUNDEDCONTEXT contexts, found 3"
        );
    }

    #[test]
    fn context_map_shape_flags_asymmetric_missing_direction() {
        // DDD-003: an asymmetric Customer-Supplier map missing `upstream`.
        let d = ctxmap_doc(
            "CONTEXTMAP-005",
            "Customer-Supplier",
            &["BOUNDEDCONTEXT-1", "BOUNDEDCONTEXT-2"],
            None,
            Some("BOUNDEDCONTEXT-2"),
        );
        let diags = check_context_map_shape(&d, Some(&context_map_shape_params()), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "ddd.context-map-shape");
        assert_eq!(diags[0].line, Some(8));
        assert_eq!(
            diags[0].message,
            "CONTEXTMAP-005: asymmetric pattern `Customer-Supplier` must declare both `upstream` and `downstream` roles"
        );
    }

    #[test]
    fn context_map_shape_flags_symmetric_declaring_direction() {
        // DDD-003: a symmetric Partnership map declaring a direction role.
        let d = ctxmap_doc(
            "CONTEXTMAP-006",
            "Partnership",
            &["BOUNDEDCONTEXT-1", "BOUNDEDCONTEXT-2"],
            Some("BOUNDEDCONTEXT-1"),
            None,
        );
        let diags = check_context_map_shape(&d, Some(&context_map_shape_params()), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "ddd.context-map-shape");
        assert_eq!(diags[0].line, Some(8));
        assert_eq!(
            diags[0].message,
            "CONTEXTMAP-006: symmetric pattern `Partnership` must not declare `upstream`/`downstream` roles"
        );
    }

    // -- core.acceptance-complete (ADR-056 § EARS-01) -------------------

    /// A document carrying a `status` and a body parsed for headings and
    /// checkboxes — the shape `core.acceptance-complete` reads.
    fn acceptance_doc(raw_id: &str, status: &str, body: &str) -> Document {
        let mut metadata = BTreeMap::new();
        metadata.insert("status".to_owned(), serde_json::Value::String(status.to_owned()));
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: format!("docs/tasks/{raw_id}.md"),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata,
            pin: None,
            ast: Some(markdown::parse_ast(body)),
            body: body.to_owned(),
        }
    }

    /// `terminal = ["done"]` — the TASK terminal status, the SPEC-003
    /// § Data model param example.
    fn task_terminal_params() -> Value {
        serde_json::json!({ "terminal": ["done"] })
    }

    #[test]
    fn acceptance_complete_flags_open_box_on_done_document() {
        // EARS-01.1: a `done` TASK with an open `- [ ]` under `Acceptance`
        // emits one diagnostic anchored at the open item's line.
        let body = "# TASK-014\n\n## Acceptance\n\n- [x] Engine wired\n- [ ] Retry path tested\n";
        let d = acceptance_doc("TASK-014", "done", body);
        let diags = check_acceptance_complete(&d, Some(&task_terminal_params()), Path::new("."));
        assert_eq!(diags.len(), 1, "one open box → one diagnostic: {diags:?}");
        assert_eq!(diags[0].code, "core.acceptance-complete");
        assert_eq!(diags[0].line, Some(6), "anchored at the `- [ ] Retry path tested` line");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Error);
        assert_eq!(
            diags[0].message,
            "TASK-014: unchecked acceptance item under `Acceptance` — a `done` document \
             must have every acceptance criterion checked"
        );
    }

    #[test]
    fn acceptance_complete_one_diagnostic_per_open_box() {
        // EARS-01.1: one diagnostic per unchecked item, not one per section.
        let body =
            "# TASK-015\n\n## Acceptance\n\n- [ ] First criterion\n- [x] Second met\n- [ ] Third criterion\n";
        let d = acceptance_doc("TASK-015", "done", body);
        let diags = check_acceptance_complete(&d, Some(&task_terminal_params()), Path::new("."));
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].line, Some(5));
        assert_eq!(diags[1].line, Some(7));
    }

    #[test]
    fn acceptance_complete_green_when_all_boxes_checked() {
        let body = "# TASK-016\n\n## Acceptance\n\n- [x] Engine wired\n- [x] Retry tested\n";
        let d = acceptance_doc("TASK-016", "done", body);
        assert!(check_acceptance_complete(&d, Some(&task_terminal_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn acceptance_complete_silent_on_non_terminal_status() {
        // EARS-01.4 (opt-in scope): a `doing` TASK may legitimately carry
        // open boxes — the rule only fires at a terminal status.
        let body = "# TASK-017\n\n## Acceptance\n\n- [ ] Retry path tested\n";
        let d = acceptance_doc("TASK-017", "doing", body);
        assert!(check_acceptance_complete(&d, Some(&task_terminal_params()), Path::new(".")).is_empty());
    }

    #[test]
    fn acceptance_complete_ignores_open_box_outside_acceptance_heading() {
        // EARS-01.2: an open `- [ ]` under `Open Questions` is deferred
        // work, not an unmet criterion — it must not fire.
        let body = "# TASK-018\n\n## Acceptance\n\n- [x] All criteria met\n\n## Open Questions\n\n- [ ] Revisit retry budget later\n";
        let d = acceptance_doc("TASK-018", "done", body);
        assert!(
            check_acceptance_complete(&d, Some(&task_terminal_params()), Path::new(".")).is_empty(),
            "open boxes outside the acceptance heading must not fire (EARS-01.2)"
        );
    }

    #[test]
    fn acceptance_complete_scans_definition_of_done_default_heading() {
        // EARS-01.3: the default heading set includes `Definition of Done`.
        let body = "# TASK-019\n\n## Definition of Done\n\n- [ ] Coverage added\n";
        let d = acceptance_doc("TASK-019", "done", body);
        let diags = check_acceptance_complete(&d, Some(&task_terminal_params()), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, Some(5));
    }

    #[test]
    fn acceptance_complete_severity_param_downgrades_to_warning() {
        let body = "# TASK-020\n\n## Acceptance\n\n- [ ] Retry path tested\n";
        let d = acceptance_doc("TASK-020", "done", body);
        let params = serde_json::json!({ "terminal": ["done"], "severity": "warning" });
        let diags = check_acceptance_complete(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
    }

    #[test]
    fn acceptance_complete_fires_on_box_immediately_under_heading() {
        // Boundary guard (review of SPEC-003): an open box on the line
        // DIRECTLY below `## Acceptance`, with no blank line, must fire.
        // `h2_section_window` returns `start` = the 1-based heading line,
        // and `skip(start)` over a 0-based enumerate skips exactly the
        // heading (index start-1) and yields from the first content line
        // (index start) — so the immediately-following item is NOT dropped.
        let body = "# TASK-021\n\n## Acceptance\n- [ ] First criterion right under the heading\n";
        let d = acceptance_doc("TASK-021", "done", body);
        let diags = check_acceptance_complete(&d, Some(&task_terminal_params()), Path::new("."));
        assert_eq!(diags.len(), 1, "the box directly under the heading must fire: {diags:?}");
        assert_eq!(diags[0].line, Some(4), "anchored at the `- [ ]` line, not the heading");
    }

    #[test]
    fn acceptance_complete_default_terminal_set_fires_on_accepted() {
        // With no `terminal` param, the default terminal-status vocabulary
        // applies, so an `accepted` SPEC with an open box fires (EARS-01.3
        // default). PRD/SPEC terminal status is `accepted`.
        let body = "# SPEC-099\n\n## Acceptance\n\n- [ ] Holdout scenarios authored\n";
        let d = acceptance_doc("SPEC-099", "accepted", body);
        let diags = check_acceptance_complete(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1);
    }

    // -- checklist pack (ADR-078) -----------------------------------------

    #[test]
    fn checklist_structure_flags_missing_fence() {
        let d = doc("docs/checklists/x.md", "no frontmatter here\n- [ ] a\n", vec![]);
        let diags = check_checklist_structure(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].code, "checklist.structure");
    }

    #[test]
    fn checklist_structure_empty_title_bad_status_and_no_box() {
        let body = "---\ntitle: \nstatus: done\n---\njust prose, no boxes\n";
        let d = doc("docs/checklists/x.md", body, vec![]);
        let diags = check_checklist_structure(&d, None, Path::new("."));
        // empty title + invalid status + no checkbox
        assert_eq!(diags.len(), 3, "{diags:?}");
    }

    #[test]
    fn checklist_structure_sealed_without_pin_errors() {
        let body = "---\ntitle: Ship it\nstatus: sealed\n---\n- [x] done\n";
        let d = doc("docs/checklists/x.md", body, vec![]);
        let diags = check_checklist_structure(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("pinned_commit"), "{diags:?}");
    }

    #[test]
    fn checklist_structure_valid_living_is_clean() {
        let body = "---\ntitle: Stripe integration\nstatus: living\n---\n- [ ] provision keys\n";
        let d = doc("docs/checklists/x.md", body, vec![]);
        let diags = check_checklist_structure(&d, None, Path::new("."));
        assert_eq!(diags.len(), 0, "{diags:?}");
    }

    #[test]
    fn checklist_complete_fires_one_per_unchecked_only_when_sealed() {
        let sealed = "---\ntitle: T\nstatus: sealed\npinned_commit: deadbeef\n---\n\
                      - [ ] a\n- [ ] b\n- [x] c\n";
        let d = doc("docs/checklists/x.md", sealed, vec![]);
        assert_eq!(check_checklist_complete(&d, None, Path::new(".")).len(), 2);

        let living = "---\ntitle: T\nstatus: living\n---\n- [ ] a\n- [ ] b\n";
        let d2 = doc("docs/checklists/x.md", living, vec![]);
        assert_eq!(check_checklist_complete(&d2, None, Path::new(".")).len(), 0);
    }

    #[test]
    fn checklist_pinned_rejects_malformed_and_skips_living() {
        let sealed = "---\ntitle: T\nstatus: sealed\npinned_commit: abc123\n---\n- [x] a\n";
        let d = doc("docs/checklists/x.md", sealed, vec![]);
        let diags = check_checklist_pinned(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("40-character hex"), "{diags:?}");

        let living = "---\ntitle: T\nstatus: living\npinned_commit: abc123\n---\n- [x] a\n";
        let d2 = doc("docs/checklists/x.md", living, vec![]);
        assert_eq!(check_checklist_pinned(&d2, None, Path::new(".")).len(), 0);
    }

    #[test]
    fn required_headings_normalized_presence_and_noop_when_unset() {
        // Doc H2s: a numbered "1. Plan" and "Go-live".
        let d = doc(
            "docs/checklists/x.md",
            "body",
            vec![(2, "1. Plan"), (2, "Go-live")],
        );
        let params = serde_json::json!({"headings": ["Plan", "Go-live", "Secret storage"]});
        let diags = check_required_headings(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("Secret storage"), "{diags:?}");

        // Unset headings param is a silent no-op.
        assert_eq!(check_required_headings(&d, None, Path::new(".")).len(), 0);
    }

    #[test]
    fn required_anchors_presence_and_noop_when_unset() {
        let d = doc(
            "docs/checklists/x.md",
            "item one <!-- @x.one --> and item two with no anchor",
            vec![],
        );
        let params = serde_json::json!({"anchors": ["@x.one", "@x.two"]});
        let diags = check_required_anchors(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("@x.two"), "{diags:?}");

        // Unset anchors param is a silent no-op.
        assert_eq!(check_required_anchors(&d, None, Path::new(".")).len(), 0);
    }

    // -- test.completion (ADR-098 § QA-003) ---------------------------

    /// A full-pipeline TEST report Document — frontmatter and AST parsed the
    /// way the ingest pipeline does, so the document-level rule reads real
    /// `metadata` / `ast` (not a re-parse).
    fn test_report_doc(body: &str) -> Document {
        let ast = markdown::parse_ast(body);
        let (metadata, frontmatter_lines) =
            crate::frontmatter::Frontmatter::parse_with_lines(body)
                .map(|(fm, lines)| (fm.metadata, lines))
                .unwrap_or_default();
        Document {
            id: "TEST-1".parse().unwrap(),
            raw_id: "TEST-1".to_string(),
            location: "docs/tests/TEST-001-release-1-0.md".to_string(),
            depends_on: Vec::new(),
            frontmatter_lines,
            metadata,
            pin: None,
            ast: Some(ast),
            body: body.to_owned(),
        }
    }

    /// A real 40-hex commit SHA (SHA-1 shape).
    const GOOD_TESTED: &str = "1cb8eaf0aa9b7d2e3f4c5a6b7c8d9e0f1a2b3c4d";
    const GOOD_SPEC: &str = "50c6166f9e8d7c6b5a4f3e2d1c0b9a8776554433";

    fn sealed_body(result: &str, pins: &str, defects: &str) -> String {
        format!(
            "---\nid: TEST-1\ntitle: Release 1.0 completion\nstatus: sealed\n\
             result: {result}\nrelease: 1.0\ndate: 2026-07-13\n\
             evidence: https://ci.example.com/runs/4210\n{pins}---\n\n\
             ## Scope\nSystem and acceptance suites for release 1.0.\n\n\
             ## Test Environment\nstaging, build 4210.\n\n\
             ## Results Summary\n204 passed, 0 failed.\n\n\
             ## Outstanding Defects\n{defects}\n\n\
             ## Exit Criteria\nAll gates met.\n\n\
             ## Sign-off\nQA lead accepted 2026-07-13.\n\n\
             ## References\nSPEC-003; CI run 4210.\n"
        )
    }

    #[test]
    fn test_completion_sealed_missing_tested_commit_fails() {
        let body = sealed_body("pass", &format!("spec_commit: {GOOD_SPEC}\n"), "None.");
        let d = test_report_doc(&body);
        let diags = check_test_completion(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "one diagnostic: {diags:?}");
        assert!(
            diags[0].message.contains("must set `tested_commit`"),
            "names the missing pin: {}",
            diags[0].message
        );
    }

    #[test]
    fn test_completion_sealed_missing_spec_commit_fails() {
        let body = sealed_body("pass", &format!("tested_commit: {GOOD_TESTED}\n"), "None.");
        let d = test_report_doc(&body);
        let diags = check_test_completion(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "one diagnostic: {diags:?}");
        assert!(
            diags[0].message.contains("must set `spec_commit`"),
            "names the missing pin: {}",
            diags[0].message
        );
    }

    #[test]
    fn test_completion_malformed_sha_fails() {
        let pins = format!("tested_commit: not-a-sha\nspec_commit: {GOOD_SPEC}\n");
        let body = sealed_body("pass", &pins, "None.");
        let d = test_report_doc(&body);
        let diags = check_test_completion(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "one diagnostic: {diags:?}");
        assert!(
            diags[0].message.contains("not a 40-character hex commit SHA"),
            "names the shape defect: {}",
            diags[0].message
        );
    }

    #[test]
    fn test_completion_conditional_pass_empty_defects_fails() {
        // Both pins valid; the empty Outstanding Defects section is the only
        // defect. An empty section body (only the heading) must fail.
        let pins = format!("tested_commit: {GOOD_TESTED}\nspec_commit: {GOOD_SPEC}\n");
        let body = sealed_body("conditional-pass", &pins, "");
        let d = test_report_doc(&body);
        let diags = check_test_completion(&d, None, Path::new("."));
        assert_eq!(diags.len(), 1, "one diagnostic: {diags:?}");
        assert!(
            diags[0].message.contains("conditional-pass") && diags[0].message.contains("empty"),
            "names the empty-waiver defect: {}",
            diags[0].message
        );
    }

    #[test]
    fn test_completion_conditional_pass_with_defects_is_clean() {
        let pins = format!("tested_commit: {GOOD_TESTED}\nspec_commit: {GOOD_SPEC}\n");
        let body = sealed_body(
            "conditional-pass",
            &pins,
            "- DEFECT-9: intermittent timeout on the nightly export, waived to 1.1.",
        );
        let d = test_report_doc(&body);
        let diags = check_test_completion(&d, None, Path::new("."));
        assert!(diags.is_empty(), "a named waiver is clean: {diags:?}");
    }

    #[test]
    fn test_completion_draft_with_no_pins_is_clean() {
        let body = "---\nid: TEST-1\ntitle: Release 1.0 completion\nstatus: draft\n\
                    result: pass\nrelease: 1.0\ndate: 2026-07-13\n\
                    evidence: https://ci.example.com/runs/4210\n---\n\n\
                    ## Scope\nDraft in progress.\n";
        let d = test_report_doc(body);
        let diags = check_test_completion(&d, None, Path::new("."));
        assert!(diags.is_empty(), "a draft carries no pins yet: {diags:?}");
    }

    #[test]
    fn test_completion_sealed_pass_empty_defects_is_clean() {
        // A clean pass with both pins present and valid; an empty Outstanding
        // Defects section is fine when the verdict is `pass`, not a waiver.
        let pins = format!("tested_commit: {GOOD_TESTED}\nspec_commit: {GOOD_SPEC}\n");
        let body = sealed_body("pass", &pins, "None.");
        let d = test_report_doc(&body);
        let diags = check_test_completion(&d, None, Path::new("."));
        assert!(diags.is_empty(), "sealed pass with valid pins is clean: {diags:?}");
    }

    // -- writing.ai-fingerprints (ADR-102) ---------------------------------

    /// Build an instruction-file Document (AST populated) for the fingerprint
    /// rule. No frontmatter needed — the rule reads the body, not metadata.
    fn fingerprint_doc(body: &str) -> Document {
        let ast = markdown::parse_ast(body);
        Document {
            id: "CLAUDE-0".parse().unwrap(),
            raw_id: String::new(),
            location: "CLAUDE.md".to_string(),
            depends_on: Vec::new(),
            frontmatter_lines: std::collections::BTreeMap::new(),
            metadata: std::collections::BTreeMap::new(),
            pin: None,
            ast: Some(ast),
            body: body.to_owned(),
        }
    }

    #[test]
    fn fingerprints_flags_curly_quote() {
        let doc = fingerprint_doc("Run the \u{201C}generated\u{201D} build.\n");
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert_eq!(diags.len(), 2, "both curly quotes flagged: {diags:?}");
        assert_eq!(diags[0].code, AI_FINGERPRINTS);
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
        assert_eq!(diags[0].line, Some(1));
    }

    #[test]
    fn fingerprints_flags_decorative_emoji() {
        // Rocket (U+1F680) in a heading and a checkmark (U+2705) in prose.
        let doc = fingerprint_doc("## \u{1F680} Getting Started\nBuild passes \u{2705}\n");
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        let emoji: Vec<_> = diags.iter().filter(|d| d.message.contains("emoji")).collect();
        assert_eq!(emoji.len(), 2, "rocket + checkmark flagged: {diags:?}");
    }

    #[test]
    fn fingerprints_emoji_disabled_by_config() {
        let doc = fingerprint_doc("Ship it \u{1F680}\n");
        let params = serde_json::json!({"flag_emoji": false});
        let diags = check_ai_fingerprints(&doc, Some(&params), Path::new("."));
        assert!(diags.is_empty(), "flag_emoji=false suppresses: {diags:?}");
    }

    #[test]
    fn fingerprints_flags_default_phrases_case_insensitively() {
        // "You're" capitalized — the compiled default is matched case-insensitively.
        let doc = fingerprint_doc("You're absolutely right about the seam.\n");
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        let phr: Vec<_> = diags.iter().filter(|d| d.message.contains("phrase")).collect();
        assert_eq!(phr.len(), 1, "one artifact phrase: {diags:?}");
        assert!(phr[0].message.contains("you're absolutely right"), "{}", phr[0].message);
    }

    #[test]
    fn fingerprints_borderline_phrase_not_in_default() {
        // "let me know if" is deliberately excluded from the shipped default (AIF-005).
        let doc = fingerprint_doc("Let me know if you need anything else.\n");
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert!(diags.is_empty(), "borderline phrase not flagged by default: {diags:?}");
    }

    #[test]
    fn fingerprints_phrases_config_overrides_default() {
        let doc = fingerprint_doc("Let me know if that works.\n");
        let params = serde_json::json!({"phrases": ["let me know if"]});
        let diags = check_ai_fingerprints(&doc, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1, "configured phrase fires: {diags:?}");
    }

    #[test]
    fn fingerprints_no_ai_slop_phrase_is_opt_in() {
        let doc = fingerprint_doc("Here's the thing: the build is broken.\n");
        assert!(
            check_ai_fingerprints(&doc, None, Path::new(".")).is_empty(),
            "approved no-ai-slop phrases are not shipped defaults"
        );
        let params = serde_json::json!({"phrases": ["here's the thing"]});
        let diags = check_ai_fingerprints(&doc, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1, "configured no-ai-slop phrase fires: {diags:?}");
    }

    #[test]
    fn fingerprints_empty_phrases_disables_class() {
        let doc = fingerprint_doc("You're absolutely right.\n");
        let params = serde_json::json!({"phrases": []});
        let diags = check_ai_fingerprints(&doc, Some(&params), Path::new("."));
        assert!(diags.is_empty(), "explicit [] disables phrase class: {diags:?}");
    }

    #[test]
    fn fingerprints_flags_em_dash_overuse() {
        let doc = fingerprint_doc(
            "The parser \u{2014} rewritten \u{2014} walks the tree \u{2014} twice \u{2014} \
             before it emits \u{2014} finally \u{2014} the result.\n",
        );
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        let dash: Vec<_> = diags.iter().filter(|d| d.message.contains("dash")).collect();
        assert_eq!(dash.len(), 1, "one density finding for the whole file: {diags:?}");
        assert!(dash[0].message.contains("density"), "{}", dash[0].message);
    }

    #[test]
    fn fingerprints_no_dash_is_clean() {
        let doc = fingerprint_doc("A plain sentence with no dashes at all here.\n");
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert!(diags.is_empty(), "no dashes, nothing else: {diags:?}");
    }

    #[test]
    fn fingerprints_dash_threshold_zero_disables() {
        let doc = fingerprint_doc("a \u{2014} b \u{2014} c \u{2014} d\n");
        let params = serde_json::json!({"max_em_dashes_per_kwords": 0});
        let diags = check_ai_fingerprints(&doc, Some(&params), Path::new("."));
        assert!(diags.is_empty(), "threshold 0 disables the dash class: {diags:?}");
    }

    #[test]
    fn fingerprints_masks_fenced_code() {
        // Curly quotes, an emoji, and a phrase all inside a fenced block: a
        // sample, not prose. Nothing fires.
        let doc = fingerprint_doc(
            "```sh\necho \"curly \u{201C}x\u{201D} \u{1F680} you're absolutely right\"\n```\n",
        );
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert!(diags.is_empty(), "fenced code is masked: {diags:?}");
    }

    #[test]
    fn fingerprints_masks_inline_code() {
        let doc = fingerprint_doc("Set the value in `config\u{2019}s key` please.\n");
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert!(diags.is_empty(), "inline code span is masked: {diags:?}");
    }

    #[test]
    fn fingerprints_prose_curly_still_fires_with_inline_code_present() {
        // The curly is in prose; the backticked span is masked. Exactly one hit.
        let doc = fingerprint_doc("A \u{201C}real\u{201D} tell near `code` here.\n");
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert_eq!(diags.len(), 2, "both prose curly quotes fire, code masked: {diags:?}");
    }

    #[test]
    fn fingerprints_reports_byte_column_on_multibyte_line() {
        // "café " is 6 bytes (é is 2), so the curly quote sits at byte col 7,
        // not char col 6 — the UTF-8/column-unit guard (AIF-001 trap #2).
        let doc = fingerprint_doc("caf\u{00E9} \u{201C}test\n");
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert_eq!(diags.len(), 1, "one curly quote: {diags:?}");
        assert_eq!(diags[0].col, Some(7), "byte column, not char column");
    }

    #[test]
    fn fingerprints_technical_vocabulary_is_clean() {
        // AIF-002: `seam` and `load-bearing` are legitimate software vocabulary;
        // the deterministic rule must never flag a correctly written doc.
        let doc = fingerprint_doc(
            "The retry logic is load-bearing; the queue drains through this exact seam.\n",
        );
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert!(diags.is_empty(), "technical prose is clean: {diags:?}");
    }

    #[test]
    fn fingerprints_ast_none_skips_maskable_but_runs_phrases() {
        // AIF-001 fallback: no AST → curly/emoji/dash skipped (can't mask),
        // but the phrase class still runs over the raw body.
        let body = "You're absolutely right \u{201C}quote\u{201D} \u{1F680}\n";
        let mut doc = fingerprint_doc(body);
        doc.ast = None;
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert_eq!(diags.len(), 1, "only the phrase fires without an AST: {diags:?}");
        assert!(diags[0].message.contains("phrase"), "{}", diags[0].message);
    }

    // -- regression: the six code-review findings on the first cut -----------

    #[test]
    fn fingerprints_multiline_inline_span_does_not_panic() {
        // Review #1: a backtick span wrapping a newline files col_end on the
        // next line, inverting the byte-slice range in the old mask — a panic
        // that aborted the whole lint. The curly quotes sit before the span and
        // must still fire; the call must not panic.
        let body =
            "Start \u{201C}alpha\u{201D} and a span `line one\ntwo` done here.\n";
        let doc = fingerprint_doc(body);
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert_eq!(diags.len(), 2, "both curly quotes fire, no panic: {diags:?}");
    }

    #[test]
    fn fingerprints_skips_yaml_frontmatter() {
        // Review #3: frontmatter is metadata, not prose. A curly apostrophe and
        // an em-dash in a frontmatter value must not be flagged.
        let body =
            "---\ntitle: Don\u{2019}t \u{2014} do it\nstatus: draft\n---\n\nBody is clean.\n";
        let doc = fingerprint_doc(body);
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert!(diags.is_empty(), "frontmatter values are not prose: {diags:?}");
    }

    #[test]
    fn fingerprints_dash_density_ignores_no_space_dashes() {
        // Review #4: word—word (no surrounding spaces, standard typography) must
        // count as two words, so spaced and unspaced dashes yield equal density.
        let spaced = fingerprint_doc("alpha \u{2014} beta \u{2014} gamma delta epsilon zeta eta\n");
        let packed = fingerprint_doc("alpha\u{2014}beta\u{2014}gamma delta epsilon zeta eta\n");
        let ds = check_ai_fingerprints(&spaced, None, Path::new("."));
        let dp = check_ai_fingerprints(&packed, None, Path::new("."));
        assert_eq!(ds.len(), 1, "spaced fires once: {ds:?}");
        assert_eq!(dp.len(), 1, "packed fires once: {dp:?}");
        assert!(ds[0].message.contains("density 285"), "spaced: {}", ds[0].message);
        assert!(dp[0].message.contains("density 285"), "packed: {}", dp[0].message);
    }

    #[test]
    fn fingerprints_multi_scalar_emoji_reported_once() {
        // Review #5: a flag is two regional-indicator scalars — one visible
        // glyph, one finding, not two.
        let doc = fingerprint_doc("Flag \u{1F1FA}\u{1F1F8} here.\n");
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert_eq!(diags.len(), 1, "one finding for the flag glyph: {diags:?}");
    }

    #[test]
    fn fingerprints_arrows_are_not_emoji() {
        // Review #6: dingbat/block arrows (➡ U+27A1, ⭐ U+2B50) legitimately
        // appear in prose and are excluded from the decorative-emoji ranges.
        let doc = fingerprint_doc("Flow: A \u{27A1} B and a \u{2B50} rating.\n");
        let diags = check_ai_fingerprints(&doc, None, Path::new("."));
        assert!(diags.is_empty(), "arrows and stars are not flagged: {diags:?}");
    }

    // -- core.file-budget (ADR-109) ------------------------------------

    /// A TODO.md-shaped document: a short `## Now` section and a long
    /// `## Shipped` archive, so the largest-section suggestion has an
    /// unambiguous answer.
    fn budget_doc(shipped_lines: usize) -> Document {
        let mut body = String::from("# Project state\n\n## Now\n\nFinish the budget rule.\n\n## Shipped\n\n");
        for n in 0..shipped_lines {
            body.push_str(&format!("- 0.{n}.0 shipped a rule\n"));
        }
        synthetic_document("TODO", "TODO.md".to_owned(), body)
    }

    fn max_chars(n: u64) -> Value {
        serde_json::json!({ "max_chars": n })
    }

    #[test]
    fn file_budget_silent_under_and_at_the_budget() {
        let doc = budget_doc(4);
        let chars = doc.body.chars().count() as u64;
        assert!(
            check_file_budget(&doc, Some(&max_chars(chars)), Path::new(".")).is_empty(),
            "a file exactly at its budget is not over it"
        );
        assert!(
            check_file_budget(&doc, Some(&max_chars(chars + 1)), Path::new(".")).is_empty(),
            "a file under its budget is silent"
        );
    }

    #[test]
    fn file_budget_warns_once_with_the_character_counts() {
        let doc = budget_doc(4);
        let chars = doc.body.chars().count() as u64;
        let diags = check_file_budget(&doc, Some(&max_chars(chars - 10)), Path::new("."));
        assert_eq!(diags.len(), 1, "one finding per over-budget file: {diags:?}");
        assert_eq!(diags[0].code, "core.file-budget");
        assert_eq!(diags[0].severity, crate::diagnostic::Severity::Warning);
        assert_eq!(
            diags[0].message,
            format!("file is {chars} characters (budget {})", chars - 10)
        );
    }

    #[test]
    fn file_budget_help_names_the_largest_section_and_the_overage() {
        let doc = budget_doc(60);
        let chars = doc.body.chars().count() as u64;
        let diags = check_file_budget(&doc, Some(&max_chars(chars - 200)), Path::new("."));
        let help = diags[0].help.as_deref().expect("help suggests a fix");
        assert!(
            help.starts_with("trim 200 characters"),
            "the help states how much has to go: {help}"
        );
        assert!(
            help.contains("`## Shipped`"),
            "the help names the biggest section, not the small one: {help}"
        );
        assert!(
            !help.contains("`## Now`"),
            "only the largest section is suggested: {help}"
        );
    }

    #[test]
    fn file_budget_help_points_at_the_preamble_when_it_outweighs_every_section() {
        // A TODO.md whose bulk is dated narrative above the first `## ` — the
        // largest *section* is small, so naming it would misdirect the fix.
        let preamble = "Shipped X, then Y, then Z. ".repeat(40);
        let body = format!("# State\n\n{preamble}\n\n## Now\n\nOne item.\n");
        let doc = synthetic_document("TODO", "TODO.md".to_owned(), body);
        let chars = doc.body.chars().count() as u64;
        let diags = check_file_budget(&doc, Some(&max_chars(chars - 100)), Path::new("."));
        let help = diags[0].help.as_deref().expect("help suggests a fix");
        assert!(
            help.contains("above the first `## ` heading"),
            "the help points at the unsectioned preamble, not `## Now`: {help}"
        );
        assert!(
            !help.contains("`## Now`"),
            "the small section is not the suggested fix: {help}"
        );
    }

    #[test]
    fn file_budget_help_falls_back_when_the_document_has_no_h2() {
        let doc = synthetic_document(
            "TODO",
            "TODO.md".to_owned(),
            "# Project state\n\nOne long paragraph and no sections at all.\n".to_owned(),
        );
        let diags = check_file_budget(&doc, Some(&max_chars(10)), Path::new("."));
        let help = diags[0].help.as_deref().expect("help suggests a fix");
        assert!(
            help.contains("move settled detail into a linked file"),
            "without H2 sections the advice is generic but still actionable: {help}"
        );
    }

    #[test]
    fn file_budget_note_names_the_namespace_to_raise_the_ceiling_in() {
        let doc = budget_doc(4);
        let diags = check_file_budget(&doc, Some(&max_chars(10)), Path::new("."));
        assert_eq!(
            diags[0].note.as_deref(),
            Some(
                "raise the ceiling with `[TODO.\"core.file-budget\"] max_chars = <n>` \
                 if this size is intended"
            )
        );
    }

    #[test]
    fn file_budget_defaults_to_the_claude_threshold_without_params() {
        // BDG-002: a bare binding reproduces the reader's own 150000-character
        // warning, so a document just over that line fires with no config.
        let doc = budget_doc(0);
        assert!(
            check_file_budget(&doc, None, Path::new(".")).is_empty(),
            "a small file is silent on the default budget"
        );

        let big = synthetic_document(
            "TODO",
            "TODO.md".to_owned(),
            "x".repeat(DEFAULT_MAX_CHARS as usize + 1),
        );
        let diags = check_file_budget(&big, None, Path::new("."));
        assert_eq!(diags.len(), 1, "150001 characters is over the default");
        assert_eq!(
            diags[0].message,
            "file is 150001 characters (budget 150000)"
        );
    }
}
