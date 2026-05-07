//! Integration tests for `ctxgrd init`'s two stderr surfaces:
//! ADR 006 § EXT-003 (body-header advisory) and ADR 007 § DOC-005
//! (paths pre-fill announcement). The two share one stderr buffer
//! by design — positive output above the advisory.
//!
//! Each test copies a read-only fixture from `tests/fixtures/` into a
//! tempdir, runs the binary against the tempdir, and inspects the
//! resulting `ctxgrd.toml` plus stderr.

use std::fs;
use std::path::Path;

use assert_cmd::Command;

/// Recursively copy `src` into `dst`. The fixtures only contain
/// directories and small `.md` files, so a hand-rolled copy keeps
/// the test free of extra dev-dependencies.
fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

fn fixture_into_tempdir(name: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    copy_dir_recursive(&src, tmp.path());
    tmp
}

#[test]
fn init_against_body_header_fixture_writes_toml_and_emits_advisory() {
    let tmp = fixture_into_tempdir("init-body-headers");

    let output = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", tmp.path().to_str().unwrap(), "init"])
        .output()
        .expect("ctxgrd executes");

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");

    // (a) init exits 0 and writes ctxgrd.toml.
    assert_eq!(
        output.status.code(),
        Some(0),
        "init must exit 0 even when advisory fires; stdout={stdout} stderr={stderr}"
    );
    assert!(
        tmp.path().join("ctxgrd.toml").is_file(),
        "ctxgrd.toml written"
    );

    // (b) advisory naming docs/adr/ + the body-header filename appears
    //     on stderr; it explains the migration shape without depending
    //     on an internal-doc link, and names the [ignore].patterns
    //     escape hatch.
    assert!(
        stderr.contains("docs/adr/"),
        "advisory names directory; stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("0001-record-architecture-decisions.md"),
        "advisory names the body-header file; stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("YAML") && stderr.contains("frontmatter") && stderr.contains("To migrate"),
        "advisory explains the migration shape; stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("[ignore].patterns"),
        "advisory names the escape hatch; stderr was:\n{stderr}"
    );

    // (c) the frontmatter ADR alone does NOT trigger the advisory —
    //     only the body-header file is listed.
    assert!(
        !stderr.contains("0002-already-frontmatter.md"),
        "frontmatter ADR must not appear in advisory; stderr was:\n{stderr}"
    );

    // ADR 007 § DOC-005: the same fixture's docs/adr/ also drives
    // the paths pre-fill — the generated ctxgrd.toml must declare
    // [ADR].paths and stderr must announce it.
    let toml_text = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();
    assert!(
        toml_text.contains(r#"paths = ["docs/adr/**"]"#),
        "[ADR].paths pre-filled; ctxgrd.toml was:\n{toml_text}"
    );
    assert!(
        stderr.contains("Pre-filled [ADR].paths from detected docs/adr/."),
        "DOC-005 announcement on stderr; stderr was:\n{stderr}"
    );
}

#[test]
fn init_against_clean_fixture_emits_announcement_but_no_advisory() {
    let tmp = fixture_into_tempdir("init-frontmatter-only");

    let output = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", tmp.path().to_str().unwrap(), "init"])
        .output()
        .expect("ctxgrd executes");

    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");

    assert_eq!(output.status.code(), Some(0));
    assert!(tmp.path().join("ctxgrd.toml").is_file());

    // No body-header files → advisory is silent.
    assert!(
        !stderr.contains("ADR-shaped directories without YAML frontmatter"),
        "clean fixture must not trigger advisory; stderr was:\n{stderr}"
    );
    // But docs/adrs/ is still detected → DOC-005 still announces.
    assert!(
        stderr.contains("Pre-filled [ADR].paths from detected docs/adrs/."),
        "DOC-005 announcement still fires on a clean tree; stderr was:\n{stderr}"
    );
    let toml_text = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();
    assert!(
        toml_text.contains(r#"paths = ["docs/adrs/**"]"#),
        "[ADR].paths pre-filled; ctxgrd.toml was:\n{toml_text}"
    );
}

#[test]
fn init_pre_fills_paths_for_multiple_dirs_in_same_namespace() {
    // ADR 007 § DOC-005: "If multiple directories for the same
    // namespace are detected (e.g., `docs/adr/` and `docs/adrs/`),
    // emit them as a list."
    let tmp = fixture_into_tempdir("init-multi-dir-adr");

    let output = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", tmp.path().to_str().unwrap(), "init"])
        .output()
        .expect("ctxgrd executes");

    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");
    assert_eq!(output.status.code(), Some(0));

    let toml_text = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();
    assert!(
        toml_text.contains(r#"paths = ["docs/adr/**", "docs/adrs/**"]"#),
        "both globs in sorted order; ctxgrd.toml was:\n{toml_text}"
    );
    assert!(
        stderr.contains("Pre-filled [ADR].paths from detected docs/adr/, docs/adrs/."),
        "announcement lists both dirs comma-separated; stderr was:\n{stderr}"
    );
}

#[test]
fn init_outputs_fire_even_when_toml_already_exists() {
    // ADR 006 § EXT-003 + ADR 007 § DOC-005: both stderr surfaces
    // must still reach the user when init refuses to write
    // ctxgrd.toml (e.g., file exists, no --force).
    let tmp = fixture_into_tempdir("init-body-headers");
    fs::write(tmp.path().join("ctxgrd.toml"), b"# pre-existing\n").unwrap();

    let output = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", tmp.path().to_str().unwrap(), "init"])
        .output()
        .expect("ctxgrd executes");

    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");

    assert_ne!(
        output.status.code(),
        Some(0),
        "init must refuse without --force"
    );
    assert!(
        stderr.contains("0001-record-architecture-decisions.md"),
        "advisory still fires on the failure path; stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("Pre-filled [ADR].paths from detected docs/adr/."),
        "announcement still fires on the failure path; stderr was:\n{stderr}"
    );
}
