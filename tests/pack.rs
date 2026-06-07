//! Integration tests for `ctxgrd pack` (ADR-013, ADR-023).
//!
//! Each test drives the built binary against a tempdir, exercising the
//! generator contract: never-clobber appends (PACK-005), dry-run that
//! touches nothing (PACK-005), built-in discovery (PACK-009), and the
//! eject-by-default guarantee that a pack leaves no runtime tie
//! (PACK-001).

use std::fs;
use std::path::Path;

use assert_cmd::Command;

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    let mut full = vec!["--root", root.to_str().unwrap()];
    full.extend_from_slice(args);
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(&full)
        .output()
        .expect("ctxgrd executes")
}

#[test]
fn pack_list_reports_both_builtins() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("project-docs"),
        "lists project-docs:\n{stdout}"
    );
    assert!(stdout.contains("agents"), "lists agents:\n{stdout}");
    assert!(
        stdout.contains("built-in"),
        "reports built-in source:\n{stdout}"
    );
    // Removed packs must not appear.
    assert!(
        !stdout.contains("llm-agents"),
        "llm-agents must not appear:\n{stdout}"
    );
    assert!(
        !stdout.contains("agent-build"),
        "agent-build must not appear:\n{stdout}"
    );
    assert!(
        !stdout.contains("agent-context"),
        "agent-context must not appear:\n{stdout}"
    );
}

#[test]
fn pack_show_is_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "show", "project-docs"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    for ns in ["[ADR]", "[PRD]", "[RFC]", "[BUG]", "[TODO]"] {
        assert!(stdout.contains(ns), "show lists {ns}:\n{stdout}");
    }
    // CR and TASK were dropped from project-docs (ADR-023 § PKC-002).
    assert!(!stdout.contains("[CR]"), "CR must not appear:\n{stdout}");
    assert!(
        !stdout.contains("[TASK]"),
        "TASK must not appear:\n{stdout}"
    );
    // PACK-004: read-only — the working tree gains no ctxgrd.toml.
    assert!(
        !tmp.path().join("ctxgrd.toml").exists(),
        "show wrote nothing"
    );
}

#[test]
fn pack_add_never_clobbers_existing_namespace() {
    let tmp = tempfile::tempdir().unwrap();
    let original = "[ADR]\nrules = [\"core.id\"]\n";
    fs::write(tmp.path().join("ctxgrd.toml"), original).unwrap();

    let out = run(tmp.path(), &["pack", "add", "project-docs"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Receipt shows added namespaces (PKC-003 receipt format).
    assert!(
        stdout.contains("Added pack 'project-docs'"),
        "receipt header present:\n{stdout}"
    );
    // Skip message for the already-present ADR.
    assert!(
        stdout.contains("skipped [ADR]"),
        "reports the skip:\n{stdout}"
    );

    let result = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();
    assert!(
        result.starts_with(original),
        "original [ADR] block preserved verbatim"
    );
    assert!(
        result.contains("# pack: project-docs"),
        "provenance comment present"
    );
    // TODO was added (not TASK — ADR-023 § PKC-002).
    assert!(result.contains("[TODO]"), "TODO block written:\n{result}");
    assert!(
        !result.contains("[CR]"),
        "CR must not be written:\n{result}"
    );
    assert!(
        result.matches("[ADR]").count() == 1,
        "ADR not duplicated:\n{result}"
    );
}

#[test]
fn pack_add_dry_run_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let original = "[ADR]\nrules = [\"core.id\"]\n";
    fs::write(tmp.path().join("ctxgrd.toml"), original).unwrap();

    let out = run(tmp.path(), &["pack", "add", "project-docs", "--dry-run"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Dry-run prints the raw TOML blocks. TODO replaces TASK in project-docs.
    assert!(stdout.contains("[TODO]"), "prints [TODO] block:\n{stdout}");
    assert!(
        !stdout.contains("[TASK]"),
        "[TASK] must not appear (dropped):\n{stdout}"
    );
    assert!(
        !stdout.contains("[CR]"),
        "[CR] must not appear (dropped):\n{stdout}"
    );

    let after = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();
    assert_eq!(after, original, "--dry-run left ctxgrd.toml untouched");
}

#[test]
fn pack_add_unknown_name_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "add", "no-such-pack"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "kernel-error exit for unknown pack"
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("pack.unknown"),
        "names the error code:\n{stderr}"
    );
}

#[test]
fn eject_by_default_yields_identical_diagnostics_after_pack_is_gone() {
    // PACK-001 verification: applying a pack writes plain config and
    // leaves no runtime tie. A local pack, once applied, can be deleted
    // and lint must produce byte-identical diagnostics.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // A self-contained local pack defining a RUN namespace with a paths
    // glob so the doc below is classified without an id-claim race.
    let pack_dir = root.join("packs/team-docs");
    fs::create_dir_all(&pack_dir).unwrap();
    fs::write(
        pack_dir.join("pack.toml"),
        "# summary: Team runbook docs.\n[RUN]\nrules = [\"core.frontmatter\", \"core.id\", \"core.required-headings\"]\n\n[RUN.\"core.required-headings\"]\nheadings = [\"Steps\"]\n",
    )
    .unwrap();

    // A runbook that violates the pack's required-headings rule.
    fs::create_dir_all(root.join("run")).unwrap();
    fs::write(
        root.join("run/RUN-001-restart-ingest.md"),
        "---\nid: RUN-001\ntitle: Restart ingest\n---\n\n## Overview\n",
    )
    .unwrap();

    let add = run(root, &["pack", "add", "team-docs"]);
    assert_eq!(add.status.code(), Some(0), "local pack applies");

    let before = run(root, &["lint"]);

    // Delete the pack entirely — the runtime tie, if any, would break here.
    fs::remove_dir_all(root.join("packs")).unwrap();

    let after = run(root, &["lint"]);
    assert_eq!(
        before.stdout, after.stdout,
        "diagnostics identical with the pack gone (eject-by-default)"
    );
    assert_eq!(before.status.code(), after.status.code());
}

// -- agents pack (ADR-023) --------------------------------------------

#[test]
fn agents_pack_shows_all_five_namespaces() {
    // PKC-001/003: the consolidated `agents` pack defines AGENTS, SKILLS,
    // SPEC, TASK, PROMPT. tasks.files-allowed is opt-in (ABP-006).
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "show", "agents"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    for ns in ["[AGENTS]", "[SKILLS]", "[SPEC]", "[TASK]", "[PROMPT]"] {
        assert!(stdout.contains(ns), "agents pack shows {ns}:\n{stdout}");
    }
    assert!(
        !stdout.contains("tasks.files-allowed"),
        "tasks.files-allowed is opt-in, not a pack default:\n{stdout}"
    );
}

#[test]
fn pack_add_receipt_splits_path_and_id_claims() {
    // PKC-003: `pack add agents` receipt splits path-claimed (AGENTS,
    // SKILLS) from id-claimed (SPEC, TASK, PROMPT) namespaces.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "add", "agents"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("Linting now"),
        "path-claim section present:\n{stdout}"
    );
    assert!(
        stdout.contains("AGENTS"),
        "AGENTS in linting-now:\n{stdout}"
    );
    assert!(
        stdout.contains("SKILLS"),
        "SKILLS in linting-now:\n{stdout}"
    );
    assert!(
        stdout.contains("Activates when you create"),
        "id-claim section present:\n{stdout}"
    );
    assert!(stdout.contains("SPEC"), "SPEC in activates:\n{stdout}");
    assert!(stdout.contains("TASK"), "TASK in activates:\n{stdout}");
    assert!(stdout.contains("PROMPT"), "PROMPT in activates:\n{stdout}");
}

/// A TASK whose `Files allowed` lists one path with a missing parent
/// directory (`src/routes/…` when only `src/` exists) and one new
/// root-level file (`top.ts`, parent is the root — exists).
fn write_task_fixture(root: &Path, rules: &str) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("ctxgrd.toml"),
        format!(
            "[TASK]\nrules = [{rules}]\n\n[TASK.\"core.required-headings\"]\nheadings = [\"Goal\", \"Files allowed\", \"Requirements\", \"Acceptance\"]\n"
        ),
    )
    .unwrap();
    fs::create_dir_all(root.join("tasks")).unwrap();
    fs::write(
        root.join("tasks/TASK-1.md"),
        "---\nid: TASK-1\ntitle: Add project creation\nstatus: doing\n---\n\n## Goal\nImplement creation.\n\n## Files allowed\n- src/routes/projects.ts\n- top.ts\n\n## Requirements\n- validate name\n\n## Acceptance\n- run the tests\n",
    )
    .unwrap();
}

#[test]
fn tasks_files_allowed_fires_when_opted_in() {
    // ABP-005: opted in, the missing-parent path warns; the new
    // root-level file does not.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_task_fixture(
        root,
        "\"core.frontmatter\", \"core.id\", \"core.required-headings\", \"tasks.files-allowed\"",
    );

    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("tasks.files-allowed"),
        "the rule fires when opted in:\n{stdout}"
    );
    assert!(
        stdout.contains("`src/routes/projects.ts`"),
        "names the missing-parent path:\n{stdout}"
    );
    assert!(
        !stdout.contains("`top.ts`"),
        "a new root-level file does not warn:\n{stdout}"
    );
}

#[test]
fn tasks_files_allowed_silent_without_opt_in() {
    // ABP-006: the same fixture, but the rule is not listed — no
    // tasks.files-allowed diagnostic.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_task_fixture(
        root,
        "\"core.frontmatter\", \"core.id\", \"core.required-headings\"",
    );

    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains("tasks.files-allowed"),
        "the rule stays silent when not opted in:\n{stdout}"
    );
}

/// A SPEC whose `Requirements` section carries one well-formed EARS
/// clause and one missing its trigger comma (ADR-031 § ESY-003).
fn write_spec_fixture(root: &Path, rules: &str) {
    fs::write(
        root.join("ctxgrd.toml"),
        format!("[SPEC]\nrules = [{rules}]\n"),
    )
    .unwrap();
    fs::create_dir_all(root.join("docs/specs")).unwrap();
    fs::write(
        root.join("docs/specs/SPEC-1.md"),
        "---\nid: SPEC-1\ntitle: Watch mode\nstatus: draft\n---\n\n\
         ## Requirements\n\
         - EARS-01: WHEN a watched file changes, the linter shall re-lint the file.\n\
         - EARS-02: WHEN ctxgrd.toml changes the linter shall reload the config.\n",
    )
    .unwrap();
}

#[test]
fn ears_clause_syntax_fires_when_opted_in() {
    // ESY-005: opted in, the missing-comma clause warns by id; the
    // well-formed clause does not.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_spec_fixture(
        root,
        "\"core.frontmatter\", \"core.id\", \"ears.clause-syntax\"",
    );

    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("ears.clause-syntax"),
        "the rule fires when opted in:\n{stdout}"
    );
    assert!(
        stdout.contains("EARS-02"),
        "names the malformed clause's id:\n{stdout}"
    );
    assert!(
        !stdout.contains("EARS-01"),
        "the well-formed clause does not warn:\n{stdout}"
    );
}

#[test]
fn ears_clause_syntax_silent_without_opt_in() {
    // ESY-005: the same fixture without the rule listed — no
    // ears.clause-syntax diagnostic.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_spec_fixture(root, "\"core.frontmatter\", \"core.id\"");

    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains("ears.clause-syntax"),
        "the rule stays silent when not opted in:\n{stdout}"
    );
}

#[test]
fn ears_clause_syntax_default_in_both_packs() {
    // ESY-005 as amended (2026-06-05): a default of the agents pack's
    // [SPEC].rules and the project-docs pack's [PRD].rules — EARS lives
    // at both altitudes (coarse EARS-NN in PRDs, refined EARS-NN.M in
    // SPECs).
    let tmp = tempfile::tempdir().unwrap();
    for pack in ["agents", "project-docs"] {
        let out = run(tmp.path(), &["pack", "show", pack]);
        assert_eq!(out.status.code(), Some(0));
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(
            stdout.contains("ears.clause-syntax"),
            "ears.clause-syntax is a {pack} pack default:\n{stdout}"
        );
    }
}
