//! Integration tests for the AGENTS + TODO namespaces and their five
//! builtin-compiled `agents.*` / `todo.*` rules (ADR-020).
//!
//! CLAUDE.md / AGENTS.md / TODO.md are id-less singletons, so these tests
//! exercise the file-level pass end-to-end through the real binary —
//! confirming the rules fire on files that never become id-keyed
//! documents.

use std::fs;
use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

const CONFIG: &str = r#"
[AGENTS]
paths = ["CLAUDE.md", "AGENTS.md"]
rules = ["agents.context-headings", "agents.context-budget", "agents.context-cache"]

[TODO]
paths = ["TODO.md"]
rules = ["todo.freshness", "todo.structure"]

[TODO."todo.freshness"]
stale_days = 30
"#;

fn run(root: &Path) -> Output {
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap()])
        .output()
        .expect("ctxgrd executes")
}

#[test]
fn instruction_file_with_volatile_state_and_missing_reference_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), CONFIG).unwrap();
    // CLAUDE.md carries a forbidden `Current State` heading and does not
    // reference the root TODO.md that exists alongside it.
    fs::write(
        tmp.path().join("CLAUDE.md"),
        "# Payments service\n\n## Current State\n\nmigrating to the new ledger\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("TODO.md"),
        "# TODO\n\n_Last updated: 2000-01-01_\n\n### TODO\nplain prose, no checkboxes\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();

    assert_eq!(
        out.status.code(),
        Some(1),
        "lint failure expected\n{stdout}"
    );
    assert!(
        stdout.contains("must not contain a 'Current State' heading"),
        "forbidden-heading error missing:\n{stdout}"
    );
    assert!(
        stdout.contains("does not import it"),
        "missing-reference error missing:\n{stdout}"
    );
    assert!(
        stdout.contains("`### TODO` section has no checklist items"),
        "TODO checklist error missing:\n{stdout}"
    );
    assert!(
        stdout.contains("state is stale"),
        "staleness warning missing:\n{stdout}"
    );
}

#[test]
fn well_formed_instruction_file_and_state_file_pass() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // High stale_days keeps a fixed, realistic date from ever going
    // stale — the staleness clock is wall-time, so the test stays
    // deterministic without computing "today".
    let config = CONFIG.replace("stale_days = 30", "stale_days = 100000");
    fs::write(tmp.path().join("ctxgrd.toml"), config).unwrap();
    fs::write(
        tmp.path().join("CLAUDE.md"),
        "# Payments service\n\nBuild with `cargo build`. Current state:\n\n@TODO.md\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("TODO.md"),
        "# TODO\n\n_Last updated: 2026-05-26_\n\n### Context\n- migrating to the new ledger\n\n### TODO\n- [ ] cut over read traffic\n- [x] dual-write enabled\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "clean run expected\n{stdout}");
    assert!(
        !stdout.contains("agents.") && !stdout.contains("todo."),
        "no agent-context diagnostics expected:\n{stdout}"
    );
}

#[test]
fn summary_counts_path_claimed_singletons() {
    // A clean run over two path-claimed singletons (CLAUDE.md under
    // AGENTS, TODO.md under TODO) must report both files and all their
    // rules in the `ok:` summary — these files never become id-keyed
    // documents, yet they are linted and can produce errors.
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = CONFIG.replace("stale_days = 30", "stale_days = 100000");
    fs::write(tmp.path().join("ctxgrd.toml"), config).unwrap();
    fs::write(
        tmp.path().join("CLAUDE.md"),
        "# Payments service\n\nBuild with `cargo build`.\n\n@TODO.md\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("TODO.md"),
        "# TODO\n\n_Last updated: 2026-05-26_\n\n### Context\n- migrating to the new ledger\n\n### TODO\n- [ ] cut over read traffic\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(0), "clean run expected\n{stderr}");
    // AGENTS (3 rules) over CLAUDE.md + TODO (2 rules) over TODO.md.
    assert!(
        stderr.contains("ok: 2 documents · 5 rules · 0 diagnostics"),
        "summary must count both singletons and all their rules:\n{stderr}"
    );
}

#[test]
fn nested_claude_with_parent_relative_import_passes_both_rules() {
    // A nested `cli/CLAUDE.md` importing the root TODO.md with the
    // file-relative `@../TODO.md` must satisfy `agents.context-headings`
    // AND produce no `agents.context-budget` dangling-import warning —
    // the two rules were mutually unsatisfiable before finding #1.
    let config = r#"
[AGENTS]
paths = ["**/CLAUDE.md", "**/AGENTS.md"]
rules = ["agents.context-headings", "agents.context-budget"]

[TODO]
paths = ["TODO.md"]
rules = ["todo.freshness", "todo.structure"]

[TODO."todo.freshness"]
stale_days = 100000
"#;
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), config).unwrap();
    fs::create_dir_all(tmp.path().join("cli")).unwrap();
    fs::write(tmp.path().join("CLAUDE.md"), "# Root\n\n@TODO.md\n").unwrap();
    fs::write(tmp.path().join("cli/CLAUDE.md"), "# CLI\n\n@../TODO.md\n").unwrap();
    fs::write(
        tmp.path().join("TODO.md"),
        "# TODO\n\n_Last updated: 2026-05-26_\n\n### Context\n- x\n\n### TODO\n- [ ] do it\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "clean run expected\n{stdout}");
    assert!(
        !stdout.contains("does not import it"),
        "headings rule must accept @../TODO.md:\n{stdout}"
    );
    assert!(
        !stdout.contains("does not exist"),
        "budget rule must not warn on the resolvable @../TODO.md:\n{stdout}"
    );
}

#[test]
fn agents_md_is_checked_like_claude_md() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), CONFIG).unwrap();
    // No TODO.md here, so the @TODO.md reference is not required — only
    // the forbidden-heading rule should fire.
    fs::write(
        tmp.path().join("AGENTS.md"),
        "# Agent guide\n\n## TODO\n\n- ship the parser\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "lint failure expected\n{stdout}"
    );
    assert!(
        stdout.contains("must not contain a 'TODO' heading"),
        "forbidden TODO-heading error missing:\n{stdout}"
    );
    assert!(
        !stdout.contains("does not import it"),
        "reference rule should not fire without a root TODO.md:\n{stdout}"
    );
}

#[test]
fn todo_sections_opt_in_rule_fires_on_wrong_shape() {
    // Same agent-context wiring, but the TODO namespace also enables the
    // opt-in `todo.sections` rule. The state file uses the old
    // ### Context / ### TODO shape, which is not Now/Next/Later/Done.
    let config = r#"
[AGENTS]
paths = ["CLAUDE.md", "AGENTS.md"]
rules = ["agents.context-headings", "agents.context-budget", "agents.context-cache"]

[TODO]
paths = ["TODO.md"]
rules = ["todo.freshness", "todo.structure", "todo.sections"]

[TODO."todo.freshness"]
stale_days = 100000
"#;
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), config).unwrap();
    fs::write(
        tmp.path().join("CLAUDE.md"),
        "# Project\n\nState:\n\n@TODO.md\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("TODO.md"),
        "# TODO\n\n_Last updated: 2026-05-29_\n\n### Context\n- x\n\n### TODO\n- [ ] do it\n",
    )
    .unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "lint failure expected\n{stdout}"
    );
    assert!(
        stdout.contains("todo.sections"),
        "todo.sections diagnostic expected:\n{stdout}"
    );
    assert!(
        stdout.contains("`## Now`, `## Next`, `## Later`, `## Done`"),
        "shape help text expected:\n{stdout}"
    );
}

#[test]
fn todo_sections_passes_on_now_next_later_done_shape() {
    let config = r#"
[AGENTS]
paths = ["CLAUDE.md", "AGENTS.md"]
rules = ["agents.context-headings"]

[TODO]
paths = ["TODO.md"]
rules = ["todo.freshness", "todo.sections"]

[TODO."todo.freshness"]
stale_days = 100000
"#;
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), config).unwrap();
    fs::write(
        tmp.path().join("CLAUDE.md"),
        "# Project\n\nState:\n\n@TODO.md\n",
    )
    .unwrap();
    let todo = "\
# TODO

_Last updated: 2026-05-29_

## Now
- [ ] ship parser

## Next
- [ ] streaming

## Later
- [ ] WASM target

## Done
- [x] CI
";
    fs::write(tmp.path().join("TODO.md"), todo).unwrap();

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "clean run expected\n{stdout}");
    assert!(
        !stdout.contains("todo.sections"),
        "no sections diagnostic expected:\n{stdout}"
    );
}

fn git(repo: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?} failed");
}

#[test]
fn churn_warns_when_instruction_file_changes_twice_in_the_window() {
    // Needs the git binary; skip cleanly where it's absent.
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let config = format!("{CONFIG}\n[AGENTS.\"agents.context-cache\"]\nchurn_min_hours = 24\n");
    fs::write(root.join("ctxgrd.toml"), config).unwrap();
    // A clean CLAUDE.md (references TODO.md, no forbidden headings) + valid
    // TODO.md so churn is the only thing that can fire.
    fs::write(
        root.join("CLAUDE.md"),
        "# Payments service\n\nBuild with `cargo build`.\n\n@TODO.md\n",
    )
    .unwrap();
    fs::write(
        root.join("TODO.md"),
        "# TODO\n\n_Last updated: 2026-05-27_\n\n### Context\n- ledger cutover\n\n### TODO\n- [ ] cut over reads\n",
    )
    .unwrap();

    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "dev@aktagon.com"]);
    git(root, &["config", "user.name", "Dev"]);
    git(root, &["add", "."]);
    git(
        root,
        &["commit", "-q", "-m", "initial CLAUDE.md and TODO.md"],
    );
    // A second commit touching CLAUDE.md, inside the 24h window.
    fs::write(
        root.join("CLAUDE.md"),
        "# Payments service\n\nBuild with `cargo build`. Test with `cargo test`.\n\n@TODO.md\n",
    )
    .unwrap();
    git(root, &["add", "CLAUDE.md"]);
    git(root, &["commit", "-q", "-m", "tweak CLAUDE.md again"]);

    // Commit context on → churn warning fires (2 changes in 24h).
    let out = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap()])
        .env("CTXGRD_COMMIT_CONTEXT", "1")
        .output()
        .expect("ctxgrd executes");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("changed 2 times in the last 24h"),
        "churn warning missing:\n{stdout}"
    );

    // Without commit context, the churn warning stays silent.
    let plain = String::from_utf8(run(root).stdout).unwrap();
    assert!(
        !plain.contains("frequent edits"),
        "churn must be silent outside commit context:\n{plain}"
    );
}
