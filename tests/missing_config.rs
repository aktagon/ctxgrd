//! Integration tests for the unconfigured-root failure path: a root
//! with no `ctxgrd.toml`, no global namespaces, and no documents that
//! claim intent must exit 2 with `cfg.missing` instead of reporting
//! a false-confidence `ok: 0 documents · 0 rules · 0 diagnostics`.
//!
//! The companion test pins the zero-config contract (config.rs § load):
//! an id-claimed document without any `ctxgrd.toml` still lints under
//! the zero-config defaults and exits 0 — the failure path must not
//! swallow zero-config mode.

use std::fs;

use assert_cmd::Command;

#[test]
fn unconfigured_root_with_nothing_to_lint_exits_2() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // A plain markdown file with no frontmatter — the realistic shape
    // of running `ctxgrd` in the wrong directory. It claims no intent,
    // so nothing is linted.
    fs::write(
        tmp.path().join("README.md"),
        "# billing-service\n\nInvoice reconciliation for acme-corp.\n",
    )
    .expect("write README.md");

    let lint = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        // Isolate from the developer's real ~/.ctxgrd global config —
        // global namespaces would legitimize the unconfigured root.
        .env("HOME", tmp.path())
        .args(["--root", tmp.path().to_str().unwrap()])
        .output()
        .expect("ctxgrd lint runs");

    let stdout = String::from_utf8(lint.stdout).expect("stdout utf-8");
    let stderr = String::from_utf8(lint.stderr).expect("stderr utf-8");

    assert_eq!(
        lint.status.code(),
        Some(2),
        "an unconfigured root with nothing to lint must exit 2; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("cfg.missing"),
        "error must carry the cfg.missing code; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("ctxgrd init"),
        "error must point at `ctxgrd init`; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("ok:"),
        "the ok summary must not render on the failure path; stderr:\n{stderr}"
    );
    // ADR-038 § HINT-004: the "fix the documents, not the config" hint
    // must not fire for a config error (exit 2) — here the fault *is*
    // the configuration, so the nudge would be self-contradictory.
    assert!(
        !stderr.contains("hint:"),
        "the document-fix hint must not render for a config error; stderr:\n{stderr}"
    );
}

#[test]
fn zero_config_root_with_id_claimed_doc_still_lints_and_exits_0() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join("docs/adrs")).expect("mkdir docs/adrs");
    // Minimal id-claimed document that passes all six ZERO_CONFIG_RULES.
    fs::write(
        tmp.path().join("docs/adrs/001-use-postgres.md"),
        "---\nid: ADR-001\ntitle: Use Postgres for invoice storage\n---\n\n# Use Postgres\n",
    )
    .expect("write ADR-001");

    let lint = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["--root", tmp.path().to_str().unwrap()])
        .output()
        .expect("ctxgrd lint runs");

    let stdout = String::from_utf8(lint.stdout).expect("stdout utf-8");
    let stderr = String::from_utf8(lint.stderr).expect("stderr utf-8");

    assert_eq!(
        lint.status.code(),
        Some(0),
        "zero-config mode (id-claimed doc, no toml) must stay clean; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("ok: 1 document"),
        "the zero-config run must report the linted document; stderr:\n{stderr}"
    );
    // ADR-038 § HINT-002: a clean run has nothing to fix — no hint.
    assert!(
        !stderr.contains("hint:"),
        "no document-fix hint on a clean run; stderr:\n{stderr}"
    );
}
