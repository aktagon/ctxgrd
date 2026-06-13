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
const SPEC_PRD: &str = "spec.requires-prd";
const TODO_LISTED: &str = "todo.listed";
const DESIGN_SECTION_ORDER: &str = "design.section-order";
const DESIGN_TOKEN_REF: &str = "design.token-ref";
const EARS_SYNTAX: &str = "ears.clause-syntax";
const STYLE_SECTION_ORDER: &str = "style.section-order";
const STYLE_SOUL_PAIR: &str = "style.soul-pair";
const SOUL_SECTIONS: &str = "soul.sections";
const PIPELINE_CONFORMANCE: &str = "pipeline.conformance";

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
        let body = std::fs::read_to_string(&path)?;
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
                    suggested_todo_link(doc)
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
                    suggested_todo_link(doc)
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
/// Resolution uses the filesystem (`canonicalize`): the root `TODO.md`
/// is known to exist (the caller checked `is_file()`), so an import that
/// fails to canonicalize is dangling and cannot match — the budget rule
/// reports that separately.
fn references_root_todo_import(doc: &Document, root: &Path, todo: &Path) -> bool {
    let dir = root.join(&doc.location);
    let dir = dir.parent().unwrap_or(root);
    let Ok(target) = std::fs::canonicalize(todo) else {
        return false;
    };
    import_paths(doc)
        .iter()
        .filter_map(|p| std::fs::canonicalize(dir.join(p)).ok())
        .any(|resolved| resolved == target)
}

/// True when a markdown link in `doc` resolves (file-relatively, like the
/// import check) to the root `TODO.md`. This is the *lazy* reference the
/// rule now prefers (ADR-036): the link is inert until something opens
/// the file, so TODO.md's tokens stay out of the session prefix.
///
/// Link hrefs are read from the parsed AST, so only real markdown links
/// count — a bare `TODO.md` in prose does not, mirroring the line-anchored
/// strictness `import_paths` applies to the eager form. A `#fragment` or
/// `?query` suffix is stripped before resolving.
fn references_root_todo_link(doc: &Document, root: &Path, todo: &Path) -> bool {
    let dir = root.join(&doc.location);
    let dir = dir.parent().unwrap_or(root);
    let Ok(target) = std::fs::canonicalize(todo) else {
        return false;
    };
    let Some(ast) = doc.ast.as_ref() else {
        return false;
    };
    ast.links
        .iter()
        .map(|link| link.href.split(['#', '?']).next().unwrap_or(&link.href))
        .filter(|href| !href.is_empty())
        .filter_map(|href| std::fs::canonicalize(dir.join(href)).ok())
        .any(|resolved| resolved == target)
}

/// The lazy markdown link a given instruction file should carry to the
/// root `TODO.md`, with `../` segments for its directory depth below the
/// root. A root-level `CLAUDE.md` yields `[TODO.md](TODO.md)`; a nested
/// `cli/CLAUDE.md` yields `[TODO.md](../TODO.md)`.
fn suggested_todo_link(doc: &Document) -> String {
    let depth = Path::new(&doc.location)
        .parent()
        .map(|p| p.components().count())
        .unwrap_or(0);
    let prefix = "../".repeat(depth);
    format!("[TODO.md]({prefix}TODO.md)")
}

/// Import paths declared on their own line — the first non-whitespace
/// token is `@<path>`. Line-anchored on purpose: a bare `@TODO.md`
/// mentioned mid-prose (e.g. documentation *about* this rule) is not an
/// import and must not satisfy `agents.context-headings` (finding #1).
/// Lines inside fenced/indented code blocks are excluded (BUG-006).
///
/// Token grammar note (BUG-004): this is deliberately a *subset* of the
/// grammar `dangling_import_diags` scans. Claude Code resolves any
/// word-starting `@path` token outside code, so the budget rule warns on
/// dangling *inline* imports too; this rule enforces the canonical
/// own-line form as a convention (visible, greppable, diff-friendly).
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
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for (idx, line_text) in doc.body.lines().enumerate() {
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        if in_code_block(doc, line_no) {
            continue;
        }
        for caps in import_regex().captures_iter(line_text) {
            let m = caps.get(1).expect("import_regex has one capture group");
            let col = u32::try_from(m.start() + 1).unwrap_or(u32::MAX);
            if in_inline_code(doc, line_no, col) {
                continue;
            }
            // Strip trailing sentence punctuation that the token grabbed
            // (`@x.md.` / `@x.md,` / `(@x.md)`), so it neither corrupts the
            // path nor makes a dotless word like `@internal.` look file-like.
            let path = m.as_str().trim_end_matches(|c: char| {
                matches!(
                    c,
                    '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'' | '>'
                )
            });
            if path.starts_with('~') || path.starts_with('/') || !path.contains('.') {
                continue;
            }
            if !seen.insert(path.to_string()) {
                continue;
            }
            if !dir.join(path).exists() {
                out.push(
                    Diagnostic::warning(
                        BUDGET,
                        doc.location.clone(),
                        0,
                        0,
                        format!("`@{path}` import points to a file that does not exist"),
                    )
                    .with_help(format!(
                        "create `{path}` or remove the `@{path}` import — a dangling import drops that context"
                    )),
                );
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
    let h2s: Vec<&crate::ast::Heading> = ast.headings.iter().filter(|h| h.level == 2).collect();
    let Some(idx) = h2s
        .iter()
        .position(|h| normalize_heading(&h.text) == "files allowed")
    else {
        return Vec::new();
    };

    let lines: Vec<&str> = doc.body.lines().collect();
    // Headings carry 1-indexed line numbers; content starts at index
    // `heading.line` (the heading itself sits at `heading.line - 1`).
    let start = h2s[idx].line as usize;
    let end = h2s
        .get(idx + 1)
        .map(|next| (next.line as usize).saturating_sub(1))
        .unwrap_or(lines.len());

    let mut out = Vec::new();
    let limit = end.min(lines.len());
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
    let h2s: Vec<&crate::ast::Heading> = ast.headings.iter().filter(|h| h.level == 2).collect();
    let Some(idx) = h2s
        .iter()
        .position(|h| normalize_heading(&h.text) == "requirements")
    else {
        return Vec::new();
    };

    let lines: Vec<&str> = doc.body.lines().collect();
    // Same slicing as `check_task_files_allowed` (ADR-022 § ABP-005):
    // headings carry 1-indexed lines; content starts at `heading.line`.
    let start = h2s[idx].line as usize;
    let end = h2s
        .get(idx + 1)
        .map(|next| (next.line as usize).saturating_sub(1))
        .unwrap_or(lines.len());

    let mut out = Vec::new();
    let limit = end.min(lines.len());
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

// -- SPEC namespace (spec.requires-prd, ADR-023) ----------------------

/// `spec.requires-prd` (PKC-007): a SPEC document's `depends_on` MUST
/// contain at least one `PRD-<n>` entry. Document-level rule (dispatched
/// per-document like `tasks.files-allowed`, not file-level).
pub(crate) fn check_spec_requires_prd(
    doc: &Document,
    _params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    let has_prd = doc
        .depends_on
        .iter()
        .any(|dep| dep.split_once('-').is_some_and(|(ns, _)| ns == "PRD"));
    if has_prd {
        return Vec::new();
    }
    let line = doc
        .frontmatter_lines
        .get("depends_on")
        .copied()
        .unwrap_or(0);
    vec![
        Diagnostic::error(
            SPEC_PRD,
            doc.location.clone(),
            line,
            0,
            "SPEC must depend on a PRD; add a PRD-<n> to depends_on",
        )
        .with_help(
            "add `- PRD-<n>` to the `depends_on:` frontmatter list — a SPEC without a PRD link is incomplete",
        ),
    ]
}

// -- pipeline.conformance (document-level) ---------------------------

/// `pipeline.conformance` (SPEC-002 EARS-06.*): flag a dependency edge
/// that skips one or more declared pipeline stages. Auto-active when a
/// `[pipeline]` table is declared, for documents in staged namespaces;
/// the declared `stages` order arrives via `params["stages"]` (the rule
/// is edge-level, so it needs config the per-document `CheckFn` channel
/// would not otherwise carry).
///
/// Edge direction follows the lift convention: a document `A` with
/// `depends_on: [B]` is the downstream end of the edge `ns(B) → ns(A)`.
/// A forward declared distance > 1 means one or more stages between
/// `ns(B)` and `ns(A)` were skipped (EARS-06.2). Edges whose endpoints
/// are not both staged are exempt (EARS-06.3).
pub(crate) fn check_pipeline_conformance(
    doc: &Document,
    params: Option<&Value>,
    _root: &Path,
) -> Vec<Diagnostic> {
    // The declared stage order arrives via `params["stages"]`; without
    // it there is nothing to measure distance against.
    let Some(stages) = params
        .and_then(|p| p.get("stages"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    let stage_pos = |ns: &str| -> Option<usize> {
        stages.iter().position(|s| s.as_str() == Some(ns))
    };

    // The downstream end of every edge is this document. An edge whose
    // downstream namespace is unstaged is exempt (EARS-06.3).
    let Some(downstream) = stage_pos(&doc.id.namespace) else {
        return Vec::new();
    };

    let line = doc
        .frontmatter_lines
        .get("depends_on")
        .copied()
        .unwrap_or(0);
    let mut out = Vec::new();
    for entry in &doc.depends_on {
        // The upstream namespace is the prefix of the depends_on id.
        let Some((upstream_ns, _)) = entry.split_once('-') else {
            continue;
        };
        // EARS-06.3: an edge touching an unstaged namespace is exempt.
        let Some(upstream) = stage_pos(upstream_ns) else {
            continue;
        };
        // A forward distance of 0 (same stage) or 1 (adjacent) skips
        // nothing. Only distance > 1 names skipped stages (EARS-06.2);
        // backward edges are a cycle concern, handled elsewhere.
        if downstream <= upstream + 1 {
            continue;
        }
        let skipped: Vec<&str> = stages[upstream + 1..downstream]
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        out.push(
            Diagnostic::error(
                PIPELINE_CONFORMANCE,
                doc.location.clone(),
                line,
                0,
                format!(
                    "{} depends directly on {entry}, skipping pipeline stage(s) {}",
                    doc.raw_id,
                    skipped.join(", ")
                ),
            )
            .with_help(
                "route the dependency through the intermediate stage(s), or drop the \
                 skipped namespaces from [pipeline].stages",
            ),
        );
    }
    out
}

// -- todo.listed (document-level) ------------------------------------

/// Default statuses that exempt a document from the `todo.listed` check.
/// Compared case-insensitively against the document's `status` field.
const DEFAULT_TERMINAL_STATUSES: &[&str] = &[
    "accepted",
    "superseded",
    "done",
    "fixed",
    "wontfix",
    "invalid",
    "duplicate",
    "closed",
    "implemented",
    "n/a",
];

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

/// `soul.sections` (SOUL-002): the three high-signal sections the SOUL.md
/// spec says to fill first — Worldview, Opinions, Boundaries — must be
/// present. One **warning** per missing section. The remaining spec
/// sections (Who I Am, Interests, Current Focus, Influences, Vocabulary,
/// Tensions & Contradictions, Pet Peeves) are optional and unrecognized
/// `##` headings pass silently — the spec instructs authors to delete
/// sections that do not apply, so v1 checks presence only (order and
/// empty-body checks are deferred, SOUL-003).
///
/// Severity is **warning**, not error: the persona pack is uniformly
/// advisory (recommended shape, not spec enforcement), `SOUL.md` is a young
/// community convention, and the sibling `style.*` rules are warnings too.
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
        let present = ast
            .headings
            .iter()
            .any(|h| h.level == 2 && h.text.trim().eq_ignore_ascii_case(name));
        if !present {
            out.push(
                Diagnostic::warning(
                    SOUL_SECTIONS,
                    doc.location.clone(),
                    0,
                    0,
                    format!("SOUL.md is missing the high-signal section '{name}'"),
                )
                .with_help(
                    "the spec says fill Worldview, Opinions, and Boundaries first — they \
                     carry the most signal; add the section, or drop soul.sections if this \
                     persona deliberately omits it",
                ),
            );
        }
    }
    out
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
    fn dangling_import_ignores_tokens_in_inline_code() {
        // BUG-006: a backticked token is documentation about the syntax;
        // Claude Code does not resolve inline-code spans.
        let tmp = tempfile::tempdir().expect("tempdir");
        let body = "Write `import @docs/missing.md` style imports.\n";
        let d = synthetic_document("AGENTS", "CLAUDE.md".to_string(), body.to_string());
        let diags = dangling_import_diags(&d, tmp.path());
        assert!(diags.is_empty(), "inline code must not warn: {diags:?}");
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
        assert_eq!(diags[0].line, 4);
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
        assert_eq!(diags[0].line, 4);
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
        assert_eq!(diags[0].line, 2, "diagnostic anchored at Overview heading");
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

    // -- design.token-ref (ADR-027 § DES-003) ----------------------------

    fn design_token_doc(metadata: BTreeMap<String, serde_json::Value>) -> Document {
        Document {
            id: "ADR-1".parse().unwrap(),
            raw_id: String::new(),
            location: "DESIGN.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata,
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
        assert_eq!(diags[0].line, 2, "anchored at the out-of-order heading");
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

    fn soul_section_doc(headings: &[&str]) -> Document {
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
            id: "SOUL-0".parse().unwrap(),
            raw_id: String::new(),
            location: "SOUL.md".to_owned(),
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            ast: Some(Ast {
                headings: ast_headings,
                ..Ast::default()
            }),
            body: String::new(),
        }
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

    // -- pipeline.conformance (EARS-06.2/06.3) --------------------------

    fn pipeline_doc(raw_id: &str, depends_on: Vec<&str>) -> Document {
        Document {
            id: raw_id.parse().expect("valid id"),
            raw_id: raw_id.to_string(),
            location: format!("{}.md", raw_id.to_lowercase()),
            depends_on: depends_on.into_iter().map(String::from).collect(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            ast: Some(Ast::default()),
            body: String::new(),
        }
    }

    fn stages_params(stages: &[&str]) -> Value {
        serde_json::json!({ "stages": stages })
    }

    #[test]
    fn conformance_flags_edge_that_skips_stages() {
        // EARS-06.2: TASK depending directly on PRD under PRD → ADR →
        // SPEC → TASK skips ADR and SPEC — both must be named.
        let d = pipeline_doc("TASK-001", vec!["PRD-001"]);
        let params = stages_params(&["PRD", "ADR", "SPEC", "TASK"]);
        let diags = check_pipeline_conformance(&d, Some(&params), Path::new("."));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "pipeline.conformance");
        assert!(diags[0].message.contains("TASK-001"), "{}", diags[0].message);
        assert!(diags[0].message.contains("PRD-001"), "{}", diags[0].message);
        assert!(diags[0].message.contains("ADR"), "{}", diags[0].message);
        assert!(diags[0].message.contains("SPEC"), "{}", diags[0].message);
    }

    #[test]
    fn conformance_allows_adjacent_edge() {
        // Distance 1 (PRD → ADR) skips nothing.
        let d = pipeline_doc("ADR-001", vec!["PRD-001"]);
        let params = stages_params(&["PRD", "ADR", "SPEC", "TASK"]);
        assert!(check_pipeline_conformance(&d, Some(&params), Path::new(".")).is_empty());
    }

    #[test]
    fn conformance_exempts_edge_with_unstaged_upstream() {
        // EARS-06.3: BUG is not staged, so a TASK → BUG edge is exempt
        // even though it would otherwise span the whole ladder.
        let d = pipeline_doc("TASK-001", vec!["BUG-001"]);
        let params = stages_params(&["PRD", "ADR", "SPEC", "TASK"]);
        assert!(check_pipeline_conformance(&d, Some(&params), Path::new(".")).is_empty());
    }

    #[test]
    fn conformance_exempts_when_downstream_unstaged() {
        // EARS-06.3: the downstream namespace (BUG) is unstaged → exempt.
        let d = pipeline_doc("BUG-001", vec!["PRD-001"]);
        let params = stages_params(&["PRD", "ADR", "SPEC", "TASK"]);
        assert!(check_pipeline_conformance(&d, Some(&params), Path::new(".")).is_empty());
    }

    #[test]
    fn conformance_noop_without_stages_param() {
        // Defensive: no declared stages means nothing to measure.
        let d = pipeline_doc("TASK-001", vec!["PRD-001"]);
        assert!(check_pipeline_conformance(&d, None, Path::new(".")).is_empty());
    }
}
