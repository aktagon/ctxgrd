//! Integration tests for `research.evidence` (ADR-093 § RSR-002/RSR-005) on
//! the id-less, path-claimed `RESEARCH` namespace (`docs/research/**`).
//!
//! Reports never become id-keyed documents, so the rule runs via the
//! file-level pass — the same reason the guide/c4/checklist frontmatter rules
//! are `Level::File`. Each scenario is a self-contained fixture root so the
//! exit-code and per-severity assertions stay isolated.

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;

fn fixture(scenario: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/research-evidence")
        .join(scenario)
}

fn run(root: &Path) -> Output {
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap()])
        .output()
        .expect("ctxgrd executes")
}

#[test]
fn well_formed_untyped_report_lints_clean() {
    let out = run(&fixture("clean"));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a report with an Evidence appendix and a Limitations section is clean\n{stdout}"
    );
    assert!(
        !stdout.contains("research.evidence"),
        "no research.evidence diagnostic on a well-formed report:\n{stdout}"
    );
}

#[test]
fn conclusion_only_report_fires_evidence_error_and_gaps_warning() {
    let out = run(&fixture("incomplete"));
    let stdout = String::from_utf8(out.stdout).unwrap();

    assert_eq!(
        out.status.code(),
        Some(1),
        "the evidence half is an error, so the run fails\n{stdout}"
    );
    // Exactly one error (evidence) + one warning (gaps).
    assert!(
        stdout.contains("found: 1 error · 1 warning"),
        "expected one evidence error and one gaps warning:\n{stdout}"
    );
    assert!(
        stdout.contains("error[research.evidence]: research report has no evidence/sources section"),
        "missing evidence section is an error:\n{stdout}"
    );
    assert!(
        stdout
            .contains("warning[research.evidence]: research report has no limitations/data-gaps section"),
        "missing data-gaps section is a warning:\n{stdout}"
    );
    // On-fire discovery note (RSR-005 hook 2): the report is untyped and the
    // rule fired, so it advertises the optional `research.type` field.
    assert!(
        stdout.contains("Optionally set `research.type: academic|market|deep-research`"),
        "an untyped firing report advertises research.type:\n{stdout}"
    );
}

#[test]
fn typed_academic_report_warns_on_missing_methods_skeleton() {
    let out = run(&fixture("typed"));
    let stdout = String::from_utf8(out.stdout).unwrap();

    // Baseline is satisfied (References + Limitations present) and Results
    // satisfies half the IMRaD skeleton; only the missing Methods heading
    // warns — a warning, so exit stays 0 (monotonic: the type only adds).
    assert_eq!(
        out.status.code(),
        Some(0),
        "a missing skeleton heading is a warning, not an error\n{stdout}"
    );
    assert!(
        stdout.contains("found: 0 errors · 1 warning"),
        "exactly one skeleton warning, no baseline errors:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "warning[research.evidence]: `research.type: academic` report is missing a `method` skeleton section"
        ),
        "the academic skeleton requires a method section:\n{stdout}"
    );
    // The type is set, so the discovery note is NOT attached.
    assert!(
        !stdout.contains("Optionally set `research.type"),
        "a typed report does not carry the discovery note:\n{stdout}"
    );
}

#[test]
fn invalid_research_type_value_errors_as_unknown_genre() {
    let out = run(&fixture("invalid-type"));
    let stdout = String::from_utf8(out.stdout).unwrap();

    assert_eq!(
        out.status.code(),
        Some(1),
        "an out-of-vocabulary research.type is an error\n{stdout}"
    );
    // Baseline is satisfied (Evidence and sources + Limitations), so the only
    // diagnostic is the invalid-genre error.
    assert!(
        stdout.contains("found: 1 error · 0 warnings"),
        "only the invalid-genre error fires:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "error[research.evidence]: `research.type: analysis` is not a valid research genre"
        ),
        "analysis is not in the academic/market/deep-research vocabulary:\n{stdout}"
    );
}
