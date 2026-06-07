//! Core rule evaluation. CORE-004 + CORE-005 (the graph-floor and
//! cross-ref parts) + conversion of parse-level diagnostics into the
//! `core.frontmatter` / `core.id` rule codes.
//!
//! Every function here is pure: it takes already-parsed state and
//! returns [`Diagnostic`]s. No I/O, no rule runners, no params. The
//! parameterised rules (`core.required-headings`,
//! `core.required-metadata`, `core.allowed-values`) land in a later
//! phase together with TOML config.
//!
//! **Parsing invariant.** None of these rules re-reads the body. They
//! consume `Document.depends_on`, `Document.ast.cross_ref_tokens`, and
//! so on. The brief's "Sources parse, rules check" rule is enforced
//! structurally: these functions never see the raw body string.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::dag::{self, Cycle};
use crate::diagnostic::Diagnostic;
use crate::document::{self, Document};
use crate::id::DocumentId;
use crate::source::markdown::{ParseDiagnostic, ParseDiagnosticKind};

const ID_REGEX_NOTE: &str = "id must match the pattern `<NAMESPACE>-<number>`, where namespace is uppercase ASCII starting with a letter (regex: `^[A-Z][A-Z0-9]*-\\d+$`).";

/// Convert a [`ParseDiagnostic`] emitted by the markdown source into
/// a rule-code-attached [`Diagnostic`].
///
/// Frontmatter errors become `core.frontmatter`; id issues become
/// `core.id`. Line / column are always 0 because the failure
/// prevented us from establishing a meaningful anchor. Each
/// diagnostic carries a cargo-style `help:` line and (where the
/// constraint benefits from it) a `note:` with the regex or
/// canonical example.
///
/// Per ADR-007 § DOC-003, these diagnostics fire only for files that
/// claim intent (id-claim or `[<NS>].paths` match). The historical
/// `[ignore].patterns` escape clause is intentionally absent from the
/// help text — under DOC-001 the diagnostic only fires for files that
/// ARE ctxgrd documents, so suggesting the user silence them via
/// `[ignore]` would be misleading. Path-claimed files should fix the
/// id; non-claimed files don't reach this path at all.
pub(crate) fn parse_diagnostic_to_diagnostic(p: &ParseDiagnostic) -> Diagnostic {
    match &p.kind {
        ParseDiagnosticKind::Frontmatter(msg) => Diagnostic::error(
            "core.frontmatter",
            p.location.clone(),
            0,
            0,
            format!("frontmatter could not be parsed: {msg}"),
        )
        .with_help(
            "add a `---`-fenced YAML block at the top of the file, e.g.\n\
             \n    ---\n    id: ADR-001\n    title: ...\n    ---",
        ),
        ParseDiagnosticKind::IdMissing => Diagnostic::error(
            "core.id",
            p.location.clone(),
            0,
            0,
            "id is missing or empty in frontmatter",
        )
        .with_help(
            "add an `id: <NAMESPACE>-<number>` field to the frontmatter, e.g. `id: ADR-001`.",
        )
        .with_note(ID_REGEX_NOTE),
        ParseDiagnosticKind::IdMalformed { raw_id } => Diagnostic::error(
            "core.id",
            p.location.clone(),
            0,
            0,
            format!("id {raw_id:?} does not match the required pattern"),
        )
        .with_help(format!(
            "wrap the id in a namespace prefix, e.g. `<NS>-{raw_id}`."
        ))
        .with_note(ID_REGEX_NOTE),
    }
}

/// `core.id-unique` — duplicate `(namespace, number)` pairs across
/// all ingested documents. Emits one diagnostic per colliding
/// location (not one per group) so every offending file is named
/// in the report. The reporter's sort brings them together
/// automatically.
pub(crate) fn id_unique(docs: &[Document]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for collision in document::find_id_collisions(docs) {
        let mates = collision.locations.to_vec();
        for location in &collision.locations {
            let others: Vec<&String> = mates.iter().filter(|l| *l != location).collect();
            let other_list = others
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            out.push(
                Diagnostic::error(
                    "core.id-unique",
                    location.clone(),
                    0,
                    0,
                    format!("id {} also appears in {}", collision.id, other_list),
                )
                .with_help(format!(
                    "rename this document or {other_list} to a different `<namespace>-<number>`"
                )),
            );
        }
    }
    out
}

/// `core.dep-resolved` — `depends_on` entries that don't match any
/// document in the run.
pub(crate) fn dep_resolved(docs: &[Document]) -> Vec<Diagnostic> {
    dag::unresolved_refs(docs)
        .into_iter()
        .map(|r| {
            let doc = &docs[r.from_doc_idx];
            let line = doc
                .frontmatter_lines
                .get("depends_on")
                .copied()
                .unwrap_or(0);
            // Namespace of the unresolved entry, for the help hint.
            let ns_hint = r
                .raw_entry
                .split_once('-')
                .map(|(ns, _)| ns.to_string())
                .unwrap_or_else(|| "<NS>".to_string());
            Diagnostic::error(
                "core.dep-resolved",
                doc.location.clone(),
                line,
                0,
                format!(
                    "depends_on entry '{}' does not resolve to a document in the run",
                    r.raw_entry
                ),
            )
            .with_help(format!(
                "create the missing document (`ctxgrd new {ns_hint} \"...\"`) or remove `{}` from `depends_on`",
                r.raw_entry
            ))
        })
        .collect()
}

/// `core.dep-cycle` — self-edges (one diagnostic per doc) and
/// non-trivial SCCs (one diagnostic per SCC, naming all members).
pub(crate) fn dep_cycle(docs: &[Document]) -> Vec<Diagnostic> {
    dag::cycles(docs)
        .into_iter()
        .map(|cycle| match cycle {
            Cycle::SelfEdge { doc_idx } => {
                let doc = &docs[doc_idx];
                Diagnostic::error(
                    "core.dep-cycle",
                    doc.location.clone(),
                    doc.frontmatter_lines
                        .get("depends_on")
                        .copied()
                        .unwrap_or(0),
                    0,
                    format!("depends_on cycle: {} references itself", doc.id),
                )
                .with_help(format!(
                    "remove `{}` from this document's `depends_on`",
                    doc.id
                ))
            }
            Cycle::Scc { members } => {
                // Report at the first member's location — same rule as
                // REP-001's sort order (location, line, col, code). All
                // members are listed in the message so the user can
                // see the full cycle.
                let first = &docs[members[0]];
                let names = members
                    .iter()
                    .map(|i| docs[*i].id.to_string())
                    .collect::<Vec<_>>()
                    .join(" -> ");
                Diagnostic::error(
                    "core.dep-cycle",
                    first.location.clone(),
                    first
                        .frontmatter_lines
                        .get("depends_on")
                        .copied()
                        .unwrap_or(0),
                    0,
                    format!("depends_on cycle: {names}"),
                )
                .with_help("break the cycle — remove one dependency edge")
            }
        })
        .collect()
}

/// `core.cross-ref` — tokens in the body that don't resolve to any
/// known document, unless suppressed via `in_code` or
/// `in_strikethrough`. Silently no-ops for documents with no AST
/// (CORE-005).
///
/// **Namespace filtering (ADR-001 § REF-005).** A token is only
/// checked when its namespace prefix is *known* to the run, defined
/// as the union of:
///
/// 1. namespaces explicitly declared via a `[<NS>]` table in
///    `ctxgrd.toml` (REF-005 strict — lets `ADR-007` be flagged even
///    in a freshly-bootstrapped project with zero ADR documents yet);
/// 2. namespaces of discovered documents (preserves zero-config
///    behaviour where the user has no `ctxgrd.toml` to declare in).
///
/// Tokens for namespaces in neither set are silently ignored as
/// internal markers (requirement IDs like `REF-001`, ticket numbers,
/// `HTTP-2`, `ISO-8601`, etc.) rather than references to missing
/// documents.
///
/// **Dedupe.** Each (document, unresolved-target) pair produces at
/// most one diagnostic, anchored at the first occurrence. Repeat
/// mentions of the same missing id in a single body don't flood the
/// report.
pub(crate) fn cross_ref(
    docs: &[Document],
    declared_namespaces: &BTreeSet<&str>,
    references: &[crate::reference::Reference],
) -> Vec<Diagnostic> {
    let known_ids: BTreeSet<DocumentId> = docs.iter().map(|d| d.id.clone()).collect();
    let mut allowed_namespaces: BTreeSet<&str> =
        docs.iter().map(|d| d.id.namespace.as_str()).collect();
    allowed_namespaces.extend(declared_namespaces.iter().copied());
    let mut out = Vec::new();
    for doc in docs {
        let Some(ast) = doc.ast.as_ref() else {
            continue;
        };
        let mut seen_in_this_doc: BTreeSet<DocumentId> = BTreeSet::new();
        for token in &ast.cross_ref_tokens {
            if token.in_code || token.in_strikethrough {
                continue;
            }
            if !allowed_namespaces.contains(token.namespace.as_str()) {
                continue;
            }
            let id = DocumentId::new(token.namespace.clone(), token.number);
            if known_ids.contains(&id) {
                continue;
            }
            if !seen_in_this_doc.insert(id) {
                continue;
            }
            out.push(
                Diagnostic::error(
                    "core.cross-ref",
                    doc.location.clone(),
                    token.line,
                    token.col,
                    format!(
                        "cross-reference '{}' does not resolve to a known document",
                        token.token
                    ),
                )
                .with_help(format!(
                    "use an existing ID, wrap in backticks `` `{token}` `` for a literal example,\n\
                     or mark retired with `~~{token}~~`",
                    token = token.token
                ))
                .with_span_len(token.token.len() as u32),
            );
        }
    }
    // ADR-001 § REF-001: scanner-emitted References are anonymous
    // pointer mentions in non-markdown files. Same dangling-token
    // semantics, separate dedup scope (each (file, line, col, token)
    // is unique by construction in the scanner's output already).
    for r in references {
        if !allowed_namespaces.contains(token_namespace(&r.token)) {
            continue;
        }
        let Some((ns, num)) = split_token(&r.token) else {
            continue;
        };
        let id = DocumentId::new(ns.to_string(), num);
        if known_ids.contains(&id) {
            continue;
        }
        out.push(
            Diagnostic::error(
                "core.cross-ref",
                r.file_path.to_string_lossy().into_owned(),
                r.line,
                r.col,
                format!(
                    "cross-reference '{}' does not resolve to a known document",
                    r.token
                ),
            )
            .with_help(
                "use an existing ID, or add `ctxgrd: ignore-line` / `ctxgrd: ignore-next`\n\
                 to suppress this match without disabling the rule"
                    .to_string(),
            )
            .with_span_len(r.token.len() as u32),
        );
    }
    out
}

/// Extract the namespace prefix from a `<NAMESPACE>-<number>` token.
/// Returns `""` if the token is malformed (caller will skip it via
/// the namespace filter).
fn token_namespace(token: &str) -> &str {
    token.rsplit_once('-').map(|(ns, _)| ns).unwrap_or("")
}

fn split_token(token: &str) -> Option<(&str, u32)> {
    let (ns, num_str) = token.rsplit_once('-')?;
    let num: u32 = num_str.parse().ok()?;
    Some((ns, num))
}

fn requirement_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Z]{2,}-\d{3,}").expect("valid regex"))
}

/// `core.requirement-ref` — opt-in rule that resolves requirement references
/// to requirement definitions across the linted corpus.
///
/// **Pass 1.** Collect every heading whose text *starts with* a token
/// matching `[A-Z]{2,}-\d{3,}` (e.g. `### FR-007 Session timeout`).
/// Build a set of `(prefix, number)` definitions and a derived set of
/// known prefixes.
///
/// **Pass 2.** For each doc, scan body lines for `- **Satisfies:**` /
/// `- **Addressed by:**` list items. Extract all requirement-ID tokens,
/// skipping those inside backtick spans. For each token: if its prefix
/// is not in `known_prefixes`, ignore (foreign code such as `RFC-7231`).
/// Otherwise, if `(prefix, number)` is not in `defined`, emit a warning.
/// Deduplication is per `(document, unresolved-target)`, anchored at
/// first occurrence.
///
/// **Severity: warning** (v1). Eases adoption on large existing corpora
/// that opt in incrementally. Flipping to error is a MAJOR bump per
/// the versioning contract.
///
/// **Opt-in.** Called corpus-wide then filtered by `aggregate.retain`
/// in `run.rs` — identical to `cross_ref`. No separate activation
/// plumbing needed.
pub(crate) fn requirement_ref(docs: &[Document]) -> Vec<Diagnostic> {
    // Pass 1: collect definitions and derived known prefixes.
    let mut defined: BTreeSet<(String, u32)> = BTreeSet::new();
    let mut known_prefixes: BTreeSet<String> = BTreeSet::new();
    let re = requirement_id_regex();
    for doc in docs {
        let Some(ast) = doc.ast.as_ref() else {
            continue;
        };
        for heading in &ast.headings {
            let Some(m) = re.find(&heading.text) else {
                continue;
            };
            if m.start() != 0 {
                continue;
            }
            let token = m.as_str();
            let Some((prefix, num_str)) = token.rsplit_once('-') else {
                continue;
            };
            let Ok(num) = num_str.parse::<u32>() else {
                continue;
            };
            defined.insert((prefix.to_string(), num));
            known_prefixes.insert(prefix.to_string());
        }
    }
    if known_prefixes.is_empty() {
        return Vec::new();
    }
    // Pass 2: check references from req_ref_tokens (populated by the parser
    // from Satisfies/Addressed-by list items).
    let mut out = Vec::new();
    for doc in docs {
        let Some(ast) = doc.ast.as_ref() else {
            continue;
        };
        let mut seen: BTreeSet<(String, u32)> = BTreeSet::new();
        for token in &ast.req_ref_tokens {
            if token.in_code || token.in_strikethrough {
                continue;
            }
            if !known_prefixes.contains(&token.namespace) {
                continue;
            }
            let key = (token.namespace.clone(), token.number);
            if defined.contains(&key) {
                continue;
            }
            if !seen.insert(key) {
                continue;
            }
            out.push(
                Diagnostic::warning(
                    "core.requirement-ref",
                    doc.location.clone(),
                    token.line,
                    token.col,
                    format!(
                        "requirement reference '{}' does not resolve to a defined requirement",
                        token.token
                    ),
                )
                .with_help(
                    "typo or stale link; define the requirement as a heading, or fix the reference",
                )
                .with_span_len(token.token.len() as u32),
            );
        }
    }
    out
}

// -- parameterised rules ----------------------------------------------

/// `core.required-headings` — every configured H2 heading string MUST
/// appear somewhere in the document's headings list. The match is
/// exact and case-sensitive. Missing headings produce a
/// line-0 / col-0 diagnostic (there's no anchor for something that
/// isn't there). Only H2 (`level == 2`) counts, per the rule's name.
///
/// No-op when the document has no AST (CORE-005).
pub(crate) fn required_headings(doc: &Document, params: &Value) -> Vec<Diagnostic> {
    let Some(ast) = doc.ast.as_ref() else {
        return Vec::new();
    };
    let Some(required) = params.get("headings").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let present: BTreeSet<&str> = ast
        .headings
        .iter()
        .filter(|h| h.level == 2)
        .map(|h| h.text.as_str())
        .collect();

    let required_list: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    let full_list_note = format!("required H2 headings: {}", required_list.join(", "));

    let mut out = Vec::new();
    for text in &required_list {
        if !present.contains(text) {
            out.push(
                Diagnostic::error(
                    "core.required-headings",
                    doc.location.clone(),
                    0,
                    0,
                    format!("missing required H2 heading '{text}'"),
                )
                .with_help(format!("add a `## {text}` section to the document body"))
                .with_note(full_list_note.clone()),
            );
        }
    }
    out
}

/// `core.required-metadata` — every configured key MUST be present in
/// the unified metadata map (`source.extra ⊕ body.frontmatter`) AND
/// non-empty. Empty means: null, empty string, empty array, or empty
/// object. A plain `false` or `0` is NOT empty — the rule checks
/// *presence of a value*, not truthiness.
pub(crate) fn required_metadata(doc: &Document, params: &Value) -> Vec<Diagnostic> {
    let Some(keys) = params.get("keys").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let required_list: Vec<&str> = keys.iter().filter_map(|v| v.as_str()).collect();
    let full_list_note = format!("required metadata keys: {}", required_list.join(", "));

    let mut out = Vec::new();
    for key in &required_list {
        // `id` is special: it's peeled off by the frontmatter parser
        // and doesn't live in `metadata`. Every document that made it
        // into `docs` already has a valid `id`, so this check is
        // satisfied by construction.
        if *key == "id" {
            continue;
        }

        let value = doc.metadata.get(*key);
        if is_empty_metadata(value) {
            let line = doc.frontmatter_lines.get(*key).copied().unwrap_or(0);
            out.push(
                Diagnostic::error(
                    "core.required-metadata",
                    doc.location.clone(),
                    line,
                    0,
                    format!("required metadata key '{key}' is missing or empty"),
                )
                .with_help(format!("add a `{key}: <value>` entry to the frontmatter"))
                .with_note(full_list_note.clone()),
            );
        }
    }
    out
}

/// `core.allowed-values` — for each configured `<key> = [v1, v2, ...]`
/// entry, if the document's unified metadata has that key, its value
/// MUST be one of the listed strings. Missing keys are silently OK
/// (those are `core.required-metadata`'s job). Non-string values are
/// stringified for comparison.
pub(crate) fn allowed_values(doc: &Document, params: &Value) -> Vec<Diagnostic> {
    let Value::Object(table) = params else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for (key, allowed) in table {
        let Some(allowed_list) = allowed.as_array() else {
            continue;
        };
        let allowed_strings: Vec<&str> = allowed_list.iter().filter_map(|v| v.as_str()).collect();

        let Some(value) = doc.metadata.get(key) else {
            continue;
        };
        let actual = metadata_value_as_string(value);
        if !allowed_strings.iter().any(|s| *s == actual) {
            let line = doc.frontmatter_lines.get(key).copied().unwrap_or(0);
            out.push(
                Diagnostic::error(
                    "core.allowed-values",
                    doc.location.clone(),
                    line,
                    0,
                    format!(
                        "metadata key '{key}' has value '{actual}' not in allowed set [{}]",
                        allowed_strings.join(", ")
                    ),
                )
                .with_help(format!(
                    "change `{key}` to one of: {}",
                    allowed_strings.join(", ")
                )),
            );
        }
    }
    out
}

fn is_empty_metadata(value: Option<&Value>) -> bool {
    match value {
        None => true,
        Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
        _ => false,
    }
}

/// Stringify a metadata value for `core.allowed-values` comparison.
///
/// Strings pass through verbatim. Booleans and numbers serialise the
/// obvious way. Composite values (arrays, objects) are rendered as
/// their JSON form — unusual for an allowed-values check but
/// deterministic and diagnosable.
fn metadata_value_as_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Ast, CrossRefToken, Heading};

    fn make_doc(raw_id: &str, depends_on: Vec<&str>, tokens: Vec<CrossRefToken>) -> Document {
        Document {
            id: raw_id.parse().expect("valid id"),
            raw_id: raw_id.to_owned(),
            location: format!("{raw_id}.md"),
            depends_on: depends_on.into_iter().map(String::from).collect(),
            frontmatter_lines: [("depends_on".to_string(), 5u32)].into(),
            metadata: Default::default(),
            ast: Some(Ast {
                cross_ref_tokens: tokens,
                ..Ast::default()
            }),
            body: String::new(),
        }
    }

    fn token(id: &str, line: u32, col: u32, in_code: bool, in_strike: bool) -> CrossRefToken {
        let dash = id.find('-').unwrap();
        CrossRefToken {
            token: id.to_owned(),
            namespace: id[..dash].to_owned(),
            number: id[dash + 1..].parse().unwrap(),
            line,
            col,
            in_code,
            in_strikethrough: in_strike,
        }
    }

    #[test]
    fn dep_resolved_matches_golden_message() {
        let docs = vec![
            make_doc("ADR-099", vec!["PRD-999"], vec![]),
            make_doc("ADR-001", vec![], vec![]),
        ];
        let diags = dep_resolved(&docs);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.dep-resolved");
        assert_eq!(diags[0].location, "ADR-099.md");
        assert_eq!(diags[0].line, 5);
        assert_eq!(
            diags[0].message,
            "depends_on entry 'PRD-999' does not resolve to a document in the run"
        );
    }

    #[test]
    fn dep_resolved_skips_present_documents() {
        let docs = vec![
            make_doc("ADR-001", vec!["PRD-001"], vec![]),
            make_doc("PRD-001", vec![], vec![]),
        ];
        assert!(dep_resolved(&docs).is_empty());
    }

    #[test]
    fn cross_ref_matches_golden_message() {
        let docs = vec![make_doc(
            "ADR-099",
            vec![],
            vec![token("ADR-042", 18, 5, false, false)],
        )];
        let diags = cross_ref(&docs, &BTreeSet::new(), &[]);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.cross-ref");
        assert_eq!(diags[0].location, "ADR-099.md");
        assert_eq!(diags[0].line, 18);
        assert_eq!(diags[0].col, 5);
        assert_eq!(
            diags[0].message,
            "cross-reference 'ADR-042' does not resolve to a known document"
        );
    }

    #[test]
    fn cross_ref_suppressed_by_in_strikethrough() {
        let docs = vec![make_doc(
            "ADR-099",
            vec![],
            vec![token("ADR-404", 18, 5, false, true)],
        )];
        assert!(cross_ref(&docs, &BTreeSet::new(), &[]).is_empty());
    }

    #[test]
    fn cross_ref_suppressed_by_in_code() {
        let docs = vec![make_doc(
            "ADR-099",
            vec![],
            vec![token("ADR-404", 18, 5, true, false)],
        )];
        assert!(cross_ref(&docs, &BTreeSet::new(), &[]).is_empty());
    }

    #[test]
    fn cross_ref_resolves_to_self_without_firing() {
        let docs = vec![make_doc(
            "ADR-001",
            vec![],
            vec![token("ADR-001", 9, 3, false, false)],
        )];
        assert!(cross_ref(&docs, &BTreeSet::new(), &[]).is_empty());
    }

    #[test]
    fn cross_ref_noops_when_ast_missing() {
        let mut doc = make_doc("ADR-001", vec![], vec![]);
        doc.ast = None;
        assert!(cross_ref(&[doc], &BTreeSet::new(), &[]).is_empty());
    }

    #[test]
    fn cross_ref_skips_unknown_namespace() {
        // FR has no documents in the run AND is not declared, so
        // FR-001 is not a cross-ref — it's a local requirement marker.
        let docs = vec![make_doc(
            "PRD-001",
            vec![],
            vec![token("FR-001", 24, 5, false, false)],
        )];
        assert!(cross_ref(&docs, &BTreeSet::new(), &[]).is_empty());
    }

    /// REF-005 verification: only tokens whose namespace is declared
    /// in `ctxgrd.toml` (or has discovered documents) are checked.
    /// Tokens from undeclared namespaces — `HTTP-2`, `ISO-8601`,
    /// `JIRA-100` etc. — are silently ignored.
    #[test]
    fn cross_ref_ref005_namespace_filter_skips_undeclared() {
        let docs = vec![make_doc(
            "PRD-001",
            vec![],
            vec![
                token("HTTP-2", 1, 1, false, false),
                token("ISO-8601", 2, 1, false, false),
                token("JIRA-100", 3, 1, false, false),
                token("ADR-042", 4, 1, false, false),
            ],
        )];
        let declared: BTreeSet<&str> = ["ADR", "PRD"].into_iter().collect();
        let diags = cross_ref(&docs, &declared, &[]);
        assert_eq!(diags.len(), 1, "only ADR-042 should fire");
        assert_eq!(
            diags[0].message,
            "cross-reference 'ADR-042' does not resolve to a known document"
        );
    }

    /// REF-005 (declared-but-no-docs case): a namespace declared in
    /// config but with zero discovered docs still has its tokens
    /// checked. Lets users bootstrapping a namespace catch dangling
    /// refs from day one.
    #[test]
    fn cross_ref_ref005_declared_namespace_without_docs_still_checks() {
        let docs = vec![make_doc(
            "PRD-001",
            vec![],
            vec![token("ADR-001", 9, 3, false, false)],
        )];
        let declared: BTreeSet<&str> = ["ADR", "PRD"].into_iter().collect();
        let diags = cross_ref(&docs, &declared, &[]);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.cross-ref");
    }

    /// Zero-config compatibility: with no declared namespaces, the
    /// "namespace has docs in run" filter still applies, so users
    /// running without `ctxgrd.toml` continue to get cross-ref
    /// diagnostics for their discovered namespaces.
    #[test]
    fn cross_ref_zero_config_falls_back_to_discovered() {
        let docs = vec![make_doc(
            "ADR-099",
            vec![],
            vec![token("ADR-042", 18, 5, false, false)],
        )];
        let diags = cross_ref(&docs, &BTreeSet::new(), &[]);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.cross-ref");
    }

    #[test]
    fn cross_ref_dedupes_repeated_mentions_in_same_doc() {
        // Two ADR-042 mentions on the same line → one diagnostic, at
        // the first occurrence.
        let docs = vec![make_doc(
            "ADR-099",
            vec![],
            vec![
                token("ADR-042", 18, 5, false, false),
                token("ADR-042", 18, 39, false, false),
            ],
        )];
        let diags = cross_ref(&docs, &BTreeSet::new(), &[]);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].col, 5, "should anchor at first occurrence");
    }

    #[test]
    fn cross_ref_does_not_dedupe_across_documents() {
        // Two separate docs each mention ADR-042 → two diagnostics.
        let docs = vec![
            make_doc(
                "ADR-100",
                vec![],
                vec![token("ADR-042", 5, 1, false, false)],
            ),
            make_doc(
                "ADR-101",
                vec![],
                vec![token("ADR-042", 5, 1, false, false)],
            ),
        ];
        let diags = cross_ref(&docs, &BTreeSet::new(), &[]);
        assert_eq!(diags.len(), 2);
    }

    #[test]
    fn id_unique_emits_one_per_location() {
        let docs = vec![
            make_doc("ADR-001", vec![], vec![]),
            make_doc("ADR-001", vec![], vec![]),
        ];
        // Force distinct locations so the collision surfaces.
        let mut docs = docs;
        docs[0].location = "a.md".to_owned();
        docs[1].location = "b.md".to_owned();
        let diags = id_unique(&docs);
        assert_eq!(diags.len(), 2);
        assert!(diags.iter().all(|d| d.code == "core.id-unique"));
        let locs: Vec<&str> = diags.iter().map(|d| d.location.as_str()).collect();
        assert!(locs.contains(&"a.md"));
        assert!(locs.contains(&"b.md"));
    }

    #[test]
    fn dep_cycle_self_edge_reported() {
        let docs = vec![make_doc("ADR-001", vec!["ADR-001"], vec![])];
        let diags = dep_cycle(&docs);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.dep-cycle");
        assert!(diags[0].message.contains("ADR-1 references itself"));
    }

    #[test]
    fn parse_diagnostic_to_core_frontmatter() {
        let p = ParseDiagnostic {
            location: "x.md".to_owned(),
            kind: ParseDiagnosticKind::Frontmatter("no fence".to_owned()),
        };
        let d = parse_diagnostic_to_diagnostic(&p);
        assert_eq!(d.code, "core.frontmatter");
        assert_eq!(d.location, "x.md");
        assert_eq!(d.line, 0);
    }

    #[test]
    fn parse_diagnostic_to_core_id_missing() {
        let p = ParseDiagnostic {
            location: "x.md".to_owned(),
            kind: ParseDiagnosticKind::IdMissing,
        };
        let d = parse_diagnostic_to_diagnostic(&p);
        assert_eq!(d.code, "core.id");
        assert!(d.message.contains("missing"));
    }

    #[test]
    fn parse_diagnostic_to_core_id_malformed() {
        let p = ParseDiagnostic {
            location: "x.md".to_owned(),
            kind: ParseDiagnosticKind::IdMalformed {
                raw_id: "not-an-id".to_owned(),
            },
        };
        let d = parse_diagnostic_to_diagnostic(&p);
        assert_eq!(d.code, "core.id");
        assert!(d.message.contains("not-an-id"));
    }

    // -- parameterised rules -------------------------------------------

    fn doc_with_headings(id: &str, headings: Vec<(u8, &str)>) -> Document {
        let mut d = make_doc(id, vec![], vec![]);
        let ast = d.ast.as_mut().unwrap();
        ast.headings = headings
            .into_iter()
            .map(|(level, text)| Heading {
                level,
                text: text.to_owned(),
                line: 0,
                col: 0,
            })
            .collect();
        d
    }

    fn doc_with_metadata(id: &str, pairs: &[(&str, serde_json::Value)]) -> Document {
        let mut d = make_doc(id, vec![], vec![]);
        for (k, v) in pairs {
            d.metadata.insert((*k).to_string(), v.clone());
            d.frontmatter_lines.insert((*k).to_string(), 4);
        }
        d
    }

    #[test]
    fn required_headings_reports_missing_entries() {
        let doc = doc_with_headings(
            "ADR-099",
            vec![(2, "Status"), (2, "Context"), (2, "Consequences")],
        );
        let params = serde_json::json!({
            "headings": ["Status", "Context", "Decision", "Consequences"]
        });
        let diags = required_headings(&doc, &params);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.required-headings");
        assert_eq!(diags[0].line, 0);
        assert_eq!(diags[0].message, "missing required H2 heading 'Decision'");
    }

    #[test]
    fn required_headings_ignores_non_h2() {
        // An H1 or H3 with matching text does NOT satisfy an H2
        // requirement.
        let doc = doc_with_headings("ADR-001", vec![(1, "Status"), (3, "Decision")]);
        let params = serde_json::json!({"headings": ["Status", "Decision"]});
        let diags = required_headings(&doc, &params);
        assert_eq!(diags.len(), 2);
    }

    #[test]
    fn required_headings_case_sensitive() {
        let doc = doc_with_headings("ADR-001", vec![(2, "status")]);
        let params = serde_json::json!({"headings": ["Status"]});
        let diags = required_headings(&doc, &params);
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn required_headings_noops_when_ast_missing() {
        let mut doc = doc_with_headings("ADR-001", vec![]);
        doc.ast = None;
        let params = serde_json::json!({"headings": ["Status"]});
        assert!(required_headings(&doc, &params).is_empty());
    }

    #[test]
    fn required_metadata_reports_missing_key() {
        let doc = doc_with_metadata("ADR-001", &[("title", serde_json::json!("t"))]);
        let params = serde_json::json!({"keys": ["id", "title", "status"]});
        let diags = required_metadata(&doc, &params);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.required-metadata");
        assert!(diags[0].message.contains("'status'"));
    }

    #[test]
    fn required_metadata_treats_empty_string_as_missing() {
        let doc = doc_with_metadata("ADR-001", &[("status", serde_json::json!(""))]);
        let params = serde_json::json!({"keys": ["status"]});
        let diags = required_metadata(&doc, &params);
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn required_metadata_treats_null_as_missing() {
        let doc = doc_with_metadata("ADR-001", &[("status", serde_json::Value::Null)]);
        let params = serde_json::json!({"keys": ["status"]});
        assert_eq!(required_metadata(&doc, &params).len(), 1);
    }

    #[test]
    fn required_metadata_passes_when_present_and_non_empty() {
        let doc = doc_with_metadata("ADR-001", &[("status", serde_json::json!("accepted"))]);
        let params = serde_json::json!({"keys": ["status"]});
        assert!(required_metadata(&doc, &params).is_empty());
    }

    #[test]
    fn required_metadata_id_key_always_passes() {
        // `id` is peeled off by the frontmatter parser; a doc that
        // made it here has an id by construction.
        let doc = doc_with_metadata("ADR-001", &[]);
        let params = serde_json::json!({"keys": ["id"]});
        assert!(required_metadata(&doc, &params).is_empty());
    }

    #[test]
    fn allowed_values_matches_golden_message() {
        let doc = doc_with_metadata("ADR-099", &[("status", serde_json::json!("cooking"))]);
        let params = serde_json::json!({
            "status": ["draft", "accepted", "rejected", "superseded"]
        });
        let diags = allowed_values(&doc, &params);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "core.allowed-values");
        assert_eq!(diags[0].line, 4);
        assert_eq!(
            diags[0].message,
            "metadata key 'status' has value 'cooking' not in allowed set [draft, accepted, rejected, superseded]"
        );
    }

    #[test]
    fn allowed_values_allows_missing_key() {
        // Missing keys are required-metadata's problem, not allowed-values'.
        let doc = doc_with_metadata("ADR-001", &[]);
        let params = serde_json::json!({"status": ["draft", "accepted"]});
        assert!(allowed_values(&doc, &params).is_empty());
    }

    #[test]
    fn allowed_values_accepts_match() {
        let doc = doc_with_metadata("ADR-001", &[("status", serde_json::json!("accepted"))]);
        let params = serde_json::json!({"status": ["draft", "accepted"]});
        assert!(allowed_values(&doc, &params).is_empty());
    }

    #[test]
    fn allowed_values_handles_multiple_keys() {
        let doc = doc_with_metadata(
            "ADR-001",
            &[
                ("status", serde_json::json!("cooking")),
                ("kind", serde_json::json!("architecture")),
            ],
        );
        let params = serde_json::json!({
            "status": ["draft", "accepted"],
            "kind": ["architecture", "product"]
        });
        // status fails, kind passes.
        let diags = allowed_values(&doc, &params);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("'status'"));
    }

    // -- requirement_ref tests -----------------------------------------

    fn make_doc_with_headings(
        raw_id: &str,
        headings: Vec<(&str, u8)>,
        req_ref_tokens: Vec<CrossRefToken>,
    ) -> Document {
        Document {
            id: raw_id.parse().expect("valid id"),
            raw_id: raw_id.to_owned(),
            location: format!("{raw_id}.md"),
            depends_on: vec![],
            frontmatter_lines: Default::default(),
            metadata: Default::default(),
            ast: Some(Ast {
                headings: headings
                    .into_iter()
                    .enumerate()
                    .map(|(i, (text, level))| Heading {
                        level,
                        text: text.to_owned(),
                        line: i as u32 + 1,
                        col: 1,
                    })
                    .collect(),
                req_ref_tokens,
                ..Ast::default()
            }),
            body: String::new(),
        }
    }

    fn req_token(id: &str, line: u32, col: u32, in_code: bool) -> CrossRefToken {
        let dash = id.find('-').unwrap();
        CrossRefToken {
            token: id.to_owned(),
            namespace: id[..dash].to_owned(),
            number: id[dash + 1..].parse().unwrap(),
            line,
            col,
            in_code,
            in_strikethrough: false,
        }
    }

    #[test]
    fn requirement_ref_resolving_reference_passes() {
        // doc_a defines FR-007 as a heading; doc_b references it via Satisfies.
        let doc_a = make_doc_with_headings(
            "PRD-001",
            vec![("FR-007 Session timeout behaviour", 3)],
            vec![],
        );
        let doc_b =
            make_doc_with_headings("ADR-001", vec![], vec![req_token("FR-007", 2, 18, false)]);
        let diags = requirement_ref(&[doc_a, doc_b]);
        assert!(diags.is_empty(), "expected 0 diagnostics, got: {diags:?}");
    }

    #[test]
    fn requirement_ref_unresolved_reference_flagged() {
        let doc_a = make_doc_with_headings(
            "PRD-001",
            vec![("FR-007 Session timeout behaviour", 3)],
            vec![],
        );
        let doc_b =
            make_doc_with_headings("ADR-001", vec![], vec![req_token("FR-300", 2, 18, false)]);
        let diags = requirement_ref(&[doc_a, doc_b]);
        assert_eq!(diags.len(), 1, "expected exactly 1 diagnostic");
        assert_eq!(diags[0].code, "core.requirement-ref");
        assert_eq!(
            diags[0].message,
            "requirement reference 'FR-300' does not resolve to a defined requirement"
        );
    }

    #[test]
    fn requirement_ref_foreign_prefix_ignored() {
        // RFC-7231 has no heading definition anywhere — foreign prefix, must be silent.
        let doc =
            make_doc_with_headings("ADR-001", vec![], vec![req_token("RFC-7231", 2, 18, false)]);
        let diags = requirement_ref(&[doc]);
        assert!(
            diags.is_empty(),
            "foreign prefix RFC must produce 0 diagnostics"
        );
    }

    #[test]
    fn requirement_ref_in_code_suppressed() {
        let doc_a = make_doc_with_headings(
            "PRD-001",
            vec![("FR-007 Session timeout behaviour", 3)],
            vec![],
        );
        // FR-300 is a known-prefix (FR) but missing number; in_code = true must suppress.
        let doc_b =
            make_doc_with_headings("ADR-001", vec![], vec![req_token("FR-300", 2, 18, true)]);
        let diags = requirement_ref(&[doc_a, doc_b]);
        assert!(diags.is_empty(), "in_code token must produce 0 diagnostics");
    }

    #[test]
    fn requirement_ref_dedupe_same_target_in_one_doc() {
        let doc_a = make_doc_with_headings(
            "PRD-001",
            vec![("FR-007 Session timeout behaviour", 3)],
            vec![],
        );
        // Two tokens for FR-300 (from Satisfies and Addressed-by lines).
        let doc_b = make_doc_with_headings(
            "ADR-001",
            vec![],
            vec![
                req_token("FR-300", 2, 18, false),
                req_token("FR-300", 3, 22, false),
            ],
        );
        let diags = requirement_ref(&[doc_a, doc_b]);
        assert_eq!(
            diags.len(),
            1,
            "same unresolved target in one doc must dedupe to 1 diagnostic"
        );
        assert_eq!(diags[0].code, "core.requirement-ref");
    }

    #[test]
    fn requirement_ref_noops_when_no_definitions_in_corpus() {
        // No headings define a requirement — known_prefixes is empty, early return.
        let doc =
            make_doc_with_headings("ADR-001", vec![], vec![req_token("SEC-001", 2, 18, false)]);
        assert!(requirement_ref(&[doc]).is_empty());
    }
}
