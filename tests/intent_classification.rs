//! Integration tests for ADR-007 § DOC-001 + DOC-003 + DOC-006:
//! intent-based document classification. The fixture mirrors the
//! shapes that motivated the ADR — a Hugo-style README and a
//! project-root `DESIGN.md` with non-ctxgrd frontmatter — both at
//! repo root with frontmatter but no `id`. Under intent-based
//! classification both must produce zero diagnostics under the
//! default `init`-generated config, without any `[ignore]` workaround.

use std::fs;
use std::path::Path;

use assert_cmd::Command;

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

/// DOC-006 (a) verification: the `[ignore]` defaults emitted by
/// `ctxgrd init` MUST NOT include `**/README.md` or `**/CHANGELOG.md`.
/// Their pre-DOC-001 reason for existing — silencing over-fire on
/// frontmatter-bearing READMEs — is gone under intent-based
/// classification.
#[test]
fn init_default_ignore_omits_readme_and_changelog() {
    let tmp = fixture_into_tempdir("intent-classification");

    let output = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", tmp.path().to_str().unwrap(), "init"])
        .output()
        .expect("ctxgrd init runs");
    assert!(
        output.status.success(),
        "init must succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let toml = fs::read_to_string(tmp.path().join("ctxgrd.toml")).expect("ctxgrd.toml written");
    assert!(
        !toml.contains("**/README.md"),
        "DEFAULT_IGNORE_PATTERNS must not contain `**/README.md` post-DOC-006; got:\n{toml}"
    );
    assert!(
        !toml.contains("**/CHANGELOG.md"),
        "DEFAULT_IGNORE_PATTERNS must not contain `**/CHANGELOG.md` post-DOC-006; got:\n{toml}"
    );
}

/// DOC-001 + DOC-003 + DOC-006 (b) verification: the canonical
/// motivating shapes — a Hugo-style README and a project-root
/// DESIGN.md with non-ctxgrd frontmatter — produce zero diagnostics
/// under the default init config. Neither is path-claimed (no
/// `[<NS>].paths` covers the repo root) and neither has an `id`,
/// so both are silently skipped at parse time.
#[test]
fn hugo_readme_and_design_md_silent_under_default_init() {
    let tmp = fixture_into_tempdir("intent-classification");

    // Init writes ctxgrd.toml using the post-DOC-006 defaults.
    let init = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", tmp.path().to_str().unwrap(), "init"])
        .output()
        .expect("ctxgrd init runs");
    assert!(init.status.success());

    // Run the linter against the fixture.
    let lint = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", tmp.path().to_str().unwrap()])
        .output()
        .expect("ctxgrd lint runs");

    let stdout = String::from_utf8(lint.stdout).expect("stdout utf-8");
    let stderr = String::from_utf8(lint.stderr).expect("stderr utf-8");

    assert_eq!(
        lint.status.code(),
        Some(0),
        "expected clean exit; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("core.frontmatter") && !stdout.contains("core.id"),
        "no parse-level diagnostics for unclaimed files; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("README.md") && !stdout.contains("DESIGN.md"),
        "README.md and DESIGN.md must not appear in any diagnostic; stdout:\n{stdout}"
    );
}
