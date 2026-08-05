//! BUG-025 regression guards for the scanner side of `core.cross-ref`:
//! the per-namespace `rules` lists gate the rule code on both sides.
//! Configuring `[references].scan` while no namespace enables
//! `core.cross-ref` produces no scanner diagnostics.

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

fn write_fixture(root: &Path, adr_rules: &str) {
    fs::create_dir_all(root.join("docs/adrs")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("ctxgrd.toml"),
        format!(
            "[ADR]\npaths = [\"docs/adrs/**\"]\nrules = [{adr_rules}]\n\n[references]\nscan = [\"src/**/*.rs\"]\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join("docs/adrs/001-a.md"),
        "---\nid: ADR-1\ntitle: T\n---\n## Status\n",
    )
    .unwrap();
    fs::write(
        root.join("src/main.rs"),
        "// see ADR-999 and again ADR-999\n// and once more ADR-999\n",
    )
    .unwrap();
}

#[test]
fn scanner_silent_when_no_namespace_enables_cross_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_fixture(tmp.path(), "\"core.frontmatter\", \"core.id\"");

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "no namespace enables core.cross-ref — the scanner side must \
         not fire either (BUG-025):\n{stdout}"
    );
    assert!(
        !stdout.contains("core.cross-ref"),
        "no scanner cross-ref diagnostics expected:\n{stdout}"
    );
}

#[test]
fn scanner_fires_once_per_file_and_target_when_enabled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_fixture(
        tmp.path(),
        "\"core.frontmatter\", \"core.id\", \"core.cross-ref\"",
    );

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(1), "stale ID must fail\n{stdout}");
    let count = stdout.matches("error[core.cross-ref]").count();
    assert_eq!(
        count, 1,
        "three mentions of one stale ID in one file dedupe to one \
         diagnostic (BUG-025):\n{stdout}"
    );
    assert!(
        stdout.contains("3 mentions"),
        "the note must carry the mention count:\n{stdout}"
    );
}
