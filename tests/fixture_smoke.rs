//! Smoke test against the real golden fixture. Lets us eyeball line
//! numbers and token extraction without needing the full reporter.

use std::path::Path;

use assert_cmd::Command;
use ctxgrd::source::markdown::scan;

#[test]
fn adr_099_broken_demo_smoke() {
    let root = Path::new("examples");
    let result = scan(root, None, None).expect("scan succeeds");

    let doc = result
        .documents
        .iter()
        .find(|d| d.location.ends_with("ADR-099-broken-demo.md"))
        .expect("broken demo present in scan");

    assert_eq!(doc.id.namespace, "ADR");
    assert_eq!(doc.id.number, 99);
    assert_eq!(doc.depends_on, vec!["PRD-999"]);
    assert_eq!(doc.frontmatter_lines.get("depends_on"), Some(&5));

    let ast = doc.ast.as_ref().expect("ast populated");
    let tokens: Vec<_> = ast
        .cross_ref_tokens
        .iter()
        .map(|t| (t.token.clone(), t.line, t.in_strikethrough, t.in_code))
        .collect();
    println!("cross-ref tokens on ADR-099: {:?}", tokens);

    let adr_042 = ast.cross_ref_tokens.iter().find(|t| t.token == "ADR-042");
    assert!(adr_042.is_some(), "expected ADR-042 token in fixture");

    // The fixture wraps `~~ADR-404~~` in backticks, so pulldown-cmark sees it
    // as inline code (in_code), not a strikethrough region. Either flag is
    // sufficient for `core.cross-ref` to suppress the token — what matters is
    // that the suppression fires.
    let adr_404 = ast
        .cross_ref_tokens
        .iter()
        .find(|t| t.token == "ADR-404")
        .expect("ADR-404 present in fixture");
    assert!(
        adr_404.in_code || adr_404.in_strikethrough,
        "ADR-404 must be suppressed by in_code or in_strikethrough"
    );
}

/// End-to-end CLI run against the example fixture. Asserts:
/// - exit code 1 (lint failure, expected because the fixture is
///   intentionally broken);
/// - the canonical 8-error count (5 from ADR-099-broken-demo,
///   3 from the reference scanner against refs/);
/// - each scanner-emitted dangling-pointer diagnostic appears with
///   its expected `<file>:<line>:<col>` anchor (REF-001 attribution).
///
/// Run last among the integration tests because `assert_cmd` builds
/// the binary on demand, which can be slow on cold caches.
#[test]
fn cli_runs_against_examples_fixture() {
    let output = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", "examples"])
        .output()
        .expect("ctxgrd executes");

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");
    assert!(
        stderr.is_empty() || !stderr.contains("panic"),
        "no panic on stderr: {stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "exit 1 for lint failure; stdout was:\n{stdout}\nstderr was:\n{stderr}"
    );

    assert!(
        stdout.contains("found: 8 errors · 0 warnings"),
        "expected 8 errors total; got:\n{stdout}"
    );

    // Reference scanner diagnostics — one per dangling pointer in
    // refs/, anchored at the exact (file, line, col) the scanner
    // emitted (ADR-001 § REF-001 attribution).
    for marker in [
        "refs/Cargo.toml:12:15",
        "refs/lib.rs:10:38",
        "refs/main.go:10:22",
    ] {
        assert!(
            stdout.contains(marker),
            "expected scanner diagnostic at {marker}; got:\n{stdout}"
        );
    }

    // ADR-1234 in refs/main.go is suppressed by `ctxgrd: ignore-next`
    // (REF-007). It must NOT appear anywhere in the output.
    assert!(
        !stdout.contains("ADR-1234"),
        "ADR-1234 should be suppressed by ignore-next marker; got:\n{stdout}"
    );

    // PRD-001, PMR-001, ADR-001 are real documents; their resolved
    // mentions in refs/ must not produce diagnostics.
    for resolved in ["'PRD-001'", "'PMR-001'", "'ADR-001'"] {
        let bad = format!("cross-reference {resolved}");
        assert!(
            !stdout.contains(&bad),
            "resolved reference must not fire core.cross-ref: {resolved}"
        );
    }
}

/// REF-008 verification: `ctxgrd refs <ID>` enumerates every pointer
/// to a document — the document itself, depends_on edges, body
/// cross-refs, and reference-scanner hits.
#[test]
fn cli_refs_subcommand_finds_all_pointer_kinds_for_adr_001() {
    let output = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", "examples", "refs", "ADR-001"])
        .output()
        .expect("ctxgrd executes");

    assert_eq!(output.status.code(), Some(0), "exit 0 for refs subcommand");
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout utf-8");

    // Self: the document itself when file-backed.
    assert!(
        stdout.contains("adrs/ADR-001-use-event-sourcing-for-audit.md:0:0: (self)"),
        "expected SelfDoc hit; got:\n{stdout}"
    );
    // Body cross-ref: ADR-001 mentions itself in its body somewhere.
    assert!(
        stdout.contains("(body ref from ADR-001)"),
        "expected BodyCrossRef hit from ADR-001; got:\n{stdout}"
    );
    // Depends_on: PMR-001's frontmatter lists ADR-001.
    assert!(
        stdout.contains("(depends_on from PMR-001)"),
        "expected DependsOn hit from PMR-001; got:\n{stdout}"
    );
    // Scanner: refs/main.go and refs/lib.rs both have a literal "ADR-001".
    assert!(
        stdout.contains("refs/main.go") && stdout.contains("(scanner)"),
        "expected scanner hit in refs/main.go; got:\n{stdout}"
    );

    // Output is grouped by file then sorted numerically by (line,
    // col) — a second invocation MUST yield byte-identical output.
    let again = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", "examples", "refs", "ADR-001"])
        .output()
        .expect("ctxgrd executes");
    assert_eq!(
        again.stdout, output.stdout,
        "refs output must be deterministic across runs"
    );
}

/// REF-008 JSON format check.
#[test]
fn cli_refs_subcommand_emits_valid_json() {
    let output = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", "examples", "refs", "ADR-001", "--format", "json"])
        .output()
        .expect("ctxgrd executes");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    // Each hit is a single-line JSON object inside the array.
    assert!(stdout.starts_with('['));
    assert!(stdout.trim_end().ends_with(']'));
    assert!(stdout.contains(r#""kind":"self""#));
    assert!(stdout.contains(r#""kind":"depends_on""#));
    assert!(stdout.contains(r#""kind":"body_cross_ref""#));
    assert!(stdout.contains(r#""kind":"scanner_hit""#));
}
