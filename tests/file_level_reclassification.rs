//! BUG-021 regression guards: listing the generic `Level::File` rules
//! (`core.required-headings` / `core.required-anchors`) in an id-keyed
//! namespace must NOT reclassify it as file-level. The reclassification
//! caused three defects, each pinned here:
//!
//! 1. the ADR-020 § ACX-003 parse-diagnostic suppression swallowed
//!    `core.frontmatter` / `core.id` for path-claimed id-keyed files
//!    (a BUG-001 regression);
//! 2. id-keyed documents were linted twice — once by the id pipeline,
//!    once by `scan_file_level` — yielding duplicate diagnostics with
//!    divergent matching semantics;
//! 3. `documents_linted` counted such files in both corpora.

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

const ADR_CONFIG: &str = r#"
[ADR]
# ADR-076 § OWN-003: an owner keeps the always-on cfg.namespace-unowned
# gate quiet, so the `ok:` summary this suite asserts on is emitted.
owner = "developer"
paths = ["docs/adrs/**"]
rules = ["core.frontmatter", "core.id", "core.required-headings"]

[ADR."core.required-headings"]
headings = ["Status", "Context"]
"#;

#[test]
fn fenceless_file_in_id_keyed_namespace_still_fires_frontmatter() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join("docs/adrs")).unwrap();
    fs::write(tmp.path().join("ctxgrd.toml"), ADR_CONFIG).unwrap();
    fs::write(
        tmp.path().join("docs/adrs/002-nofm.md"),
        "# no frontmatter at all\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(1), "lint failure expected\n{stdout}");
    assert!(
        stdout.contains("core.frontmatter"),
        "a fence-less file under an id-keyed path claim must fire \
         core.frontmatter (BUG-021 defect 1 / BUG-001 regression):\n{stdout}"
    );
}

#[test]
fn missing_heading_in_id_keyed_document_reported_exactly_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join("docs/adrs")).unwrap();
    fs::write(tmp.path().join("ctxgrd.toml"), ADR_CONFIG).unwrap();
    fs::write(
        tmp.path().join("docs/adrs/001-plain.md"),
        "---\nid: ADR-1\ntitle: Plain\n---\n## Status\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(1), "lint failure expected\n{stdout}");
    let count = stdout.matches("error[core.required-headings]").count();
    assert_eq!(
        count, 1,
        "one missing heading must yield exactly one diagnostic, not one \
         per dispatch path (BUG-021 defect 2):\n{stdout}"
    );
}

#[test]
fn id_keyed_document_counted_once_in_summary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join("docs/adrs")).unwrap();
    fs::write(tmp.path().join("ctxgrd.toml"), ADR_CONFIG).unwrap();
    fs::write(
        tmp.path().join("docs/adrs/001-plain.md"),
        "---\nid: ADR-1\ntitle: Plain\n---\n## Status\n\nx\n\n## Context\n\nx\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    // The `ok:` trailer prints on stderr, keeping stdout a pure
    // diagnostic stream.
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(0), "clean run expected\n{stdout}");
    assert!(
        stderr.contains("ok: 1 document ·"),
        "one file on disk must be counted once (BUG-021 defect 3):\n{stderr}"
    );
}

#[test]
fn required_headings_normalized_match_in_id_pipeline() {
    // The semantics unification half of BUG-021: the id pipeline must use
    // the same normalized matching (enumerator-stripped, case-insensitive,
    // trailing-colon-dropped) that ADR-078 documents and the file-level
    // pass already implements.
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join("docs/adrs")).unwrap();
    fs::write(tmp.path().join("ctxgrd.toml"), ADR_CONFIG).unwrap();
    fs::write(
        tmp.path().join("docs/adrs/003-numbered.md"),
        "---\nid: ADR-3\ntitle: Numbered\n---\n## 1. Status\n\nx\n\n## CONTEXT:\n\nx\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "enumerated / case-variant headings must satisfy the requirement \
         under normalized matching:\n{stdout}"
    );
}
