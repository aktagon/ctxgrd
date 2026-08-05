//! Integration tests for the ADR-076 namespace coverage gates —
//! `cfg.namespace-undeclared` (OWN-004), `cfg.namespace-unowned`
//! (OWN-003), and the `namespaces_undeclared` summary field (OWN-005).
//!
//! The fixture is the tree that motivated OWN-004: a config declaring only
//! `[ADR]`, and a `docs/reports/` convention invented ad hoc, carrying
//! `id: REPORT-<n>` with no `[REPORT]` block anywhere. Before this gate
//! that run reported `ok` while the REPORT documents linted under six
//! rules instead of the declared shape.

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

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn summary_json(root: &Path) -> serde_json::Value {
    let out = run(root, &["--format", "json"]);
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("--format json emits one object");
    v["summary"].clone()
}

const DECLARED_ADR_ONLY: &str = r#"
[ADR]
owner = "developer"
paths = ["docs/adrs/**"]
rules = ["core.frontmatter", "core.id", "core.id-unique", "core.cross-ref"]
"#;

/// One declared `[ADR]` document plus two documents claiming the
/// undeclared `REPORT` namespace. `002` is written first so the
/// lowest-numbered-claimant anchor is not merely walk order.
fn fixture(config: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("ctxgrd.toml"), config).unwrap();
    for dir in ["docs/adrs", "docs/reports"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::write(
        root.join("docs/adrs/001-a-real-one.md"),
        "---\nid: ADR-001\ntitle: A real one\nstatus: accepted\n---\n\n# ADR-001\n",
    )
    .unwrap();
    fs::write(
        root.join("docs/reports/002-second.md"),
        "---\nid: REPORT-002\ntitle: Second\n---\n\n# Second\n",
    )
    .unwrap();
    fs::write(
        root.join("docs/reports/001-phase-0.md"),
        "---\nid: REPORT-001\ntitle: Phase 0\n---\n\n# Phase 0\n",
    )
    .unwrap();
    tmp
}

/// OWN-004: one diagnostic per undeclared namespace — not per document —
/// anchored at the lowest-numbered claimant's `id:` line, and warning
/// severity, so the run still exits 0.
#[test]
fn undeclared_namespace_warns_once_and_exits_zero() {
    let tmp = fixture(DECLARED_ADR_ONLY);
    let out = run(tmp.path(), &[]);
    let text = stdout(&out);

    assert_eq!(
        text.matches("cfg.namespace-undeclared").count(),
        1,
        "one diagnostic per undeclared namespace, not per document:\n{text}"
    );
    assert!(
        text.contains("2 documents claim namespace 'REPORT'"),
        "message names the claimant count:\n{text}"
    );
    assert!(
        text.contains("docs/reports/001-phase-0.md:2:0"),
        "anchored at the lowest-numbered claimant's id: line:\n{text}"
    );
    assert!(
        text.contains("warning[cfg.namespace-undeclared]"),
        "warning severity — these runs exit 0 today:\n{text}"
    );
    assert_eq!(out.status.code(), Some(0), "a warning never escalates");
}

/// The same tree with `[REPORT]` declared is clean, and the summary
/// count returns to zero.
#[test]
fn declaring_the_namespace_clears_the_gate() {
    let config = format!(
        "{DECLARED_ADR_ONLY}\n[REPORT]\nowner = \"developer\"\n\
         rules = [\"core.frontmatter\", \"core.id\", \"core.id-unique\"]\n"
    );
    let tmp = fixture(&config);
    let out = run(tmp.path(), &[]);
    let text = stdout(&out);
    assert!(
        !text.contains("cfg.namespace-undeclared"),
        "declared namespace must not be reported:\n{text}"
    );
    assert_eq!(summary_json(tmp.path())["namespaces_undeclared"], 0);
}

/// Zero-config mode stays silent: with no `ctxgrd.toml` every namespace
/// is undeclared by definition, and the `zero_config` fallback is the
/// documented behaviour there rather than a defect.
#[test]
fn zero_config_mode_is_silent() {
    let tmp = fixture(DECLARED_ADR_ONLY);
    fs::remove_file(tmp.path().join("ctxgrd.toml")).unwrap();
    let out = run(tmp.path(), &[]);
    let text = stdout(&out);
    assert!(
        !text.contains("cfg.namespace-undeclared"),
        "zero-config mode must not fire the gate:\n{text}"
    );
    assert_eq!(summary_json(tmp.path())["namespaces_undeclared"], 0);
}

/// A scoped run reports only its own slice, so out-of-scope claimants
/// must not be mistaken for a coverage gap — the config was narrowed to
/// the scope before the diff runs.
#[test]
fn scoped_run_is_silent() {
    let tmp = fixture(DECLARED_ADR_ONLY);
    let out = run(tmp.path(), &["lint", "--namespace", "ADR"]);
    let text = stdout(&out);
    assert!(
        !text.contains("cfg.namespace-undeclared"),
        "a scoped run must not report out-of-scope namespaces:\n{text}"
    );
    assert_eq!(out.status.code(), Some(0));
}

/// `[ignore].namespaces` silences the warning without zeroing the count:
/// the documents really are linting under six rules, and OWN-005's field
/// is what keeps the `ok:` line honest about it.
#[test]
fn ignored_namespace_is_quiet_but_still_counted() {
    let config = format!("[ignore]\nnamespaces = [\"REPORT\"]\n{DECLARED_ADR_ONLY}");
    let tmp = fixture(&config);
    let out = run(tmp.path(), &[]);
    let text = stdout(&out);
    assert!(
        !text.contains("cfg.namespace-undeclared"),
        "the exemption silences the warning:\n{text}"
    );
    assert_eq!(summary_json(tmp.path())["namespaces_undeclared"], 1);
    // The human line carries the field only when nonzero — and here it is
    // reachable precisely because the warning was suppressed.
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("· 1 namespace undeclared"),
        "the ok: line reports coverage:\n{stderr}"
    );
}

/// OWN-003: a declared, document-bearing namespace with no `owner` is a
/// coverage gap; one that declares a role is clean.
#[test]
fn unowned_namespace_warns() {
    let config = DECLARED_ADR_ONLY.replace("owner = \"developer\"\n", "");
    let tmp = fixture(&config);
    let text = stdout(&run(tmp.path(), &[]));
    assert!(
        text.contains("warning[cfg.namespace-unowned]: [ADR] declares no owning role"),
        "an owner-less active namespace is reported:\n{text}"
    );

    let tmp = fixture(DECLARED_ADR_ONLY);
    let text = stdout(&run(tmp.path(), &[]));
    assert!(
        !text.contains("cfg.namespace-unowned"),
        "a namespace with an owner is clean:\n{text}"
    );
}

/// A namespace the config declares but no document claims is an empty
/// shelf, not a coverage gap — the gate stays off it.
#[test]
fn declared_but_empty_namespace_is_not_gated() {
    let config = format!("{DECLARED_ADR_ONLY}\n[PRD]\npaths = [\"docs/prds/**\"]\nrules = []\n");
    let tmp = fixture(&config);
    let text = stdout(&run(tmp.path(), &[]));
    assert!(
        !text.contains("[PRD] declares no owning role"),
        "a document-less namespace is not gated:\n{text}"
    );
}

/// `owner` is validated only against a declared vocabulary. Without a
/// `[roles]` table it is declare-only; with one, a value outside the list
/// is reported — ctxgrd checks a string against a config-declared list and
/// never discovers the harness's skill registry.
#[test]
fn owner_is_checked_against_declared_roles_only() {
    let leaf_skill = DECLARED_ADR_ONLY.replace("\"developer\"", "\"docs-requirements\"");
    let tmp = fixture(&leaf_skill);
    let text = stdout(&run(tmp.path(), &[]));
    assert!(
        !text.contains("cfg.namespace-unowned"),
        "no [roles] table means owner is declare-only:\n{text}"
    );

    let with_vocab = format!("[roles]\nallowed = [\"developer\", \"writer\"]\n{leaf_skill}");
    let tmp = fixture(&with_vocab);
    let text = stdout(&run(tmp.path(), &[]));
    assert!(
        text.contains("owner 'docs-requirements' is not in [roles].allowed"),
        "a value outside the declared vocabulary is reported:\n{text}"
    );
}

/// A `[roles]` table with no `allowed` key would reject every owner —
/// almost certainly a typo, so it is a config error (exit 2), not a
/// silently inert table.
#[test]
fn roles_table_without_allowed_is_a_config_error() {
    let tmp = fixture(&format!("[roles]\n{DECLARED_ADR_ONLY}"));
    let out = run(tmp.path(), &[]);
    assert_eq!(out.status.code(), Some(2));
    let text = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
    assert!(
        text.contains("cfg.roles-invalid"),
        "the malformed table is named:\n{text}"
    );
}
