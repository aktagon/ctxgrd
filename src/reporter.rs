//! Reporter. REP-001 in the brief.
//!
//! Three renderers, selected via `--format`:
//!
//! - [`render`] — the `simple` REP-001 line format:
//!   `<location>:<line>:<col>: <sev>: [<code>] <message>`
//! - [`render_rich`] — cargo-style multi-line output with source
//!   snippets, carets, and actionable `help:` / `note:` lines.
//!   Default for the CLI; optimised for LLM and human readers.
//!
//! JSON output lives on the `Diagnostic` struct's serde derive plus
//! [`crate::run::render_json_outcome`].
//!
//! Sort order is shared: `(location, line, col, code)`.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use crate::diagnostic::{Diagnostic, KernelMessage};

/// The run-level advisory printed whenever `ctxgrd lint` reports
/// diagnostics (ADR-038 § HINT-001). One canonical string, reused
/// verbatim by every output surface: the Rich and Simple text formats
/// emit it on stderr, and the JSON wire carries it in its `hint` field.
/// The message names the correct fix (edit the documents) over the
/// tempting wrong one (relax `ctxgrd.toml`).
pub const LINT_HINT: &str =
    "fix the documents the rules flag — their content, headings, IDs, or paths — \
     not `ctxgrd.toml`. Relaxing or removing a rule hides the problem instead of fixing it.";

/// Sort `diagnostics` in place using REP-001's key order.
pub fn sort(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|a, b| {
        (&a.location, a.line, a.col, &a.code).cmp(&(&b.location, b.line, b.col, &b.code))
    });
}

/// Render `diagnostics` as one `<location>:<line>:<col>: <sev>:
/// [<code>] <message>` line per entry — the grep-friendly format
/// the kernel brief documents as REP-001.
pub fn render(diagnostics: &[Diagnostic]) -> String {
    let mut out = String::new();
    for d in diagnostics {
        let _ = writeln!(
            out,
            "{}:{}:{}: {}: [{}] {}",
            d.location,
            d.line.unwrap_or(0),
            d.col.unwrap_or(0),
            d.severity.as_str(),
            d.code,
            d.message,
        );
    }
    out
}

/// Render `diagnostics` in cargo-style rich format:
///
/// ```text
/// error[core.cross-ref]: cross-reference 'ADR-042' does not resolve…
///   --> adrs/099-broken-demo.md:18:5
///    |
/// 18 | See ADR-042 for background — though ADR-042 does not exist…
///    |     ^^^^^^^
///    |
///   help: use an existing ID, wrap in backticks…
/// ```
///
/// Source snippets are read from `<root>/<location>` at render time
/// when `line > 0` and the file is readable. Diagnostics without a
/// line anchor render as a location-only block (no snippet). A
/// trailing summary line (`found: N error(s) · M warning(s)`) follows
/// the block when either channel carried something — paired in
/// register with the `ok:` summary the binary emits on success.
///
/// `kernel_messages` is taken for the tally only; the binary renders the
/// messages themselves ahead of this block via
/// [`render_kernel_message_rich`]. Both the count and the empty-input
/// early return read both channels (`BUG-039`). Gating either on
/// `diagnostics` alone produced a run that printed a kernel warning, no
/// `found:` line (this returned early), and no `ok:` line either
/// ([`ok_summary`] is suppressed by the same message per ADR-119
/// § CLM-004) — a finding on screen with nothing summarising it.
pub fn render_rich(
    diagnostics: &[Diagnostic],
    kernel_messages: &[KernelMessage],
    root: &Path,
) -> String {
    if diagnostics.is_empty() && kernel_messages.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (i, d) in diagnostics.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        render_rich_one(&mut out, d, root);
    }
    let (errors, warnings) = count_by_severity(diagnostics, kernel_messages);
    let _ = writeln!(
        out,
        "\nfound: {} · {}",
        plural(errors, "error"),
        plural(warnings, "warning"),
    );
    out
}

/// The success trailer (`ok: N documents · M rules · 0 diagnostics`),
/// or `None` when any diagnostic was emitted.
///
/// Warnings do not change the exit status, but they *are* diagnostics —
/// printing `ok: … 0 diagnostics` after a `found: … 1 warning` trailer
/// contradicts itself. Gating on `diagnostics.is_empty()` keeps the two
/// trailers mutually exclusive: a clean run shows `ok:`, any run with
/// diagnostics shows only the `found:` line from [`render_rich`].
///
/// `namespaces_undeclared` (ADR-076 § OWN-005) appends `· N namespaces
/// undeclared` when nonzero. Reachable on a clean run because
/// `[ignore].namespaces` silences the warning without zeroing the count:
/// `ok: … 0 diagnostics` alone reads as "every document ran every rule",
/// which is false whenever a namespace is linting under the six
/// zero-config rules. Omitted at zero so the common line stays quiet.
///
/// `kernel_messages` suppresses the trailer on the same terms as
/// `diagnostics` (ADR-119 § CLM-004). Kernel messages are a separate
/// channel — `src.runtime-error` and friends are not `Diagnostic`s — so
/// gating on diagnostics alone let `error[src.runtime-error]: …` print
/// directly above `ok: … 0 diagnostics`, which is the self-contradiction
/// the rest of this doc comment exists to prevent. Both channels or
/// neither.
pub fn ok_summary(
    documents: usize,
    rules: usize,
    namespaces_undeclared: usize,
    diagnostics: &[Diagnostic],
    kernel_messages: &[KernelMessage],
) -> Option<String> {
    if !diagnostics.is_empty() || !kernel_messages.is_empty() {
        return None;
    }
    let mut line = format!(
        "ok: {} · {} · 0 diagnostics",
        plural(documents, "document"),
        plural(rules, "rule"),
    );
    if namespaces_undeclared > 0 {
        let _ = write!(
            line,
            " · {} undeclared",
            plural(namespaces_undeclared, "namespace")
        );
    }
    Some(line)
}

/// `"1 error"` vs `"2 errors"` — used by trailers and the `ok:` summary
/// line alike so singular/plural stays consistent across surfaces.
pub(crate) fn plural(n: usize, singular: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {singular}s")
    }
}

/// Render a single diagnostic as a cargo-style block — header,
/// optional anchor, optional snippet, optional help/note — without
/// the aggregate trailer.
///
/// Used both inside [`render_rich`] for each rule diagnostic and
/// directly by kernel / config / IO error paths so every error
/// surface (rule, config, IO, usage) renders with the same shape.
pub fn render_error_block(d: &Diagnostic, root: &Path) -> String {
    let mut out = String::new();
    render_rich_one(&mut out, d, root);
    out
}

fn render_rich_one(out: &mut String, d: &Diagnostic, root: &Path) {
    // Header: severity[code]: message
    let _ = writeln!(
        out,
        "{sev}[{code}]: {msg}",
        sev = d.severity.as_str(),
        code = d.code,
        msg = d.message,
    );

    // Location anchor.
    if let Some(line) = d.line {
        let _ = writeln!(out, "  --> {}:{}:{}", d.location, line, d.col.unwrap_or(0));
    } else {
        let _ = writeln!(out, "  --> {}", d.location);
    }

    // Source snippet when we have a line anchor + readable file.
    let snippet = d.line.and_then(|line| source_line(root, &d.location, line));
    if let Some(line_text) = snippet {
        let line = d.line.unwrap_or(1);
        let line_label = format!("{:>width$}", line, width = line_number_width(line));
        let label_pad = " ".repeat(line_label.len());
        let _ = writeln!(out, "   {label_pad} |");
        let _ = writeln!(out, "   {line_label} | {line_text}");
        let caret_col = d.col.filter(|&c| c > 0).unwrap_or(1) as usize;
        // Column is 1-indexed; caret offset in the underline is col-1.
        let caret_offset = caret_col.saturating_sub(1);
        let caret_len = d.span_len.unwrap_or(1).max(1) as usize;
        let caret = "^".repeat(caret_len);
        let _ = writeln!(
            out,
            "   {label_pad} | {pad}{caret}",
            pad = " ".repeat(caret_offset),
        );
        let _ = writeln!(out, "   {label_pad} |");
    } else if d.help.is_some() || d.note.is_some() {
        // Visually separate the anchor line from help/note when we
        // don't have a snippet in between.
        out.push('\n');
    }

    // help: / note: suffix lines.
    if let Some(help) = &d.help {
        render_wrapped(out, "help", help);
    }
    if let Some(note) = &d.note {
        render_wrapped(out, "note", note);
    }
}

/// Render one [`KernelMessage`] in the rich shape: header line plus
/// optional `help:` / `note:` indented underneath. Mirrors the
/// non-anchor portion of [`render_error_block`] so the two diagnostic-
/// shaped types render with one common look.
pub fn render_kernel_message_rich(msg: &KernelMessage) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{sev}[{code}]: {message}",
        sev = msg.severity.as_str(),
        code = msg.code,
        message = msg.message,
    );
    if let Some(help) = &msg.help {
        render_wrapped(&mut out, "help", help);
    }
    if let Some(note) = &msg.note {
        render_wrapped(&mut out, "note", note);
    }
    out
}

/// Render one [`KernelMessage`] in the simple grep-friendly shape:
/// `<sev>: [<code>] <message>`. No help/note suffix — the simple
/// format is one line per record by contract.
pub fn render_kernel_message_simple(msg: &KernelMessage) -> String {
    format!(
        "{sev}: [{code}] {message}\n",
        sev = msg.severity.as_str(),
        code = msg.code,
        message = msg.message,
    )
}

/// Render a `help:` or `note:` label with a newline-aware body.
/// Multi-line help/note strings indent every continuation line so
/// block copy-paste stays correct.
fn render_wrapped(out: &mut String, label: &str, body: &str) {
    let mut lines = body.lines();
    let Some(first) = lines.next() else {
        return;
    };
    let _ = writeln!(out, "  {label}: {first}");
    let indent = " ".repeat(label.len() + 4); // "  " + label + ": "
    for line in lines {
        let _ = writeln!(out, "{indent}{line}");
    }
}

fn source_line(root: &Path, location: &str, line: u32) -> Option<String> {
    let path = root.join(location);
    let body = fs::read_to_string(&path).ok()?;
    body.lines()
        .nth((line as usize).saturating_sub(1))
        .map(String::from)
}

fn line_number_width(line: u32) -> usize {
    let mut n = line.max(1);
    let mut w = 0usize;
    while n > 0 {
        w += 1;
        n /= 10;
    }
    w.max(2)
}

/// The `found:` tally, summed across **both** report channels.
///
/// `BUG-039`: this counted `diagnostics` alone, so a kernel message the
/// binary had just printed above the trailer went untallied — `ok:` was
/// suppressed by it (ADR-119 § CLM-004) while `found:` denied it existed.
/// The two channels differ only in whether a document anchor is meaningful,
/// never in whether the finding counts.
fn count_by_severity(
    diagnostics: &[Diagnostic],
    kernel_messages: &[KernelMessage],
) -> (usize, usize) {
    use crate::diagnostic::Severity;
    let mut errors = 0usize;
    let mut warnings = 0usize;
    let severities = diagnostics
        .iter()
        .map(|d| d.severity)
        .chain(kernel_messages.iter().map(|m| m.severity));
    for severity in severities {
        match severity {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            // Info is advisory — it never escalates the exit code and is
            // not tallied into the `found:` error/warning trailer.
            Severity::Info => {}
        }
    }
    (errors, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(code: &str, location: &str, line: u32, col: u32, msg: &str) -> Diagnostic {
        // Routes through the normalising constructor: a `0` line/col
        // argument becomes `None` (ADR-086 § WIRE-004).
        Diagnostic::error(code, location, line, col, msg)
    }

    #[test]
    fn ok_summary_suppressed_when_warnings_present() {
        // A warning-only run must NOT print the `ok: … 0 diagnostics`
        // trailer — it would contradict the `found: … 1 warning` line.
        let w = Diagnostic::warning("agents.context-budget", "cli/CLAUDE.md", 0, 0, "x");
        assert_eq!(ok_summary(6, 9, 0, std::slice::from_ref(&w), &[]), None);
    }

    /// ADR-119 § CLM-004: a kernel message suppresses the trailer on the
    /// same terms as a diagnostic. The error case is the FEEDBACK-008
    /// transcript — `error[src.runtime-error]` above `ok: … 0
    /// diagnostics`; the warning case matters because it exits 0, so the
    /// trailer is the only thing that would carry the contradiction.
    #[test]
    fn ok_summary_suppressed_when_kernel_messages_present() {
        let e = KernelMessage::error("src.runtime-error", "source 'statute' exited with code 3");
        assert_eq!(ok_summary(16, 22, 0, &[], std::slice::from_ref(&e)), None);

        let w = KernelMessage::warning("src.too-few-documents", "source 'statute' emitted nothing");
        assert_eq!(ok_summary(16, 22, 0, &[], std::slice::from_ref(&w)), None);
    }

    #[test]
    fn ok_summary_present_when_clean() {
        assert_eq!(
            ok_summary(6, 9, 0, &[], &[]).as_deref(),
            Some("ok: 6 documents · 9 rules · 0 diagnostics"),
        );
    }

    /// ADR-076 § OWN-005: the coverage field is conditional on the human
    /// line — silent at zero, explicit when a namespace is linting under
    /// the six zero-config rules.
    #[test]
    fn ok_summary_reports_undeclared_namespaces() {
        assert_eq!(
            ok_summary(213, 116, 1, &[], &[]).as_deref(),
            Some("ok: 213 documents · 116 rules · 0 diagnostics · 1 namespace undeclared"),
        );
        assert_eq!(
            ok_summary(213, 116, 2, &[], &[]).as_deref(),
            Some("ok: 213 documents · 116 rules · 0 diagnostics · 2 namespaces undeclared"),
        );
    }

    #[test]
    fn render_matches_rep_001_format() {
        let d = diag("core.cross-ref", "adrs/a.md", 18, 5, "no such id");
        assert_eq!(
            render(&[d]),
            "adrs/a.md:18:5: error: [core.cross-ref] no such id\n"
        );
    }

    #[test]
    fn sort_by_location_then_line_then_col_then_code() {
        let mut v = vec![
            diag("core.cross-ref", "b.md", 2, 0, "z"),
            diag("core.dep-cycle", "a.md", 10, 0, "later"),
            diag("core.dep-resolved", "a.md", 5, 0, "earlier"),
            diag("core.cross-ref", "a.md", 5, 0, "same line, earlier code"),
        ];
        sort(&mut v);
        let rendered = render(&v);
        let expected = "\
a.md:5:0: error: [core.cross-ref] same line, earlier code
a.md:5:0: error: [core.dep-resolved] earlier
a.md:10:0: error: [core.dep-cycle] later
b.md:2:0: error: [core.cross-ref] z
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn empty_input_renders_empty_string() {
        assert_eq!(render(&[]), "");
        assert_eq!(render_rich(&[], &[], Path::new(".")), "");
    }

    #[test]
    fn rich_renders_line_only_header_without_snippet() {
        let d = diag(
            "core.required-headings",
            "adrs/x.md",
            0,
            0,
            "missing 'Decision'",
        )
        .with_help("add a `## Decision` section");
        let out = render_rich(&[d], &[], Path::new("."));
        assert!(out.contains("error[core.required-headings]"));
        assert!(out.contains("--> adrs/x.md\n"));
        assert!(out.contains("  help: add a `## Decision` section"));
        assert!(!out.contains(" --> adrs/x.md:"));
    }

    #[test]
    fn rich_renders_source_snippet_with_single_caret() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("x.md");
        fs::write(&file, "line 1\nline 2 with important content\nline 3\n").unwrap();
        let d = diag("core.dep-resolved", "x.md", 2, 7, "missing target");
        let out = render_rich(&[d], &[], tmp.path());
        assert!(out.contains("line 2 with important content"));
        // caret should appear 6 chars after the `|` (col 7 → offset 6).
        let caret_line = out
            .lines()
            .find(|l| l.contains('^'))
            .expect("caret line present");
        // Expect one `^`.
        assert_eq!(caret_line.matches('^').count(), 1);
    }

    #[test]
    fn rich_uses_span_len_for_caret_width() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("x.md");
        fs::write(&file, "See ADR-042 elsewhere\n").unwrap();
        let d = diag(
            "core.cross-ref",
            "x.md",
            1,
            5,
            "cross-reference 'ADR-042' …",
        )
        .with_span_len(7);
        let out = render_rich(&[d], &[], tmp.path());
        let caret_line = out.lines().find(|l| l.contains('^')).unwrap();
        assert_eq!(caret_line.matches('^').count(), 7);
    }

    #[test]
    fn rich_emits_trailer_with_counts() {
        let e = diag("core.id", "a.md", 0, 0, "bad id");
        let w = Diagnostic::warning("ext.malformed-output", "a.md", 0, 0, "x");
        let out = render_rich(&[e, w], &[], Path::new("."));
        assert!(
            out.contains("found: 1 error · 1 warning"),
            "expected normalised trailer, got:\n{out}"
        );
    }

    // --- BUG-039: the trailer counts both channels ---------------------

    #[test]
    fn rich_trailer_counts_kernel_messages() {
        // `found:` claims to be the run's tally. Kernel messages are
        // printed above it by the binary and were not counted, so a run
        // showing one kernel warning reported `0 warnings` directly
        // underneath it.
        let e = diag("core.id", "a.md", 0, 0, "bad id");
        let k = KernelMessage::warning("cfg.paths-skipped", "[NOTE] paths match 2 files");
        let out = render_rich(&[e], &[k], Path::new("."));
        assert!(
            out.contains("found: 1 error · 1 warning"),
            "the kernel warning is part of what was found; got:\n{out}"
        );
    }

    #[test]
    fn rich_emits_a_trailer_for_a_kernel_only_run() {
        // The larger hole behind `BUG-039`: `render_rich` bailed on
        // `diagnostics.is_empty()`, so a run whose only finding was a
        // kernel message printed no `found:` line at all — and
        // `ok_summary` is suppressed by the same message (ADR-119
        // § CLM-004), so the run ended with no trailer whatsoever.
        let k = KernelMessage::warning("cfg.paths-skipped", "[NOTE] paths match 2 files");
        let out = render_rich(&[], &[k], Path::new("."));
        assert!(
            out.contains("found: 0 errors · 1 warning"),
            "a kernel-only run must still say what it found; got:\n{out}"
        );
    }

    #[test]
    fn rich_kernel_errors_are_counted_as_errors() {
        let k = KernelMessage::error("src.runtime-error", "source 'jira' timed out");
        let out = render_rich(&[], &[k], Path::new("."));
        assert!(
            out.contains("found: 1 error · 0 warnings"),
            "got:\n{out}"
        );
    }

    #[test]
    fn rich_stays_empty_when_both_channels_are_empty() {
        // The paired half: the early return still exists, it is just
        // gated on both channels rather than one. A genuinely clean run
        // must print nothing here so the binary's `ok:` line stands alone.
        assert_eq!(render_rich(&[], &[], Path::new(".")), "");
    }

    #[test]
    fn rich_renders_note_when_set() {
        let d = diag("core.id", "a.md", 0, 0, "bad id")
            .with_help("add `id: <NS>-<n>`")
            .with_note("id must match `^[A-Z][A-Z0-9]*-\\d+$`");
        let out = render_rich(&[d], &[], Path::new("."));
        assert!(out.contains("  help: add `id: <NS>-<n>`"));
        assert!(out.contains("  note: id must match"));
    }

    #[test]
    fn rich_omits_help_and_note_when_unset() {
        let d = diag("core.id-unique", "a.md", 0, 0, "collides with b.md");
        let out = render_rich(&[d], &[], Path::new("."));
        assert!(!out.contains("help:"));
        assert!(!out.contains("note:"));
    }

    #[test]
    fn kernel_message_rich_emits_header_help_and_note() {
        let msg = KernelMessage::warning("ref.scan-error", "[references] scan failed: NotFound")
            .with_help("check the `[references].scan` globs in ctxgrd.toml")
            .with_note("the walker aborted before any file was searched");
        let out = render_kernel_message_rich(&msg);
        // Header in the same shape as Diagnostic.
        assert!(out.starts_with("warning[ref.scan-error]: [references] scan failed: NotFound"));
        // help: / note: indented two spaces, mirroring render_error_block.
        assert!(out.contains("\n  help: check the `[references].scan` globs"));
        assert!(out.contains("\n  note: the walker aborted before any file was searched"));
    }

    #[test]
    fn kernel_message_rich_omits_help_and_note_when_unset() {
        let msg = KernelMessage::error("src.runtime-error", "source 'jira' timed out");
        let out = render_kernel_message_rich(&msg);
        assert!(out.contains("error[src.runtime-error]: source 'jira' timed out"));
        assert!(!out.contains("help:"));
        assert!(!out.contains("note:"));
    }

    #[test]
    fn kernel_message_simple_is_one_line_per_record() {
        // Simple format ignores help/note by contract — one line per
        // record is what tooling pipelines expect.
        let msg = KernelMessage::warning("cfg.reserved-source", "[sources.markdown-file] ignored")
            .with_help("if you set this somehow it would not work");
        let out = render_kernel_message_simple(&msg);
        assert_eq!(
            out,
            "warning: [cfg.reserved-source] [sources.markdown-file] ignored\n"
        );
    }
}
