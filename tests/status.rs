//! `ctxgrd status` acceptance suite, rewritten for ADR-118.
//!
//! The stage layer is gone: no ladder, no gates, no frontier, no BUG
//! tripwire. What `status` answers now is per document — what can be picked
//! up, and what is waiting on what.
//!
//! The three `BUG-036` reproductions are the spine of this file, and each is
//! asserted as a **pair** per `ADR-112` § CLR-007. A test that only shows the
//! fixed case passing cannot tell a fix from a check that stopped firing —
//! which matters more than usual here, because the fix *removes* output.
//!
//! - R1 → `stg001_*`: every namespace reaches the queue, and the new rows
//!   carry a computed verdict rather than a placeholder.
//! - R2 → `stg004_*`: the done-signal can pass, and can still fail.
//! - R3 → `no_advice_to_author_a_prerequisite_for_shipped_work`.

use std::fs;
use std::path::Path;

use assert_cmd::Command;

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Run `ctxgrd status` against `root`, isolated from any real `~/.ctxgrd`
/// global config.
fn status_args(root: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["status", "--root", root.to_str().unwrap()];
    args.extend_from_slice(extra);
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", root)
        .args(&args)
        .output()
        .expect("ctxgrd status runs")
}

fn status(root: &Path) -> std::process::Output {
    status_args(root, &[])
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("stdout utf-8")
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("stderr utf-8")
}

fn status_json(root: &Path) -> serde_json::Value {
    let out = status_args(root, &["--format", "json"]);
    let text = stdout_of(&out);
    assert_eq!(out.status.code(), Some(0), "stdout:\n{text}");
    serde_json::from_str(&text).expect("valid JSON")
}

/// A tree spanning four namespaces, only two of which the deleted ladder
/// would have staged. `SPEC-001` is held by the draft `ADR-002`; everything
/// else is either settled or workable.
fn write_corpus(root: &Path) {
    write(
        &root.join("ctxgrd.toml"),
        "[ADR]\nrules = []\n\n[SPEC]\nrules = []\n\n[BUG]\nrules = []\n\n[HANDOFF]\nrules = []\n",
    );
    write(
        &root.join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: accepted\n---\n\n# ADR-001\n",
    );
    write(
        &root.join("docs/adrs/002-retention.md"),
        "---\nid: ADR-002\ntitle: Retention\nstatus: draft\n---\n\n# ADR-002\n",
    );
    write(
        &root.join("docs/specs/001-reconciliation.md"),
        "---\nid: SPEC-001\ntitle: Reconciliation\nstatus: draft\ndepends_on: [ADR-002]\n---\n\n# SPEC-001\n",
    );
    write(
        &root.join("docs/bugs/001-drift.md"),
        "---\nid: BUG-001\ntitle: Drift\nstatus: open\ndepends_on: [ADR-001]\n---\n\n# BUG-001\n",
    );
    write(
        &root.join("docs/handoffs/001-carry-on.md"),
        "---\nid: HANDOFF-001\ntitle: Carry on\nstatus: pending\n---\n\n# HANDOFF-001\n",
    );
}

// --- ADR-118 § STG-001 -------------------------------------------------

#[test]
fn stg001_the_queue_covers_every_namespace_not_only_a_staged_ladder() {
    // `BUG-036` R1. Under the stage layer the queue was the union of
    // `stages[].docs`, so BUG and HANDOFF — absent from
    // ["PRD","ADR","SPEC","TASK"] — produced no rows at all, and an agent
    // asking what was open was told the open bugs did not exist.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());

    let parsed = status_json(tmp.path());
    let ids: Vec<&str> = parsed["documents"]
        .as_array()
        .expect("documents array")
        .iter()
        .map(|d| d["id"].as_str().expect("id"))
        .collect();
    assert_eq!(
        ids,
        vec!["ADR-001", "ADR-002", "BUG-001", "HANDOFF-001", "SPEC-001"],
        "STG-001: every id-carrying document gets a row; json:\n{parsed:#}"
    );
}

#[test]
fn stg001_rows_from_unstaged_namespaces_carry_a_real_verdict() {
    // The paired half: presence alone is not the fix. BUG-001 depends on a
    // settled ADR so it is workable; SPEC-001 depends on a draft so it is
    // not, and names it. A placeholder row would pass the test above and
    // fail this one.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());

    let parsed = status_json(tmp.path());
    let row = |id: &str| -> serde_json::Value {
        parsed["documents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == id)
            .unwrap_or_else(|| panic!("{id} present"))
            .clone()
    };
    assert_eq!(row("BUG-001")["ready"], serde_json::json!(true));
    assert_eq!(row("BUG-001")["blocked_by"], serde_json::json!([]));
    assert_eq!(row("SPEC-001")["ready"], serde_json::json!(false));
    assert_eq!(
        row("SPEC-001")["blocked_by"],
        serde_json::json!(["ADR-002"]),
        "STG-001: a blocked row must name its blocker"
    );
    // And a settled document is `ready: false` with nothing blocking it —
    // done, not stuck. `BUG-036` R1 was filed against this shape in error.
    assert_eq!(row("ADR-001")["ready"], serde_json::json!(false));
    assert_eq!(row("ADR-001")["blocked_by"], serde_json::json!([]));
    assert_eq!(row("ADR-001")["status"], serde_json::json!("accepted"));
}

// --- ADR-118 § STG-002 -------------------------------------------------

#[test]
fn stg002_a_declared_pipeline_is_refused_and_the_error_names_the_adr() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());
    let cfg = tmp.path().join("ctxgrd.toml");
    let existing = fs::read_to_string(&cfg).unwrap();
    write(
        &cfg,
        &format!("{existing}\n[pipeline]\nstages = [\"ADR\", \"SPEC\"]\n"),
    );

    let out = status(tmp.path());
    let combined = format!("{}{}", stdout_of(&out), stderr_of(&out));
    assert_eq!(
        out.status.code(),
        Some(2),
        "STG-002: a removed config key is a config error; out:\n{combined}"
    );
    assert!(
        combined.contains("cfg.pipeline-removed"),
        "out:\n{combined}"
    );
    assert!(combined.contains("ADR-118"), "out:\n{combined}");
    assert!(
        combined.contains("core.dep-shape"),
        "STG-002: the error must point at the replacement; out:\n{combined}"
    );
}

#[test]
fn stg002_the_same_tree_without_the_block_succeeds() {
    // The paired half — otherwise "declaring a pipeline fails" is
    // indistinguishable from "this tree fails".
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());
    let out = status(tmp.path());
    assert_eq!(out.status.code(), Some(0), "out:\n{}", stdout_of(&out));
}

// --- ADR-118 § STG-003 -------------------------------------------------

#[test]
fn stg003_json_carries_the_work_queue_and_none_of_the_stage_fields() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());
    let parsed = status_json(tmp.path());

    assert!(parsed["documents"].is_array(), "json:\n{parsed:#}");
    for gone in [
        "stages",
        "edges",
        "frontier",
        "blockers",
        "blocker_stages",
        "next_action",
        "source",
        "source_hint",
    ] {
        assert!(
            parsed.get(gone).is_none(),
            "STG-003: `{gone}` must be gone; json:\n{parsed:#}"
        );
    }
    // Each row is exactly the ADR-107 § RDY-001 shape — no more, no less.
    // Compared as a set: `serde_json::Value` is BTreeMap-backed, so parsing
    // discards emission order and any order assertion here would be testing
    // the parser rather than the contract.
    let first = &parsed["documents"][0];
    let keys: Vec<&str> = first.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["blocked_by", "id", "namespace", "ready", "status"],
        "json:\n{parsed:#}"
    );
}

// --- ADR-118 § STG-004 -------------------------------------------------

#[test]
fn stg004_exit_code_is_one_while_a_document_is_blocked() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());
    let out = status_args(tmp.path(), &["--exit-code"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "SPEC-001 is held by the draft ADR-002; out:\n{}",
        stdout_of(&out)
    );
}

#[test]
fn stg004_exit_code_is_zero_once_the_blocker_settles() {
    // The paired half, and the one that matters: accepting ADR-002 must
    // flip the signal. Under the stage layer it could not — `BUG-036` R2
    // showed `--exit-code` stuck at 1 even for a shipped feature.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());
    write(
        &tmp.path().join("docs/adrs/002-retention.md"),
        "---\nid: ADR-002\ntitle: Retention\nstatus: accepted\n---\n\n# ADR-002\n",
    );
    let out = status_args(tmp.path(), &["--exit-code"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "nothing is blocked once ADR-002 is accepted; out:\n{}",
        stdout_of(&out)
    );
}

#[test]
fn stg004_a_shipped_feature_certifies_through_its_own_lineage() {
    // `BUG-036` R2 directly: ADR-001 is accepted, shipped, and nothing
    // depends on it except an open BUG that is itself unblocked. Its
    // lineage gate must pass even while the wider tree has blocked work.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());

    let global = status_args(tmp.path(), &["--exit-code"]);
    assert_eq!(global.status.code(), Some(1), "the tree as a whole is blocked");

    let scoped = status_args(tmp.path(), &["--exit-code", "--lineage", "ADR-001"]);
    assert_eq!(
        scoped.status.code(),
        Some(0),
        "STG-004: a per-feature gate must be able to certify a finished feature; out:\n{}",
        stdout_of(&scoped)
    );
}

#[test]
fn a_config_error_exits_two_not_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(&tmp.path().join("ctxgrd.toml"), "[ADR]\nrules = [\n");
    let out = status_args(tmp.path(), &["--exit-code"]);
    assert_eq!(out.status.code(), Some(2), "out:\n{}", stderr_of(&out));
}

// --- ADR-118 § STG-005 -------------------------------------------------

#[test]
fn stg005_granularity_namespace_is_rejected_rather_than_silently_redirected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());
    let out = status_args(
        tmp.path(),
        &["--granularity", "namespace", "--format", "mermaid"],
    );
    let combined = format!("{}{}", stdout_of(&out), stderr_of(&out));
    assert_eq!(out.status.code(), Some(2), "out:\n{combined}");
    assert!(combined.contains("cli.bad-granularity"), "out:\n{combined}");
    assert!(combined.contains("ADR-118"), "out:\n{combined}");
}

#[test]
fn stg005_granularity_doc_is_accepted_and_matches_the_default() {
    // The flag is retained as a no-op so existing invocations keep working.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());
    let explicit = status_args(
        tmp.path(),
        &["--granularity", "doc", "--format", "mermaid"],
    );
    let default = status_args(tmp.path(), &["--format", "mermaid"]);
    assert_eq!(explicit.status.code(), Some(0));
    assert_eq!(stdout_of(&explicit), stdout_of(&default));
}

#[test]
fn stg005_mermaid_and_dot_draw_the_document_graph() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());

    let mermaid = stdout_of(&status_args(tmp.path(), &["--format", "mermaid"]));
    assert!(mermaid.starts_with("flowchart LR\n"), "out:\n{mermaid}");
    assert!(
        mermaid.contains("ADR_001[\"ADR-001: accepted\"]:::done"),
        "out:\n{mermaid}"
    );
    assert!(
        mermaid.contains("ADR_002 --> SPEC_001"),
        "STG-005: edges are document `depends_on`, not namespace adjacency; out:\n{mermaid}"
    );
    // A BUG is an ordinary node — the dashed tripwire overlay went with the
    // namespace view it lived in.
    assert!(
        mermaid.contains("BUG_001[\"BUG-001: open\"]"),
        "out:\n{mermaid}"
    );
    assert!(!mermaid.contains("blocks"), "out:\n{mermaid}");

    let dot = stdout_of(&status_args(tmp.path(), &["--format", "dot"]));
    assert!(dot.starts_with("digraph documents {\n"), "out:\n{dot}");
    assert!(dot.contains("\"ADR-002\" -> \"SPEC-001\";"), "out:\n{dot}");
    assert!(!dot.contains("style=dashed"), "out:\n{dot}");
}

// --- BUG-036 R3 --------------------------------------------------------

#[test]
fn no_advice_to_author_a_prerequisite_for_shipped_work() {
    // R3: the old default ladder made `ADR` need `PRD`, and this corpus has
    // no PRD, so `status` told you to "create the first PRD document" about
    // work that had already shipped. With no ladder there is no unsatisfiable
    // stage to report and nothing to advise creating.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());
    let text = stdout_of(&status(tmp.path()));
    assert!(!text.contains("create the first"), "out:\n{text}");
    assert!(!text.contains("needs PRD"), "out:\n{text}");
    assert!(
        text.contains("5 documents · 3 ready · 1 blocked · 1 settled"),
        "out:\n{text}"
    );
    assert!(text.contains("SPEC-001"), "the blocked list names it; out:\n{text}");
    assert!(text.contains("← ADR-002"), "and what holds it; out:\n{text}");
}

// --- lineage (ADR-059, scope unchanged by ADR-118) ---------------------

#[test]
fn lineage_scopes_the_queue_to_the_dependents_of_the_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());
    let out = status_args(
        tmp.path(),
        &["--format", "json", "--lineage", "ADR-002"],
    );
    let text = stdout_of(&out);
    assert_eq!(out.status.code(), Some(0), "out:\n{text}");
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let ids: Vec<&str> = parsed["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["ADR-002", "SPEC-001"], "json:\n{parsed:#}");
    assert_eq!(parsed["lineage"], serde_json::json!("ADR-002"));
}

#[test]
fn an_unknown_lineage_root_exits_two() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());
    let out = status_args(tmp.path(), &["--lineage", "ADR-404"]);
    let combined = format!("{}{}", stdout_of(&out), stderr_of(&out));
    assert_eq!(out.status.code(), Some(2), "out:\n{combined}");
    assert!(
        combined.contains("pipeline.lineage-not-found"),
        "out:\n{combined}"
    );
}

// --- invariants that outlive the stage layer ---------------------------

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
fn status_modifies_no_file() {
    // EARS-05.3, unchanged by ADR-118: `status` reads and reports.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());
    let before = snapshot_tree(tmp.path());

    assert_eq!(status(tmp.path()).status.code(), Some(0));
    let _ = status_json(tmp.path());
    let _ = status_args(tmp.path(), &["--format", "mermaid"]);

    assert_eq!(
        before,
        snapshot_tree(tmp.path()),
        "EARS-05.3: `status` must leave every file unmodified"
    );
}

#[test]
fn dep_shape_exempts_edges_to_unmanaged_namespaces() {
    // ADR-039 § DAG-003, preserved by ADR-118 § STG-006: `core.dep-shape` is
    // now the *only* namespace-level edge declaration, so its admissibility
    // semantics matter more than before, not less. PRD is managed (SPEC
    // admits it); BUG is managed by nothing, so its edge is exempt.
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

    let out = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["lint", "--root", tmp.path().to_str().unwrap()])
        .output()
        .expect("ctxgrd lint runs");
    let stdout = stdout_of(&out);
    assert!(
        !stdout.contains("core.dep-shape"),
        "DAG-003: admitted and unmanaged-namespace edges are exempt; stdout:\n{stdout}"
    );
}

// --- The census partition (HANDOFF-037 § A2) ---------------------------
//
// `DocState` is 3-way exclusive, but the two bucket accessors were keyed
// off different things: `settled` counted `state() == Done`, while
// `blocked()` filtered on `!blocked_by.is_empty()` and ignored state
// entirely. A finished document pointing at open work therefore landed in
// two buckets at once, and the census line over-counted the corpus.
//
// Asserted as a pair per `ADR-112` § CLR-007 — the fix narrows what
// `blocked:` reports, so a test that only shows the narrowed case passing
// cannot tell the fix from a check that stopped firing. The paired half is
// `stg004_exit_code_is_one_while_a_document_is_blocked` above, which must
// keep failing on a genuinely stuck document.

/// The `HANDOFF-036` § 1 reproduction: one `accepted` ADR depending on one
/// `draft` ADR. The terminal document is pinned to `accepted` deliberately —
/// a status that is terminal both before and after the vocabulary widened,
/// so this fixture isolates the census fix from `DEFAULT_TERMINAL_STATUSES`.
fn write_settled_on_open_work(root: &Path) {
    write(
        &root.join("ctxgrd.toml"),
        "[ADR]\nowner = \"architect\"\npaths = [\"docs/adrs/**\"]\nrules = [\"core.id\"]\n",
    );
    write(
        &root.join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: accepted\ndepends_on: [ADR-002]\n---\n\n# ADR-001\n",
    );
    write(
        &root.join("docs/adrs/002-retention.md"),
        "---\nid: ADR-002\ntitle: Retention\nstatus: draft\n---\n\n# ADR-002\n",
    );
}

#[test]
fn census_buckets_partition_the_corpus() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_settled_on_open_work(tmp.path());

    let out = status(tmp.path());
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("2 documents · 1 ready · 0 blocked · 1 settled"),
        "the three buckets must sum to the document total; stdout:\n{stdout}"
    );
}

#[test]
fn census_a_settled_document_is_not_also_listed_as_blocked() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_settled_on_open_work(tmp.path());

    let stdout = stdout_of(&status(tmp.path()));
    assert!(
        !stdout.contains("\nblocked:\n"),
        "ADR-001 is accepted — it is finished, not stuck; stdout:\n{stdout}"
    );
}

#[test]
fn census_settled_on_open_work_is_disclosed_by_name() {
    // Narrowing `blocked:` must not silently drop the fact. `ADR-059`
    // § LIN-005's `shared:` disclosure is the precedent: name the members,
    // never fold them into a count.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_settled_on_open_work(tmp.path());

    let stdout = stdout_of(&status(tmp.path()));
    assert!(
        stdout.contains("settled on open work:"),
        "the disclosure line must survive the narrowing; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("ADR-001 ← ADR-002"),
        "the disclosure names both ends of the edge; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("enable core.dep-status to gate this"),
        "the disclosure points at the rule that turns it into a diagnostic; stdout:\n{stdout}"
    );
}

#[test]
fn census_exit_code_is_zero_when_only_a_settled_document_points_at_open_work() {
    // `unblocked()` follows `blocked()`, so `--exit-code` asks whether
    // anything is *stuck*. A finished document pointing at open work is
    // not stuck — that is `core.dep-status`'s question, and it is opt-in.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_settled_on_open_work(tmp.path());

    let out = status_args(tmp.path(), &["--exit-code"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "nothing in this corpus is blocked; stdout:\n{}",
        stdout_of(&out)
    );
}

#[test]
fn census_disclosure_is_absent_when_no_settled_document_points_at_open_work() {
    // The paired half of the disclosure: it must be driven by the corpus,
    // not printed unconditionally.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_corpus(tmp.path());

    let stdout = stdout_of(&status(tmp.path()));
    assert!(
        !stdout.contains("settled on open work:"),
        "no document here is both terminal and pointing at open work; stdout:\n{stdout}"
    );
}

// --- Terminal vocabulary (HANDOFF-037 § A3) ----------------------------
//
// `consumed` is terminal: a consumed handoff has been *executed*, so it
// has stopped moving and resting on it is safe.
//
// `rejected` and `deferred` stay out, for the same reason. Both have
// stopped moving in the claim-protocol sense — no agent should pick either
// up — but this vocabulary answers a second question that `ADR-106`
// § DPS-003 deliberately unified with the first: may a finished document
// rest on this one? A rejected decision settles nothing, and deferred work
// has not happened. Asserted as a trio, because the widening and the two
// exclusions are one decision, and the exclusions are the half that can
// silently rot.

/// One HANDOFF at `status`, and one ADR depending on it.
fn write_dependent_on_status(root: &Path, status: &str) {
    write(
        &root.join("ctxgrd.toml"),
        "[ADR]\nrules = [\"core.id\"]\n\n[HANDOFF]\npaths = [\"docs/handoffs/**\"]\nrules = [\"core.id\"]\n",
    );
    write(
        &root.join("docs/handoffs/001-wire-the-reconciler.md"),
        &format!("---\nid: HANDOFF-001\ntitle: Wire the reconciler\nstatus: {status}\n---\n\n# HANDOFF-001\n"),
    );
    write(
        &root.join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: accepted\ndepends_on: [HANDOFF-001]\n---\n\n# ADR-001\n",
    );
}

#[test]
fn a_consumed_document_is_settled_and_arms() {
    // The contrast case for the two below: `consumed` is in *both*
    // vocabularies (ADR-120 § TRM-001), so it settles the census AND arms
    // the dependency read. The absent disclosure is the arming half — if
    // `consumed` stopped arming, ADR-001 would surface as settled-on-open-work.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_dependent_on_status(tmp.path(), "consumed");

    let stdout = stdout_of(&status(tmp.path()));
    assert!(
        stdout.contains("2 documents · 0 ready · 0 blocked · 2 settled"),
        "a consumed handoff has been executed — it has stopped moving; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("settled on open work:"),
        "nothing is open here; stdout:\n{stdout}"
    );
}

// --- ADR-121 § SPL-001/SPL-002: settled is not arming -------------------
//
// These two are the split, asserted as a pair *inside each test* rather than
// across a pair of tests. Each status here answers the two questions
// differently, so a single assertion can never distinguish the fix from
// either failure mode: asserting only the census passes if the vocabularies
// were merged (BUG-037's warning — a change marking everything settled would
// satisfy it), and asserting only the disclosure passes if nothing changed
// at all. Both halves, or neither proves anything.

#[test]
fn a_deferred_document_is_settled_but_never_arms() {
    // `ADR-105` calls `deferred` an "unexecuted terminal": nobody should
    // pick a deferred handoff up, so it is **settled** and out of the work
    // queue. It still does not **arm** — deferred work is paused, not done,
    // and a finished document resting on it is precisely what
    // `core.dep-status` reports. `ADR-120` § TRM-002 declined it from the
    // arming vocabulary for `rejected`'s reason; `ADR-121` gave the census
    // its own set instead of widening that one.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_dependent_on_status(tmp.path(), "deferred");

    let stdout = stdout_of(&status(tmp.path()));
    assert!(
        stdout.contains("2 documents · 0 ready · 0 blocked · 2 settled"),
        "settled: a deferred handoff is not workable; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("ADR-001 ← HANDOFF-001"),
        "not arming: the edge into deferred work stays disclosed; stdout:\n{stdout}"
    );
}

#[test]
fn a_rejected_document_is_settled_but_never_arms() {
    // `BUG-037`'s motivating case. A rejected decision is finished — there
    // is no work left and it does not belong in a queue — but it settles
    // nothing, so `ADR-106` § DPS-003 keeps it out of the arming set and the
    // edge must stay visible.
    let tmp = tempfile::tempdir().expect("tempdir");
    write_dependent_on_status(tmp.path(), "rejected");

    let stdout = stdout_of(&status(tmp.path()));
    assert!(
        stdout.contains("2 documents · 0 ready · 0 blocked · 2 settled"),
        "settled: a rejected decision is not workable; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("ADR-001 ← HANDOFF-001"),
        "not arming: the edge into the rejected document stays disclosed; stdout:\n{stdout}"
    );
}

#[test]
fn a_draft_with_no_dependencies_is_still_ready() {
    // The green companion `BUG-037` names explicitly. Every assertion above
    // is satisfied by a mutation that marks *everything* settled; this is
    // the one that is not. If the settled set ever swallows the open
    // vocabulary, the queue empties and this fails.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("ctxgrd.toml"),
        "[ADR]\nrules = [\"core.id\"]\n",
    );
    write(
        &tmp.path().join("docs/adrs/001-ledger-store.md"),
        "---\nid: ADR-001\ntitle: Ledger store\nstatus: draft\n---\n\n# ADR-001\n",
    );

    let stdout = stdout_of(&status(tmp.path()));
    assert!(
        stdout.contains("1 documents · 1 ready · 0 blocked · 0 settled"),
        "an undecided draft with nothing holding it is workable; stdout:\n{stdout}"
    );
}

#[test]
fn every_arming_status_is_also_settled() {
    // The union is computed, not transcribed (`is_settled_status` is defined
    // over `DEFAULT_TERMINAL_STATUSES`). This pins the property end-to-end:
    // a status added to the arming vocabulary must never leave a document in
    // the ready queue. A copied literal would drift here first.
    for arming in [
        "accepted",
        "superseded",
        "done",
        "fixed",
        "wontfix",
        "invalid",
        "duplicate",
        "closed",
        "implemented",
        "consumed",
        "n/a",
    ] {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            &tmp.path().join("ctxgrd.toml"),
            "[ADR]\nrules = [\"core.id\"]\n",
        );
        write(
            &tmp.path().join("docs/adrs/001-ledger-store.md"),
            &format!("---\nid: ADR-001\ntitle: Ledger store\nstatus: {arming}\n---\n\n# ADR-001\n"),
        );

        let stdout = stdout_of(&status(tmp.path()));
        assert!(
            stdout.contains("1 documents · 0 ready · 0 blocked · 1 settled"),
            "`{arming}` arms, so it must settle too; stdout:\n{stdout}"
        );
    }
}
