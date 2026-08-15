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
    for ns in ["[ADR]", "[PRD]", "[ROADMAP]", "[RFC]", "[BUG]", "[TODO]"] {
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
fn pack_show_intake_lists_cr_and_feedback() {
    // ADR-079 § INT-001: the intake pack exposes exactly the CR and FEEDBACK
    // namespaces; project-docs is unchanged (still no CR).
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "show", "intake"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    for ns in ["[CR]", "[FEEDBACK]"] {
        assert!(stdout.contains(ns), "intake shows {ns}:\n{stdout}");
    }
    // INT-001: intake is a dedicated pack, not a re-add of CR to project-docs.
    let docs = run(tmp.path(), &["pack", "show", "project-docs"]);
    assert_eq!(docs.status.code(), Some(0));
    let docs_out = String::from_utf8(docs.stdout).unwrap();
    assert!(
        !docs_out.contains("[CR]"),
        "project-docs must still list no CR:\n{docs_out}"
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
    // ROADMAP was added (ADR-088).
    assert!(
        result.contains("[ROADMAP]"),
        "ROADMAP block written:\n{result}"
    );
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
        stdout.contains("[ROADMAP]"),
        "prints [ROADMAP] block:\n{stdout}"
    );
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
fn split_packs_show_their_namespaces() {
    // ADR-051: the old catch-all `agents` pack split per-provider. `agents` now
    // owns only AGENTS (AGENTS.md); SPEC/TASK/PROMPT moved to `workflow`; the
    // Claude-proprietary files live in `claude`.
    let tmp = tempfile::tempdir().unwrap();

    let agents = String::from_utf8(run(tmp.path(), &["pack", "show", "agents"]).stdout).unwrap();
    assert!(
        agents.contains("[AGENTS]"),
        "agents shows [AGENTS]:\n{agents}"
    );
    for moved in ["[SKILLS]", "[SPEC]", "[TASK]", "[PROMPT]"] {
        assert!(
            !agents.contains(moved),
            "{moved} moved out of agents pack:\n{agents}"
        );
    }

    let workflow =
        String::from_utf8(run(tmp.path(), &["pack", "show", "workflow"]).stdout).unwrap();
    for ns in ["[SPEC]", "[TASK]", "[PROMPT]"] {
        assert!(workflow.contains(ns), "workflow shows {ns}:\n{workflow}");
    }

    let claude = String::from_utf8(run(tmp.path(), &["pack", "show", "claude"]).stdout).unwrap();
    for ns in ["[CLAUDE]", "[CLAUDESKILLS]", "[CLAUDEAGENTS]"] {
        assert!(claude.contains(ns), "claude shows {ns}:\n{claude}");
    }
}

#[test]
fn pack_add_receipts_render_both_claim_sections() {
    // PKC-003 + ADR-051: the receipt splits path-claimed ("Linting now") from
    // id-claimed ("Activates when you create"). After the split no single agent
    // pack has both, so `workflow` (id-claimed SPEC/TASK/PROMPT) and `agents`
    // (path-claimed AGENTS) cover the two sections.
    let tmp = tempfile::tempdir().unwrap();

    let wf = String::from_utf8(run(tmp.path(), &["pack", "add", "workflow"]).stdout).unwrap();
    assert!(
        wf.contains("Activates when you create"),
        "id-claim section present:\n{wf}"
    );
    assert!(wf.contains("SPEC"), "SPEC in activates:\n{wf}");

    let ag = String::from_utf8(run(tmp.path(), &["pack", "add", "agents"]).stdout).unwrap();
    assert!(
        ag.contains("Linting now"),
        "path-claim section present:\n{ag}"
    );
    assert!(ag.contains("AGENTS"), "AGENTS in linting-now:\n{ag}");
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

// -- pack migrate / outdated (ADR-053) --------------------------------

/// The current `claude` pack's CLAUDEAGENTS block text, taken verbatim
/// from the source `pack.toml` (the same bytes the binary embeds). Used
/// to build a clean `[CLAUDECODE]` fixture by reverse-substituting the
/// namespace token, so the fixture clean-detects against the live pack.
fn claudeagents_block() -> String {
    let pack_toml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/packs/claude/pack.toml"
    ))
    .unwrap();
    // Segment out the [CLAUDEAGENTS] block: its header through the lines up
    // to the next top-level namespace header, trailing-trimmed.
    let mut lines = pack_toml.lines();
    let mut block = String::new();
    for line in lines.by_ref() {
        if line.trim_start().starts_with("[CLAUDEAGENTS]") {
            block.push_str(line);
            block.push('\n');
            break;
        }
    }
    for line in lines {
        let t = line.trim_start();
        // Stop at the next top-level namespace header (an uppercase `[X]`
        // that is not a `[CLAUDEAGENTS.` sub-table).
        if t.starts_with('[')
            && !t.starts_with("[CLAUDEAGENTS")
            && t.chars().nth(1).is_some_and(|c| c.is_ascii_uppercase())
        {
            break;
        }
        block.push_str(line);
        block.push('\n');
    }
    block.trim_end().to_string()
}

/// A clean `[CLAUDECODE]` block: the live CLAUDEAGENTS canonical with the
/// namespace token reverse-substituted to the pre-rename name *everywhere*
/// it appears — the `[..]` header, the `[..."..."]` sub-table headers, and
/// the commented-out example sub-table headers (`# [..."..."]`). A real
/// pre-ADR-061 block carried the old name in its comments too, so a faithful
/// fixture must rename them; renaming only the header would mask a
/// substitution that skips comment lines. Built with a blanket string
/// replace (independent of the migrate engine's own substitution) so this
/// genuinely exercises clean-detection rather than round-tripping one
/// function against itself.
fn clean_claudecode_block() -> String {
    claudeagents_block().replace("CLAUDEAGENTS", "CLAUDECODE")
}

#[test]
fn pack_migrate_rewrites_clean_claudecode_and_is_idempotent() {
    // ADR-053 § PKM-002: a clean (unedited) [CLAUDECODE] block carrying bare
    // provenance migrates to the current [CLAUDEAGENTS] shape with v2
    // provenance, the result lints, and a second migrate is byte-identical.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let config = format!(
        "# ctxgrd.toml\n\n# pack: claude\n{}\n",
        clean_claudecode_block()
    );
    fs::write(root.join("ctxgrd.toml"), &config).unwrap();

    let out = run(root, &["pack", "migrate"]);
    assert_eq!(out.status.code(), Some(0), "clean migrate exits 0");

    let migrated = fs::read_to_string(root.join("ctxgrd.toml")).unwrap();
    assert!(
        migrated.contains("[CLAUDEAGENTS]"),
        "renamed to CLAUDEAGENTS:\n{migrated}"
    );
    assert!(
        !migrated.contains("[CLAUDECODE]"),
        "old name gone:\n{migrated}"
    );
    assert!(
        migrated.contains("# pack: claude@"),
        "v2 provenance stamped:\n{migrated}"
    );

    // The migrated config lints without error (a valid namespace block).
    let lint = run(root, &["lint"]);
    assert!(
        lint.status.code() == Some(0) || lint.status.code() == Some(1),
        "lint runs without a kernel error after migrate (got {:?})",
        lint.status.code()
    );

    // A second migrate is a no-op: the file is byte-identical.
    let before = migrated.clone();
    let out2 = run(root, &["pack", "migrate"]);
    assert_eq!(out2.status.code(), Some(0));
    let after = fs::read_to_string(root.join("ctxgrd.toml")).unwrap();
    assert_eq!(after, before, "second migrate is byte-identical");
}

#[test]
fn pack_migrate_leaves_hand_edited_block_and_reports_dirty_diff() {
    // ADR-053 § PKM-003: a hand-edited [CLAUDECODE] block is left untouched;
    // `pack migrate --dry-run --format json` reports it as a dirty diff.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let edited = clean_claudecode_block().replace(
        "rules = [\"agent.frontmatter\"]",
        "rules = [\"agent.frontmatter\", \"core.min-docs\"]",
    );
    let config = format!("# pack: claude\n{edited}\n");
    fs::write(root.join("ctxgrd.toml"), &config).unwrap();

    let out = run(root, &["pack", "migrate", "--dry-run", "--format", "json"]);
    assert_eq!(out.status.code(), Some(1), "dirty diff exits 1");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let plan: serde_json::Value = serde_json::from_str(&stdout).expect("clean json stream");
    assert_eq!(
        plan["rewrites"].as_array().unwrap().len(),
        0,
        "no clean rewrites:\n{stdout}"
    );
    let diffs = plan["diffs"].as_array().unwrap();
    assert_eq!(diffs.len(), 1, "one dirty diff:\n{stdout}");
    assert_eq!(diffs[0]["namespace"], "CLAUDECODE");
    assert_eq!(diffs[0]["kind"], "rename");

    // --dry-run wrote nothing: the file is unchanged.
    let after = fs::read_to_string(root.join("ctxgrd.toml")).unwrap();
    assert_eq!(after, config, "--dry-run left ctxgrd.toml untouched");
}

#[test]
fn pack_outdated_flags_stale_and_clean_configs() {
    // ADR-053 § PKM-004: outdated exits 1 on a stale config, 0 on a current
    // one. Read-only — touches no file.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Stale: a [CLAUDECODE] block awaiting the ADR-061 rename.
    let stale = format!("# pack: claude\n{}\n", clean_claudecode_block());
    fs::write(root.join("ctxgrd.toml"), &stale).unwrap();
    let out = run(root, &["pack", "outdated"]);
    assert_eq!(out.status.code(), Some(1), "stale config flags drift");
    // outdated is read-only.
    let after = fs::read_to_string(root.join("ctxgrd.toml")).unwrap();
    assert_eq!(after, stale, "outdated wrote nothing");

    // Current: the already-renamed CLAUDEAGENTS block at canonical shape.
    let current = format!("# pack: claude\n{}\n", claudeagents_block());
    fs::write(root.join("ctxgrd.toml"), &current).unwrap();
    let out = run(root, &["pack", "outdated"]);
    assert_eq!(out.status.code(), Some(0), "current config is clean");
}

#[test]
fn pack_migrate_unreadable_config_is_kernel_error() {
    // ADR-053: a missing config is a kernel error (exit 2), not a panic.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "migrate"]);
    assert_eq!(out.status.code(), Some(2), "missing config exits 2");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("pack.config-unreadable"),
        "names the error code:\n{stderr}"
    );
}

// -- gdpr pack (ADR-066) ----------------------------------------------

#[test]
fn pack_list_includes_gdpr() {
    // CMP-001: the gdpr regulation pack is a distinct builtin pack.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("gdpr"), "lists gdpr:\n{stdout}");
}

#[test]
fn pack_show_gdpr_lists_ropa_dpia_dpa() {
    // GDPR-001: the gdpr pack exposes the three statutory document namespaces.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "show", "gdpr"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    for ns in ["[ROPA]", "[DPIA]", "[DPA]"] {
        assert!(stdout.contains(ns), "gdpr shows {ns}:\n{stdout}");
    }
    // CMP-002 interim: POLICY/RISK/VULN are reused from `security`, never
    // redefined here.
    for base in ["[POLICY]", "[RISK]", "[VULN]"] {
        assert!(
            !stdout.contains(base),
            "{base} must not be redefined in gdpr:\n{stdout}"
        );
    }
}

#[test]
fn gdpr_pack_regenerates_byte_for_byte() {
    // CMP-005 verification: regenerating the gdpr pack from the unchanged
    // committed regulation.json reproduces the committed pack.toml
    // byte-for-byte. Drives the cargo example generator against the real
    // packs/gdpr/regulation.json, writes to a temp copy, and compares.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let committed = fs::read_to_string(manifest.join("packs/gdpr/pack.toml"))
        .expect("committed gdpr pack.toml exists");

    // Stage an isolated regulation tree so the generator does not overwrite
    // the committed file; pass the temp root as the generator's second
    // argument, where it reads regulation.json and writes pack.toml.
    let tmp = tempfile::tempdir().unwrap();
    let gdpr_dir = tmp.path().join("packs/gdpr");
    fs::create_dir_all(&gdpr_dir).unwrap();
    fs::copy(
        manifest.join("packs/gdpr/regulation.json"),
        gdpr_dir.join("regulation.json"),
    )
    .unwrap();

    let status = std::process::Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--example",
            "gen_compliance_pack",
            "--",
            "gdpr",
            tmp.path().to_str().unwrap(),
        ])
        .current_dir(manifest)
        .status()
        .expect("generator runs");
    assert!(status.success(), "generator exits 0");

    let regenerated =
        fs::read_to_string(gdpr_dir.join("pack.toml")).expect("generator wrote pack.toml");
    assert_eq!(
        regenerated, committed,
        "regenerating from an unchanged extract reproduces pack.toml byte-for-byte"
    );
}

#[test]
fn every_builtin_id_claiming_namespace_has_an_id_legal_name() {
    // BUG-013 regression: a namespace that lists `core.id` must have a name
    // that parses as an id prefix — otherwise its documents are all flagged
    // malformed (the SR-MAP defect). Guards the whole built-in pack set.
    for pack in ctxgrd::pack::builtin_packs() {
        for ns in ctxgrd::pack::namespace_views(&pack) {
            if ns.rules.iter().any(|r| r == "core.id") {
                let id = format!("{}-1", ns.name);
                let parsed = id.parse::<ctxgrd::id::DocumentId>();
                assert!(
                    parsed.as_ref().is_ok_and(|d| d.namespace == ns.name),
                    "pack '{}' namespace '{}' lists core.id but '{}' is not an id-legal name",
                    pack.name,
                    ns.name,
                    ns.name
                );
            }
        }
    }
}

#[test]
fn pack_add_gdpr_pulls_security_base() {
    // ADR-068 § PKD-002: `pack add gdpr` applies the `security` base first,
    // then gdpr, each with its own provenance — one command installs both.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "add", "gdpr"]);
    assert_eq!(out.status.code(), Some(0));

    let result = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();
    // gdpr's own statutory namespaces.
    for ns in ["[ROPA]", "[DPIA]", "[DPA]"] {
        assert!(result.contains(ns), "{ns} written:\n{result}");
    }
    // security base namespaces, pulled transitively.
    for ns in ["[POLICY]", "[RISK]", "[VULN]"] {
        assert!(result.contains(ns), "{ns} pulled from security:\n{result}");
    }
    // Per-pack provenance for both.
    assert!(
        result.contains("# pack: security"),
        "security provenance present:\n{result}"
    );
    assert!(
        result.contains("# pack: gdpr"),
        "gdpr provenance present:\n{result}"
    );
    // PKD-001: the `# depends:` comment is pack metadata, never copied.
    assert!(
        !result.contains("# depends:"),
        "the depends comment must not land in ctxgrd.toml:\n{result}"
    );
    // PKD-002 ordering: the security base is written before gdpr's blocks.
    assert!(
        result.find("[POLICY]").unwrap() < result.find("[ROPA]").unwrap(),
        "security base precedes gdpr namespaces:\n{result}"
    );
}

#[test]
fn pack_add_gdpr_dry_run_shows_security_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "add", "gdpr", "--dry-run"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("[POLICY]"), "dry-run shows the base:\n{stdout}");
    assert!(stdout.contains("[ROPA]"), "dry-run shows gdpr:\n{stdout}");
    assert!(
        !tmp.path().join("ctxgrd.toml").exists(),
        "--dry-run wrote no config"
    );
}

// -- hipaa pack (ADR-066) ---------------------------------------------

#[test]
fn pack_list_includes_hipaa() {
    // CMP-001: the hipaa regulation pack is a distinct builtin pack.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("hipaa"), "lists hipaa:\n{stdout}");
}

#[test]
fn pack_show_hipaa_lists_safeguard_baa() {
    // HIPAA-001/002/003: the hipaa pack exposes the Security Rule safeguard
    // register and the Business Associate Agreement register.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "show", "hipaa"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    for ns in ["[SAFEGUARD]", "[BAA]"] {
        assert!(stdout.contains(ns), "hipaa shows {ns}:\n{stdout}");
    }
    // CMP-002 interim: POLICY/RISK/VULN are reused from `security`, never
    // redefined here.
    for base in ["[POLICY]", "[RISK]", "[VULN]"] {
        assert!(
            !stdout.contains(base),
            "{base} must not be redefined in hipaa:\n{stdout}"
        );
    }
}

#[test]
fn hipaa_pack_regenerates_byte_for_byte() {
    // CMP-005 verification: regenerating the hipaa pack from the unchanged
    // committed regulation.json reproduces the committed pack.toml
    // byte-for-byte. Drives the cargo example generator against the real
    // packs/hipaa/regulation.json, writes to a temp copy, and compares.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let committed = fs::read_to_string(manifest.join("packs/hipaa/pack.toml"))
        .expect("committed hipaa pack.toml exists");

    // Stage an isolated regulation tree so the generator does not overwrite
    // the committed file; pass the temp root as the generator's second
    // argument, where it reads regulation.json and writes pack.toml.
    let tmp = tempfile::tempdir().unwrap();
    let hipaa_dir = tmp.path().join("packs/hipaa");
    fs::create_dir_all(&hipaa_dir).unwrap();
    fs::copy(
        manifest.join("packs/hipaa/regulation.json"),
        hipaa_dir.join("regulation.json"),
    )
    .unwrap();

    let status = std::process::Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--example",
            "gen_compliance_pack",
            "--",
            "hipaa",
            tmp.path().to_str().unwrap(),
        ])
        .current_dir(manifest)
        .status()
        .expect("generator runs");
    assert!(status.success(), "generator exits 0");

    let regenerated =
        fs::read_to_string(hipaa_dir.join("pack.toml")).expect("generator wrote pack.toml");
    assert_eq!(
        regenerated, committed,
        "regenerating from an unchanged extract reproduces pack.toml byte-for-byte"
    );
}

// -- soc2 pack (ADR-069) ----------------------------------------------

#[test]
fn pack_list_includes_soc2() {
    // SOC-005: the soc2 regulation pack is a distinct builtin pack.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("soc2"), "lists soc2:\n{stdout}");
}

#[test]
fn pack_show_soc2_lists_control_register_only() {
    // SOC-002: the soc2 pack exposes the single SOC2 control-to-evidence
    // register and no statutory document namespace of its own.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "show", "soc2"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("[SOC2]"), "soc2 shows [SOC2]:\n{stdout}");
    // The register reuses the security base; it never redefines POLICY/RISK/VULN.
    for base in ["[POLICY]", "[RISK]", "[VULN]"] {
        assert!(
            !stdout.contains(base),
            "{base} must not be redefined in soc2:\n{stdout}"
        );
    }
}

#[test]
fn soc2_pack_regenerates_byte_for_byte() {
    // SOC-004 verification: regenerating the soc2 pack from the unchanged
    // committed regulation.json reproduces the committed pack.toml
    // byte-for-byte. Drives the cargo example generator against the real
    // packs/soc2/regulation.json, writes to a temp copy, and compares.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let committed = fs::read_to_string(manifest.join("packs/soc2/pack.toml"))
        .expect("committed soc2 pack.toml exists");

    // Stage an isolated regulation tree so the generator does not overwrite
    // the committed file; pass the temp root as the generator's second
    // argument, where it reads regulation.json and writes pack.toml.
    let tmp = tempfile::tempdir().unwrap();
    let soc2_dir = tmp.path().join("packs/soc2");
    fs::create_dir_all(&soc2_dir).unwrap();
    fs::copy(
        manifest.join("packs/soc2/regulation.json"),
        soc2_dir.join("regulation.json"),
    )
    .unwrap();

    let status = std::process::Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--example",
            "gen_compliance_pack",
            "--",
            "soc2",
            tmp.path().to_str().unwrap(),
        ])
        .current_dir(manifest)
        .status()
        .expect("generator runs");
    assert!(status.success(), "generator exits 0");

    let regenerated =
        fs::read_to_string(soc2_dir.join("pack.toml")).expect("generator wrote pack.toml");
    assert_eq!(
        regenerated, committed,
        "regenerating from an unchanged extract reproduces pack.toml byte-for-byte"
    );
}

#[test]
fn pack_add_soc2_pulls_security_base() {
    // SOC-005 / ADR-068 § PKD-002: `pack add soc2` applies the `security`
    // base first, then soc2, each with its own provenance.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "add", "soc2"]);
    assert_eq!(out.status.code(), Some(0));

    let result = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();
    // soc2's own control register.
    assert!(result.contains("[SOC2]"), "[SOC2] written:\n{result}");
    // security base namespaces, pulled transitively.
    for ns in ["[POLICY]", "[RISK]", "[VULN]"] {
        assert!(result.contains(ns), "{ns} pulled from security:\n{result}");
    }
    // Per-pack provenance for both.
    assert!(
        result.contains("# pack: security"),
        "security provenance present:\n{result}"
    );
    assert!(
        result.contains("# pack: soc2"),
        "soc2 provenance present:\n{result}"
    );
    // PKD-002 ordering: the security base is written before soc2's blocks.
    assert!(
        result.find("[POLICY]").unwrap() < result.find("[SOC2]").unwrap(),
        "security base precedes soc2 namespace:\n{result}"
    );
}

// -- iso-27001 pack (ADR-070) -----------------------------------------

#[test]
fn pack_list_includes_iso_27001() {
    // ISO-005: the iso-27001 regulation pack is a distinct builtin pack.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("iso-27001"), "lists iso-27001:\n{stdout}");
}

#[test]
fn pack_show_iso_27001_lists_control_register_only() {
    // ISO-002: the iso-27001 pack exposes the single ISO27001 control-to-
    // evidence register and no statutory document namespace of its own.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "show", "iso-27001"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("[ISO27001]"), "iso-27001 shows [ISO27001]:\n{stdout}");
    // The register reuses the security base; it never redefines POLICY/RISK/VULN.
    for base in ["[POLICY]", "[RISK]", "[VULN]"] {
        assert!(
            !stdout.contains(base),
            "{base} must not be redefined in iso-27001:\n{stdout}"
        );
    }
}

#[test]
fn iso_27001_pack_regenerates_byte_for_byte() {
    // ISO-004 verification: regenerating the iso-27001 pack from the unchanged
    // committed regulation.json reproduces the committed pack.toml byte-for-byte.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let committed = fs::read_to_string(manifest.join("packs/iso-27001/pack.toml"))
        .expect("committed iso-27001 pack.toml exists");

    let tmp = tempfile::tempdir().unwrap();
    let pack_dir = tmp.path().join("packs/iso-27001");
    fs::create_dir_all(&pack_dir).unwrap();
    fs::copy(
        manifest.join("packs/iso-27001/regulation.json"),
        pack_dir.join("regulation.json"),
    )
    .unwrap();

    let status = std::process::Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--example",
            "gen_compliance_pack",
            "--",
            "iso-27001",
            tmp.path().to_str().unwrap(),
        ])
        .current_dir(manifest)
        .status()
        .expect("generator runs");
    assert!(status.success(), "generator exits 0");

    let regenerated =
        fs::read_to_string(pack_dir.join("pack.toml")).expect("generator wrote pack.toml");
    assert_eq!(
        regenerated, committed,
        "regenerating from an unchanged extract reproduces pack.toml byte-for-byte"
    );
}

#[test]
fn pack_add_iso_27001_pulls_security_base() {
    // ISO-005 / ADR-068 § PKD-002: `pack add iso-27001` applies the `security`
    // base first, then iso-27001, each with its own provenance.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "add", "iso-27001"]);
    assert_eq!(out.status.code(), Some(0));

    let result = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();
    assert!(result.contains("[ISO27001]"), "[ISO27001] written:\n{result}");
    for ns in ["[POLICY]", "[RISK]", "[VULN]"] {
        assert!(result.contains(ns), "{ns} pulled from security:\n{result}");
    }
    assert!(
        result.contains("# pack: security"),
        "security provenance present:\n{result}"
    );
    assert!(
        result.contains("# pack: iso-27001"),
        "iso-27001 provenance present:\n{result}"
    );
    assert!(
        result.find("[POLICY]").unwrap() < result.find("[ISO27001]").unwrap(),
        "security base precedes iso-27001 namespace:\n{result}"
    );
}

// -- nist-800-53 pack (ADR-071) ---------------------------------------

#[test]
fn pack_list_includes_nist_800_53() {
    // NIST-005: the nist-800-53 regulation pack is a distinct builtin pack.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("nist-800-53"), "lists nist-800-53:\n{stdout}");
}

#[test]
fn pack_show_nist_800_53_lists_control_register_only() {
    // NIST-002: the nist-800-53 pack exposes the single NIST80053 control-to-
    // evidence register and no statutory document namespace of its own.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "show", "nist-800-53"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("[NIST80053]"), "nist-800-53 shows [NIST80053]:\n{stdout}");
    for base in ["[POLICY]", "[RISK]", "[VULN]"] {
        assert!(
            !stdout.contains(base),
            "{base} must not be redefined in nist-800-53:\n{stdout}"
        );
    }
}

#[test]
fn nist_800_53_pack_regenerates_byte_for_byte() {
    // NIST-004 verification: regenerating the nist-800-53 pack from the
    // unchanged committed regulation.json reproduces pack.toml byte-for-byte.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let committed = fs::read_to_string(manifest.join("packs/nist-800-53/pack.toml"))
        .expect("committed nist-800-53 pack.toml exists");

    let tmp = tempfile::tempdir().unwrap();
    let pack_dir = tmp.path().join("packs/nist-800-53");
    fs::create_dir_all(&pack_dir).unwrap();
    fs::copy(
        manifest.join("packs/nist-800-53/regulation.json"),
        pack_dir.join("regulation.json"),
    )
    .unwrap();

    let status = std::process::Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--example",
            "gen_compliance_pack",
            "--",
            "nist-800-53",
            tmp.path().to_str().unwrap(),
        ])
        .current_dir(manifest)
        .status()
        .expect("generator runs");
    assert!(status.success(), "generator exits 0");

    let regenerated =
        fs::read_to_string(pack_dir.join("pack.toml")).expect("generator wrote pack.toml");
    assert_eq!(
        regenerated, committed,
        "regenerating from an unchanged extract reproduces pack.toml byte-for-byte"
    );
}

#[test]
fn pack_add_nist_800_53_pulls_security_base() {
    // NIST-005 / ADR-068 § PKD-002: `pack add nist-800-53` applies the
    // `security` base first, then nist-800-53, each with its own provenance.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "add", "nist-800-53"]);
    assert_eq!(out.status.code(), Some(0));

    let result = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();
    assert!(result.contains("[NIST80053]"), "[NIST80053] written:\n{result}");
    for ns in ["[POLICY]", "[RISK]", "[VULN]"] {
        assert!(result.contains(ns), "{ns} pulled from security:\n{result}");
    }
    assert!(
        result.contains("# pack: security"),
        "security provenance present:\n{result}"
    );
    assert!(
        result.contains("# pack: nist-800-53"),
        "nist-800-53 provenance present:\n{result}"
    );
    assert!(
        result.find("[POLICY]").unwrap() < result.find("[NIST80053]").unwrap(),
        "security base precedes nist-800-53 namespace:\n{result}"
    );
}

#[test]
fn ears_clause_syntax_default_in_both_packs() {
    // ESY-005 as amended (2026-06-05): a default of the workflow pack's
    // [SPEC].rules (moved out of `agents` by ADR-051) and the project-docs
    // pack's [PRD].rules — EARS lives at both altitudes (coarse EARS-NN in
    // PRDs, refined EARS-NN.M in SPECs).
    let tmp = tempfile::tempdir().unwrap();
    for pack in ["workflow", "project-docs"] {
        let out = run(tmp.path(), &["pack", "show", pack]);
        assert_eq!(out.status.code(), Some(0));
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(
            stdout.contains("ears.clause-syntax"),
            "ears.clause-syntax is a {pack} pack default:\n{stdout}"
        );
    }
}

// -- ddd pack (ADR-082) -----------------------------------------------

/// A complete, clean BOUNDEDCONTEXT doc body with all eight required
/// headings and the five required metadata keys. Callers mutate a copy to
/// inject a single defect.
fn clean_bc_doc(id: &str, subdomain_type: &str) -> String {
    format!(
        "---\nid: {id}\ntitle: Billing\nstatus: active\nowner: platform-team\n\
         subdomain_type: {subdomain_type}\n---\n\n\
         ## Purpose\nWhat this context owns.\n\n\
         ## Ubiquitous Language\nInvoice, Charge, Dunning.\n\n\
         ## Aggregates\nInvoice.\n\n\
         ## Domain Events\nInvoiceIssued.\n\n\
         ## Boundaries\nOwns billing, not the ledger.\n\n\
         ## Team / Ownership\nPlatform team.\n\n\
         ## Open Questions\nNone.\n\n\
         ## References\nADR 082.\n"
    )
}

#[test]
fn pack_list_includes_ddd() {
    // DDD-001: the ddd pack is a distinct builtin pack.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("ddd"), "lists ddd:\n{stdout}");
}

#[test]
fn pack_show_ddd_lists_exactly_two_namespaces() {
    // DDD-001 / DDD-005: exactly BOUNDEDCONTEXT and CONTEXTMAP — no tactical
    // AGGREGATE/DOMAINEVENT/GLOSSARY namespaces.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "show", "ddd"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("[BOUNDEDCONTEXT]"),
        "shows [BOUNDEDCONTEXT]:\n{stdout}"
    );
    assert!(
        stdout.contains("[CONTEXTMAP]"),
        "shows [CONTEXTMAP]:\n{stdout}"
    );
    for absent in ["[AGGREGATE]", "[DOMAINEVENT]", "[GLOSSARY]"] {
        assert!(
            !stdout.contains(absent),
            "{absent} must not appear (DDD-005):\n{stdout}"
        );
    }
}

#[test]
fn pack_add_ddd_writes_both_blocks() {
    // DDD-001 verification: `pack add ddd` writes [BOUNDEDCONTEXT] and
    // [CONTEXTMAP] into ctxgrd.toml. ddd declares no `depends`, so no base
    // pack is pulled.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "add", "ddd"]);
    assert_eq!(out.status.code(), Some(0));
    let result = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();
    assert!(
        result.contains("[BOUNDEDCONTEXT]"),
        "[BOUNDEDCONTEXT] written:\n{result}"
    );
    assert!(
        result.contains("[CONTEXTMAP]"),
        "[CONTEXTMAP] written:\n{result}"
    );
    assert!(
        result.contains("ddd.context-map-shape"),
        "binds ddd.context-map-shape:\n{result}"
    );
}

#[test]
fn ddd_bounded_context_clean_doc_lints_green() {
    // DDD-002 verification: a complete BOUNDEDCONTEXT doc lints clean.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(run(root, &["pack", "add", "ddd"]).status.code(), Some(0));
    fs::create_dir_all(root.join("docs/ddd/bounded-contexts")).unwrap();
    fs::write(
        root.join("docs/ddd/bounded-contexts/billing.md"),
        clean_bc_doc("BOUNDEDCONTEXT-1", "core"),
    )
    .unwrap();

    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "clean BOUNDEDCONTEXT lints green:\n{stdout}"
    );
}

#[test]
fn ddd_bounded_context_flags_missing_heading_metadata_and_value() {
    // DDD-002 verification: a BC missing a required heading, one missing
    // subdomain_type, and one with an out-of-allowlist subdomain_type each
    // assert their diagnostic.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(run(root, &["pack", "add", "ddd"]).status.code(), Some(0));
    let dir = root.join("docs/ddd/bounded-contexts");
    fs::create_dir_all(&dir).unwrap();

    // Missing the `## Domain Events` heading.
    fs::write(
        dir.join("no-heading.md"),
        clean_bc_doc("BOUNDEDCONTEXT-1", "core").replace("## Domain Events\nInvoiceIssued.\n\n", ""),
    )
    .unwrap();
    // Missing the subdomain_type metadata key.
    fs::write(
        dir.join("no-subdomain.md"),
        clean_bc_doc("BOUNDEDCONTEXT-2", "core").replace("subdomain_type: core\n", ""),
    )
    .unwrap();
    // subdomain_type outside the allowlist.
    fs::write(
        dir.join("bad-value.md"),
        clean_bc_doc("BOUNDEDCONTEXT-3", "peripheral"),
    )
    .unwrap();

    let out = run(root, &["lint"]);
    assert_eq!(out.status.code(), Some(1), "diagnostics present");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("core.required-headings"),
        "missing heading fires core.required-headings:\n{stdout}"
    );
    assert!(
        stdout.contains("core.required-metadata"),
        "missing subdomain_type fires core.required-metadata:\n{stdout}"
    );
    assert!(
        stdout.contains("core.allowed-values"),
        "out-of-allowlist subdomain_type fires core.allowed-values:\n{stdout}"
    );
}

#[test]
fn ddd_context_map_valid_edge_lints_green_and_bad_edge_fires() {
    // DDD-003 verification: a valid two-endpoint map lints clean; a one-BC
    // map fires ddd.context-map-shape.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(run(root, &["pack", "add", "ddd"]).status.code(), Some(0));
    let bc_dir = root.join("docs/ddd/bounded-contexts");
    let map_dir = root.join("docs/ddd/context-maps");
    fs::create_dir_all(&bc_dir).unwrap();
    fs::create_dir_all(&map_dir).unwrap();
    fs::write(
        bc_dir.join("billing.md"),
        clean_bc_doc("BOUNDEDCONTEXT-1", "core"),
    )
    .unwrap();
    fs::write(
        bc_dir.join("ledger.md"),
        clean_bc_doc("BOUNDEDCONTEXT-2", "supporting"),
    )
    .unwrap();

    // A valid symmetric Partnership edge between the two contexts.
    fs::write(
        map_dir.join("billing-ledger.md"),
        "---\nid: CONTEXTMAP-1\ntitle: Billing <-> Ledger\npattern: Partnership\n\
         depends_on:\n  - BOUNDEDCONTEXT-1\n  - BOUNDEDCONTEXT-2\n---\n\n\
         Two teams evolve billing and ledger together.\n",
    )
    .unwrap();
    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "valid two-endpoint map lints green:\n{stdout}"
    );

    // Now break it: only one endpoint.
    fs::write(
        map_dir.join("billing-ledger.md"),
        "---\nid: CONTEXTMAP-1\ntitle: Billing edge\npattern: Partnership\n\
         depends_on:\n  - BOUNDEDCONTEXT-1\n---\n\n\
         A dangling half-edge.\n",
    )
    .unwrap();
    let out = run(root, &["lint"]);
    assert_eq!(out.status.code(), Some(1), "one-endpoint map fails");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("ddd.context-map-shape"),
        "one-endpoint map fires ddd.context-map-shape:\n{stdout}"
    );
    assert!(
        stdout.contains("exactly 2 BOUNDEDCONTEXT contexts, found 1"),
        "names the cardinality gap:\n{stdout}"
    );
}

// -- ROADMAP (ADR-088) -------------------------------------------------

/// A complete, valid `ROADMAP` initiative satisfying RDM-002/003.
fn clean_roadmap_doc(id: &str, status: &str) -> String {
    format!(
        "---\nid: {id}\ntitle: Ship the thing\nstatus: {status}\ndate: 2026-07-09\nowner: alex\n---\n\n\
         ## Problem\nUsers cannot do the thing.\n\n\
         ## Outcome\nUsers can do the thing.\n\n\
         ## Ideas\n- Build the thing.\n\n\
         ## Success Metrics\nThing-usage rate up.\n"
    )
}

#[test]
fn roadmap_clean_initiative_lints_green() {
    // RDM-001..003 verification: a complete NNL initiative lints clean.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(
        run(root, &["pack", "add", "project-docs"]).status.code(),
        Some(0)
    );
    // README and PRD also bind core.min-docs (project-docs pack) — satisfy
    // both so this test isolates ROADMAP.
    fs::write(root.join("README.md"), "# Example\n").unwrap();
    fs::create_dir_all(root.join("docs/prds")).unwrap();
    fs::write(
        root.join("docs/prds/001-example.md"),
        "---\nid: PRD-1\ntitle: Example\nstatus: draft\n---\n\n\
         ## Context\nx\n\n## Goals\nx\n\n## Non-goals\nx\n\n## User stories\nx\n\n\
         ## Requirements\nx\n\n## Definition of Done\nx\n\n## Open Questions\nx\n\n\
         ## References\nx\n\n## Change log\nx\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("docs/roadmap")).unwrap();
    fs::write(
        root.join("docs/roadmap/001-ship-the-thing.md"),
        clean_roadmap_doc("ROADMAP-1", "now"),
    )
    .unwrap();

    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "clean ROADMAP initiative lints green:\n{stdout}"
    );
}

#[test]
fn roadmap_missing_success_metrics_and_owner_each_fire_their_diagnostic() {
    // RDM-002 verification: a fixture missing `Success Metrics` and one
    // missing `owner` each assert exactly that diagnostic.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(
        run(root, &["pack", "add", "project-docs"]).status.code(),
        Some(0)
    );
    let dir = root.join("docs/roadmap");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("001-no-metrics.md"),
        clean_roadmap_doc("ROADMAP-1", "now")
            .replace("\n## Success Metrics\nThing-usage rate up.\n", ""),
    )
    .unwrap();
    fs::write(
        dir.join("002-no-owner.md"),
        clean_roadmap_doc("ROADMAP-2", "next").replace("owner: alex\n", ""),
    )
    .unwrap();

    let out = run(root, &["lint"]);
    assert_eq!(out.status.code(), Some(1), "diagnostics present");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("core.required-headings"),
        "missing Success Metrics fires core.required-headings:\n{stdout}"
    );
    assert!(
        stdout.contains("core.required-metadata"),
        "missing owner fires core.required-metadata:\n{stdout}"
    );
}

#[test]
fn roadmap_bad_status_fires_allowed_values_and_empty_claim_fires_min_docs() {
    // RDM-003 verification: an out-of-vocabulary status fires
    // core.allowed-values. RDM-006 verification: claiming ROADMAP with no
    // initiatives fires core.min-docs.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(
        run(root, &["pack", "add", "project-docs"]).status.code(),
        Some(0)
    );

    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("core.min-docs") && stdout.contains("namespace `ROADMAP`"),
        "empty ROADMAP claim fires core.min-docs:\n{stdout}"
    );

    fs::create_dir_all(root.join("docs/roadmap")).unwrap();
    fs::write(
        root.join("docs/roadmap/001-someday.md"),
        clean_roadmap_doc("ROADMAP-1", "someday"),
    )
    .unwrap();
    let out = run(root, &["lint"]);
    assert_eq!(out.status.code(), Some(1), "diagnostics present");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("core.allowed-values"),
        "out-of-vocabulary status fires core.allowed-values:\n{stdout}"
    );
}

/// A complete, valid `living` Stripe integration checklist: the seven phase
/// headings and all twelve inline-tier `@stripe.*` anchors the INTSTRIPE pack
/// requires. `living` (not `sealed`), so checklist.complete/pinned do not gate
/// the unchecked boxes — this isolates core.required-headings/anchors (ADR-085).
fn stripe_checklist_doc() -> String {
    "---\n\
     title: Stripe billing sign-off\n\
     status: living\n\
     ---\n\
     \n\
     ## Plan / account structure\n\
     - [ ] One account per legal entity\n\
     \n\
     ## API key provisioning\n\
     - [ ] Restricted key, least-privilege scopes <!-- @stripe.key-scopes tier=attest -->\n\
     \n\
     ## Secret storage\n\
     - [ ] No secret committed to git <!-- @stripe.no-committed-secret tier=attest -->\n\
     \n\
     ## Implement\n\
     - [ ] getStripe guard <!-- @stripe.getstripe-guard tier=code -->\n\
     - [ ] Wrap SDK calls <!-- @stripe.error-handling tier=code -->\n\
     - [ ] Tax customer update <!-- @stripe.tax-customer-update tier=code -->\n\
     - [ ] Metered line unconditional <!-- @stripe.metered-unconditional tier=code -->\n\
     - [ ] Webhook signature <!-- @stripe.webhook-signature tier=code -->\n\
     - [ ] Webhook idempotency <!-- @stripe.webhook-idempotency tier=code -->\n\
     - [ ] Client posts plan <!-- @stripe.plan-not-priceid tier=code -->\n\
     - [ ] lookup_key resolution <!-- @stripe.atomic-price-resolution tier=code -->\n\
     - [ ] Shared-account webhook scoping <!-- @stripe.app-scope tier=code -->\n\
     \n\
     ## Test\n\
     - [ ] verify_prices confirms amounts\n\
     \n\
     ## Go-live\n\
     - [ ] Key mode matches price source <!-- @stripe.mode-parity tier=attest -->\n\
     \n\
     ## Post-go-live\n\
     - [ ] Lifecycle webhooks handled\n"
        .to_string()
}

#[test]
fn pack_add_stripe_integration_web_writes_intstripe() {
    // SIW-001 / SIW-002 verification: `pack add stripe-integration-web` writes the
    // [INTSTRIPE] block on its own path, binding the checklist.* rules plus the
    // two generic core.* rules with the seven phases and twelve anchors.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let out = run(root, &["pack", "add", "stripe-integration-web"]);
    assert_eq!(out.status.code(), Some(0));
    let toml = fs::read_to_string(root.join("ctxgrd.toml")).unwrap();
    assert!(toml.contains("[INTSTRIPE]"), "[INTSTRIPE] written:\n{toml}");
    assert!(
        toml.contains("docs/integrations/stripe/**"),
        "claims its own path:\n{toml}"
    );
    for needed in [
        "checklist.pinned",
        "core.required-headings",
        "core.required-anchors",
        "Plan / account structure",
        "Post-go-live",
        "@stripe.error-handling tier=code",
        "@stripe.mode-parity tier=attest",
    ] {
        assert!(toml.contains(needed), "contains {needed:?}:\n{toml}");
    }
}

#[test]
fn intstripe_living_checklist_all_present_lints_green() {
    // SIW-001 verification: a living checklist with every phase + anchor is clean.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(
        run(root, &["pack", "add", "stripe-integration-web"])
            .status
            .code(),
        Some(0)
    );
    fs::create_dir_all(root.join("docs/integrations/stripe")).unwrap();
    fs::write(
        root.join("docs/integrations/stripe/billing.md"),
        stripe_checklist_doc(),
    )
    .unwrap();
    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "complete checklist green:\n{stdout}");
}

#[test]
fn intstripe_living_checklist_missing_phase_and_anchor_fires() {
    // SIW-003 verification: dropping one phase heading and one required anchor
    // (its tier included) each fires its rule — the presence contract holds.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(
        run(root, &["pack", "add", "stripe-integration-web"])
            .status
            .code(),
        Some(0)
    );
    fs::create_dir_all(root.join("docs/integrations/stripe")).unwrap();
    let broken = stripe_checklist_doc()
        .replace("## Test\n- [ ] verify_prices confirms amounts\n\n", "")
        .replace(" <!-- @stripe.error-handling tier=code -->", "");
    fs::write(root.join("docs/integrations/stripe/billing.md"), broken).unwrap();
    let out = run(root, &["lint"]);
    assert_eq!(out.status.code(), Some(1), "diagnostics present");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("core.required-headings"),
        "missing phase fires core.required-headings:\n{stdout}"
    );
    assert!(
        stdout.contains("core.required-anchors"),
        "missing anchor fires core.required-anchors:\n{stdout}"
    );
}

// -- governance / DEC (ADR-092) ----------------------------------------

/// A complete, valid `DEC` decision record satisfying GOV-003.
fn clean_dec_doc(id: &str, status: &str) -> String {
    format!(
        "---\nid: {id}\ntitle: Adopt the vendor\nstatus: {status}\n\
         decision-maker: Program Board\ndate: 2026-07-10\n---\n\n\
         ## Decision\nWe will adopt the vendor.\n\n\
         ## Rationale\nBuild cost exceeds buy cost within a year.\n\n\
         ## Impact\nCuts the schedule by two months; adds a licence line to the budget.\n\n\
         ## Approval\nApproved by the Program Board on 2026-07-10.\n"
    )
}

#[test]
fn pack_list_includes_governance() {
    // GOV-001: the governance pack is a distinct builtin pack.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("governance"), "lists governance:\n{stdout}");
}

#[test]
fn pack_show_governance_lists_dec_and_project_docs_has_none() {
    // GOV-001: the governance pack exposes exactly DEC; project-docs is
    // unchanged (a governance register is not folded into the front door).
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "show", "governance"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("[DEC]"), "governance shows [DEC]:\n{stdout}");

    let docs = run(tmp.path(), &["pack", "show", "project-docs"]);
    let docs_out = String::from_utf8(docs.stdout).unwrap();
    assert!(
        !docs_out.contains("[DEC]"),
        "project-docs must list no DEC:\n{docs_out}"
    );
}

#[test]
fn governance_clean_decision_lints_green() {
    // GOV-002/003 verification: a complete DEC record lints clean. governance
    // ships no core.min-docs, so an empty claim does not nag.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(
        run(root, &["pack", "add", "governance"]).status.code(),
        Some(0)
    );
    fs::create_dir_all(root.join("docs/decisions")).unwrap();
    fs::write(
        root.join("docs/decisions/001-adopt-the-vendor.md"),
        clean_dec_doc("DEC-1", "approved"),
    )
    .unwrap();

    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "clean DEC record lints green:\n{stdout}"
    );
}

#[test]
fn governance_glob_claims_numbered_records_and_spares_the_index() {
    // CR-006: filenames are NNN-<slug>.md — the namespace lives in the id, not
    // the filename. The [0-9]* shape is what keeps a human README index in the
    // register directory unclaimed; a frontmatter-less index would otherwise
    // fire core.frontmatter the way docs/adrs/README.md does under docs/adrs/**.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(
        run(root, &["pack", "add", "governance"]).status.code(),
        Some(0)
    );
    let dir = root.join("docs/decisions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("001-adopt-the-vendor.md"), clean_dec_doc("DEC-1", "approved")).unwrap();
    fs::write(
        dir.join("README.md"),
        "# Decision register\n\n| id | title |\n| -- | ----- |\n",
    )
    .unwrap();

    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "numbered record claimed, README index left alone:\n{stdout}"
    );
}

#[test]
fn governance_old_prefixed_filename_still_claimed_by_id() {
    // CR-006 compatibility: [DEC] is id-claimed, so the paths glob does not gate
    // documents that carry a matching id. A record written under the pre-1.1.0
    // DEC-*.md convention keeps linting after the glob narrows to [0-9]*.md —
    // the rename is cosmetic, not a migration.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(
        run(root, &["pack", "add", "governance"]).status.code(),
        Some(0)
    );
    let dir = root.join("docs/decisions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("DEC-1-adopt-the-vendor.md"),
        clean_dec_doc("DEC-1", "approved"),
    )
    .unwrap();
    assert_eq!(
        run(root, &["lint"]).status.code(),
        Some(0),
        "a well-formed record under the old filename still lints clean"
    );

    // Clean-and-claimed and never-claimed are indistinguishable from a green
    // run, so prove the claim positively: break the record and require the
    // namespace's own rule to fire on it.
    fs::write(
        dir.join("DEC-1-adopt-the-vendor.md"),
        clean_dec_doc("DEC-1", "tabled"),
    )
    .unwrap();
    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(1), "diagnostics present:\n{stdout}");
    assert!(
        stdout.contains("core.allowed-values"),
        "the old-named record is still governed by [DEC], not silently skipped:\n{stdout}"
    );
}

#[test]
fn governance_malformed_decision_fires_expected_diagnostics() {
    // GOV-003 verification: a record missing the Impact heading, missing the
    // decision-maker key, and carrying an out-of-vocabulary status each fire
    // their own diagnostic.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(
        run(root, &["pack", "add", "governance"]).status.code(),
        Some(0)
    );
    let dir = root.join("docs/decisions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("001-no-impact.md"),
        clean_dec_doc("DEC-1", "approved")
            .replace("\n## Impact\nCuts the schedule by two months; adds a licence line to the budget.\n", ""),
    )
    .unwrap();
    fs::write(
        dir.join("002-no-authority.md"),
        clean_dec_doc("DEC-2", "approved").replace("decision-maker: Program Board\n", ""),
    )
    .unwrap();
    fs::write(
        dir.join("003-bad-status.md"),
        clean_dec_doc("DEC-3", "tabled"),
    )
    .unwrap();

    let out = run(root, &["lint"]);
    assert_eq!(out.status.code(), Some(1), "diagnostics present");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("core.required-headings"),
        "missing Impact fires core.required-headings:\n{stdout}"
    );
    assert!(
        stdout.contains("core.required-metadata"),
        "missing decision-maker fires core.required-metadata:\n{stdout}"
    );
    assert!(
        stdout.contains("core.allowed-values"),
        "out-of-vocabulary status fires core.allowed-values:\n{stdout}"
    );
}

// -- qa pack (ADR-098) ------------------------------------------------

/// A complete, clean sealed Test Completion Report body. The pins are
/// 40-hex commit SHAs (shape-only, never resolved against git). No
/// `depends_on`, so core.dep-resolved is a no-op in the self-contained
/// fixture; the pinned trace itself is `test.completion`'s concern.
fn clean_test_report(id: &str) -> String {
    format!(
        "---\nid: {id}\ntitle: Release 1.0 completion\nstatus: sealed\n\
         result: pass\nrelease: 1.0\ndate: 2026-07-13\n\
         evidence: https://ci.example.com/runs/4210\n\
         tested_commit: 1cb8eaf0aa9b7d2e3f4c5a6b7c8d9e0f1a2b3c4d\n\
         spec_commit: 50c6166f9e8d7c6b5a4f3e2d1c0b9a8776554433\n---\n\n\
         ## Scope\nSystem and acceptance suites for release 1.0.\n\n\
         ## Test Environment\nstaging, build 4210.\n\n\
         ## Results Summary\n204 passed, 0 failed.\n\n\
         ## Outstanding Defects\nNone.\n\n\
         ## Exit Criteria\nAll release gates met.\n\n\
         ## Sign-off\nQA lead accepted on 2026-07-13.\n\n\
         ## References\nSPEC-003; CI run 4210.\n"
    )
}

#[test]
fn pack_list_includes_qa() {
    // QA-001: the qa pack is a distinct builtin pack.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("qa"), "lists qa:\n{stdout}");
}

#[test]
fn pack_show_qa_lists_test_namespace() {
    // QA-001: the qa pack exposes the TEST completion-report namespace (the
    // pack is named qa, but the record namespace is [TEST], id: TEST-<N>).
    // project-docs is unchanged.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "show", "qa"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("[TEST]"), "qa shows [TEST]:\n{stdout}");
    assert!(
        stdout.contains("test.completion"),
        "qa binds test.completion:\n{stdout}"
    );

    let docs = run(tmp.path(), &["pack", "show", "project-docs"]);
    let docs_out = String::from_utf8(docs.stdout).unwrap();
    assert!(
        !docs_out.contains("[TEST]"),
        "project-docs must list no TEST:\n{docs_out}"
    );
}

#[test]
fn pack_add_qa_writes_test_block() {
    // QA-001 verification: `pack add qa` writes the [TEST] namespace binding
    // test.completion. qa declares no `depends`, so no base pack is pulled.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["pack", "add", "qa"]);
    assert_eq!(out.status.code(), Some(0));
    let result = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();
    assert!(result.contains("[TEST]"), "[TEST] written:\n{result}");
    assert!(
        result.contains("test.completion"),
        "binds test.completion:\n{result}"
    );
    assert!(
        result.contains("docs/tests/**"),
        "path-claims docs/tests/**:\n{result}"
    );
}

#[test]
fn qa_clean_sealed_report_lints_green() {
    // QA-001/002/003 verification: a complete sealed completion report with
    // both commit-SHA pins and a filled-in body lints clean. qa ships no
    // core.min-docs, so an empty claim does not nag.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(run(root, &["pack", "add", "qa"]).status.code(), Some(0));
    fs::create_dir_all(root.join("docs/tests")).unwrap();
    fs::write(
        root.join("docs/tests/TEST-1-release-1-0.md"),
        clean_test_report("TEST-1"),
    )
    .unwrap();

    let out = run(root, &["lint"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "clean sealed completion report lints green:\n{stdout}"
    );
}

#[test]
fn qa_sealed_report_missing_pin_and_empty_waiver_fire() {
    // QA-003 verification end-to-end: a sealed report missing spec_commit fires
    // test.completion, and a conditional-pass with an empty Outstanding Defects
    // section fires it too.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(run(root, &["pack", "add", "qa"]).status.code(), Some(0));
    let dir = root.join("docs/tests");
    fs::create_dir_all(&dir).unwrap();

    // Missing spec_commit.
    fs::write(
        dir.join("TEST-1-no-spec-pin.md"),
        clean_test_report("TEST-1")
            .replace("spec_commit: 50c6166f9e8d7c6b5a4f3e2d1c0b9a8776554433\n", ""),
    )
    .unwrap();
    // conditional-pass verdict but the Outstanding Defects section is empty.
    fs::write(
        dir.join("TEST-2-empty-waiver.md"),
        clean_test_report("TEST-2")
            .replace("result: pass", "result: conditional-pass")
            .replace("## Outstanding Defects\nNone.\n", "## Outstanding Defects\n"),
    )
    .unwrap();

    let out = run(root, &["lint"]);
    assert_eq!(out.status.code(), Some(1), "diagnostics present");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("test.completion"),
        "test.completion fires:\n{stdout}"
    );
    assert!(
        stdout.contains("spec_commit"),
        "names the missing pin:\n{stdout}"
    );
    assert!(
        stdout.contains("conditional-pass"),
        "names the empty waiver:\n{stdout}"
    );
}

// -- `pack outdated`'s three states (ADR-126) ---------------------------

/// A config carrying one customized block with a bare v1 stamp: the shape
/// that reaches the no-baseline category.
fn config_with_a_baseline_less_block(root: &Path) {
    run(root, &["init"]);
    run(root, &["pack", "add", "claude"]);
    let path = root.join("ctxgrd.toml");
    let toml = fs::read_to_string(&path).unwrap();
    // Strip the fingerprint off CLAUDEAGENTS' stamp and customize the block,
    // exactly as a config written before v2 provenance would look.
    let toml = toml
        .replace(
            &format!("# pack: claude@{} sha:", env!("CARGO_PKG_VERSION")),
            "# pack: claude sha-was:",
        )
        .replace(
            "[CLAUDEAGENTS]\n",
            "[CLAUDEAGENTS]\nowner = \"developer\"\n",
        );
    fs::write(&path, toml).unwrap();
}

#[test]
fn outdated_does_not_set_the_exit_code_for_a_block_with_no_baseline() {
    // DRF-008's central claim. A block whose pack-moved question cannot be
    // asked must be reported and must NOT fail the gate — otherwise every
    // config predating v2 provenance is permanently red, which is the defect
    // ADR-126 exists to remove.
    let tmp = tempfile::tempdir().unwrap();
    config_with_a_baseline_less_block(tmp.path());

    let out = run(tmp.path(), &["pack", "outdated"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("carry no baseline") && stdout.contains("CLAUDEAGENTS"),
        "the block is reported by name:\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0), "…but never sets exit 1:\n{stdout}");
    // The remedy must not name a command that cannot deliver one.
    assert!(
        !stdout.contains("gains one the next time"),
        "no unactionable remedy:\n{stdout}"
    );
}

#[test]
fn outdated_json_separates_the_three_states_without_parsing_text() {
    // CLAUDE.md's agent-drivability rule: an agent must be able to branch on
    // drift / no-baseline / current from JSON alone.
    let tmp = tempfile::tempdir().unwrap();
    config_with_a_baseline_less_block(tmp.path());

    let out = run(tmp.path(), &["pack", "outdated", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is a clean JSON stream");

    // Nothing drifted: no pack moved under any of these blocks.
    assert_eq!(json["diffs"].as_array().unwrap().len(), 0, "{json}");

    // The customized bare-stamped block lands in `unknown`, carrying the pack
    // to act on and the digest that would resolve it.
    let unknown: Vec<(&str, &str)> = json["unknown"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (
                u["namespace"].as_str().unwrap(),
                u["pack"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(unknown.contains(&("CLAUDEAGENTS", "claude")), "{json}");
    assert!(
        json["unknown"]
            .as_array()
            .unwrap()
            .iter()
            .all(|u| u["fingerprint"].as_str().is_some_and(|f| f.len() == 16)),
        "every row carries the digest an agent would write back:\n{json}"
    );

    // `init`'s own [ADR]/[PRD] used to be here too — stamped by nothing, they
    // were unresolvable in a file ctxgrd had just written (BUG-071). This
    // assertion is the inversion of what this test originally pinned.
    assert!(
        !unknown.contains(&("ADR", "project-docs")),
        "init stamps what it writes:\n{json}"
    );

    // The untouched blocks that already match their pack are stamp-only
    // rewrites — housekeeping migrate will do, which is why exit is still 0.
    let rewrites = json["rewrites"].as_array().unwrap();
    assert!(!rewrites.is_empty(), "{json}");
    assert!(
        rewrites.iter().all(|r| r["stamp_only"] == true),
        "no real swap is pending:\n{json}"
    );
}

#[test]
fn outdated_is_silent_and_clean_when_a_customization_is_the_only_divergence() {
    // BUG-067's repro, end to end through the binary: the linter asks for
    // `owner`, so adding it must not make the pack gate red.
    let tmp = tempfile::tempdir().unwrap();
    run(tmp.path(), &["init"]);
    run(tmp.path(), &["pack", "add", "claude"]);
    let path = tmp.path().join("ctxgrd.toml");
    let toml = fs::read_to_string(&path)
        .unwrap()
        .replace("[CLAUDEAGENTS]\n", "[CLAUDEAGENTS]\nowner = \"developer\"\n");
    fs::write(&path, toml).unwrap();

    let out = run(tmp.path(), &["pack", "outdated"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        !stdout.contains("CLAUDEAGENTS"),
        "a customized-but-current block is silent:\n{stdout}"
    );

    // And migrate leaves the edit alone (DRF-007).
    run(tmp.path(), &["pack", "migrate"]);
    assert!(fs::read_to_string(&path)
        .unwrap()
        .contains("owner = \"developer\""));
}

/// The digest a baseline-less block needs is on the row that reported it
/// (ADR-126 § DRF-008). Without it the remedy is a cross-reference: read the
/// pack name here, then go find the namespace in `pack show`'s array.
#[test]
fn outdated_json_carries_the_digest_each_baseline_less_block_needs() {
    let tmp = tempfile::tempdir().unwrap();
    run(tmp.path(), &["init"]);
    run(tmp.path(), &["pack", "add", "intake"]);
    let path = tmp.path().join("ctxgrd.toml");

    // A customized block under a v1 (`sha:`-less) stamp: the one state whose
    // pack-moved question has no answer.
    let toml = fs::read_to_string(&path)
        .unwrap()
        .replace("[CR]\n", "[CR]\nowner = \"product-strategist\"\n");
    fs::write(&path, set_stamp_sha(&toml, "CR", None)).unwrap();

    let out = run(tmp.path(), &["pack", "outdated", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    let plan: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("outdated emits valid JSON");
    let cr = plan["unknown"]
        .as_array()
        .expect("unknown array")
        .iter()
        .find(|u| u["namespace"] == "CR")
        .expect("[CR] has no baseline");
    assert_eq!(
        cr["fingerprint"], "79ebf75f5c1a1492",
        "the row names the digest that would resolve it"
    );
}

/// `docs/guides/keeping-packs-current.md` § "Blocks with no baseline" tells a
/// reader to read the current digest and write it into the stamp. Performed
/// literally here, because a remedy nobody executed is a remedy nobody can.
#[test]
fn the_guides_manual_remedy_clears_a_baseline_less_block() {
    let tmp = tempfile::tempdir().unwrap();
    run(tmp.path(), &["init"]);
    run(tmp.path(), &["pack", "add", "intake"]);
    let path = tmp.path().join("ctxgrd.toml");
    let toml = fs::read_to_string(&path)
        .unwrap()
        .replace("[CR]\n", "[CR]\nowner = \"product-strategist\"\n");
    fs::write(&path, set_stamp_sha(&toml, "CR", None)).unwrap();

    let before = String::from_utf8(run(tmp.path(), &["pack", "outdated"]).stdout).unwrap();
    assert!(
        before.contains("no baseline") && before.contains("CR"),
        "precondition — [CR] is listed as baseline-less:\n{before}"
    );

    // Guide step: read the fingerprint for the namespace off `pack show`.
    let show: serde_json::Value =
        serde_json::from_slice(&run(tmp.path(), &["pack", "show", "intake", "--format", "json"]).stdout)
            .unwrap();
    let digest = show["namespaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ns| ns["namespace"] == "CR")
        .unwrap()["fingerprint"]
        .as_str()
        .unwrap()
        .to_owned();

    // Guide step: append it to that block's `# pack:` line as `sha:<digest>`.
    let toml = fs::read_to_string(&path).unwrap();
    let patched = set_stamp_sha(&toml, "CR", Some(&digest));
    assert_ne!(patched, toml, "the stamp was rewritten");
    fs::write(&path, &patched).unwrap();

    let out = run(tmp.path(), &["pack", "outdated"]);
    let after = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{after}");
    assert!(
        !after.contains("[CR]"),
        "the remedy clears the block it was applied to:\n{after}"
    );
    // And clears it as *current*, not by demoting it to drift — the whole
    // point is that a customized block with a good baseline is silent.
    assert!(
        !after.contains("hand-edited"),
        "the owner edit is still not drift:\n{after}"
    );
}

/// Rewrite the `sha:` on the `# pack:` line that stamps `[ns]` — the line
/// immediately above it. `sha = None` reproduces a v1 stamp written before
/// ADR-126; `Some(d)` is the manual baseline the guide describes. Only that
/// one block's stamp moves: a whole-file replace would also restamp its
/// neighbours with the wrong digest, which is a different test.
fn set_stamp_sha(toml: &str, ns: &str, sha: Option<&str>) -> String {
    let header = format!("[{ns}]");
    let mut lines: Vec<String> = toml.lines().map(str::to_string).collect();
    let at = lines
        .iter()
        .position(|l| l.trim() == header)
        .unwrap_or_else(|| panic!("[{ns}] is in the config"));
    let stamp = at
        .checked_sub(1)
        .filter(|&i| lines[i].starts_with("# pack:"))
        .unwrap_or_else(|| panic!("[{ns}] is stamped"));
    let bare = match lines[stamp].find(" sha:") {
        Some(cut) => lines[stamp][..cut].to_string(),
        None => lines[stamp].clone(),
    };
    lines[stamp] = match sha {
        Some(d) => format!("{bare} sha:{d}"),
        None => bare,
    };
    lines.join("\n") + "\n"
}

/// BUG-071/BUG-052: ctxgrd's own scaffold must pass ctxgrd's own gate. A
/// virgin `init` wrote no `# pack:` stamp at all and hardcoded a `rules` array
/// the pack had outgrown, so a project that wires `pack outdated` into CI —
/// which `docs/ci.md` presents as supported — started red on day one. The
/// absence of this test is why it survived thirteen months of releases.
#[test]
fn a_virgin_init_reports_nothing_from_pack_outdated() {
    let tmp = tempfile::tempdir().unwrap();
    let init = run(tmp.path(), &["init"]);
    assert_eq!(init.status.code(), Some(0));

    let out = run(tmp.path(), &["pack", "outdated"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        !stdout.contains("no baseline") && !stdout.contains("hand-edited"),
        "a config ctxgrd just wrote has nothing outstanding against the packs \
         it was written from:\n{stdout}"
    );
    assert!(
        stdout.contains("up to date"),
        "and says so positively:\n{stdout}"
    );
}

/// The other half of BUG-052: `init` must bind what the pack binds. Asserted
/// against the pack on disk rather than a list, so a rule added to
/// `project-docs` tomorrow is covered without editing this test.
#[test]
fn init_binds_every_rule_the_owning_pack_binds() {
    let tmp = tempfile::tempdir().unwrap();
    run(tmp.path(), &["init"]);
    let written = fs::read_to_string(tmp.path().join("ctxgrd.toml")).unwrap();

    let pack_toml = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("packs/project-docs/pack.toml"),
    )
    .unwrap();
    let pack: toml::Table = pack_toml.parse().unwrap();
    let config: toml::Table = written.parse().expect("init writes parseable TOML");

    for ns in ["ADR", "PRD"] {
        // `core.min-docs` asserts a document exists, which is false in the
        // repo `init` was just run on. It is offered commented instead, so
        // first-touch stays silent (ADR-007 § DOC-001) without the rule going
        // missing. Every other rule the pack binds must be bound.
        let expected: Vec<&str> = pack[ns]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .filter(|r| *r != "core.min-docs")
            .collect();
        let actual: Vec<&str> = config[ns]["rules"]
            .as_array()
            .unwrap_or_else(|| panic!("init writes [{ns}].rules"))
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(
            actual, expected,
            "[{ns}]: init must render the pack's rule list, not a parallel copy"
        );
    }

    // The deferred rule is offered, not dropped — the reader must be able to
    // see that the pack binds it.
    assert!(
        written.contains("# \"core.min-docs\","),
        "min-docs is offered commented:\n{written}"
    );

    // And a fresh config still lints clean, which is the whole reason for
    // the exception.
    let out = run(tmp.path(), &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a config ctxgrd just wrote reports nothing:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // And the two things init adds that no pack does are still there
    // (owner: ADR-076 seeds it; the commented alternative: CLAUDE.md
    // advertises it). Both are safe now — the stamp is a pack-side digest.
    assert!(written.contains("owner = "), "init still seeds owner");
    assert!(
        written.contains("Conventional minimal shape"),
        "init still offers the Nygard four-heading alternative"
    );
}
