//! Reference scanner — ADR-001 § REF-001..REF-002, REF-004, REF-007, REF-010.
//!
//! A [`Reference`] is a pointer mention found in a non-markdown file —
//! `// see ADR-042` in source code, `# see PRD-001` in `Cargo.toml`,
//! ticket-style mentions in commit messages, etc. References are a
//! separate input class from [`Document`](crate::document::Document):
//! they have no identity, no metadata, no body. They are anonymous
//! edges INTO the document graph that the kernel checks for resolution
//! via `core.cross-ref` (with the namespace-prefix filter from
//! REF-005 already in place).
//!
//! Scanning uses the ripgrep stack (`grep-searcher`, `grep-regex`,
//! `ignore`) so we inherit literal pre-filter, lazy DFA, parallel
//! walk, and gitignore semantics for free.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch};
use ignore::WalkBuilder;

/// The token shape recognised as a reference. Matches the same regex
/// used by the cross-ref tokenizer in [`crate::ast`] so the two
/// agree on what a "potential reference" is.
const REFERENCE_TOKEN_REGEX: &str = r"\b[A-Z][A-Z0-9]*-[0-9]+\b";

/// One reference mention. Identity-less; only the location matters.
///
/// ADR-001 § REF-001 specifies this exact shape: `file_path`, `line`,
/// `col`, `token`. Position fields are 1-indexed for humans;
/// `col` points at the first byte of the token within the line.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Reference {
    pub file_path: PathBuf,
    pub line: u32,
    pub col: u32,
    pub token: String,
}

/// Inline-suppression markers (ADR-001 § REF-007). Recognised as
/// substrings anywhere on the relevant source line; we deliberately
/// don't parse comment delimiters per format.
const SUPPRESS_LINE: &str = "ctxgrd: ignore-line";
const SUPPRESS_NEXT: &str = "ctxgrd: ignore-next";

/// Result of a [`scan`] call: the references it found, plus counts
/// of per-file failures it absorbed silently to keep going.
///
/// Per-file errors (a directory entry that failed to stat, a single
/// unreadable file) MUST NOT halt the whole walk — a parallel scan
/// over thousands of files routinely encounters one or two such
/// entries and the user wants the rest of the references back. The
/// caller decides whether to surface a `ref.scan-error` warning to
/// the kernel based on the counts.
#[derive(Debug, Default)]
pub(crate) struct ScanReport {
    pub references: Vec<Reference>,
    /// Directory entries the walker could not produce (permissions,
    /// broken symlinks, filesystem races).
    pub walker_errors: usize,
    /// Files the searcher could not read end-to-end (mid-stream IO
    /// failures; open-failures are surfaced by the walker layer).
    pub searcher_errors: usize,
}

/// Scan `root` for files matching `globs`, emitting one [`Reference`]
/// per token of the form `<NAMESPACE>-<number>` found in any matched
/// file.
///
/// Honours `.gitignore` / `.ignore` / `.rgignore` by default
/// (REF-010). Inline suppression markers (REF-007) are decided in the
/// same pass as matching: a `Sink::context` callback (driven by
/// `SearcherBuilder::before_context(1)`) lets us inspect the line
/// preceding each match without re-reading the file.
///
/// REF-004: callers MUST NOT include markdown documents in `globs` —
/// document bodies are already tokenised by the markdown walker, and
/// re-scanning them would double-emit and lose code-span /
/// strikethrough suppression. We don't enforce this in code (some
/// users legitimately have non-document `.md` files outside their
/// document tree); it is documented in `docs/rules.md`.
pub(crate) fn scan(root: &Path, globs: &[String]) -> io::Result<ScanReport> {
    if globs.is_empty() {
        return Ok(ScanReport::default());
    }

    let matcher = RegexMatcher::new(REFERENCE_TOKEN_REGEX)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Build a globset over the scan patterns so we filter walk hits
    // without invoking the regex on files the user didn't ask for.
    let globset =
        build_globset(globs).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let collected = Mutex::new(Vec::<Reference>::new());
    let walker_errors = AtomicUsize::new(0);
    let searcher_errors = AtomicUsize::new(0);

    let walker = WalkBuilder::new(root)
        .standard_filters(true) // honour .gitignore / .ignore / .rgignore (REF-010)
        .build_parallel();

    walker.run(|| {
        let matcher = matcher.clone();
        let globset = &globset;
        let collected = &collected;
        let walker_errors = &walker_errors;
        let searcher_errors = &searcher_errors;
        let root = root.to_path_buf();
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    walker_errors.fetch_add(1, Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                }
            };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return ignore::WalkState::Continue;
            }
            let path = entry.path();
            // Match globs relative to the lint root so users can write
            // patterns like `src/**/*.rs` rather than absolute paths.
            let rel = path.strip_prefix(&root).unwrap_or(path);
            if !globset.is_match(rel) {
                return ignore::WalkState::Continue;
            }
            // Per-file: `prev` MUST NOT survive between files (REF-007
            // — ignore-next must not bleed across file boundaries).
            // The sink is constructed inside the inner closure so a
            // future refactor that hoists it out fails loudly.
            let mut local = Vec::<Reference>::new();
            let mut sink = ReferenceSink {
                // Store paths relative to the lint root for consistency
                // with markdown `Document.location`. The diagnostic
                // report shows paths the user can paste into their
                // editor regardless of how they invoked ctxgrd.
                file_path: rel.to_path_buf(),
                refs: &mut local,
                prev: None,
            };
            // grep-searcher runs the lazy DFA + literal pre-filter; we
            // hand off raw bytes from the file so we never UTF-8 the
            // hot path. `before_context(1)` makes the searcher emit a
            // `Sink::context` callback for the line preceding each
            // match — that is what lets us honour `ignore-next` in a
            // single pass instead of re-reading the file afterwards.
            let mut searcher = SearcherBuilder::new().before_context(1).build();
            if searcher.search_path(&matcher, path, &mut sink).is_err() {
                searcher_errors.fetch_add(1, Ordering::Relaxed);
            }
            if !local.is_empty() {
                // Recover from a poisoned mutex rather than silently
                // dropping results: only `Vec::extend` runs under it,
                // which cannot panic except OOM, so any poisoning is
                // benign and the data past the panic is still valid.
                let mut g = collected.lock().unwrap_or_else(|p| p.into_inner());
                g.extend(local);
            }
            ignore::WalkState::Continue
        })
    });

    let mut references = collected.into_inner().unwrap_or_else(|p| p.into_inner());
    // Determinism: sort by (file_path, line, col, token) so callers
    // (and `ctxgrd refs <ID>`) see stable output across runs.
    // `sort_unstable_by` is faster and the equality fields ARE the
    // sort key, so dedup behaviour is identical.
    references.sort_unstable_by(|a, b| {
        (&a.file_path, a.line, a.col, &a.token).cmp(&(&b.file_path, b.line, b.col, &b.token))
    });
    references.dedup();
    Ok(ScanReport {
        references,
        walker_errors: walker_errors.into_inner(),
        searcher_errors: searcher_errors.into_inner(),
    })
}

fn build_globset(globs: &[String]) -> Result<globset::GlobSet, globset::Error> {
    let mut builder = globset::GlobSetBuilder::new();
    for g in globs {
        builder.add(globset::Glob::new(g)?);
    }
    builder.build()
}

/// `Sink` adapter that turns each grep-searcher match into one or
/// more [`Reference`] records, applying inline suppression markers
/// (REF-007) in the same pass.
///
/// `prev` carries the line number of the most recent line the
/// searcher told us about (either as `Sink::context` Before, or as
/// the trailing assignment in `Sink::matched`) and a precomputed
/// flag for whether THAT line contained `ctxgrd: ignore-next`. We
/// never need to look at the line bytes again — the marker check
/// runs once at storage time and the bytes are discarded — so the
/// sink stays allocation-free on the hot path.
struct ReferenceSink<'a> {
    file_path: PathBuf,
    refs: &'a mut Vec<Reference>,
    prev: Option<(u64, bool)>,
}

impl<'a> Sink for ReferenceSink<'a> {
    type Error = io::Error;

    fn context(&mut self, _searcher: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, io::Error> {
        if matches!(ctx.kind(), SinkContextKind::Before) {
            self.prev = Some((
                ctx.line_number().unwrap_or(0),
                line_has_marker(ctx.bytes(), SUPPRESS_NEXT),
            ));
        }
        Ok(true)
    }

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        let line_no = mat.line_number().unwrap_or(0);
        let line_bytes = mat.bytes();

        // ignore-line: the marker appears anywhere on the matched
        // line itself; suppresses ALL tokens on that line.
        let suppress_line = line_has_marker(line_bytes, SUPPRESS_LINE);

        // ignore-next: the marker appeared on the IMMEDIATELY
        // preceding line. Adjacent matches chain correctly because
        // we update `prev` at the end of `matched` too — if line N
        // is itself a match carrying `ignore-next`, the searcher
        // will NOT re-emit it as Before context for line N+1, so
        // the trailing assignment is what keeps the chain accurate.
        let suppress_next = self.prev.is_some_and(|(prev_no, prev_has_marker)| {
            prev_has_marker && prev_no == line_no.saturating_sub(1)
        });

        if !(suppress_line || suppress_next) {
            // Decode lazily — we only UTF-8 the matched line, not
            // the whole file. Use std `regex` here because
            // `grep_regex::RegexMatcher` does not expose iterator-
            // style match positions per line. The pattern is
            // identical to REFERENCE_TOKEN_REGEX.
            if let Ok(line_text) = std::str::from_utf8(line_bytes) {
                for cap in TOKEN_RE.find_iter(line_text) {
                    self.refs.push(Reference {
                        file_path: self.file_path.clone(),
                        line: line_no as u32,
                        col: (cap.start() as u32).saturating_add(1),
                        token: cap.as_str().to_string(),
                    });
                }
            }
        }

        // Track this line for the next match's potential
        // ignore-next check. Required because the searcher won't
        // re-emit a matched line as before-context for an adjacent
        // match. We precompute the marker check now and discard the
        // bytes — see the field comment on `prev`.
        self.prev = Some((line_no, line_has_marker(line_bytes, SUPPRESS_NEXT)));
        Ok(true)
    }
}

// Compile the token regex once. Using `LazyLock` keeps everything in
// the safe-stdlib lane; no `once_cell`.
static TOKEN_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(REFERENCE_TOKEN_REGEX).expect("regex compiles"));

/// ASCII-only substring check on raw line bytes. The two suppression
/// markers are pure ASCII so byte-level windowing is safe and avoids
/// a UTF-8 decode of the previous line just to look for a marker.
fn line_has_marker(line: &[u8], marker: &str) -> bool {
    let needle = marker.as_bytes();
    if line.len() < needle.len() {
        return false;
    }
    line.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
        path
    }

    /// REF-002 verification: a TOML file, a Go file, and a Rust file
    /// containing both real and shape-matching tokens emit the
    /// expected References with correct line/col.
    #[test]
    fn scan_finds_tokens_across_formats() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "Cargo.toml",
            "# tracks PRD-001 and ADR-042\nname = \"x\"\n",
        );
        write(
            root.path(),
            "src/main.go",
            "// implementing per ADR-007\npackage main\n",
        );
        write(
            root.path(),
            "src/lib.rs",
            "// see PRD-001 for context\nfn main() {}\n",
        );
        let refs = scan(
            root.path(),
            &[
                "**/*.toml".to_string(),
                "**/*.go".to_string(),
                "**/*.rs".to_string(),
            ],
        )
        .unwrap()
        .references;
        let tokens: Vec<&str> = refs.iter().map(|r| r.token.as_str()).collect();
        assert!(tokens.contains(&"PRD-001"));
        assert!(tokens.contains(&"ADR-042"));
        assert!(tokens.contains(&"ADR-007"));
        // PRD-001 appears in two files → two references with distinct
        // file_paths.
        let prd_count = refs.iter().filter(|r| r.token == "PRD-001").count();
        assert_eq!(prd_count, 2);
    }

    /// REF-002 verification: the line and column attribution is
    /// 1-indexed and points at the first byte of the token.
    #[test]
    fn scan_emits_correct_line_and_col() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "src/lib.rs",
            "// preamble line\n// implementing per ADR-042 here\n",
        );
        let refs = scan(root.path(), &["**/*.rs".to_string()])
            .unwrap()
            .references;
        assert_eq!(refs.len(), 1);
        let r = &refs[0];
        assert_eq!(r.token, "ADR-042");
        assert_eq!(r.line, 2);
        // "// implementing per ADR-042" — A at byte 20 (0-indexed)
        // → col 21 (1-indexed).
        assert_eq!(r.col, 21);
    }

    /// REF-007 verification: `ctxgrd: ignore-line` and
    /// `ctxgrd: ignore-next` suppress matches; the same file without
    /// the marker still emits diagnostics.
    #[test]
    fn scan_honours_inline_suppression_markers() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "src/a.rs",
            "// ctxgrd: ignore-next\nconst topic = \"ADR-9999\";\n",
        );
        write(
            root.path(),
            "src/b.rs",
            "// see ADR-1234 ctxgrd: ignore-line\n",
        );
        write(root.path(), "src/c.rs", "// real ADR-5555 reference\n");
        let refs = scan(root.path(), &["**/*.rs".to_string()])
            .unwrap()
            .references;
        let tokens: Vec<&str> = refs.iter().map(|r| r.token.as_str()).collect();
        assert!(!tokens.contains(&"ADR-9999"), "ignore-next should suppress");
        assert!(!tokens.contains(&"ADR-1234"), "ignore-line should suppress");
        assert!(tokens.contains(&"ADR-5555"), "unmarked match should fire");
    }

    /// REF-010 verification: files listed in an `.ignore` file are
    /// excluded from the scan. We use `.ignore` rather than
    /// `.gitignore` because the `ignore` crate only recognises
    /// `.gitignore` inside an actual git repo (presence of `.git/`).
    /// ADR-001 § REF-010 names `.gitignore`, `.ignore`, and
    /// `.rgignore` together; honouring any of them runs the same
    /// `standard_filters` code path.
    #[test]
    fn scan_honours_ignore_files() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), ".ignore", "target/\n");
        write(
            root.path(),
            "target/build.rs",
            "// see ADR-9999 should be ignored\n",
        );
        write(root.path(), "src/main.rs", "// see ADR-7777 should fire\n");
        let refs = scan(root.path(), &["**/*.rs".to_string()])
            .unwrap()
            .references;
        let tokens: Vec<&str> = refs.iter().map(|r| r.token.as_str()).collect();
        assert!(!tokens.contains(&"ADR-9999"), "target/ must be ignored");
        assert!(tokens.contains(&"ADR-7777"));
    }

    #[test]
    fn scan_with_no_globs_returns_empty() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "src/main.rs", "// ADR-001 here\n");
        let report = scan(root.path(), &[]).unwrap();
        assert!(report.references.is_empty());
        assert_eq!(report.walker_errors, 0);
        assert_eq!(report.searcher_errors, 0);
    }

    #[test]
    fn scan_skips_files_outside_glob() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "src/included.rs", "// ADR-111 fires\n");
        write(
            root.path(),
            "src/excluded.py",
            "# ADR-222 should not fire\n",
        );
        let refs = scan(root.path(), &["**/*.rs".to_string()])
            .unwrap()
            .references;
        let tokens: Vec<&str> = refs.iter().map(|r| r.token.as_str()).collect();
        assert!(tokens.contains(&"ADR-111"));
        assert!(!tokens.contains(&"ADR-222"));
    }

    /// REF-007 chained-suppression: when `ignore-next` lives on a
    /// matched line, the FOLLOWING matched line must still see it.
    /// grep-searcher does not re-emit a matched line as before-context
    /// for an adjacent match, so this case can only pass if `Sink::
    /// matched` updates `prev_line` after handling each match.
    #[test]
    fn scan_honours_ignore_next_chained_through_adjacent_match() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "src/x.rs",
            "const a = \"ADR-1111\"; // ctxgrd: ignore-next\nconst b = \"ADR-2222\";\n",
        );
        let refs = scan(root.path(), &["**/*.rs".to_string()])
            .unwrap()
            .references;
        let tokens: Vec<&str> = refs.iter().map(|r| r.token.as_str()).collect();
        assert!(
            tokens.contains(&"ADR-1111"),
            "the carrier line itself must still fire (ignore-next ≠ ignore-line)"
        );
        assert!(
            !tokens.contains(&"ADR-2222"),
            "ignore-next on a matched line must propagate to the adjacent match"
        );
    }

    #[test]
    fn scan_dedupes_repeated_tokens_at_same_position() {
        // Single mention at one position → one reference, not many.
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "src/x.rs", "// ADR-001 ADR-001\n");
        let refs = scan(root.path(), &["**/*.rs".to_string()])
            .unwrap()
            .references;
        // Two distinct columns → two references (this is correct;
        // each mention is its own pointer).
        assert_eq!(refs.len(), 2);
        assert!(refs[0].col < refs[1].col);
    }

    /// REF-007 line-1 edge: a match on the very first line of a file
    /// cannot be suppressed by `ignore-next` (there is no line zero).
    /// `prev` is `None` when `matched` fires, so `is_some_and` short-
    /// circuits to `false` and the token fires. Pins the boundary.
    #[test]
    fn scan_ignore_next_does_not_apply_at_line_one() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "src/first.rs",
            "const a = \"ADR-1\"; // very first line, no preceding line\n",
        );
        let refs = scan(root.path(), &["**/*.rs".to_string()])
            .unwrap()
            .references;
        let tokens: Vec<&str> = refs.iter().map(|r| r.token.as_str()).collect();
        assert!(
            tokens.contains(&"ADR-1"),
            "match at line 1 must fire — no preceding line could carry ignore-next"
        );
    }

    /// REF-007: `ignore-line` suppresses ALL tokens on the carrier
    /// line, not just the first. The match-line check wraps the
    /// whole regex iterator, so multiple tokens on the same line
    /// share the suppression decision.
    #[test]
    fn scan_ignore_line_suppresses_every_token_on_the_line() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "src/multi.rs",
            "// see ADR-1 and ADR-2 ctxgrd: ignore-line\nconst c = \"ADR-3\";\n",
        );
        let refs = scan(root.path(), &["**/*.rs".to_string()])
            .unwrap()
            .references;
        let tokens: Vec<&str> = refs.iter().map(|r| r.token.as_str()).collect();
        assert!(!tokens.contains(&"ADR-1"), "first token on suppressed line");
        assert!(
            !tokens.contains(&"ADR-2"),
            "second token on suppressed line"
        );
        assert!(tokens.contains(&"ADR-3"), "next line still fires");
    }

    /// Output determinism: the parallel walker uses thread-local
    /// accumulators, but the post-walk sort + dedup yields identical
    /// output across invocations. Pins the contract that
    /// `ctxgrd refs <ID>` and `core.cross-ref` diagnostics depend on.
    #[test]
    fn scan_output_is_deterministic_across_invocations() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..20 {
            write(
                root.path(),
                &format!("src/f{i}.rs"),
                &format!("// see ADR-{i}\n"),
            );
        }
        let a = scan(root.path(), &["**/*.rs".to_string()])
            .unwrap()
            .references;
        let b = scan(root.path(), &["**/*.rs".to_string()])
            .unwrap()
            .references;
        assert_eq!(a, b, "two independent scans must produce identical output");
    }
}
