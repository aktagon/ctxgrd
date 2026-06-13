//! Integration tests for the structural rules on path-claimed id-less
//! singletons: DESIGN.md (`design.section-order`, `design.token-ref`),
//! STYLE.md (`style.section-order`, `style.soul-pair`), and SOUL.md
//! (`soul.sections`).
//!
//! These files never become id-keyed documents, so the rules must run via
//! the file-level pass. Exercising them through the real binary is the
//! regression guard for BUG-007: `design.section-order` shipped registered
//! `Level::Document`, where it never fired on a real DESIGN.md and the
//! per-file unit tests (hand-built `Document`s) could not catch it.

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

const DESIGN_CONFIG: &str = r#"
[DESIGN]
paths = ["DESIGN.md"]
rules = ["core.frontmatter", "design.section-order", "design.token-ref"]
"#;

const STYLE_CONFIG: &str = r#"
[STYLE]
paths = ["STYLE.md"]
rules = ["core.frontmatter", "style.section-order", "style.soul-pair"]
"#;

const SOUL_CONFIG: &str = r#"
[SOUL]
paths = ["SOUL.md", "soul/SOUL.md"]
rules = ["core.frontmatter", "soul.sections"]
"#;

#[test]
fn design_section_order_and_token_ref_fire_on_real_design_md() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), DESIGN_CONFIG).unwrap();
    // Colors-before-Overview is out of canonical order; {colors.missing}
    // resolves to nothing.
    fs::write(
        tmp.path().join("DESIGN.md"),
        "---\nname: Acme\ncolors:\n  brand: \"#ffffff\"\nbutton: \"{colors.missing}\"\n---\n\n## Colors\n\nx\n\n## Overview\n\nx\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();

    assert_eq!(out.status.code(), Some(1), "lint failure expected\n{stdout}");
    assert!(
        stdout.contains("design.section-order"),
        "section-order must fire on a path-claimed DESIGN.md (BUG-007):\n{stdout}"
    );
    assert!(
        stdout.contains("design.token-ref"),
        "token-ref must fire on a path-claimed DESIGN.md (BUG-007):\n{stdout}"
    );
    // The spurious core.id parse error must NOT appear — DESIGN.md is
    // path-claimed, not id-claimed (the second half of BUG-007).
    assert!(
        !stdout.contains("core.id"),
        "path-claimed DESIGN.md must not emit core.id:\n{stdout}"
    );
}

#[test]
fn well_formed_design_md_is_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), DESIGN_CONFIG).unwrap();
    fs::write(
        tmp.path().join("DESIGN.md"),
        "---\nname: Acme\ncolors:\n  brand: \"#ffffff\"\nbutton: \"{colors.brand}\"\n---\n\n## Overview\n\nx\n\n## Colors\n\nx\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "well-formed DESIGN.md must be clean\n{stdout}");
}

#[test]
fn style_section_order_and_soul_pair_fire_on_real_style_md() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), STYLE_CONFIG).unwrap();
    // Vocabulary-before-Voice-Principles is out of template order; no
    // SOUL.md sibling exists.
    fs::write(
        tmp.path().join("STYLE.md"),
        "---\nname: Acme Voice\n---\n\n## Vocabulary\n\nx\n\n## Voice Principles\n\nx\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();

    // Both are warnings, so the run exits 0.
    assert_eq!(out.status.code(), Some(0), "style rules are warnings\n{stdout}");
    assert!(
        stdout.contains("style.section-order"),
        "section-order must fire on a path-claimed STYLE.md:\n{stdout}"
    );
    assert!(
        stdout.contains("style.soul-pair"),
        "soul-pair must fire when no SOUL.md sibling exists:\n{stdout}"
    );
    assert!(
        !stdout.contains("core.id"),
        "path-claimed STYLE.md must not emit core.id:\n{stdout}"
    );
}

#[test]
fn style_md_with_malformed_frontmatter_does_not_panic() {
    // Exercises the synthetic_document `unwrap_or_default()` branch through the
    // real binary: a broken `---` fence must yield empty metadata (no panic),
    // the AST-based section-order rule must still run, and core.frontmatter is
    // suppressed for a file-level namespace.
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), STYLE_CONFIG).unwrap();
    fs::write(tmp.path().join("SOUL.md"), "---\nname: Acme\n---\n\n# Identity\n").unwrap();
    fs::write(
        tmp.path().join("STYLE.md"),
        "---\nbad: [unterminated\n---\n\n## Voice Principles\n\nx\n\n## Vocabulary\n\nx\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Sibling present + template order → no diagnostics; malformed frontmatter
    // is silently tolerated (not surfaced as core.frontmatter on a file-level ns).
    assert_eq!(out.status.code(), Some(0), "malformed frontmatter must not crash or fail\n{stdout}");
    assert!(
        !stdout.contains("core.frontmatter"),
        "core.frontmatter is suppressed for a file-level namespace:\n{stdout}"
    );
}

#[test]
fn soul_sections_fire_on_real_soul_md() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), SOUL_CONFIG).unwrap();
    // Worldview and Boundaries present, Opinions missing → one warning.
    fs::write(
        tmp.path().join("SOUL.md"),
        "---\nname: Acme\n---\n\n## Worldview\n\nx\n\n## Boundaries\n\nx\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();

    // soul.sections is a warning, so the run exits 0.
    assert_eq!(out.status.code(), Some(0), "soul.sections is a warning\n{stdout}");
    assert!(
        stdout.contains("soul.sections"),
        "soul.sections must fire on a path-claimed SOUL.md:\n{stdout}"
    );
    assert!(
        stdout.contains("Opinions"),
        "the diagnostic must name the missing section:\n{stdout}"
    );
    assert!(
        !stdout.contains("core.id"),
        "path-claimed SOUL.md must not emit core.id:\n{stdout}"
    );
}

#[test]
fn well_formed_soul_md_is_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), SOUL_CONFIG).unwrap();
    fs::write(
        tmp.path().join("SOUL.md"),
        "---\nname: Acme\n---\n\n## Worldview\n\nx\n\n## Opinions\n\nx\n\n## Boundaries\n\nx\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "well-formed SOUL.md must be clean\n{stdout}");
    assert!(
        !stdout.contains("soul."),
        "no soul diagnostics expected:\n{stdout}"
    );
}

#[test]
fn style_md_with_soul_sibling_and_template_order_is_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), STYLE_CONFIG).unwrap();
    fs::write(
        tmp.path().join("STYLE.md"),
        "---\nname: Acme Voice\n---\n\n## Voice Principles\n\nx\n\n## Vocabulary\n\nx\n",
    )
    .unwrap();
    fs::write(tmp.path().join("SOUL.md"), "---\nname: Acme\n---\n\n# Identity\n").unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "well-formed STYLE+SOUL must be clean\n{stdout}");
    assert!(
        !stdout.contains("style."),
        "no style diagnostics expected:\n{stdout}"
    );
}
