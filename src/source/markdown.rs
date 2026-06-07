//! Built-in `markdown-file` source. CORE-001 in the brief.
//!
//! Walks a root directory, picks every `.md` file in byte-sorted path
//! order, and turns each into a [`Document`] whose `ast` is populated
//! via `pulldown-cmark`. Files whose frontmatter won't parse or whose
//! `id` is missing / malformed are NOT dropped silently — the scan
//! returns parse-level diagnostics (`core.frontmatter`, `core.id`) for
//! the rule layer to include in the report.
//!
//! Symlinks are NOT followed. Hidden files (dot-prefixed) are walked
//! like any other — the brief's dot-skip rule applies to *source*
//! subdirectories (SRC-001), not markdown content.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pulldown_cmark::{CodeBlockKind as CmarkKind, Event, Options, Parser, Tag, TagEnd};
use regex::Regex;
use walkdir::WalkDir;

use crate::ast::{
    Ast, CodeBlock, CodeBlockKind, CrossRefToken, Heading, InlineCodeSpan, Link, ListItem,
    StrikethroughSpan,
};
use crate::document::Document;
use crate::frontmatter::{self, Frontmatter};
use crate::id::DocumentId;
use crate::path_claims::{PathClaims, PathConflict};

/// What the source's walk produced.
///
/// `documents` only contains records that cleared frontmatter + id
/// parsing. Everything else shows up as a pre-rule diagnostic in
/// `parse_diagnostics` so downstream rule evaluation can merge them
/// into the final report.
#[derive(Debug, Default, Clone)]
pub struct ScanResult {
    pub documents: Vec<Document>,
    pub parse_diagnostics: Vec<ParseDiagnostic>,
    /// Files claimed by two or more namespaces' `[<NS>].paths` without
    /// an id-claim resolving the ambiguity. Surfaced as
    /// `cfg.path-conflict` `KernelMessage`s by the orchestrator
    /// (ADR-007 § DOC-007).
    pub path_conflicts: Vec<PathConflict>,
}

/// A diagnostic generated while turning a file into a [`Document`].
///
/// Kept separate from the rule-layer `Diagnostic` type so the source
/// doesn't need to know about rule codes yet — the reporter attaches
/// `core.frontmatter` / `core.id` when it folds these in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDiagnostic {
    pub location: String,
    pub kind: ParseDiagnosticKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseDiagnosticKind {
    /// Body had no recognisable frontmatter fence, or the YAML inside
    /// didn't parse. Maps to `core.frontmatter`.
    Frontmatter(String),
    /// Frontmatter parsed but `id` was missing or the empty string.
    /// Maps to `core.id`.
    IdMissing,
    /// Frontmatter had an `id` but it didn't match the CORE-003 regex.
    /// Maps to `core.id`.
    IdMalformed { raw_id: String },
}

/// Outcome of turning one file into a document.
///
/// `Skip` exists to distinguish "no frontmatter at all" (likely a
/// README, notes, or any non-ctxgrd file) from "frontmatter present
/// but broken" (a failed document the user wants flagged). See the
/// brief's CORE-001 / CORE-002 interpretation notes.
// `Document` is ~300 bytes — materially larger than other variants. We
// allow the enum variance here because every outcome path immediately
// consumes the value (push into a Vec or discard), so the hot path
// never keeps the enum in memory; boxing would only add an allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum ParseOutcome {
    Document(Document),
    Diagnostic(ParseDiagnostic),
    Skip,
    /// File was claimed by two or more namespaces' `[<NS>].paths`
    /// globs and the id-claim could not resolve the ambiguity
    /// (ADR-007 § DOC-007). Surfaced upstream as a
    /// `cfg.path-conflict` `KernelMessage`; the file is excluded
    /// from rule execution.
    Conflict(PathConflict),
}

/// Walk `root` and produce a [`ScanResult`].
///
/// `ignore`, when `Some`, is matched against each entry's path
/// relative to `root`; matching entries are skipped. Entire
/// sub-trees are pruned when a directory matches — the globset acts
/// as a `filter_entry` on the `WalkDir` iterator.
///
/// `path_claims` indexes `[<NS>].paths` GlobSets per namespace
/// (ADR-007 § DOC-001 plumbing). External callers may pass `None`;
/// it is treated as an empty index and is functionally identical
/// to passing [`PathClaims::empty`]. `parse_one` reads it but does
/// not yet act on it — the behavior flip lands in DOC-001.
pub fn scan(
    root: &Path,
    ignore: Option<&globset::GlobSet>,
    path_claims: Option<&PathClaims>,
) -> io::Result<ScanResult> {
    let empty;
    let claims = match path_claims {
        Some(c) => c,
        None => {
            empty = PathClaims::empty();
            &empty
        }
    };
    let paths = sorted_markdown_files(root, ignore, claims)?;
    let mut documents = Vec::with_capacity(paths.len());
    let mut parse_diagnostics = Vec::new();
    let mut path_conflicts = Vec::new();

    for path in paths {
        let body = fs::read_to_string(&path)?;
        let location = render_location(root, &path);
        match parse_one(&body, location, claims) {
            ParseOutcome::Document(doc) => documents.push(doc),
            ParseOutcome::Diagnostic(diag) => parse_diagnostics.push(diag),
            ParseOutcome::Conflict(conflict) => path_conflicts.push(conflict),
            ParseOutcome::Skip => {}
        }
    }

    Ok(ScanResult {
        documents,
        parse_diagnostics,
        path_conflicts,
    })
}

/// Assemble one [`Document`] from a body string, or classify the
/// failure.
///
/// Returns [`ParseOutcome::Skip`] when the body has no frontmatter
/// (no fence, or broken YAML) AND the file is not path-claimed — those
/// files are unlikely to be ctxgrd documents (READMEs, scratch notes,
/// …) and should not clutter the report with `core.frontmatter` false
/// positives. A path-claimed file is a ctxgrd document by configured
/// intent, so a missing fence or broken YAML there is a real defect and
/// produces [`ParseOutcome::Diagnostic`] (ADR-007 § DOC-002 (e)).
pub(crate) fn parse_one(body: &str, location: String, path_claims: &PathClaims) -> ParseOutcome {
    // ADR-007 § DOC-001: a markdown file is a document candidate only
    // when it claims intent — either an `id:` field (id-claim, checked
    // below) or a configured `[<NS>].paths` match (path-claim, this
    // line). Files satisfying neither are silently skipped, including
    // when frontmatter is malformed (DOC-001 verification (d)) or
    // missing an `id` field (DOC-003).
    //
    // ADR-007 § DOC-007: when more than one namespace's `[<NS>].paths`
    // claims the file, the id-claim resolves the ambiguity if present;
    // otherwise the file is excluded from rule execution and a
    // `cfg.path-conflict` is reported via ParseOutcome::Conflict.
    let claimed_by: Vec<String> = path_claims
        .matching_namespaces(&location)
        .map(String::from)
        .collect();
    let path_claimed = !claimed_by.is_empty();
    let multi_claimed = claimed_by.len() > 1;

    let conflict = || {
        ParseOutcome::Conflict(PathConflict {
            location: location.clone(),
            namespaces: claimed_by.clone(),
        })
    };

    let (fm, frontmatter_lines) = match Frontmatter::parse_with_lines(body) {
        Ok(fm) => fm,
        Err(e) => {
            // DOC-001 (d): frontmatter trouble on a non-claimed file is
            // silent, whether the YAML is malformed OR the fence is
            // absent entirely (MissingFence). READMEs and scratch notes
            // never become diagnostics.
            //
            // DOC-002 (e): a path-claimed file still gets
            // core.frontmatter — it IS a ctxgrd document by configured
            // intent, so a missing/broken frontmatter block is a real
            // defect. The MissingFence case is the BUG-001 fix: a
            // governed file with no frontmatter at all is no longer
            // silently skipped once its location is path-claimed.
            if !path_claimed {
                return ParseOutcome::Skip;
            }
            // DOC-007: with the id unparseable we cannot apply id-
            // claim resolution, so multiple path-claims become a
            // configuration error rather than a Frontmatter
            // diagnostic.
            if multi_claimed {
                return conflict();
            }
            return ParseOutcome::Diagnostic(ParseDiagnostic {
                location,
                kind: ParseDiagnosticKind::Frontmatter(e.to_string()),
            });
        }
    };

    let Some(raw_id) = fm.id.as_deref() else {
        // DOC-003: IdMissing fires only for path-claimed files. A file
        // with frontmatter but no `id:` and no path-claim is not a
        // ctxgrd document by intent.
        if !path_claimed {
            return ParseOutcome::Skip;
        }
        // DOC-007: no id to resolve the conflict.
        if multi_claimed {
            return conflict();
        }
        return ParseOutcome::Diagnostic(ParseDiagnostic {
            location,
            kind: ParseDiagnosticKind::IdMissing,
        });
    };

    let id: DocumentId = match raw_id.parse() {
        Ok(id) => id,
        Err(_) => {
            // DOC-007: malformed id can't resolve a multi-claim
            // conflict. The IdMalformed diagnostic still fires for
            // non-conflicting cases — a present-but-broken id is an
            // intent claim regardless of path-claim status.
            if multi_claimed {
                return conflict();
            }
            return ParseOutcome::Diagnostic(ParseDiagnostic {
                location,
                kind: ParseDiagnosticKind::IdMalformed {
                    raw_id: raw_id.to_owned(),
                },
            });
        }
    };

    // DOC-007: if multiple namespaces claim this path and the parsed
    // id's namespace is NOT one of them, the id-claim cannot resolve
    // the conflict. (If it IS one of them, we proceed and let the
    // file be classified into that namespace via id-claim.)
    if multi_claimed && !claimed_by.iter().any(|ns| ns == &id.namespace) {
        return conflict();
    }

    let ast = parse_ast(body);

    ParseOutcome::Document(Document {
        id,
        raw_id: raw_id.to_owned(),
        location,
        depends_on: fm.depends_on,
        frontmatter_lines,
        metadata: fm.metadata,
        ast: Some(ast),
        body: body.to_owned(),
    })
}

/// Parse the markdown body into an [`Ast`] populated per CORE-006.
///
/// Public so downstream source implementations (Phase 6 JSON-on-stdin
/// sources) can use the same parser if they're producing markdown-ish
/// bodies.
pub fn parse_ast(body: &str) -> Ast {
    let body_start = frontmatter::body_start_offset(body);
    let markdown = &body[body_start..];
    let line_starts = compute_line_starts(body);

    let mut builder = AstBuilder::new(body, body_start, &line_starts);
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);

    for (event, range) in Parser::new_ext(markdown, opts).into_offset_iter() {
        builder.on_event(event, range);
    }
    builder.finish_ref_tokens();
    builder.into_ast()
}

// -- line-position helpers ---------------------------------------------

/// Byte offsets of the first char of each line in `body`.
///
/// `line_starts[0]` is always `0`; additional entries are the byte
/// position one past each `\n`. Used to turn an arbitrary byte offset
/// into a `(line, col)` pair in constant time after a binary search.
fn compute_line_starts(body: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in body.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn byte_to_line_col(offset: usize, line_starts: &[usize]) -> (u32, u32) {
    let idx = match line_starts.binary_search(&offset) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    let line_start = line_starts[idx];
    // u32 is enough for any real document; truncating protects against
    // pathological inputs without panicking.
    let line = u32::try_from(idx + 1).unwrap_or(u32::MAX);
    let col = u32::try_from(offset.saturating_sub(line_start) + 1).unwrap_or(u32::MAX);
    (line, col)
}

// -- walking the filesystem --------------------------------------------

pub(crate) fn sorted_markdown_files(
    root: &Path,
    ignore: Option<&globset::GlobSet>,
    claims: &PathClaims,
) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_ignored(root, e.path(), e.file_type().is_dir(), ignore, claims));
    for entry in walker {
        let entry = entry.map_err(io::Error::from)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    // Byte-sorted path order, as CORE-001 demands.
    out.sort_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
    Ok(out)
}

/// True if `path` should be skipped according to the configured
/// ignore globset. The root itself is never ignored (otherwise the
/// whole walk would abort at depth 0). Match is against the path
/// relative to root so patterns like `**/.*` behave as users expect.
///
/// PKC-006 splits the claim exemption into two predicates:
///
/// - **Traversability** (directories): a directory that is an
///   ancestor-of, equal-to, or descendant-of any claim prefix is never
///   pruned, even when it matches an ignore pattern — otherwise a deep
///   claimed file (`.claude/skills/…/SKILL.md`) is unreachable under
///   the default `**/.*` ignore.
/// - **Lintability** (files): an ignore-matched file is admitted only
///   when a `[<NS>].paths` glob positively matches it. The prefix
///   relation alone would whitelist every markdown file around a
///   claimed one (e.g. a nested repo's ADRs beside a claimed SKILL.md)
///   — files this config never claimed (BUG-005).
fn is_ignored(
    root: &Path,
    path: &Path,
    is_dir: bool,
    ignore: Option<&globset::GlobSet>,
    claims: &PathClaims,
) -> bool {
    let Some(set) = ignore else {
        return false;
    };
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    if rel.as_os_str().is_empty() {
        return false;
    }
    if !set.is_match(rel) {
        return false;
    }
    if is_dir {
        // Ancestor check (`prefix.starts_with(rel)`) prevents pruning the
        // dot-dir parents before the claimed subtree is reached. Descendant
        // check (`rel.starts_with(prefix)`) is needed because globset's
        // default `*` matches `/`, so `**/.*` matches ANY directory whose
        // first component starts with `.` — including deep ones like
        // `.claude/skills/my-skill`.
        claims
            .prefix_dirs()
            .iter()
            .all(|prefix| !prefix.starts_with(rel) && !rel.starts_with(prefix))
    } else {
        claims.matching_namespaces(rel).next().is_none()
    }
}

pub(crate) fn render_location(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

// -- cross-ref token regex ---------------------------------------------

fn cross_ref_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Unanchored match; boundary checks happen in code because the
        // `regex` crate doesn't do lookaround.
        Regex::new(r"[A-Z][A-Z0-9]*-[0-9]+").expect("static cross-ref regex compiles")
    })
}

fn req_ref_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Z]{2,}-[0-9]{3,}").expect("static req-ref regex compiles"))
}

/// Returns `true` when `text` (the list item text after stripping the
/// marker) normalises to a `Satisfies` or `Addressed by` label.
/// Strips leading `**` bold markers, then strips up to the first `**`
/// close or `:` to isolate the label keyword (without colon).
fn is_req_ref_item(text: &str) -> bool {
    let inner = text.trim_start_matches("**");
    // Extract up to first `**` or first `:`, whichever comes first.
    let label = if let Some((before_bold_close, _)) = inner.split_once("**") {
        before_bold_close
            .split_once(':')
            .map(|(l, _)| l)
            .unwrap_or(before_bold_close)
    } else {
        inner.split_once(':').map(|(l, _)| l).unwrap_or(inner)
    };
    let lower = label.trim().to_lowercase();
    lower == "satisfies" || lower == "addressed by"
}

// -- event-driven AST construction -------------------------------------

struct AstBuilder<'a> {
    body: &'a str,
    body_start: usize,
    line_starts: &'a [usize],

    headings: Vec<Heading>,
    code_blocks: Vec<CodeBlock>,
    inline_code_spans: Vec<InlineCodeSpan>,
    strikethrough_spans: Vec<StrikethroughSpan>,
    cross_ref_tokens: Vec<CrossRefToken>,
    req_ref_tokens: Vec<CrossRefToken>,
    list_items: Vec<ListItem>,
    links: Vec<Link>,

    /// Byte ranges (absolute, in `body`) that count as "code" for
    /// `in_code` — fenced blocks, indented blocks, inline code spans.
    code_ranges: Vec<(usize, usize)>,
    /// Absolute byte ranges of strikethrough regions. Cross-ref tokens
    /// falling inside any range get `in_strikethrough = true`.
    strikethrough_ranges: Vec<(usize, usize)>,

    /// Heading currently being collected (text is accumulated from Text
    /// events between Start(Heading) and End(Heading)).
    heading_open: Option<HeadingOpen>,
    /// Link currently being collected, same pattern as `heading_open`.
    link_open: Option<LinkOpen>,
    /// Code block currently open; lang (if any) is known from the Start
    /// event.
    code_block_open: Option<CodeBlockOpen>,
    /// Strikethrough span currently open.
    strikethrough_open: Option<StrikethroughOpen>,
}

struct HeadingOpen {
    level: u8,
    line: u32,
    col: u32,
    text: String,
}

struct LinkOpen {
    href: String,
    line: u32,
    col: u32,
    text: String,
}

struct CodeBlockOpen {
    line_start: u32,
    open_start: usize,
    kind: CodeBlockKind,
    lang: Option<String>,
}

struct StrikethroughOpen {
    line: u32,
    col_start: u32,
    open_start: usize,
    text: String,
}

impl<'a> AstBuilder<'a> {
    fn new(body: &'a str, body_start: usize, line_starts: &'a [usize]) -> Self {
        Self {
            body,
            body_start,
            line_starts,
            headings: Vec::new(),
            code_blocks: Vec::new(),
            inline_code_spans: Vec::new(),
            strikethrough_spans: Vec::new(),
            cross_ref_tokens: Vec::new(),
            req_ref_tokens: Vec::new(),
            list_items: Vec::new(),
            links: Vec::new(),
            code_ranges: Vec::new(),
            strikethrough_ranges: Vec::new(),
            heading_open: None,
            link_open: None,
            code_block_open: None,
            strikethrough_open: None,
        }
    }

    fn abs(&self, local: usize) -> usize {
        local + self.body_start
    }

    fn pos(&self, local: usize) -> (u32, u32) {
        byte_to_line_col(self.abs(local), self.line_starts)
    }

    fn on_event(&mut self, event: Event<'_>, range: std::ops::Range<usize>) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let (line, col) = self.pos(range.start);
                self.heading_open = Some(HeadingOpen {
                    level: level as u8,
                    line,
                    col,
                    text: String::new(),
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(HeadingOpen {
                    level,
                    line,
                    col,
                    text,
                }) = self.heading_open.take()
                {
                    self.headings.push(Heading {
                        level,
                        text: text.trim().to_owned(),
                        line,
                        col,
                    });
                }
            }
            Event::Start(Tag::CodeBlock(ref kind)) => {
                let (line_start, _) = self.pos(range.start);
                let (kind_enum, lang) = match kind {
                    CmarkKind::Fenced(lang) if !lang.is_empty() => {
                        (CodeBlockKind::Fenced, Some(lang.to_string()))
                    }
                    CmarkKind::Fenced(_) => (CodeBlockKind::Fenced, None),
                    CmarkKind::Indented => (CodeBlockKind::Indented, None),
                };
                self.code_block_open = Some(CodeBlockOpen {
                    line_start,
                    open_start: self.abs(range.start),
                    kind: kind_enum,
                    lang,
                });
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(CodeBlockOpen {
                    line_start,
                    open_start,
                    kind,
                    lang,
                }) = self.code_block_open.take()
                {
                    let close_abs = self.abs(range.end);
                    // line_end: line of the last byte of the block,
                    // which is range.end - 1 unless the block is empty.
                    let end_local_byte = range.end.saturating_sub(1).max(range.start);
                    let (line_end, _) = self.pos(end_local_byte);
                    self.code_ranges.push((open_start, close_abs));
                    self.code_blocks.push(CodeBlock {
                        kind,
                        lang,
                        line_start,
                        line_end,
                    });
                }
            }
            Event::Code(ref text) => {
                let (line, col_start) = self.pos(range.start);
                let (_, col_end) = self.pos(range.end);
                self.code_ranges
                    .push((self.abs(range.start), self.abs(range.end)));
                self.inline_code_spans.push(InlineCodeSpan {
                    line,
                    col_start,
                    col_end,
                    text: text.to_string(),
                });
            }
            Event::Start(Tag::Strikethrough) => {
                let (line, col_start) = self.pos(range.start);
                self.strikethrough_open = Some(StrikethroughOpen {
                    line,
                    col_start,
                    open_start: self.abs(range.start),
                    text: String::new(),
                });
            }
            Event::End(TagEnd::Strikethrough) => {
                if let Some(StrikethroughOpen {
                    line,
                    col_start,
                    open_start,
                    text,
                }) = self.strikethrough_open.take()
                {
                    let (_, col_end) = self.pos(range.end);
                    let close_abs = self.abs(range.end);
                    self.strikethrough_ranges.push((open_start, close_abs));
                    self.strikethrough_spans.push(StrikethroughSpan {
                        line,
                        col_start,
                        col_end,
                        text,
                    });
                }
            }
            Event::Start(Tag::Link { ref dest_url, .. }) => {
                let (line, col) = self.pos(range.start);
                self.link_open = Some(LinkOpen {
                    href: dest_url.to_string(),
                    line,
                    col,
                    text: String::new(),
                });
            }
            Event::End(TagEnd::Link) => {
                if let Some(LinkOpen {
                    href,
                    line,
                    col,
                    text,
                }) = self.link_open.take()
                {
                    self.links.push(Link {
                        href,
                        text,
                        line,
                        col,
                    });
                }
            }
            Event::Start(Tag::Item) => {
                let (line, _) = self.pos(range.start);
                let line_idx = usize::try_from(line)
                    .unwrap_or(usize::MAX)
                    .saturating_sub(1);
                let line_start_byte = self.line_starts.get(line_idx).copied().unwrap_or(0);
                let line_text = &self.body[line_start_byte..]
                    .split('\n')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches('\r');
                let indent = u32::try_from(
                    line_text
                        .chars()
                        .take_while(|c| *c == ' ' || *c == '\t')
                        .count(),
                )
                .unwrap_or(0);
                let after_indent = line_text.trim_start_matches([' ', '\t']);
                let marker = extract_list_marker(after_indent);
                let text = after_indent
                    .trim_start_matches(&*marker)
                    .trim_start()
                    .to_owned();
                self.list_items.push(ListItem {
                    line,
                    indent,
                    marker,
                    text,
                });
            }
            Event::Text(ref text) => {
                if let Some(h) = self.heading_open.as_mut() {
                    h.text.push_str(text);
                }
                if let Some(l) = self.link_open.as_mut() {
                    l.text.push_str(text);
                }
                if let Some(s) = self.strikethrough_open.as_mut() {
                    s.text.push_str(text);
                }
            }
            _ => {}
        }
    }

    /// Populate both `cross_ref_tokens` and `req_ref_tokens` (ADR-029 § PIP-002).
    /// Must be called after the main event loop so code and strikethrough
    /// ranges are fully populated.
    fn finish_ref_tokens(&mut self) {
        self.extract_cross_refs();
        self.extract_req_refs();
    }

    fn extract_cross_refs(&mut self) {
        let re = cross_ref_regex();
        let body = self.body;
        for m in re.find_iter(body) {
            let start = m.start();
            let end = m.end();
            if !is_token_boundary(body, start, end) {
                continue;
            }
            // Only tokenise inside markdown (not inside frontmatter).
            if start < self.body_start {
                continue;
            }
            let matched = m.as_str();
            let dash = matched.find('-').expect("regex guarantees a dash");
            let namespace = matched[..dash].to_owned();
            let number: u32 = match matched[dash + 1..].parse() {
                Ok(n) => n,
                // Number out of range — skip; not a valid ID the kernel
                // can resolve against anyway.
                Err(_) => continue,
            };
            let (line, col) = byte_to_line_col(start, self.line_starts);
            let in_code = range_contains(&self.code_ranges, start);
            let in_strikethrough = range_contains(&self.strikethrough_ranges, start);
            self.cross_ref_tokens.push(CrossRefToken {
                token: matched.to_owned(),
                namespace,
                number,
                line,
                col,
                in_code,
                in_strikethrough,
            });
        }
    }

    fn extract_req_refs(&mut self) {
        let re = req_ref_regex();
        // Collect qualifying line numbers first to avoid borrowing `self.list_items`
        // while mutating `self.req_ref_tokens`.
        let qualifying_lines: Vec<u32> = self
            .list_items
            .iter()
            .filter(|li| is_req_ref_item(&li.text))
            .map(|li| li.line)
            .collect();

        for line_num in qualifying_lines {
            let line_idx = usize::try_from(line_num)
                .unwrap_or(usize::MAX)
                .saturating_sub(1);
            let line_start_byte = match self.line_starts.get(line_idx).copied() {
                Some(b) => b,
                None => continue,
            };
            // Skip lines inside frontmatter.
            if line_start_byte < self.body_start {
                continue;
            }
            let line_text = self.body[line_start_byte..]
                .split('\n')
                .next()
                .unwrap_or("");
            for m in re.find_iter(line_text) {
                let abs_start = line_start_byte + m.start();
                let abs_end = line_start_byte + m.end();
                if !is_token_boundary(self.body, abs_start, abs_end) {
                    continue;
                }
                let matched = m.as_str();
                let dash = matched.find('-').expect("regex guarantees a dash");
                let namespace = matched[..dash].to_owned();
                let number: u32 = match matched[dash + 1..].parse() {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let (line, col) = byte_to_line_col(abs_start, self.line_starts);
                let in_code = range_contains(&self.code_ranges, abs_start);
                let in_strikethrough = range_contains(&self.strikethrough_ranges, abs_start);
                self.req_ref_tokens.push(CrossRefToken {
                    token: matched.to_owned(),
                    namespace,
                    number,
                    line,
                    col,
                    in_code,
                    in_strikethrough,
                });
            }
        }
    }

    fn into_ast(self) -> Ast {
        Ast {
            headings: self.headings,
            code_blocks: self.code_blocks,
            inline_code_spans: self.inline_code_spans,
            strikethrough_spans: self.strikethrough_spans,
            cross_ref_tokens: self.cross_ref_tokens,
            req_ref_tokens: self.req_ref_tokens,
            list_items: self.list_items,
            links: self.links,
        }
    }
}

/// True if the bytes immediately before `start` and at `end` are not
/// part of what would be a larger ID-like token. Guards against
/// partial-match hits like `XADR-1` or `ADR-12a`.
fn is_token_boundary(body: &str, start: usize, end: usize) -> bool {
    let before_ok = if start == 0 {
        true
    } else {
        let prev = body.as_bytes()[start - 1];
        !is_id_continuation(prev)
    };
    let after_ok = if end >= body.len() {
        true
    } else {
        let next = body.as_bytes()[end];
        !is_id_continuation(next)
    };
    before_ok && after_ok
}

fn is_id_continuation(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

fn range_contains(ranges: &[(usize, usize)], offset: usize) -> bool {
    ranges.iter().any(|(s, e)| *s <= offset && offset < *e)
}

fn extract_list_marker(after_indent: &str) -> String {
    let bytes = after_indent.as_bytes();
    if bytes.is_empty() {
        return String::new();
    }
    let first = bytes[0];
    if matches!(first, b'-' | b'*' | b'+') {
        return (first as char).to_string();
    }
    if first.is_ascii_digit() {
        let end = bytes
            .iter()
            .position(|b| !b.is_ascii_digit())
            .unwrap_or(bytes.len());
        let suffix = bytes.get(end).copied();
        if matches!(suffix, Some(b'.') | Some(b')')) {
            return String::from_utf8_lossy(&bytes[..=end]).into_owned();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_BODY: &str = "---\nid: ADR-001\n---\n# Heading\n\nSome text.\n";

    fn ast_of(body: &str) -> Ast {
        parse_ast(body)
    }

    #[test]
    fn parses_heading_level_and_text() {
        let ast = ast_of(MINIMAL_BODY);
        assert_eq!(ast.headings.len(), 1);
        assert_eq!(ast.headings[0].level, 1);
        assert_eq!(ast.headings[0].text, "Heading");
    }

    #[test]
    fn heading_line_numbers_reflect_original_body() {
        let body = "---\nid: ADR-001\n---\n\n## Status\n";
        let ast = ast_of(body);
        // Frontmatter is 3 lines (1:---, 2:id, 3:---). Line 4 is blank,
        // line 5 is `## Status`.
        assert_eq!(ast.headings[0].line, 5);
        assert_eq!(ast.headings[0].level, 2);
    }

    #[test]
    fn multiple_headings_captured_in_order() {
        let body = "---\nid: ADR-001\n---\n## Status\n## Context\n## Decision\n";
        let ast = ast_of(body);
        let texts: Vec<&str> = ast.headings.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, vec!["Status", "Context", "Decision"]);
    }

    #[test]
    fn fenced_code_block_captured_with_lang() {
        let body = "---\nid: ADR-001\n---\n\n```rust\nfn foo() {}\n```\n";
        let ast = ast_of(body);
        assert_eq!(ast.code_blocks.len(), 1);
        assert_eq!(ast.code_blocks[0].kind, CodeBlockKind::Fenced);
        assert_eq!(ast.code_blocks[0].lang.as_deref(), Some("rust"));
    }

    #[test]
    fn fenced_code_block_without_lang() {
        let body = "---\nid: ADR-001\n---\n\n```\nplain\n```\n";
        let ast = ast_of(body);
        assert_eq!(ast.code_blocks.len(), 1);
        assert_eq!(ast.code_blocks[0].kind, CodeBlockKind::Fenced);
        assert_eq!(ast.code_blocks[0].lang, None);
    }

    #[test]
    fn inline_code_span_captured() {
        let body = "---\nid: ADR-001\n---\n\nUse `let x = 1` here.\n";
        let ast = ast_of(body);
        assert_eq!(ast.inline_code_spans.len(), 1);
        assert_eq!(ast.inline_code_spans[0].text, "let x = 1");
    }

    #[test]
    fn strikethrough_span_captured() {
        let body = "---\nid: ADR-001\n---\n\nSee ~~ADR-404~~ for context.\n";
        let ast = ast_of(body);
        assert_eq!(ast.strikethrough_spans.len(), 1);
        assert_eq!(ast.strikethrough_spans[0].text, "ADR-404");
    }

    #[test]
    fn cross_ref_in_regular_text_flagged_as_neither_code_nor_strike() {
        let body = "---\nid: ADR-001\n---\n\nSee ADR-042 for details.\n";
        let ast = ast_of(body);
        assert_eq!(ast.cross_ref_tokens.len(), 1);
        let t = &ast.cross_ref_tokens[0];
        assert_eq!(t.token, "ADR-042");
        assert_eq!(t.namespace, "ADR");
        assert_eq!(t.number, 42);
        assert!(!t.in_code);
        assert!(!t.in_strikethrough);
    }

    #[test]
    fn cross_ref_inside_strikethrough_sets_in_strikethrough() {
        let body = "---\nid: ADR-001\n---\n\n~~ADR-404~~ was retired.\n";
        let ast = ast_of(body);
        let t = ast
            .cross_ref_tokens
            .iter()
            .find(|t| t.token == "ADR-404")
            .expect("ADR-404 token present");
        assert!(t.in_strikethrough);
        assert!(!t.in_code);
    }

    #[test]
    fn cross_ref_inside_inline_code_sets_in_code() {
        let body = "---\nid: ADR-001\n---\n\nWrite `ADR-042` literally.\n";
        let ast = ast_of(body);
        let t = ast
            .cross_ref_tokens
            .iter()
            .find(|t| t.token == "ADR-042")
            .expect("ADR-042 token present");
        assert!(t.in_code);
        assert!(!t.in_strikethrough);
    }

    #[test]
    fn cross_ref_inside_fenced_code_block_sets_in_code() {
        let body = "---\nid: ADR-001\n---\n\n```\nADR-042 in a fenced block\n```\n";
        let ast = ast_of(body);
        let t = ast
            .cross_ref_tokens
            .iter()
            .find(|t| t.token == "ADR-042")
            .expect("ADR-042 token present");
        assert!(t.in_code);
    }

    #[test]
    fn cross_ref_tokens_not_extracted_from_frontmatter() {
        // `id: ADR-001` sits in frontmatter; must not appear as a
        // cross-ref token. Otherwise every doc cross-references itself.
        let body = "---\nid: ADR-001\n---\n\nBody with no cross-refs.\n";
        let ast = ast_of(body);
        assert!(ast.cross_ref_tokens.is_empty());
    }

    #[test]
    fn id_like_substring_rejected_on_boundary_check() {
        // `ADR-12a` is NOT a cross-ref — trailing `a` is a word continuation.
        // (An isolated `XADR-1` would be a legitimate match: XADR is a valid
        // namespace under the CORE-006 regex. Boundary rejection only fires
        // for tokens embedded in a larger alnum/`-`/`_` word.)
        let body = "---\nid: ADR-001\n---\n\nADR-12a is NOT a ref.\n";
        let ast = ast_of(body);
        assert!(
            ast.cross_ref_tokens.is_empty(),
            "unexpected tokens: {:?}",
            ast.cross_ref_tokens
        );
    }

    #[test]
    fn isolated_xadr_namespace_tokenises() {
        // `XADR-1` on its own IS a valid cross-ref per the regex — it just
        // points at a namespace that probably has no documents.
        let body = "---\nid: ADR-001\n---\n\nXADR-1 is a token.\n";
        let ast = ast_of(body);
        assert_eq!(ast.cross_ref_tokens.len(), 1);
        assert_eq!(ast.cross_ref_tokens[0].namespace, "XADR");
        assert_eq!(ast.cross_ref_tokens[0].number, 1);
    }

    #[test]
    fn cross_ref_number_overflow_is_skipped() {
        let body = "---\nid: ADR-001\n---\n\nADR-99999999999999 is too big.\n";
        let ast = ast_of(body);
        assert!(ast.cross_ref_tokens.is_empty());
    }

    #[test]
    fn link_with_text_captured() {
        let body = "---\nid: ADR-001\n---\n\nSee [ADR-001](../adrs/ADR-001.md) for background.\n";
        let ast = ast_of(body);
        assert_eq!(ast.links.len(), 1);
        assert_eq!(ast.links[0].href, "../adrs/ADR-001.md");
        assert_eq!(ast.links[0].text, "ADR-001");
    }

    #[test]
    fn list_items_captured() {
        let body = "---\nid: ADR-001\n---\n\n- First\n- Second\n- Third\n";
        let ast = ast_of(body);
        assert_eq!(ast.list_items.len(), 3);
        let texts: Vec<&str> = ast.list_items.iter().map(|li| li.text.as_str()).collect();
        assert_eq!(texts, vec!["First", "Second", "Third"]);
        assert!(ast.list_items.iter().all(|li| li.marker == "-"));
    }

    #[test]
    fn compute_line_starts_matches_expected() {
        let body = "a\nbc\n\nd";
        let starts = compute_line_starts(body);
        assert_eq!(starts, vec![0, 2, 5, 6]);
    }

    #[test]
    fn byte_to_line_col_maps_offsets_correctly() {
        let body = "abc\ndef\nghi";
        let starts = compute_line_starts(body);
        assert_eq!(byte_to_line_col(0, &starts), (1, 1));
        assert_eq!(byte_to_line_col(2, &starts), (1, 3));
        assert_eq!(byte_to_line_col(4, &starts), (2, 1));
        assert_eq!(byte_to_line_col(10, &starts), (3, 3));
    }

    fn expect_doc(outcome: ParseOutcome) -> Document {
        match outcome {
            ParseOutcome::Document(d) => d,
            other => panic!("expected Document, got {:?}", other),
        }
    }

    fn expect_diagnostic(outcome: ParseOutcome) -> ParseDiagnostic {
        match outcome {
            ParseOutcome::Diagnostic(d) => d,
            other => panic!("expected Diagnostic, got {:?}", other),
        }
    }

    /// Build a `PathClaims` that claims `glob` for `namespace`. Used
    /// by tests that need to exercise the path-claimed branches of
    /// `parse_one` without standing up a full `Config`.
    fn claims_for(namespace: &str, glob: &str) -> PathClaims {
        claims_for_many(&[(namespace, glob)])
    }

    /// Build a `PathClaims` covering multiple `(namespace, glob)`
    /// pairs. Used by DOC-007 conflict tests where a single file
    /// must match two namespaces' paths.
    fn claims_for_many(entries: &[(&str, &str)]) -> PathClaims {
        let mut cfg = crate::config::Config::default();
        for (ns, glob) in entries {
            let mut builder = globset::GlobSetBuilder::new();
            builder.add(globset::Glob::new(glob).unwrap());
            cfg.namespaces.insert(
                (*ns).to_owned(),
                crate::config::NamespaceConfig {
                    rules: Vec::new(),
                    params: std::collections::BTreeMap::new(),
                    paths: Some(builder.build().unwrap()),
                    path_patterns: vec![(*glob).to_owned()],
                },
            );
        }
        PathClaims::from_config(&cfg)
    }

    fn expect_conflict(outcome: ParseOutcome) -> PathConflict {
        match outcome {
            ParseOutcome::Conflict(c) => c,
            other => panic!("expected Conflict, got {:?}", other),
        }
    }

    #[test]
    fn parse_one_produces_populated_document() {
        let body = "---\nid: ADR-001\ntitle: T\n---\n\n# ADR-001\n\nSee ADR-002.\n";
        let doc = expect_doc(parse_one(
            body,
            "adrs/ADR-001.md".to_owned(),
            &PathClaims::empty(),
        ));
        assert_eq!(doc.id, DocumentId::new("ADR", 1));
        assert_eq!(doc.raw_id, "ADR-001");
        assert_eq!(doc.metadata.len(), 1);
        let ast = doc.ast.unwrap();
        let tokens: Vec<&str> = ast
            .cross_ref_tokens
            .iter()
            .map(|t| t.token.as_str())
            .collect();
        assert_eq!(tokens, vec!["ADR-001", "ADR-002"]);
    }

    #[test]
    fn parse_one_skips_files_without_fence() {
        // README-style bodies have no fence at all — the source must
        // NOT treat them as failed documents.
        assert_eq!(
            parse_one(
                "no fence here\n",
                "README.md".to_owned(),
                &PathClaims::empty()
            ),
            ParseOutcome::Skip
        );
        assert_eq!(
            parse_one("", "empty.md".to_owned(), &PathClaims::empty()),
            ParseOutcome::Skip
        );
        assert_eq!(
            parse_one(
                "# Just a heading\n",
                "notes.md".to_owned(),
                &PathClaims::empty()
            ),
            ParseOutcome::Skip
        );
    }

    #[test]
    fn parse_one_reports_broken_yaml_as_frontmatter_diagnostic_when_path_claimed() {
        // Fence present, YAML unterminated → the file looked like a
        // document but the content is wrong. Under DOC-001 (d) the
        // diagnostic only fires for path-claimed files; a file at the
        // same location with no path-claim must skip silently (covered
        // by `doc_001_d_malformed_frontmatter_silent_when_unclaimed`).
        let body = "---\nid: ADR-001\ntags: [security, audit\n---\n";
        let claims = claims_for("ADR", "**/*.md");
        let diag = expect_diagnostic(parse_one(body, "x.md".to_owned(), &claims));
        assert_eq!(diag.location, "x.md");
        assert!(matches!(diag.kind, ParseDiagnosticKind::Frontmatter(_)));
    }

    #[test]
    fn parse_one_reports_id_missing_when_path_claimed() {
        // DOC-003: IdMissing fires only for path-claimed files.
        let body = "---\ntitle: anon\n---\n";
        let claims = claims_for("ADR", "**/*.md");
        let diag = expect_diagnostic(parse_one(body, "x.md".to_owned(), &claims));
        assert!(matches!(diag.kind, ParseDiagnosticKind::IdMissing));
    }

    // --- ADR-007 § DOC-001 verification ---

    #[test]
    fn doc_001_a_no_id_no_path_match_is_skipped() {
        // Verification (a): file with frontmatter containing only
        // `version: alpha` (no id) and located outside any configured
        // [<NS>].paths produces zero diagnostics.
        let body = "---\nversion: alpha\nname: Studio\n---\n";
        let claims = claims_for("ADR", "docs/adrs/**");
        assert_eq!(
            parse_one(body, "DESIGN.md".to_owned(), &claims),
            ParseOutcome::Skip,
        );
    }

    #[test]
    fn doc_001_b_path_claimed_no_id_fires_id_missing() {
        // Verification (b): same body inside `[ADR].paths` produces a
        // core.id (IdMissing) diagnostic.
        let body = "---\nversion: alpha\n---\n";
        let claims = claims_for("ADR", "docs/adrs/**");
        let diag = expect_diagnostic(parse_one(body, "docs/adrs/draft.md".to_owned(), &claims));
        assert!(matches!(diag.kind, ParseDiagnosticKind::IdMissing));
    }

    #[test]
    fn doc_001_c_id_claim_outside_paths_still_classifies() {
        // Verification (c): file with `id: ADR-001` outside any
        // configured path is still classified as an ADR.
        let body = "---\nid: ADR-001\ntitle: orphan\n---\n# body\n";
        let claims = claims_for("ADR", "docs/adrs/**");
        let doc = expect_doc(parse_one(body, "scratch/orphan.md".to_owned(), &claims));
        assert_eq!(doc.id.namespace, "ADR");
        assert_eq!(doc.id.number, 1);
    }

    #[test]
    fn doc_001_d_malformed_frontmatter_silent_when_unclaimed() {
        // Verification (d): a file with neither id nor path match is
        // silently skipped even when its frontmatter is malformed (no
        // core.frontmatter diagnostic).
        let body = "---\nid: ADR-001\ntags: [unterminated\n---\n";
        let claims = claims_for("ADR", "docs/adrs/**");
        assert_eq!(
            parse_one(body, "outside/garbage.md".to_owned(), &claims),
            ParseOutcome::Skip,
        );
    }

    #[test]
    fn doc_002_e_path_claimed_no_fence_fires_frontmatter() {
        // BUG-001 / DOC-002 (e): a governed file with NO frontmatter
        // fence at all, located under a configured `[<NS>].paths`, is a
        // real defect — it must fire core.frontmatter, not skip silently.
        let body = "# PRD-001: Title\n\nBody with no frontmatter.\n";
        let claims = claims_for("PRD", "docs/prds/**");
        let diag = expect_diagnostic(parse_one(body, "docs/prds/001-foo.md".to_owned(), &claims));
        assert_eq!(diag.location, "docs/prds/001-foo.md");
        assert!(matches!(diag.kind, ParseDiagnosticKind::Frontmatter(_)));
    }

    #[test]
    fn no_fence_unclaimed_still_skips() {
        // The DOC-001 silence is load-bearing: a no-fence file outside
        // any configured path stays skipped after the BUG-001 fix.
        let body = "# Just a heading\n";
        let claims = claims_for("PRD", "docs/prds/**");
        assert_eq!(
            parse_one(body, "notes/scratch.md".to_owned(), &claims),
            ParseOutcome::Skip,
        );
    }

    #[test]
    fn no_fence_multi_claim_yields_conflict() {
        // DOC-007: with no fence there is no id to resolve a multi-claim,
        // so two overlapping paths produce a conflict, not a diagnostic.
        let body = "# heading only\n";
        let claims = claims_for_many(&[("ADR", "docs/**"), ("PRD", "docs/**")]);
        let conflict = expect_conflict(parse_one(body, "docs/orphan.md".to_owned(), &claims));
        assert_eq!(conflict.namespaces, vec!["ADR", "PRD"]);
    }

    // --- ADR-007 § DOC-007 verification ---

    #[test]
    fn doc_007_no_id_multi_claim_yields_conflict() {
        // Verification: `[ADR].paths = ["docs/**"]` and
        // `[PRD].paths = ["docs/**"]` → a file under docs/ without an
        // id produces a path-conflict outcome (not a per-document
        // diagnostic). The namespaces are sorted (BTreeMap order).
        let body = "---\ntitle: ambiguous\n---\n";
        let claims = claims_for_many(&[("ADR", "docs/**"), ("PRD", "docs/**")]);
        let conflict = expect_conflict(parse_one(body, "docs/something.md".to_owned(), &claims));
        assert_eq!(conflict.location, "docs/something.md");
        assert_eq!(conflict.namespaces, vec!["ADR", "PRD"]);
    }

    #[test]
    fn doc_007_id_claim_resolves_multi_claim() {
        // Verification: same overlapping paths but with `id: ADR-001`
        // → file is classified as ADR, no conflict.
        let body = "---\nid: ADR-001\ntitle: resolved\n---\n# body\n";
        let claims = claims_for_many(&[("ADR", "docs/**"), ("PRD", "docs/**")]);
        let doc = expect_doc(parse_one(body, "docs/ADR-001.md".to_owned(), &claims));
        assert_eq!(doc.id.namespace, "ADR");
        assert_eq!(doc.id.number, 1);
    }

    #[test]
    fn doc_007_id_for_unclaiming_namespace_still_conflicts() {
        // id-claim resolves the conflict ONLY when the id's namespace
        // matches one of the conflicting namespaces. An id like
        // FOO-001 against an ADR/PRD overlap is no help.
        let body = "---\nid: FOO-001\n---\n";
        let claims = claims_for_many(&[("ADR", "docs/**"), ("PRD", "docs/**")]);
        let conflict = expect_conflict(parse_one(body, "docs/foo.md".to_owned(), &claims));
        assert_eq!(conflict.namespaces, vec!["ADR", "PRD"]);
    }

    #[test]
    fn doc_007_malformed_frontmatter_under_multi_claim_yields_conflict() {
        // Frontmatter that won't parse leaves us no id to apply id-
        // claim resolution → conflict, not core.frontmatter.
        let body = "---\nid: ADR-001\ntags: [unterminated\n---\n";
        let claims = claims_for_many(&[("ADR", "docs/**"), ("PRD", "docs/**")]);
        let conflict = expect_conflict(parse_one(body, "docs/broken.md".to_owned(), &claims));
        assert_eq!(conflict.namespaces, vec!["ADR", "PRD"]);
    }

    #[test]
    fn doc_007_malformed_id_under_multi_claim_yields_conflict() {
        // A present-but-malformed id is still an intent claim, but
        // it can't resolve a multi-claim because we never extract a
        // namespace from it.
        let body = "---\nid: not-an-id\n---\n";
        let claims = claims_for_many(&[("ADR", "docs/**"), ("PRD", "docs/**")]);
        let conflict = expect_conflict(parse_one(body, "docs/broken-id.md".to_owned(), &claims));
        assert_eq!(conflict.namespaces, vec!["ADR", "PRD"]);
    }

    #[test]
    fn doc_007_kernel_message_renders_with_help() {
        // Conversion to KernelMessage: code, severity, namespaces in
        // message, and the resolution help text.
        let conflict = PathConflict {
            location: "docs/x.md".to_owned(),
            namespaces: vec!["ADR".to_owned(), "PRD".to_owned()],
        };
        let msg = conflict.to_kernel_message();
        assert_eq!(msg.code, "cfg.path-conflict");
        assert_eq!(msg.severity, crate::diagnostic::Severity::Error);
        assert!(msg.message.contains("docs/x.md"));
        assert!(msg.message.contains("ADR, PRD"));
        let help = msg.help.as_deref().expect("help present");
        assert!(help.contains("`id:"));
        assert!(help.contains("`[<NS>].paths`"));
    }

    #[test]
    fn parse_one_reports_id_malformed() {
        let body = "---\nid: not-an-id\n---\n";
        let diag = expect_diagnostic(parse_one(body, "x.md".to_owned(), &PathClaims::empty()));
        match diag.kind {
            ParseDiagnosticKind::IdMalformed { raw_id } => assert_eq!(raw_id, "not-an-id"),
            other => panic!("expected IdMalformed, got {:?}", other),
        }
    }

    #[test]
    fn frontmatter_lines_populated() {
        let body = "---\nid: ADR-099\ndepends_on:\n  - PRD-999\nstatus: cooking\n---\n\nBody.\n";
        let doc = expect_doc(parse_one(body, "x.md".to_owned(), &PathClaims::empty()));
        assert_eq!(doc.frontmatter_lines.get("id"), Some(&2));
        assert_eq!(doc.frontmatter_lines.get("depends_on"), Some(&3));
        assert_eq!(doc.frontmatter_lines.get("status"), Some(&5));
    }

    #[test]
    fn scan_respects_ignore_globset() {
        let root = tempfile::tempdir().unwrap();
        // Three files: one we want linted, two we want ignored.
        let keep = root.path().join("adrs").join("ADR-001.md");
        std::fs::create_dir_all(keep.parent().unwrap()).unwrap();
        std::fs::write(&keep, "---\nid: ADR-001\ntitle: keep\n---\n# real\n").unwrap();

        let hidden_dir = root.path().join(".hidden").join("SKILL.md");
        std::fs::create_dir_all(hidden_dir.parent().unwrap()).unwrap();
        std::fs::write(&hidden_dir, "---\nid: not-valid\n---\nhidden skill\n").unwrap();

        let build_out = root.path().join("target").join("docs").join("OUT.md");
        std::fs::create_dir_all(build_out.parent().unwrap()).unwrap();
        std::fs::write(&build_out, "---\nid: 001\n---\nbuild artefact\n").unwrap();

        let patterns = ["**/.*", "target/**"];
        let mut builder = globset::GlobSetBuilder::new();
        for p in patterns {
            builder.add(globset::Glob::new(p).unwrap());
        }
        let set = builder.build().unwrap();

        let result = scan(root.path(), Some(&set), None).unwrap();
        // Only the kept ADR should be picked up; the ignored files
        // mustn't appear in documents OR parse_diagnostics.
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].id.namespace, "ADR");
        assert!(
            result.parse_diagnostics.is_empty(),
            "ignored files must not produce diagnostics: {:?}",
            result.parse_diagnostics
        );
    }

    #[test]
    fn path_claim_prefix_escapes_dotfile_ignore() {
        // PKC-006: a SKILLS-claimed .claude/skills/.../SKILL.md under the
        // default **/.*  ignore is linted; .git stays pruned.
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join(".claude/skills/my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: does stuff\n---\n# Skill\n",
        )
        .unwrap();
        // Also create a .git directory to verify it stays pruned
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        let patterns = ["**/.*"];
        let mut builder = globset::GlobSetBuilder::new();
        for p in patterns {
            builder.add(globset::Glob::new(p).unwrap());
        }
        let set = builder.build().unwrap();

        // With no claims, .claude/ is pruned — 0 files found.
        let without_exemption =
            sorted_markdown_files(root.path(), Some(&set), &PathClaims::empty()).unwrap();
        assert!(
            without_exemption.is_empty(),
            "without prefix exemption, .claude/ is pruned: {:?}",
            without_exemption
        );

        // With a SKILLS claim, SKILL.md is found.
        // NOTE: globset's default `*` matches `/`, so `**/.*` matches
        // any path whose first component starts with `.`, including deep
        // descendants like `.claude/skills/my-skill`. The exemption must
        // cover ancestors AND descendants of the claim prefix (PKC-006).
        let claims = test_claims(&[("SKILLS", &[".claude/skills/**/SKILL.md"])]);
        let with_exemption = sorted_markdown_files(root.path(), Some(&set), &claims).unwrap();
        assert_eq!(
            with_exemption.len(),
            1,
            "SKILL.md found with prefix exemption: {:?}",
            with_exemption
        );
        assert!(
            with_exemption[0].ends_with("SKILL.md"),
            "found file is SKILL.md: {:?}",
            with_exemption
        );
    }

    /// Build a [`PathClaims`] from `(namespace, paths globs)` pairs, the
    /// way `Config` would after parsing `[<NS>].paths`.
    fn test_claims(namespaces: &[(&str, &[&str])]) -> PathClaims {
        let mut cfg = crate::config::Config::default();
        for (name, patterns) in namespaces {
            let mut builder = globset::GlobSetBuilder::new();
            for p in *patterns {
                builder.add(globset::Glob::new(p).unwrap());
            }
            cfg.namespaces.insert(
                (*name).to_owned(),
                crate::config::NamespaceConfig {
                    rules: Vec::new(),
                    params: std::collections::BTreeMap::new(),
                    paths: Some(builder.build().unwrap()),
                    path_patterns: patterns.iter().map(|s| (*s).to_owned()).collect(),
                },
            );
        }
        PathClaims::from_config(&cfg)
    }

    #[test]
    fn claim_prefix_does_not_admit_unclaimed_descendants() {
        // BUG-005: the PKC-006 exemption must admit only files a claim
        // glob positively matches — not every markdown file under the
        // claim prefix. A nested repo's id-claimed ADR next to a claimed
        // SKILL.md must stay ignored under the default `**/.*` pattern,
        // while the SKILL.md itself is still reached.
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join(".claude/skills/my-skill");
        let adr_dir = skill_dir.join("docs/adrs");
        std::fs::create_dir_all(&adr_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: does stuff\n---\n# Skill\n",
        )
        .unwrap();
        std::fs::write(
            adr_dir.join("001-reuse-boundary.md"),
            "---\nid: ADR-1\ntitle: Reuse boundary\nstatus: accepted\n---\n# ADR\n",
        )
        .unwrap();

        let mut builder = globset::GlobSetBuilder::new();
        builder.add(globset::Glob::new("**/.*").unwrap());
        let set = builder.build().unwrap();
        let claims = test_claims(&[("SKILLS", &[".claude/skills/**/SKILL.md"])]);

        let found = sorted_markdown_files(root.path(), Some(&set), &claims).unwrap();
        assert_eq!(
            found.len(),
            1,
            "only the claimed SKILL.md escapes [ignore]: {:?}",
            found
        );
        assert!(
            found[0].ends_with("SKILL.md"),
            "found file is SKILL.md: {:?}",
            found
        );
    }

    // -- req_ref_tokens tests ---------------------------------------------

    #[test]
    fn satisfies_line_produces_req_ref_token() {
        // `- **Satisfies:** FR-007` → one entry: prefix FR, number 7, in_code false.
        let body = "---\nid: ADR-001\n---\n\n- **Satisfies:** FR-007\n";
        let ast = ast_of(body);
        assert_eq!(ast.req_ref_tokens.len(), 1);
        let t = &ast.req_ref_tokens[0];
        assert_eq!(t.token, "FR-007");
        assert_eq!(t.namespace, "FR");
        assert_eq!(t.number, 7);
        assert!(!t.in_code);
        assert!(!t.in_strikethrough);
    }

    #[test]
    fn addressed_by_line_produces_req_ref_token() {
        // `- **Addressed by:** NFR-003` is also picked up.
        let body = "---\nid: ADR-001\n---\n\n- **Addressed by:** NFR-003\n";
        let ast = ast_of(body);
        assert_eq!(ast.req_ref_tokens.len(), 1);
        let t = &ast.req_ref_tokens[0];
        assert_eq!(t.token, "NFR-003");
        assert_eq!(t.namespace, "NFR");
        assert_eq!(t.number, 3);
        assert!(!t.in_code);
    }

    #[test]
    fn req_ref_token_inside_backticks_sets_in_code() {
        // Token inside backticks on a Satisfies line must have in_code = true.
        let body = "---\nid: ADR-001\n---\n\n- **Satisfies:** `FR-300`\n";
        let ast = ast_of(body);
        assert_eq!(ast.req_ref_tokens.len(), 1);
        assert!(ast.req_ref_tokens[0].in_code);
    }

    #[test]
    fn non_satisfies_line_produces_no_req_ref_tokens() {
        // A line that is not a Satisfies/Addressed-by item must not populate req_ref_tokens.
        let body = "---\nid: ADR-001\n---\n\nSee FR-007 for details.\n";
        let ast = ast_of(body);
        assert!(
            ast.req_ref_tokens.is_empty(),
            "prose line must not produce req_ref_tokens: {:?}",
            ast.req_ref_tokens
        );
    }
}
