//! SPEC-002 acceptance suite — the single consolidated integration
//! harness for `ctxgrd status` and the `core.dep-shape` admissibility
//! check (ADR-039 § DAG-003, which replaced `pipeline.conformance`).
//! One test per SPEC-002 § Acceptance scenario, run by `cargo test` /
//! `make ci`:
//!
//! 1. Cold start → `source: default`, ladder restricted to active
//!    namespaces (EARS-01.3/01.4).
//! 2. Inference with the direct edge reduced away (EARS-01.2), plus the
//!    declared-wins and namespace-cycle cases (EARS-01.1/01.5).
//! 3. Branch join waits on both parents (EARS-02.6).
//! 4. Accepted-but-lint-failing SPEC holds its stage (EARS-02.2), plus
//!    the accepted-clean positive.
//! 5. Open-BUG tripwire blocks then releases (EARS-03.1/03.2).
//! 6. `core.dep-shape` admissibility flags an inadmissible edge (DAG-003),
//!    plus the unmanaged-namespace exemption.
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

/// True when the status table has a row whose first two whitespace-
/// separated tokens are `namespace` then `state` — the column-aligned
/// KISS format (`ADR   done   …`), independent of padding width.
fn row_has(stdout: &str, namespace: &str, state: &str) -> bool {
    stdout.lines().any(|line| {
        let mut tok = line.split_whitespace();
        tok.next() == Some(namespace) && tok.next() == Some(state)
    })
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
    // dependency edges. The DAG must come from the built-in ladder and SAY
    // SO — `source: default` in the JSON contract, no pretending it was
    // derived (EARS-01.3, EARS-01.4). The human text table omits it.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(&tmp.path().join("ctxgrd.toml"), FOUR_NAMESPACES);
    write(
        &tmp.path().join("docs/prds/001-billing-reconciliation.md"),
        "---\nid: PRD-001\ntitle: Billing reconciliation\nstatus: accepted\n---\n\n# PRD-001: Billing reconciliation\n",
    );

    let out = status(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    // Provenance moved to the JSON contract (EARS-01.4); the human text table
    // must not print a `source:` line.
    assert!(
        !stdout.contains("source:"),
        "the text table must not print a source line; stdout:\n{stdout}"
    );
    assert_json_source(tmp.path(), "default");
    // The DAG shape lives in the per-row `needs` column: ADR needs PRD,
    // SPEC needs ADR, TASK needs SPEC — the built-in PRD → ADR → SPEC →
    // TASK ladder restricted to active namespaces (EARS-01.3).
    assert!(stdout.contains("needs PRD"), "stdout:\n{stdout}");
    assert!(stdout.contains("needs ADR"), "stdout:\n{stdout}");
    assert!(stdout.contains("needs SPEC"), "stdout:\n{stdout}");
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
    assert_json_source(tmp.path(), "default");
    // ADR → SPEC, now read off SPEC's `needs` column.
    assert!(stdout.contains("needs ADR"), "stdout:\n{stdout}");
    assert!(!stdout.contains("PRD"), "stdout:\n{stdout}");
    assert!(!stdout.contains("TASK"), "stdout:\n{stdout}");
}

#[test]
fn scenario_2_declared_chain_reduces_direct_edge() {
    // SPEC-002 § Acceptance scenario 2, recast for ADR-039 § DAG-007:
    // runtime resolution is declared-or-default — inference moved to
    // `init`. The same PRD → ADR → SPEC shape, now *declared* via
    // dep-shape (ADR requires PRD; SPEC requires PRD and ADR), resolves
    // `declared` with the direct PRD → SPEC edge reduced away.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        concat!(
            "[PRD]\nrules = []\n\n",
            "[ADR]\nrules = [\"core.dep-shape\"]\n[ADR.\"core.dep-shape\"]\nrequires = [\"PRD\"]\n\n",
            "[SPEC]\nrules = [\"core.dep-shape\"]\n[SPEC.\"core.dep-shape\"]\nrequires = [\"PRD\", \"ADR\"]\n\n",
            "[TASK]\nrules = []\n",
        ),
    );
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
    assert_json_source(tmp.path(), "declared");
    // DAG-002: the lifted declared chain, read off the `needs` columns —
    // ADR needs PRD, SPEC needs ADR.
    assert!(stdout.contains("needs PRD"), "stdout:\n{stdout}");
    assert!(stdout.contains("needs ADR"), "stdout:\n{stdout}");
    // The direct PRD → SPEC edge is reduced away: SPEC needs ADR only,
    // never the un-reduced `needs ADR, PRD`.
    assert!(
        !stdout.contains("needs ADR, PRD"),
        "the direct PRD → SPEC edge must be reduced away; stdout:\n{stdout}"
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
    assert_json_source(tmp.path(), "declared");
    // SPEC → ADR declared verbatim: ADR needs SPEC.
    assert!(stdout.contains("needs SPEC"), "stdout:\n{stdout}");
}

#[test]
fn namespace_cycle_reports_and_exits_non_zero() {
    // EARS-01.5, recast for ADR-039 § DAG-007: a namespace cycle declared
    // through dep-shape (ADR requires SPEC and SPEC requires ADR) must be
    // reported and exit non-zero at resolution time.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        concat!(
            "[ADR]\nrules = [\"core.dep-shape\"]\n[ADR.\"core.dep-shape\"]\nrequires = [\"SPEC\"]\n\n",
            "[SPEC]\nrules = [\"core.dep-shape\"]\n[SPEC.\"core.dep-shape\"]\nrequires = [\"ADR\"]\n",
        ),
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
    // SPEC-002 § Acceptance scenario 3 (EARS-02.6), recast for ADR-039 §
    // DAG-007: SPEC depends on both ADR and DESIGN, declared via
    // dep-shape (SPEC requires ADR + DESIGN). SPEC is itself accepted —
    // its own gate is satisfied — but DESIGN is still a draft, so the
    // join stage must wait: SPEC is pending, not done. The discriminator:
    // without parent-gating SPEC would read `done`.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        concat!(
            "[ADR]\nrules = []\n\n",
            "[DESIGN]\nrules = []\n\n",
            "[SPEC]\nrules = [\"core.dep-shape\"]\n",
            "[SPEC.\"core.dep-shape\"]\nrequires = [\"ADR\", \"DESIGN\"]\n",
        ),
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
    assert!(row_has(&stdout, "ADR", "done"), "stdout:\n{stdout}");
    assert!(row_has(&stdout, "DESIGN", "current"), "stdout:\n{stdout}");
    assert!(
        row_has(&stdout, "SPEC", "pending"),
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
        !row_has(&stdout, "SPEC", "done"),
        "EARS-02.2: an accepted-but-failing SPEC must NOT be done; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("held by") && stdout.contains("SPEC-001"),
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
    assert!(row_has(&stdout, "SPEC", "done"), "stdout:\n{stdout}");
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
        row_has(&stdout2, "SPEC", "current"),
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

/// Assert the JSON output labels the DAG source (EARS-01.4). Provenance lives
/// on the machine contract now — the human text table no longer prints a
/// `source:` line — so scenarios verify the label here.
fn assert_json_source(root: &Path, expected: &str) {
    let out = status_json(root);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("status --format json emits valid JSON");
    assert_eq!(parsed["source"], expected, "json:\n{parsed:#}");
}

#[test]
fn scenario_7_json_output_validates_against_schema() {
    // SPEC-002 § Acceptance scenario 7 (EARS-04.2): `--format json`
    // emits a single object matching the Data model schema —
    // source / stages(namespace,state,docs,verdict) / frontier /
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
    // EARS-01.4: a plain-language hint accompanies the stable `source` enum.
    assert_eq!(parsed["source_hint"], "order you set in ctxgrd.toml");
    assert!(parsed["stages"].is_array(), "json:\n{parsed:#}");
    // ADR-039 § DAG-006: `frontier` is an array of ready stages; the
    // single `current` cursor is removed from the wire.
    assert!(parsed["frontier"].is_array(), "json:\n{parsed:#}");
    assert!(parsed.get("current").is_none(), "json:\n{parsed:#}");
    assert!(parsed["blockers"].is_array(), "json:\n{parsed:#}");
    assert!(parsed["next_action"].is_string(), "json:\n{parsed:#}");

    // ADR-037: edges mirror the declared chain; blocker_stages is an
    // object (here empty — no open BUG cites the lineage).
    assert_eq!(
        parsed["edges"],
        serde_json::json!([{"from": "ADR", "to": "SPEC"}]),
        "json:\n{parsed:#}"
    );
    assert!(parsed["blocker_stages"].is_object(), "json:\n{parsed:#}");

    // Each stage carries namespace / state / docs / verdict plus the
    // ADR-037 gate_met / hold fields.
    let stages = parsed["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 2);
    for stage in stages {
        assert!(stage["namespace"].is_string(), "json:\n{parsed:#}");
        assert!(stage["state"].is_string(), "json:\n{parsed:#}");
        assert!(stage["docs"].is_array(), "json:\n{parsed:#}");
        assert!(stage["verdict"].is_string(), "json:\n{parsed:#}");
        assert!(stage["gate_met"].is_boolean(), "json:\n{parsed:#}");
        assert!(stage["hold"].is_array(), "json:\n{parsed:#}");
    }
    // ADR accepted+clean → done, gate met; SPEC draft → current, gate
    // not met.
    assert_eq!(stages[0]["namespace"], "ADR");
    assert_eq!(stages[0]["state"], "done");
    assert_eq!(stages[0]["gate_met"], true);
    assert_eq!(stages[1]["namespace"], "SPEC");
    assert_eq!(stages[1]["state"], "current");
    assert_eq!(stages[1]["gate_met"], false);
    // ADR is done, SPEC is the lone ready stage → frontier = ["SPEC"].
    assert_eq!(parsed["frontier"], serde_json::json!(["SPEC"]));
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

    // (c) EARS-05.2: a namespace-level cycle (declared via dep-shape,
    // ADR-039 § DAG-007) exits 2.
    let cyclic = tempfile::tempdir().expect("tempdir");
    write(
        &cyclic.path().join("ctxgrd.toml"),
        concat!(
            "[ADR]\nrules = [\"core.dep-shape\"]\n[ADR.\"core.dep-shape\"]\nrequires = [\"SPEC\"]\n\n",
            "[SPEC]\nrules = [\"core.dep-shape\"]\n[SPEC.\"core.dep-shape\"]\nrequires = [\"ADR\"]\n",
        ),
    );
    assert_eq!(
        status(cyclic.path()).status.code(),
        Some(2),
        "EARS-05.2: a namespace cycle exits 2"
    );
}

/// Run `ctxgrd lint --format simple` against `root` (one diagnostic per
/// line, grep-friendly). Used to assert core.dep-shape findings.
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
fn scenario_6_dep_shape_flags_inadmissible_edge() {
    // SPEC-002 § Acceptance scenario 6, recast for ADR-039 § DAG-003: a
    // TASK depending directly on a PRD when TASK only admits SPEC emits
    // error[core.dep-shape] — PRD is managed (SPEC admits it) but not in
    // TASK's requires/allows, so the edge is inadmissible.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        concat!(
            "[PRD]\nrules = []\n\n",
            "[ADR]\nrules = []\n\n",
            "[SPEC]\nrules = [\"core.dep-shape\"]\n",
            "[SPEC.\"core.dep-shape\"]\nrequires = [\"PRD\"]\nallows = [\"ADR\"]\n\n",
            "[TASK]\nrules = [\"core.dep-shape\"]\n",
            "[TASK.\"core.dep-shape\"]\nrequires = [\"SPEC\"]\n",
        ),
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
        "DAG-003: an inadmissible edge is a lint error (exit 1); stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("core.dep-shape"),
        "the finding must carry the rule code; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("PRD") && stdout.contains("TASK"),
        "DAG-003: the inadmissible edge must name PRD and TASK; stdout:\n{stdout}"
    );
}

#[test]
fn complete_pipeline_reports_pipeline_complete_and_empty_frontier() {
    // ADR-039 § DAG-006: when every stage is done the frontier (ready
    // set) is empty. The `exit_code_matrix` "done" case only asserts
    // exit 0; this pins the rendered position end-to-end through the
    // CLI: the text footer says "pipeline complete", carries NO
    // `frontier:` line, and `--format json` emits an empty `frontier`
    // array (never a `current` cursor).
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[pipeline]\nstages = [\"ADR\"]\n",
    );
    write(
        &tmp.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: accepted\n---\n\n# ADR-001\n",
    );

    // Text path: complete pipeline, no frontier line.
    let out = status(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(row_has(&stdout, "ADR", "done"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("next: pipeline complete"),
        "DAG-006: a complete pipeline's next action is `pipeline complete`; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("ready:"),
        "DAG-006: an empty frontier must NOT render a `ready:` line; stdout:\n{stdout}"
    );

    // JSON path: frontier is an explicit empty array, current is gone.
    let json = status_json(tmp.path());
    let json_stdout = String::from_utf8(json.stdout).expect("stdout utf-8");
    assert_eq!(json.status.code(), Some(0), "json:\n{json_stdout}");
    let parsed: serde_json::Value =
        serde_json::from_str(&json_stdout).expect("valid JSON");
    assert_eq!(
        parsed["frontier"],
        serde_json::json!([]),
        "DAG-006: a complete pipeline emits an empty frontier array; json:\n{parsed:#}"
    );
    assert!(
        parsed.get("current").is_none(),
        "DAG-006: the single `current` cursor is removed from the wire; json:\n{parsed:#}"
    );
    assert_eq!(parsed["next_action"], "pipeline complete");
}

#[test]
fn two_disconnected_workflows_report_both_frontiers() {
    // ADR-039 § DAG-006 (Verification): "two disconnected workflows
    // report both frontiers". Two independent chains, each declared via
    // dep-shape:
    //   PRD → ADR        (PRD root done, ADR not done)
    //   RUNBOOK → POSTMORTEM  (RUNBOOK root done, POSTMORTEM not done)
    // With both roots done and each second stage unfinished, the frontier
    // is the 2-element antichain {ADR, POSTMORTEM}, name-sorted. This
    // exercises the real compute_frontier / render path end-to-end.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        concat!(
            "[PRD]\nrules = []\n\n",
            "[ADR]\nrules = [\"core.dep-shape\"]\n[ADR.\"core.dep-shape\"]\nrequires = [\"PRD\"]\n\n",
            "[RUNBOOK]\nrules = []\n\n",
            "[POSTMORTEM]\nrules = [\"core.dep-shape\"]\n",
            "[POSTMORTEM.\"core.dep-shape\"]\nrequires = [\"RUNBOOK\"]\n",
        ),
    );
    // Workflow A: PRD root accepted (done), ADR draft (not done, ready).
    write(
        &tmp.path().join("docs/prds/001-billing-reconciliation.md"),
        "---\nid: PRD-001\ntitle: Billing reconciliation\nstatus: accepted\n---\n\n# PRD-001\n",
    );
    write(
        &tmp.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: draft\ndepends_on: [PRD-001]\n---\n\n# ADR-001\n",
    );
    // Workflow B: RUNBOOK root accepted (done), POSTMORTEM draft (ready).
    write(
        &tmp.path().join("docs/runbooks/001-failover-drill.md"),
        "---\nid: RUNBOOK-001\ntitle: Failover drill\nstatus: accepted\n---\n\n# RUNBOOK-001\n",
    );
    write(
        &tmp.path().join("docs/postmortems/001-outage-review.md"),
        "---\nid: POSTMORTEM-001\ntitle: Outage review\nstatus: draft\ndepends_on: [RUNBOOK-001]\n---\n\n# POSTMORTEM-001\n",
    );

    // Text path: the `ready:` line lists BOTH ready stages, name-sorted.
    let out = status(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(
        stdout.contains("ready: ADR, POSTMORTEM"),
        "DAG-006: two disconnected workflows must report both frontiers, name-sorted; stdout:\n{stdout}"
    );

    // JSON path: frontier array holds both ready stages, name-sorted.
    let json = status_json(tmp.path());
    let json_stdout = String::from_utf8(json.stdout).expect("stdout utf-8");
    assert_eq!(json.status.code(), Some(0), "json:\n{json_stdout}");
    let parsed: serde_json::Value =
        serde_json::from_str(&json_stdout).expect("valid JSON");
    assert_eq!(
        parsed["frontier"],
        serde_json::json!(["ADR", "POSTMORTEM"]),
        "DAG-006: the JSON frontier holds both disconnected ready stages; json:\n{parsed:#}"
    );
}

#[test]
fn dep_shape_exempts_edges_to_unmanaged_namespaces() {
    // ADR-039 § DAG-003: PRD is managed (SPEC admits it), but BUG is not
    // managed by any namespace's dep-shape. A SPEC → PRD edge is admitted
    // (PRD ∈ SPEC.requires); a BUG → PRD edge is exempt because BUG has no
    // dep-shape contract. No dep-shape error fires.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        concat!(
            "[PRD]\nrules = []\n\n",
            "[SPEC]\nrules = [\"core.dep-shape\"]\n",
            "[SPEC.\"core.dep-shape\"]\nrequires = [\"PRD\"]\n\n",
            "[BUG]\nrules = []\n",
        ),
    );
    write(
        &tmp.path().join("docs/prds/001-billing.md"),
        "---\nid: PRD-001\ntitle: Billing\nstatus: accepted\n---\n\n# PRD-001\n",
    );
    write(
        &tmp.path().join("docs/specs/001-wire-it.md"),
        "---\nid: SPEC-001\ntitle: Wire it\nstatus: draft\ndepends_on: [PRD-001]\n---\n\n# SPEC-001\n",
    );
    write(
        &tmp.path().join("docs/bugs/001-drift.md"),
        "---\nid: BUG-001\ntitle: Drift\nstatus: open\ndepends_on: [PRD-001]\n---\n\n# BUG-001\n",
    );

    let out = lint_simple(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert!(
        !stdout.contains("core.dep-shape"),
        "DAG-003: admitted and unmanaged-namespace edges are exempt; stdout:\n{stdout}"
    );
}

#[test]
fn status_mermaid_and_dot_emit_diagram_source() {
    // A declared single-stage pipeline with a draft ADR. Both diagram
    // formats emit valid source and exit 0 (EARS-05.1 — position is data).
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[pipeline]\nstages = [\"ADR\"]\n",
    );
    write(
        &tmp.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: draft\n---\n\n# ADR-001\n",
    );

    for (fmt, header, node) in [
        ("mermaid", "flowchart LR", "ADR[\"ADR: current"),
        ("dot", "digraph pipeline {", "\"ADR\" [label=\"ADR"),
    ] {
        let out = Command::cargo_bin("ctxgrd")
            .expect("binary built")
            .env("HOME", tmp.path())
            .args(["status", "--root", tmp.path().to_str().unwrap(), "--format", fmt])
            .output()
            .expect("ctxgrd status runs");
        let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
        assert_eq!(out.status.code(), Some(0), "{fmt} stdout:\n{stdout}");
        assert!(stdout.contains(header), "{fmt} missing header; stdout:\n{stdout}");
        assert!(stdout.contains(node), "{fmt} missing ADR node; stdout:\n{stdout}");
    }
}

// -- SPEC-003 § Acceptance: done-gate + per-lineage scope -------------
//
// The nine scenario fixtures from SPEC-003 § Acceptance, end-to-end
// through the CLI:
//   1. Shared SPEC — lineage-local verdict + `shared` disclosure.
//   2. Empty TASK subset — the `any:`-over-empty pin holds (`--exit-code` 1).
//   3. Direction — a leaf's lineage is just itself (transpose, no forward walk).
//   4. Exit-code scopes — whole-project vs per-lineage diverge.
//   5. Acceptance-complete holds a stage (the done-gate's checkbox dimension).
//   6. Heading scope — open box outside the acceptance heading does not fire.
//   7. Default unchanged — bare `status` carries no `lineage`/`shared` keys.
//   8. Unresolved lineage — exit 2.
//   9. Read-only — `--exit-code` touches no file.

/// Run `ctxgrd status` against `root` with extra args (e.g. `--lineage`,
/// `--exit-code`, `--format json`) appended after `--root`.
fn status_args(root: &Path, extra: &[&str]) -> std::process::Output {
    let mut args: Vec<String> =
        vec!["status".into(), "--root".into(), root.to_str().unwrap().into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", root)
        .args(&args)
        .output()
        .expect("ctxgrd status runs")
}

/// Parse `status --lineage <id> --format json` and return the object.
fn lineage_json(root: &Path, id: &str) -> serde_json::Value {
    let out = status_args(root, &["--lineage", id, "--format", "json"]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    serde_json::from_str(&stdout).expect("status --lineage --format json emits valid JSON")
}

/// The stage object for `namespace` in a parsed status JSON document.
fn stage_of<'a>(parsed: &'a serde_json::Value, namespace: &str) -> &'a serde_json::Value {
    parsed["stages"]
        .as_array()
        .expect("stages array")
        .iter()
        .find(|s| s["namespace"] == namespace)
        .unwrap_or_else(|| panic!("no `{namespace}` stage in {parsed:#}"))
}

/// A two-feature corpus, `[PRD, SPEC]` pipeline: feature PRD-3 (PRD-3 ←
/// SPEC-9) and an unrelated PRD-7 that SPEC-9 also depends on, so SPEC-9 is
/// a shared node. `prd7_status` lets a test leave PRD-7 incomplete.
fn write_shared_spec_corpus(root: &Path, prd7_status: &str) {
    write(
        &root.join("ctxgrd.toml"),
        "[PRD]\nrules = []\n\n[SPEC]\nrules = []\n\n[pipeline]\nstages = [\"PRD\", \"SPEC\"]\n",
    );
    write(
        &root.join("docs/prds/003-billing-reconciliation.md"),
        "---\nid: PRD-003\ntitle: Billing reconciliation\nstatus: accepted\n---\n\n# PRD-003\n",
    );
    write(
        &root.join("docs/prds/007-payouts.md"),
        &format!("---\nid: PRD-007\ntitle: Payouts\nstatus: {prd7_status}\n---\n\n# PRD-007\n"),
    );
    write(
        &root.join("docs/specs/009-reconciliation-engine.md"),
        "---\nid: SPEC-009\ntitle: Reconciliation engine\nstatus: accepted\ndepends_on: [PRD-003, PRD-007]\n---\n\n# SPEC-009\n",
    );
}

#[test]
fn spec003_scenario_1_shared_spec_is_lineage_local_and_disclosed() {
    // Fixture 1 (EARS-04.1/04.3/04.4): SPEC-9 is depended on by PRD-3 and
    // PRD-7. `status --lineage PRD-3` reports PRD-3's stage done and SPEC-9
    // flagged `shared: ["PRD-7"]`, and SPEC-9 stays done even though PRD-7
    // is draft — the verdict is lineage-local, not folded.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_shared_spec_corpus(tmp.path(), "draft");

    let parsed = lineage_json(tmp.path(), "PRD-003");
    assert_eq!(parsed["lineage"], "PRD-003");

    let prd = stage_of(&parsed, "PRD");
    assert_eq!(prd["docs"], serde_json::json!(["PRD-003"]), "PRD-7 is not a member of PRD-3's lineage");
    assert_eq!(prd["state"], "done");

    let spec = stage_of(&parsed, "SPEC");
    assert_eq!(spec["docs"], serde_json::json!(["SPEC-009"]));
    assert_eq!(spec["state"], "done", "lineage-local: PRD-7 draft must not hold SPEC-9");
    // EARS-04.4: the shared node discloses its other lineage root.
    assert_eq!(spec["shared"], serde_json::json!(["PRD-007"]));

    // The text view carries the marker too.
    let text = status_args(tmp.path(), &["--lineage", "PRD-003"]);
    let stdout = String::from_utf8(text.stdout).expect("utf-8");
    assert!(stdout.contains("shared with PRD-007"), "stdout:\n{stdout}");

    // LIN-003: the per-feature done-gate is green even with PRD-7 draft.
    let ec = status_args(tmp.path(), &["--lineage", "PRD-003", "--exit-code"]);
    assert_eq!(ec.status.code(), Some(0), "PRD-3's lineage is complete");
}

/// A `[PRD, SPEC, TASK]` pipeline with one feature (PRD-3 ← SPEC-9) and
/// the requested `task` shape. With `task: None`, the lineage's TASK
/// subset is empty.
fn write_three_stage_feature(root: &Path, task: Option<(&str, &str)>) {
    write(
        &root.join("ctxgrd.toml"),
        "[PRD]\nrules = []\n\n[SPEC]\nrules = []\n\n[TASK]\nrules = []\n\n[pipeline]\nstages = [\"PRD\", \"SPEC\", \"TASK\"]\n",
    );
    write(
        &root.join("docs/prds/003-billing.md"),
        "---\nid: PRD-003\ntitle: Billing\nstatus: accepted\n---\n\n# PRD-003\n",
    );
    write(
        &root.join("docs/specs/009-engine.md"),
        "---\nid: SPEC-009\ntitle: Engine\nstatus: accepted\ndepends_on: [PRD-003]\n---\n\n# SPEC-009\n",
    );
    if let Some((id, status)) = task {
        write(
            &root.join(format!("docs/tasks/{}-wire.md", &id[5..])),
            &format!("---\nid: {id}\ntitle: Wire it\nstatus: {status}\ndepends_on: [SPEC-009]\n---\n\n# {id}\n"),
        );
    }
}

#[test]
fn spec003_scenario_2_empty_task_subset_holds_the_lineage() {
    // Fixture 2 (EARS-03.1, LIN-006): PRD-3's lineage has no TASK, so the
    // TASK stage is not done (the `any:`/`all:`-over-empty pin) and
    // `--lineage PRD-3 --exit-code` exits 1.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_three_stage_feature(tmp.path(), None);

    let parsed = lineage_json(tmp.path(), "PRD-003");
    let task = stage_of(&parsed, "TASK");
    assert_eq!(task["docs"], serde_json::json!([]), "the TASK subset is empty");
    assert_ne!(task["state"], "done", "an empty staged namespace holds (EARS-03.1)");
    assert_eq!(parsed["frontier"], serde_json::json!(["TASK"]));

    let ec = status_args(tmp.path(), &["--lineage", "PRD-003", "--exit-code"]);
    assert_eq!(ec.status.code(), Some(1), "an empty TASK subset is not done");
}

#[test]
fn spec003_scenario_3_leaf_lineage_is_just_itself() {
    // Fixture 3 (EARS-04.1): the lineage of a leaf TASK-7 is just TASK-7 —
    // no forward walk into its PRD/SPEC prerequisites (transpose direction).
    let tmp = tempfile::tempdir().expect("tempdir");
    write_three_stage_feature(tmp.path(), Some(("TASK-007", "done")));

    let parsed = lineage_json(tmp.path(), "TASK-007");
    assert_eq!(parsed["lineage"], "TASK-007");
    assert_eq!(stage_of(&parsed, "TASK")["docs"], serde_json::json!(["TASK-007"]));
    // Prerequisites are NOT pulled in — the PRD/SPEC stages are empty.
    assert_eq!(stage_of(&parsed, "PRD")["docs"], serde_json::json!([]));
    assert_eq!(stage_of(&parsed, "SPEC")["docs"], serde_json::json!([]));
}

#[test]
fn spec003_scenario_4_exit_code_scopes_diverge() {
    // Fixture 4 (EARS-02.1/02.3): whole-project `--exit-code` and
    // `--lineage PRD-3 --exit-code` diverge when an unrelated lineage is
    // incomplete. Feature A (PRD-3) is complete; feature B (PRD-7) has an
    // unfinished TASK, so the global TASK stage (all:done) is not done.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[PRD]\nrules = []\n\n[SPEC]\nrules = []\n\n[TASK]\nrules = []\n\n[pipeline]\nstages = [\"PRD\", \"SPEC\", \"TASK\"]\n",
    );
    // Feature A — complete.
    write(&tmp.path().join("docs/prds/003-a.md"), "---\nid: PRD-003\ntitle: A\nstatus: accepted\n---\n\n# PRD-003\n");
    write(&tmp.path().join("docs/specs/009-a.md"), "---\nid: SPEC-009\ntitle: A\nstatus: accepted\ndepends_on: [PRD-003]\n---\n\n# SPEC-009\n");
    write(&tmp.path().join("docs/tasks/007-a.md"), "---\nid: TASK-007\ntitle: A\nstatus: done\ndepends_on: [SPEC-009]\n---\n\n# TASK-007\n");
    // Feature B — incomplete (TASK still doing).
    write(&tmp.path().join("docs/prds/007-b.md"), "---\nid: PRD-007\ntitle: B\nstatus: accepted\n---\n\n# PRD-007\n");
    write(&tmp.path().join("docs/specs/011-b.md"), "---\nid: SPEC-011\ntitle: B\nstatus: accepted\ndepends_on: [PRD-007]\n---\n\n# SPEC-011\n");
    write(&tmp.path().join("docs/tasks/009-b.md"), "---\nid: TASK-009\ntitle: B\nstatus: doing\ndepends_on: [SPEC-011]\n---\n\n# TASK-009\n");

    let global = status_args(tmp.path(), &["--exit-code"]);
    assert_eq!(global.status.code(), Some(1), "feature B's unfinished TASK makes the whole project incomplete");

    let lineage = status_args(tmp.path(), &["--lineage", "PRD-003", "--exit-code"]);
    assert_eq!(lineage.status.code(), Some(0), "feature A's lineage is complete on its own");
}

/// A single-stage `[TASK]` pipeline with `core.acceptance-complete` opted
/// in (terminal = done), and a `done` TASK carrying `body`.
fn write_acceptance_task(root: &Path, body: &str) {
    write(
        &root.join("ctxgrd.toml"),
        concat!(
            "[TASK]\nrules = [\"core.acceptance-complete\"]\n",
            "[TASK.\"core.acceptance-complete\"]\nterminal = [\"done\"]\n\n",
            "[pipeline]\nstages = [\"TASK\"]\n",
        ),
    );
    write(
        &root.join("docs/tasks/001-wire-the-engine.md"),
        &format!("---\nid: TASK-001\ntitle: Wire the engine\nstatus: done\n---\n\n{body}"),
    );
}

#[test]
fn spec003_scenario_5_acceptance_complete_holds_the_stage() {
    // Fixture 5 (EARS-01.1/01.5): a `done` TASK with an open `- [ ]` under
    // `Acceptance` emits a diagnostic and holds the TASK stage — the
    // done-gate's checkbox dimension, via the terminal-but-dirty path.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_acceptance_task(
        tmp.path(),
        "# TASK-001\n\n## Acceptance\n\n- [x] Engine wired\n- [ ] Retry path tested\n",
    );

    let out = status_args(tmp.path(), &["--format", "json"]);
    let stdout = String::from_utf8(out.stdout).expect("utf-8");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let task = stage_of(&parsed, "TASK");
    assert_ne!(task["state"], "done", "an open acceptance box holds the stage");
    assert_eq!(task["hold"], serde_json::json!(["TASK-001"]), "TASK-001 is held");

    // The done-gate is not green.
    let ec = status_args(tmp.path(), &["--exit-code"]);
    assert_eq!(ec.status.code(), Some(1));
}

#[test]
fn spec003_scenario_6_open_box_outside_acceptance_does_not_fire() {
    // Fixture 6 (EARS-01.2): an open `- [ ]` under `Open Questions` is
    // deferred work, not an unmet criterion — the stage stays done.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_acceptance_task(
        tmp.path(),
        "# TASK-001\n\n## Acceptance\n\n- [x] Engine wired\n\n## Open Questions\n\n- [ ] Revisit retry budget later\n",
    );

    let parsed: serde_json::Value = {
        let out = status_args(tmp.path(), &["--format", "json"]);
        serde_json::from_str(&String::from_utf8(out.stdout).expect("utf-8")).expect("valid JSON")
    };
    assert_eq!(stage_of(&parsed, "TASK")["state"], "done", "deferred work must not hold the stage");

    let ec = status_args(tmp.path(), &["--exit-code"]);
    assert_eq!(ec.status.code(), Some(0), "no unmet acceptance criterion → done");
}

#[test]
fn spec003_scenario_7_bare_status_carries_no_lineage_or_shared_keys() {
    // Fixture 7 (EARS-04.7): without `--lineage`, the JSON is byte-identical
    // to pre-SPEC-003 — the additive `lineage` and per-stage `shared` keys
    // are absent in the global view.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_shared_spec_corpus(tmp.path(), "accepted");

    let out = status_json(tmp.path());
    let stdout = String::from_utf8(out.stdout).expect("utf-8");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert!(parsed.get("lineage").is_none(), "global view must omit `lineage`; json:\n{parsed:#}");
    for stage in parsed["stages"].as_array().expect("stages") {
        assert!(stage.get("shared").is_none(), "global stage must omit `shared`; stage:\n{stage:#}");
    }
}

#[test]
fn spec003_scenario_8_unresolved_lineage_exits_2() {
    // Fixture 8 (EARS-04.5): a `--lineage <ID>` that resolves to no
    // document exits 2.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_shared_spec_corpus(tmp.path(), "accepted");

    let out = status_args(tmp.path(), &["--lineage", "NOPE-9"]);
    assert_eq!(out.status.code(), Some(2), "an unresolved lineage id is a kernel error");
}

#[test]
fn spec003_scenario_9_exit_code_run_is_read_only() {
    // Fixture 9 (EARS-02.4): a `--lineage --exit-code` run leaves the tree
    // byte-identical.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_three_stage_feature(tmp.path(), Some(("TASK-007", "done")));

    let before = snapshot_tree(tmp.path());
    let _ = status_args(tmp.path(), &["--lineage", "PRD-003", "--exit-code"]);
    let _ = status_args(tmp.path(), &["--lineage", "PRD-003", "--exit-code", "--format", "json"]);
    let after = snapshot_tree(tmp.path());
    assert_eq!(before, after, "EARS-02.4: --exit-code must modify no file");
}
