//! SPEC-002 acceptance suite — the single consolidated integration
//! harness for `ctxgrd status` and the `pipeline.conformance` lint rule
//! (sprint-plan T4.3). One test per SPEC-002 § Acceptance scenario, run
//! by `cargo test` / `make ci`:
//!
//! 1. Cold start → `source: default`, ladder restricted to active
//!    namespaces (EARS-01.3/01.4).
//! 2. Inference with the direct edge reduced away (EARS-01.2), plus the
//!    declared-wins and namespace-cycle cases (EARS-01.1/01.5).
//! 3. Branch join waits on both parents (EARS-02.6).
//! 4. Accepted-but-lint-failing SPEC holds its stage (EARS-02.2), plus
//!    the accepted-clean positive.
//! 5. Open-BUG tripwire blocks then releases (EARS-03.1/03.2).
//! 6. `pipeline.conformance` flags a stage-skipping edge (EARS-06.2),
//!    plus the unstaged-namespace exemption (EARS-06.3).
//! 7. `--format json` validates against the Data model schema
//!    (EARS-04.2).
//! 8. Read-only invariant — the tree is byte-identical before/after a
//!    `status` run (EARS-05.3).
//!
//! The exit-code matrix (EARS-05.1/05.2) is pinned end-to-end by
//! `exit_code_matrix_success_config_error_and_cycle`.

use std::fs;
use std::path::Path;

use assert_cmd::Command;

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Run `ctxgrd status` against `root`, isolated from any real
/// `~/.ctxgrd` global config.
fn status(root: &Path) -> std::process::Output {
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", root)
        .args(["status", "--root", root.to_str().unwrap()])
        .output()
        .expect("ctxgrd status runs")
}

/// Namespace blocks with no rules — enough to make each namespace
/// active without dragging rule fixtures into a DAG-resolution test.
const FOUR_NAMESPACES: &str = "\
[PRD]\nrules = []\n
[ADR]\nrules = []\n
[SPEC]\nrules = []\n
[TASK]\nrules = []\n";

#[test]
fn scenario_1_cold_start_resolves_default_ladder() {
    // SPEC-002 § Acceptance scenario 1: a single accepted PRD and no
    // dependency edges. The DAG must come from the built-in ladder and
    // SAY SO (`source: default`) — no pretending it was derived
    // (EARS-01.3, EARS-01.4).
    let tmp = tempfile::tempdir().expect("tempdir");
    write(&tmp.path().join("ctxgrd.toml"), FOUR_NAMESPACES);
    write(
        &tmp.path().join("docs/prds/001-billing-reconciliation.md"),
        "---\nid: PRD-001\ntitle: Billing reconciliation\nstatus: accepted\n---\n\n# PRD-001: Billing reconciliation\n",
    );

    let out = status(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(
        stdout.contains("source: default"),
        "EARS-01.4: the default ladder must be labeled `default`; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("PRD → ADR → SPEC → TASK"),
        "EARS-01.3: built-in ladder restricted to active namespaces; stdout:\n{stdout}"
    );
}

#[test]
fn scenario_1_default_ladder_restricted_to_active_namespaces() {
    // Same cold start, but only ADR + SPEC are active — the ladder
    // must shrink to the active subset, not invent PRD/TASK stages.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[SPEC]\nrules = []\n",
    );

    let out = status(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(stdout.contains("source: default"), "stdout:\n{stdout}");
    assert!(stdout.contains("ADR → SPEC"), "stdout:\n{stdout}");
    assert!(!stdout.contains("PRD"), "stdout:\n{stdout}");
    assert!(!stdout.contains("TASK"), "stdout:\n{stdout}");
}

#[test]
fn scenario_2_inferred_chain_reduces_direct_edge() {
    // SPEC-002 § Acceptance scenario 2: PRD ← ADR ← SPEC edge set,
    // with SPEC also citing PRD directly. The inferred chain is
    // PRD → ADR → SPEC and the direct PRD → SPEC edge is reduced away
    // (EARS-01.2).
    let tmp = tempfile::tempdir().expect("tempdir");
    write(&tmp.path().join("ctxgrd.toml"), FOUR_NAMESPACES);
    write(
        &tmp.path().join("docs/prds/001-billing-reconciliation.md"),
        "---\nid: PRD-001\ntitle: Billing reconciliation\nstatus: accepted\n---\n\n# PRD-001\n",
    );
    write(
        &tmp.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: accepted\ndepends_on: [PRD-001]\n---\n\n# ADR-001\n",
    );
    write(
        &tmp.path().join("docs/specs/001-reconciliation-engine.md"),
        "---\nid: SPEC-001\ntitle: Reconciliation engine\nstatus: draft\ndepends_on: [ADR-001, PRD-001]\n---\n\n# SPEC-001\n",
    );

    let out = status(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(
        stdout.contains("source: inferred"),
        "EARS-01.4: a derived DAG must be labeled `inferred`; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("PRD → ADR → SPEC"),
        "EARS-01.2: lifted chain; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("PRD → SPEC"),
        "EARS-01.2: the direct PRD → SPEC edge must be reduced away; stdout:\n{stdout}"
    );
}

#[test]
fn declared_pipeline_wins_over_inference() {
    // EARS-01.1: a declared `[pipeline]` is used verbatim — even when
    // dep edges imply a different order — and labeled `declared`.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[SPEC]\nrules = []\n\n[pipeline]\nstages = [\"SPEC\", \"ADR\"]\n",
    );
    write(
        &tmp.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: accepted\n---\n\n# ADR-001\n",
    );
    write(
        &tmp.path().join("docs/specs/001-reconciliation-engine.md"),
        "---\nid: SPEC-001\ntitle: Reconciliation engine\nstatus: draft\ndepends_on: [ADR-001]\n---\n\n# SPEC-001\n",
    );

    let out = status(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(stdout.contains("source: declared"), "stdout:\n{stdout}");
    assert!(stdout.contains("SPEC → ADR"), "stdout:\n{stdout}");
}

#[test]
fn namespace_cycle_reports_and_exits_non_zero() {
    // EARS-01.5: ADR-001 cites SPEC-001 while SPEC-002 cites ADR-001 —
    // no document-level cycle, but ADR ↔ SPEC at namespace level.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[SPEC]\nrules = []\n",
    );
    write(
        &tmp.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: accepted\ndepends_on: [SPEC-001]\n---\n\n# ADR-001\n",
    );
    write(
        &tmp.path().join("docs/specs/001-reconciliation-engine.md"),
        "---\nid: SPEC-001\ntitle: Reconciliation engine\nstatus: accepted\n---\n\n# SPEC-001\n",
    );
    write(
        &tmp.path().join("docs/specs/002-settlement-feed.md"),
        "---\nid: SPEC-002\ntitle: Settlement feed\nstatus: draft\ndepends_on: [ADR-001]\n---\n\n# SPEC-002\n",
    );

    let out = status(tmp.path());
    let stderr = String::from_utf8(out.stderr).expect("stderr utf-8");
    assert_ne!(
        out.status.code(),
        Some(0),
        "EARS-01.5: a namespace cycle must exit non-zero; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("pipeline.namespace-cycle"),
        "the cycle must be reported with a named code; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("ADR") && stderr.contains("SPEC"),
        "the cycle members must be named; stderr:\n{stderr}"
    );
}

#[test]
fn scenario_3_branch_join_waits_on_both_parents() {
    // SPEC-002 § Acceptance scenario 3 (EARS-02.6): SPEC depends on
    // both ADR and DESIGN. SPEC is itself accepted — its own gate is
    // satisfied — but DESIGN is still a draft, so the join stage must
    // wait: SPEC is pending, not done. The discriminator: without
    // parent-gating SPEC would read `done`.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[DESIGN]\nrules = []\n\n[SPEC]\nrules = []\n",
    );
    write(
        &tmp.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: accepted\n---\n\n# ADR-001\n",
    );
    write(
        &tmp.path().join("docs/designs/001-ledger-ui.md"),
        "---\nid: DESIGN-001\ntitle: Ledger UI\nstatus: draft\n---\n\n# DESIGN-001\n",
    );
    write(
        &tmp.path().join("docs/specs/001-reconciliation-engine.md"),
        "---\nid: SPEC-001\ntitle: Reconciliation engine\nstatus: accepted\ndepends_on: [ADR-001, DESIGN-001]\n---\n\n# SPEC-001\n",
    );

    let out = status(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(stdout.contains("ADR: done"), "stdout:\n{stdout}");
    assert!(stdout.contains("DESIGN: current"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("SPEC: pending"),
        "EARS-02.6: the join stage must wait on the unfinished DESIGN parent; stdout:\n{stdout}"
    );
}

#[test]
fn scenario_4_accepted_but_lint_failing_spec_holds_its_stage() {
    // SPEC-002 § Acceptance scenario 4 (EARS-02.2): an accepted SPEC
    // that fails core.required-headings carries a terminal status but
    // produces a diagnostic — its stage is held, not done, and the
    // failing document is named.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        r#"
[SPEC]
paths = ["docs/specs/**"]
rules = ["core.frontmatter", "core.required-headings"]
[SPEC."core.required-headings"]
headings = ["Context", "Decision"]

[pipeline]
stages = ["SPEC"]
"#,
    );
    // Accepted, but missing the required "Decision" heading.
    write(
        &tmp.path().join("docs/specs/001-reconciliation-engine.md"),
        "---\nid: SPEC-001\ntitle: Reconciliation engine\nstatus: accepted\n---\n\n# SPEC-001\n\n## Context\n\nBody.\n",
    );

    let out = status(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stage position is data, not an error (EARS-05.1); stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("SPEC: done"),
        "EARS-02.2: an accepted-but-failing SPEC must NOT be done; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("held") && stdout.contains("SPEC-001"),
        "EARS-02.2: the held stage must name the failing document; stdout:\n{stdout}"
    );
}

#[test]
fn accepted_clean_single_stage_is_done() {
    // The positive of scenario 4: an accepted, lint-clean SPEC under a
    // single-stage pipeline is done (EARS-02.1).
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        r#"
[SPEC]
paths = ["docs/specs/**"]
rules = ["core.frontmatter", "core.required-headings"]
[SPEC."core.required-headings"]
headings = ["Context", "Decision"]

[pipeline]
stages = ["SPEC"]
"#,
    );
    write(
        &tmp.path().join("docs/specs/001-reconciliation-engine.md"),
        "---\nid: SPEC-001\ntitle: Reconciliation engine\nstatus: accepted\n---\n\n# SPEC-001\n\n## Context\n\nB.\n\n## Decision\n\nB.\n",
    );

    let out = status(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(stdout.contains("SPEC: done"), "stdout:\n{stdout}");
}

#[test]
fn invalid_pipeline_config_exits_non_zero_with_named_error() {
    // EARS-05.2 (parse-time half, T1.1) surfaced through the
    // subcommand: a stage naming an inactive namespace is a config
    // error, not a silent fallback.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[pipeline]\nstages = [\"ADR\", \"SPEC\"]\n",
    );

    let out = status(tmp.path());
    let stderr = String::from_utf8(out.stderr).expect("stderr utf-8");
    assert_eq!(out.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(
        stderr.contains("cfg.pipeline-stage-unknown"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn scenario_5_open_bug_citing_lineage_blocks_then_releases() {
    // SPEC-002 § Acceptance scenario 5 (EARS-03.1/03.2): an `open` BUG
    // citing the lineage SPEC blocks the pipeline and is named; once the
    // BUG leaves `open`, the block clears. A declared single-stage
    // pipeline keeps BUG out of the DAG (it is not a pipeline stage).
    let config = "[SPEC]\nrules = []\n\n[BUG]\nrules = []\n\n[pipeline]\nstages = [\"SPEC\"]\n";

    // Blocked: BUG-001 is open and cites SPEC-001.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(&tmp.path().join("ctxgrd.toml"), config);
    write(
        &tmp.path().join("docs/specs/001-reconciliation-engine.md"),
        "---\nid: SPEC-001\ntitle: Reconciliation engine\nstatus: draft\n---\n\n# SPEC-001\n",
    );
    write(
        &tmp.path().join("docs/bugs/001-reconciliation-drift.md"),
        "---\nid: BUG-001\ntitle: Reconciliation drift\nstatus: open\ndepends_on: [SPEC-001]\n---\n\n# BUG-001\n",
    );

    let out = status(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(
        out.status.code(),
        Some(0),
        "EARS-05.1: a blocked pipeline is still a successful computation; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("blocked"),
        "EARS-03.1: the open BUG must block the pipeline; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("BUG-001"),
        "EARS-03.1: the citing BUG must be named; stdout:\n{stdout}"
    );

    // Released: the same fixture with BUG-001 marked `fixed`.
    let tmp2 = tempfile::tempdir().expect("tempdir");
    write(&tmp2.path().join("ctxgrd.toml"), config);
    write(
        &tmp2.path().join("docs/specs/001-reconciliation-engine.md"),
        "---\nid: SPEC-001\ntitle: Reconciliation engine\nstatus: draft\n---\n\n# SPEC-001\n",
    );
    write(
        &tmp2.path().join("docs/bugs/001-reconciliation-drift.md"),
        "---\nid: BUG-001\ntitle: Reconciliation drift\nstatus: fixed\ndepends_on: [SPEC-001]\n---\n\n# BUG-001\n",
    );

    let out2 = status(tmp2.path());
    let stdout2 = String::from_utf8(out2.stdout).expect("stdout utf-8");
    assert_eq!(out2.status.code(), Some(0), "stdout:\n{stdout2}");
    assert!(
        !stdout2.contains("blocked"),
        "EARS-03.2: a non-open BUG must not block; stdout:\n{stdout2}"
    );
    assert!(
        stdout2.contains("SPEC: current"),
        "with the block cleared, the draft SPEC is simply the current stage; stdout:\n{stdout2}"
    );
}

/// Run `ctxgrd status --format json` against `root`.
fn status_json(root: &Path) -> std::process::Output {
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", root)
        .args([
            "status",
            "--format",
            "json",
            "--root",
            root.to_str().unwrap(),
        ])
        .output()
        .expect("ctxgrd status runs")
}

#[test]
fn scenario_7_json_output_validates_against_schema() {
    // SPEC-002 § Acceptance scenario 7 (EARS-04.2): `--format json`
    // emits a single object matching the Data model schema —
    // source / stages(namespace,state,docs,verdict) / current /
    // blockers / next_action.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[SPEC]\nrules = []\n\n[pipeline]\nstages = [\"ADR\", \"SPEC\"]\n",
    );
    write(
        &tmp.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: accepted\n---\n\n# ADR-001\n",
    );
    write(
        &tmp.path().join("docs/specs/001-reconciliation-engine.md"),
        "---\nid: SPEC-001\ntitle: Reconciliation engine\nstatus: draft\ndepends_on: [ADR-001]\n---\n\n# SPEC-001\n",
    );

    let out = status_json(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("status --format json emits valid JSON");

    // Top-level keys and types (SPEC-002 § Data model).
    assert!(parsed["source"].is_string(), "json:\n{parsed:#}");
    assert_eq!(parsed["source"], "declared");
    assert!(parsed["stages"].is_array(), "json:\n{parsed:#}");
    assert!(
        parsed["current"].is_string() || parsed["current"].is_null(),
        "json:\n{parsed:#}"
    );
    assert!(parsed["blockers"].is_array(), "json:\n{parsed:#}");
    assert!(parsed["next_action"].is_string(), "json:\n{parsed:#}");

    // Each stage carries exactly namespace / state / docs / verdict.
    let stages = parsed["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 2);
    for stage in stages {
        assert!(stage["namespace"].is_string(), "json:\n{parsed:#}");
        assert!(stage["state"].is_string(), "json:\n{parsed:#}");
        assert!(stage["docs"].is_array(), "json:\n{parsed:#}");
        assert!(stage["verdict"].is_string(), "json:\n{parsed:#}");
    }
    // ADR accepted+clean → done; SPEC draft → current.
    assert_eq!(stages[0]["namespace"], "ADR");
    assert_eq!(stages[0]["state"], "done");
    assert_eq!(stages[1]["namespace"], "SPEC");
    assert_eq!(stages[1]["state"], "current");
    assert_eq!(parsed["current"], "SPEC");
}

/// Snapshot every file under `root` as (relative path → bytes), sorted.
/// A "tree hash" in the strongest form — full content equality — so a
/// stray write of any kind is caught (acceptance scenario 8).
fn snapshot_tree(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn walk(
        dir: &Path,
        base: &Path,
        acc: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, base, acc);
            } else {
                let rel = path.strip_prefix(base).expect("under base").to_path_buf();
                acc.insert(rel, fs::read(&path).expect("read file"));
            }
        }
    }
    let mut acc = std::collections::BTreeMap::new();
    walk(root, root, &mut acc);
    acc
}

#[test]
fn scenario_8_status_modifies_no_file() {
    // SPEC-002 § Acceptance scenario 8 (EARS-05.3): the directory tree
    // is byte-identical before and after a `status` run, in either
    // output format.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[SPEC]\nrules = []\n\n[BUG]\nrules = []\n",
    );
    write(
        &tmp.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: accepted\n---\n\n# ADR-001\n",
    );
    write(
        &tmp.path().join("docs/specs/001-reconciliation-engine.md"),
        "---\nid: SPEC-001\ntitle: Reconciliation engine\nstatus: draft\ndepends_on: [ADR-001]\n---\n\n# SPEC-001\n",
    );
    write(
        &tmp.path().join("docs/bugs/001-drift.md"),
        "---\nid: BUG-001\ntitle: Drift\nstatus: open\ndepends_on: [SPEC-001]\n---\n\n# BUG-001\n",
    );

    let before = snapshot_tree(tmp.path());

    let text = status(tmp.path());
    assert_eq!(text.status.code(), Some(0));
    let json = status_json(tmp.path());
    assert_eq!(json.status.code(), Some(0));

    let after = snapshot_tree(tmp.path());
    assert_eq!(
        before, after,
        "EARS-05.3: `status` must leave every file unmodified"
    );
}

#[test]
fn exit_code_matrix_success_config_error_and_cycle() {
    // T3.5 — the exit-code matrix (EARS-05.1/05.2), pinned end to end:
    //   successful computation, any position → 0
    //   invalid configuration                → 2
    //   cyclic namespace graph               → 2

    // (a) EARS-05.1: a complete pipeline is a success — exit 0.
    let done = tempfile::tempdir().expect("tempdir");
    write(
        &done.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[pipeline]\nstages = [\"ADR\"]\n",
    );
    write(
        &done.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: accepted\n---\n\n# ADR-001\n",
    );
    assert_eq!(
        status(done.path()).status.code(),
        Some(0),
        "EARS-05.1: a done pipeline exits 0"
    );

    // (a') EARS-05.1: an early/blocked position is still data — exit 0.
    let early = tempfile::tempdir().expect("tempdir");
    write(
        &early.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[SPEC]\nrules = []\n",
    );
    write(
        &early.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: draft\n---\n\n# ADR-001\n",
    );
    assert_eq!(
        status(early.path()).status.code(),
        Some(0),
        "EARS-05.1: an early position exits 0"
    );

    // (b) EARS-05.2: a gate naming a status outside the namespace's
    // allowed-values is a configuration error — exit 2.
    let bad_cfg = tempfile::tempdir().expect("tempdir");
    write(
        &bad_cfg.path().join("ctxgrd.toml"),
        "[ADR]\nrules = [\"core.allowed-values\"]\n[ADR.\"core.allowed-values\"]\nstatus = [\"draft\", \"accepted\"]\n\n[pipeline]\nstages = [\"ADR\"]\n[pipeline.gate]\nADR = \"any:shipped\"\n",
    );
    let cfg_out = status(bad_cfg.path());
    assert_eq!(
        cfg_out.status.code(),
        Some(2),
        "EARS-05.2: an invalid gate status exits 2; stderr:\n{}",
        String::from_utf8_lossy(&cfg_out.stderr)
    );

    // (c) EARS-05.2: a namespace-level cycle exits 2.
    let cyclic = tempfile::tempdir().expect("tempdir");
    write(
        &cyclic.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[SPEC]\nrules = []\n",
    );
    write(
        &cyclic.path().join("docs/adrs/001-a.md"),
        "---\nid: ADR-001\ntitle: A\nstatus: accepted\ndepends_on: [SPEC-001]\n---\n\n# ADR-001\n",
    );
    write(
        &cyclic.path().join("docs/specs/001-b.md"),
        "---\nid: SPEC-001\ntitle: B\nstatus: accepted\n---\n\n# SPEC-001\n",
    );
    write(
        &cyclic.path().join("docs/specs/002-c.md"),
        "---\nid: SPEC-002\ntitle: C\nstatus: draft\ndepends_on: [ADR-001]\n---\n\n# SPEC-002\n",
    );
    assert_eq!(
        status(cyclic.path()).status.code(),
        Some(2),
        "EARS-05.2: a namespace cycle exits 2"
    );
}

/// Run `ctxgrd lint --format simple` against `root` (one diagnostic per
/// line, grep-friendly). Used to assert pipeline.conformance findings.
fn lint_simple(root: &Path) -> std::process::Output {
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", root)
        .args([
            "lint",
            "--format",
            "simple",
            "--root",
            root.to_str().unwrap(),
        ])
        .output()
        .expect("ctxgrd lint runs")
}

#[test]
fn scenario_6_conformance_flags_stage_skipping_edge() {
    // SPEC-002 § Acceptance scenario 6 (EARS-06.2): a TASK depending
    // directly on a PRD under a declared PRD → ADR → SPEC → TASK
    // pipeline emits error[pipeline.conformance] naming ADR and SPEC.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[PRD]\nrules = []\n\n[ADR]\nrules = []\n\n[SPEC]\nrules = []\n\n[TASK]\nrules = []\n\n[pipeline]\nstages = [\"PRD\", \"ADR\", \"SPEC\", \"TASK\"]\n",
    );
    write(
        &tmp.path().join("docs/prds/001-billing.md"),
        "---\nid: PRD-001\ntitle: Billing\nstatus: accepted\n---\n\n# PRD-001\n",
    );
    write(
        &tmp.path().join("docs/tasks/001-wire-it.md"),
        "---\nid: TASK-001\ntitle: Wire it\nstatus: doing\ndepends_on: [PRD-001]\n---\n\n# TASK-001\n",
    );

    let out = lint_simple(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(
        out.status.code(),
        Some(1),
        "EARS-06.2: a stage-skipping edge is a lint error (exit 1); stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("pipeline.conformance"),
        "the finding must carry the rule code; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("ADR") && stdout.contains("SPEC"),
        "EARS-06.2: the skipped stages must be named; stdout:\n{stdout}"
    );
}

#[test]
fn conformance_exempts_edges_touching_unstaged_namespaces() {
    // EARS-06.3: with only PRD and TASK staged, a TASK → PRD edge spans
    // distance 1 (adjacent in the declared 2-stage ladder) — and an
    // unrelated BUG → PRD edge touches the unstaged BUG namespace. No
    // conformance error fires.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[PRD]\nrules = []\n\n[TASK]\nrules = []\n\n[BUG]\nrules = []\n\n[pipeline]\nstages = [\"PRD\", \"TASK\"]\n",
    );
    write(
        &tmp.path().join("docs/prds/001-billing.md"),
        "---\nid: PRD-001\ntitle: Billing\nstatus: accepted\n---\n\n# PRD-001\n",
    );
    write(
        &tmp.path().join("docs/tasks/001-wire-it.md"),
        "---\nid: TASK-001\ntitle: Wire it\nstatus: doing\ndepends_on: [PRD-001]\n---\n\n# TASK-001\n",
    );
    write(
        &tmp.path().join("docs/bugs/001-drift.md"),
        "---\nid: BUG-001\ntitle: Drift\nstatus: open\ndepends_on: [PRD-001]\n---\n\n# BUG-001\n",
    );

    let out = lint_simple(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert!(
        !stdout.contains("pipeline.conformance"),
        "EARS-06.3: adjacent and unstaged-namespace edges are exempt; stdout:\n{stdout}"
    );
}
