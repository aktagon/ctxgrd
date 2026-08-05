//! Integration tests for the ADR-080 `lint --pack` / `--namespace` scope
//! selectors (AVS-001, AVS-004).
//!
//! The fixture is one project with three namespaces: a clean `[GUIDE]`
//! and a dirty `[ADR]`, both stamped `# pack: …` the way `pack add`
//! writes them, plus a hand-written unstamped `[PRD]`. That layout
//! exercises every verification line in one tree — scoped clean vs
//! scoped dirty, an unknown pack, and the unstamped block that `--pack`
//! cannot reach but `--namespace` can.

use std::fs;
use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

fn run(root: &Path, args: &[&str]) -> Output {
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .output()
        .expect("ctxgrd executes")
}

/// `[ADR]` and `[GUIDE]` carry the stamp `pack add` writes (one comment
/// immediately before each block); `[PRD]` is hand-written and has none.
const CONFIG: &str = r#"
# pack: project-docs@0.73.0 sha:1a2b3c4d5e6f7a8b
[ADR]
paths = ["docs/adrs/**"]
rules = ["core.frontmatter", "core.id", "core.required-headings"]

[ADR."core.required-headings"]
headings = ["Status", "Context", "Decision", "Consequences"]

# pack: guide
[GUIDE]
paths = ["docs/guides/**"]
rules = ["guide.frontmatter"]

[PRD]
paths = ["docs/prds/**"]
rules = ["core.frontmatter", "core.id", "core.required-headings"]

[PRD."core.required-headings"]
headings = ["Problem", "Solution"]
"#;

/// A project where ADR-001 and PRD-001 are each missing a required
/// heading and the one guide is clean.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("ctxgrd.toml"), CONFIG).unwrap();
    for dir in ["docs/adrs", "docs/guides", "docs/prds"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    // Dirty: no `## Decision`, no `## Consequences`.
    fs::write(
        root.join("docs/adrs/001-use-rust.md"),
        "---\nid: ADR-001\ntitle: Use Rust\nstatus: accepted\ndate: 2026-07-21\n---\n\n\
         # ADR-001: Use Rust\n\n## Status\n\naccepted\n\n## Context\n\nWe need a fast linter.\n",
    )
    .unwrap();
    // Clean.
    fs::write(
        root.join("docs/guides/getting-started.md"),
        "---\ntitle: Getting started\ndiataxis:\n  type: tutorial\n---\n\n\
         # Getting started\n\nInstall ctxgrd and run it.\n",
    )
    .unwrap();
    // Dirty: no `## Solution`.
    fs::write(
        root.join("docs/prds/001-scoped-lint.md"),
        "---\nid: PRD-001\ntitle: Scoped lint\nstatus: draft\ndate: 2026-07-21\n---\n\n\
         # PRD-001: Scoped lint\n\n## Problem\n\nAgents lint documents they do not own.\n",
    )
    .unwrap();
    tmp
}

/// AVS-001 Verification, first half: the clean namespace exits 0 while
/// the whole-repo run over the same tree exits 1.
#[test]
fn namespace_scope_to_a_clean_namespace_exits_zero() {
    let tmp = fixture();

    let unscoped = run(tmp.path(), &[]);
    assert_eq!(unscoped.status.code(), Some(1));

    let scoped = run(tmp.path(), &["--namespace", "GUIDE"]);
    assert_eq!(scoped.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&scoped.stdout).trim(), "");
}

/// AVS-001 Verification, second half: the dirty namespace exits 1 and
/// reports only its own diagnostics — the sibling namespace's failures
/// are skipped, not reported and not errored (DOC-001 silence).
#[test]
fn namespace_scope_to_a_dirty_namespace_reports_only_its_own_diagnostics() {
    let tmp = fixture();
    let out = run(tmp.path(), &["--namespace", "ADR", "--format", "simple"]);
    assert_eq!(out.status.code(), Some(1));

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.lines().all(|l| l.contains("docs/adrs/001-use-rust.md")),
        true,
        "every diagnostic anchors in the scoped namespace: {stdout}"
    );
    assert_eq!(
        stdout.contains("docs/prds/001-scoped-lint.md"),
        false,
        "the out-of-scope PRD is skipped: {stdout}"
    );
}

/// AVS-004 Verification: `--pack` resolves through the config's own
/// provenance stamps, so it reaches the stamped `[ADR]` block.
#[test]
fn pack_scope_resolves_through_config_provenance() {
    let tmp = fixture();

    let clean = run(tmp.path(), &["--pack", "guide"]);
    assert_eq!(clean.status.code(), Some(0));

    let dirty = run(tmp.path(), &["--pack", "project-docs", "--format", "simple"]);
    assert_eq!(dirty.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&dirty.stdout);
    assert_eq!(
        stdout.contains("docs/adrs/001-use-rust.md"),
        true,
        "the stamped [ADR] block is in the pack's scope: {stdout}"
    );
    assert_eq!(
        stdout.contains("docs/prds/001-scoped-lint.md"),
        false,
        "the unstamped [PRD] block is not: {stdout}"
    );
}

/// AVS-004 Verification: an unstamped hand-written block belongs to no
/// pack, so `--pack` cannot reach it — but `--namespace` can, and finds
/// it dirty. The pair is the guard against a false clean.
#[test]
fn unstamped_block_is_unreachable_by_pack_but_reachable_by_namespace() {
    let tmp = fixture();

    // The built-in `project-docs` pack *declares* PRD; this project's
    // config never stamped it, so resolving through the definition here
    // would be the AVS-004 error.
    let by_pack = run(tmp.path(), &["--pack", "project-docs", "--format", "simple"]);
    assert_eq!(
        String::from_utf8_lossy(&by_pack.stdout).contains("PRD"),
        false
    );

    let by_namespace = run(tmp.path(), &["--namespace", "PRD", "--format", "simple"]);
    assert_eq!(by_namespace.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&by_namespace.stdout).contains("docs/prds/001-scoped-lint.md"),
        true
    );
}

/// AVS-004: a scope value matching nothing is a config-class error
/// (exit 2), never a quiet exit-0 run over nothing.
#[test]
fn unknown_pack_and_unknown_namespace_exit_two() {
    let tmp = fixture();

    let pack = run(tmp.path(), &["--pack", "marketing"]);
    assert_eq!(pack.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&pack.stderr).contains("cli.unknown-pack"),
        true
    );

    let namespace = run(tmp.path(), &["--namespace", "CAMPAIGN"]);
    assert_eq!(namespace.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&namespace.stderr).contains("cli.unknown-namespace"),
        true
    );
}

/// AVS-001: the two flags compose as an intersection — and an empty
/// intersection is an error, not a run that lints nothing.
#[test]
fn pack_and_namespace_intersect() {
    let tmp = fixture();

    let inside = run(tmp.path(), &["--pack", "project-docs", "--namespace", "ADR"]);
    assert_eq!(inside.status.code(), Some(1));

    let disjoint = run(tmp.path(), &["--pack", "guide", "--namespace", "ADR"]);
    assert_eq!(disjoint.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&disjoint.stderr).contains("cli.empty-scope"),
        true
    );
}

/// AVS-001: the flags attach to the explicit `lint` subcommand exactly as
/// they do to the default action, and repeat / comma-separate as a union.
#[test]
fn scope_flags_work_on_the_lint_subcommand_and_repeat() {
    let tmp = fixture();

    let subcommand = run(tmp.path(), &["lint", "--namespace", "GUIDE"]);
    assert_eq!(subcommand.status.code(), Some(0));

    let repeated = run(
        tmp.path(),
        &["lint", "--namespace", "GUIDE", "--namespace", "ADR"],
    );
    assert_eq!(repeated.status.code(), Some(1));

    let comma_separated = run(tmp.path(), &["lint", "--namespace", "GUIDE,ADR"]);
    assert_eq!(comma_separated.status.code(), Some(1));
}

/// AVS-001: the scope composes with `--harness claude` (ADR-062) — the
/// Stop gate still always exits 0, and blocks only on in-scope findings.
#[test]
fn scope_composes_with_the_claude_stop_harness() {
    let tmp = fixture();

    let clean = run(tmp.path(), &["--harness", "claude", "--namespace", "GUIDE"]);
    assert_eq!(clean.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&clean.stdout).trim(), "");

    let dirty = run(tmp.path(), &["--harness", "claude", "--namespace", "ADR"]);
    assert_eq!(dirty.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&dirty.stdout).contains("\"decision\":\"block\""),
        true
    );
}

/// AVS-001: the JSON wire shape is unchanged — the same
/// `{exit_code, diagnostics, kernel_messages}` object, with `diagnostics`
/// filtered to the scope.
#[test]
fn json_wire_shape_is_unchanged_under_a_scope() {
    let tmp = fixture();
    let out = run(tmp.path(), &["--namespace", "ADR", "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));

    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is a single JSON object");
    assert_eq!(v["exit_code"], serde_json::json!(1));
    assert_eq!(v["kernel_messages"], serde_json::json!([]));
    let diagnostics = v["diagnostics"].as_array().expect("diagnostics array");
    assert_eq!(
        diagnostics
            .iter()
            .all(|d| d["location"] == "docs/adrs/001-use-rust.md"),
        true,
        "diagnostics are filtered to the scope: {diagnostics:?}"
    );
}
