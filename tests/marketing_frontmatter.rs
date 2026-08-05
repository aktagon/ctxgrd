//! Integration tests for `marketing.frontmatter` (ADR-100 § MKT-002) on the
//! id-less, path-claimed marketing namespaces (CAMPAIGN/PERSONA/POSITIONING/ICP).
//!
//! The rule is a monotonic opt-in (the `research.type` shape): a doc MAY declare
//! a nested `marketing.type`, and only a value outside the pack's `types`
//! allowlist is an error. An absent/frontmatter-less doc adds no finding — which
//! is what lets the frontmatter-less CAMPAIGN placeholder bind the rule. Each
//! scenario is a self-contained fixture root so exit-code assertions stay
//! isolated.

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;

fn fixture(scenario: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/marketing-frontmatter")
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
fn valid_typed_persona_lints_clean() {
    let out = run(&fixture("clean-typed"));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a persona with a valid marketing.type and all five rings is clean\n{stdout}"
    );
    assert!(
        !stdout.contains("marketing.frontmatter"),
        "no marketing.frontmatter diagnostic on a valid typed doc:\n{stdout}"
    );
}

#[test]
fn frontmatter_less_campaign_adds_no_finding() {
    let out = run(&fixture("no-frontmatter"));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a frontmatter-less brief with Overview + Success metrics is clean (monotonic no-op)\n{stdout}"
    );
    assert!(
        !stdout.contains("marketing.frontmatter"),
        "the monotonic rule adds nothing when no marketing.type is declared:\n{stdout}"
    );
}

#[test]
fn off_allowlist_type_fires_one_error() {
    let out = run(&fixture("invalid-type"));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "an off-allowlist marketing.type is an error\n{stdout}"
    );
    assert!(
        stdout.contains("found: 1 error"),
        "exactly one error — the required headings are all present:\n{stdout}"
    );
    assert!(
        stdout.contains("error[marketing.frontmatter]")
            && stdout.contains("is not one of the allowed types"),
        "the off-allowlist type is reported by marketing.frontmatter:\n{stdout}"
    );
}
