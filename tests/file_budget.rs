//! `core.file-budget` (ADR-109) end to end, across both halves of its
//! dual dispatch (§ BDG-003): the `Level::File` path that lints id-less
//! path-claimed singletons like `TODO.md`, and the `run.rs` step-6 path
//! that lints id-keyed documents like an ADR.
//!
//! The pair matters because one rule code backed by two implementations
//! is exactly the defect BUG-021 records. These tests pin that both
//! paths reach the same function: same message shape, same
//! largest-section suggestion, and — critically — one finding per file,
//! never two.

use std::fs;
use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

fn run(root: &Path) -> Output {
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap()])
        .output()
        .expect("ctxgrd executes")
}

/// A document with a small `## Now` section and a large `## Shipped`
/// archive: `pad` lines of history, so the largest-section suggestion has
/// one right answer.
fn body(front: &str, pad: usize) -> String {
    let mut out = String::from(front);
    out.push_str("\n## Now\n\nFinish the budget rule.\n\n## Shipped\n\n");
    for n in 0..pad {
        out.push_str(&format!("- 0.{n}.0 shipped a rule\n"));
    }
    out
}

#[test]
fn path_claimed_singleton_over_budget_warns_once_and_still_exits_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(
        tmp.path().join("ctxgrd.toml"),
        r#"
[TODO]
paths = ["TODO.md"]
rules = ["core.file-budget"]

[TODO."core.file-budget"]
max_chars = 400
"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("TODO.md"),
        body("# Project state\n", 60),
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "an over-budget file is a warning, never a build failure:\n{stdout}"
    );
    assert_eq!(
        stdout.matches("warning[core.file-budget]").count(),
        1,
        "exactly one finding for one file on disk:\n{stdout}"
    );
    assert!(
        stdout.contains("(budget 400)"),
        "the message reports the configured budget:\n{stdout}"
    );
    assert!(
        stdout.contains("`## Shipped`"),
        "the help names the largest section as the thing to move:\n{stdout}"
    );
}

#[test]
fn id_keyed_document_over_budget_warns_once_on_the_document_path() {
    // BDG-003: the rule is registered `Level::File`, so without the step-6
    // arm an id-keyed namespace would list it and get nothing.
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join("docs/adrs")).unwrap();
    fs::write(
        tmp.path().join("ctxgrd.toml"),
        r#"
[ADR]
paths = ["docs/adrs/**"]
rules = ["core.frontmatter", "core.id", "core.file-budget"]

[ADR."core.file-budget"]
max_chars = 400
"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("docs/adrs/001-long.md"),
        body(
            "---\nid: ADR-1\ntitle: A very long decision\n---\n\n# ADR-1\n",
            60,
        ),
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "warning only:\n{stdout}");
    assert_eq!(
        stdout.matches("warning[core.file-budget]").count(),
        1,
        "one finding — the id pipeline and the file-level scan must not \
         both lint this document (BUG-021):\n{stdout}"
    );
    assert!(
        stdout.contains("docs/adrs/001-long.md"),
        "the finding is anchored on the document:\n{stdout}"
    );
}

#[test]
fn a_bare_binding_runs_on_the_default_budget() {
    // BDG-002: no `[NS."core.file-budget"]` table at all. The rule must
    // still run — a params-gated dispatch would silently skip it — and a
    // document under 150000 characters must stay clean.
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join("docs/adrs")).unwrap();
    fs::write(
        tmp.path().join("ctxgrd.toml"),
        r#"
[ADR]
paths = ["docs/adrs/**"]
rules = ["core.frontmatter", "core.id", "core.file-budget"]
"#,
    )
    .unwrap();
    let short = tmp.path().join("docs/adrs/001-short.md");
    fs::write(&short, "---\nid: ADR-1\ntitle: Short\n---\n\n# ADR-1\n").unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "clean run:\n{stdout}");
    assert!(
        !stdout.contains("core.file-budget"),
        "a short document is under the 150000-character default:\n{stdout}"
    );

    fs::write(
        &short,
        format!("---\nid: ADR-1\ntitle: Short\n---\n\n# ADR-1\n\n{}", "x".repeat(150_000)),
    )
    .unwrap();
    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("core.file-budget") && stdout.contains("(budget 150000)"),
        "past the default the bare binding fires:\n{stdout}"
    );
}
