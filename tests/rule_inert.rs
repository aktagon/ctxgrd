//! `cfg.rule-inert` — a namespace binding a `Level::File` rule it can
//! never run (`BUG-040`, `HANDOFF-037` § A4).
//!
//! Two independent suppressors produce the inertness, and detecting either
//! one alone would be wrong:
//!
//! 1. `Config::file_level_namespaces` (`src/config.rs`) excludes any
//!    namespace carrying `core.id`, so an id-claimed namespace never
//!    reaches the file-level scan.
//! 2. `run`'s step-6 per-document loop dispatches only a handful of codes
//!    by name and swallows the rest in `_ => {}`, so 27 of the 29
//!    `Level::File` rules have no id-keyed arm to fall back on.
//!
//! A namespace hitting *both* binds a rule that cannot fire. `ctxgrd rules`
//! listed it as active anyway and the lint summary counted it in `N rules`
//! — the command whose whole job is answering *what is checked here*
//! returning a check that cannot run. That is the `ADR-119` family of
//! defect (a surface reporting on something that did not or cannot run),
//! one layer further in: `ADR-119` closed it at ingestion, this closes it
//! at dispatch.

use std::fs;
use std::path::Path;

use assert_cmd::Command;

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    let mut argv: Vec<&str> = args.to_vec();
    argv.extend_from_slice(&["--root", root.to_str().unwrap()]);
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", root)
        .args(&argv)
        .output()
        .expect("ctxgrd runs")
}

fn combined(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `BUG-040`'s reproduction verbatim: `[NOTE]` binds `core.requires-link`
/// (a `Level::File` rule with no id-keyed arm) alongside `core.id`.
fn write_inert_repro(root: &Path) {
    write(
        root.join("ctxgrd.toml").as_path(),
        "[NOTE]\nowner = \"writer\"\npaths = [\"docs/notes/**\"]\nrules = [\"core.id\", \"core.requires-link\"]\n\n[NOTE.\"core.requires-link\"]\ntargets = [\"docs/guides/getting-started.md\"]\n\n[roles]\nallowed = [\"writer\"]\n",
    );
    write(
        root.join("docs/guides/getting-started.md").as_path(),
        "# Getting started\n\nInstall the binary and run it.\n",
    );
    write(
        root.join("docs/notes/001-retention-window.md").as_path(),
        "---\nid: NOTE-001\ntitle: Retention window\n---\n\n# NOTE-001\n\nNothing references the guide.\n",
    );
}

#[test]
fn an_id_keyed_namespace_binding_a_file_level_rule_is_reported_inert() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_inert_repro(tmp.path());

    let out = run(tmp.path(), &["lint"]);
    let text = combined(&out);
    assert!(
        text.contains("cfg.rule-inert"),
        "BUG-040: the binding cannot fire and must say so; output:\n{text}"
    );
    assert!(
        text.contains("[NOTE] binds core.requires-link"),
        "the message names the namespace and the dead rule; output:\n{text}"
    );
    assert!(
        text.contains("core.id"),
        "and names the suppressor, since that is the thing to change; output:\n{text}"
    );
}

#[test]
fn the_same_namespace_without_core_id_is_silent_and_the_rule_fires() {
    // The control from `BUG-040`'s report, and the half that proves
    // `cfg.rule-inert` detects the *combination* rather than the mere
    // presence of a file-level rule. Drop `core.id` and the namespace
    // becomes path-claimed: the rule runs, and reports the real finding.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_inert_repro(tmp.path());
    write(
        tmp.path().join("ctxgrd.toml").as_path(),
        "[NOTE]\nowner = \"writer\"\npaths = [\"docs/notes/**\"]\nrules = [\"core.requires-link\"]\n\n[NOTE.\"core.requires-link\"]\ntargets = [\"docs/guides/getting-started.md\"]\n\n[roles]\nallowed = [\"writer\"]\n",
    );

    let out = run(tmp.path(), &["lint"]);
    let text = combined(&out);
    assert!(
        !text.contains("cfg.rule-inert"),
        "nothing is suppressed here; output:\n{text}"
    );
    assert!(
        text.contains("core.requires-link"),
        "the rule works — the classification was suppressing it; output:\n{text}"
    );
}

#[test]
fn an_id_keyed_namespace_may_still_bind_the_dual_dispatch_file_level_rules() {
    // `core.required-headings` and `core.file-budget` are registered
    // `Level::File` but carry an explicit id-keyed arm in step 6, so they
    // are *not* inert on an id-claimed namespace. Detecting inertness from
    // `Level::File` alone would false-positive on both — and on this
    // repo's own `[ADR]` block, which binds `core.required-headings`.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        tmp.path().join("ctxgrd.toml").as_path(),
        "[ADR]\nrules = [\"core.id\", \"core.required-headings\"]\n\n[ADR.\"core.required-headings\"]\nheadings = [\"Status\", \"Context\"]\n",
    );
    write(
        tmp.path().join("docs/adrs/001-ledger-store.md").as_path(),
        "---\nid: ADR-001\ntitle: Ledger store\n---\n\n# ADR-001\n\n## Status\n\nAccepted.\n\n## Context\n\nLedgers.\n",
    );

    let out = run(tmp.path(), &["lint"]);
    let text = combined(&out);
    assert!(
        !text.contains("cfg.rule-inert"),
        "core.required-headings has an id-keyed arm; output:\n{text}"
    );
}

#[test]
fn the_dual_dispatch_allow_list_actually_runs_on_an_id_keyed_document() {
    // The allow-list in `builtin_rules` is a second statement of a fact
    // `run`'s step-6 dispatch owns, which is the drift shape this whole
    // batch exists to close. Pin it behaviourally: if an arm is ever
    // removed from step 6 without updating the list, the rule goes silently
    // inert and `cfg.rule-inert` stays quiet about it — so assert each
    // allow-listed code really does produce a diagnostic on an id-keyed
    // document that violates it.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        tmp.path().join("ctxgrd.toml").as_path(),
        "[ADR]\nrules = [\"core.id\", \"core.required-headings\", \"core.file-budget\"]\n\n[ADR.\"core.required-headings\"]\nheadings = [\"Decision Record\"]\n\n[ADR.\"core.file-budget\"]\nmax_chars = 40\n",
    );
    write(
        tmp.path().join("docs/adrs/001-ledger-store.md").as_path(),
        "---\nid: ADR-001\ntitle: Ledger store\n---\n\n# ADR-001\n\n## Status\n\nAccepted, and long enough to exceed a forty-character budget.\n",
    );

    let out = run(tmp.path(), &["lint"]);
    let text = combined(&out);
    assert!(
        text.contains("core.required-headings"),
        "step 6 must still dispatch core.required-headings by id; output:\n{text}"
    );
    assert!(
        text.contains("core.file-budget"),
        "step 6 must still dispatch core.file-budget by id (ADR-109 § BDG-003); output:\n{text}"
    );
}
