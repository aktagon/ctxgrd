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
