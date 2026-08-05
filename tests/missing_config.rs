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
    // BUG-048: the run still succeeds, but it no longer passes silently.
    // Before, this printed `ok: 1 document · 6 rules` — indistinguishable
    // from a fully configured clean run, which is what let a subdirectory
    // invocation report success after checking almost nothing.
    assert!(
        stdout.contains("cfg.zero-config"),
        "a run with no config anywhere must disclose that it ran reduced; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("1 document"),
        "the disclosure must still report the corpus it linted — a warning \
         replaces the `ok:` trailer, so the count has to ride the message \
         or the fix costs the very information it exists to surface; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("ADR"),
        "the disclosure names the namespaces that fell back; stdout:\n{stdout}"
    );
    // ADR-038 § HINT-002: a clean run has nothing to fix — no hint. Still
    // true: `cfg.zero-config` is about the config, not a document defect.
    assert!(
        !stderr.contains("hint:"),
        "no document-fix hint on a clean run; stderr:\n{stderr}"
    );
}

/// The other half of BUG-048's disclosure: a *configured* clean run must
/// stay silent. Asserting only that the zero-config run warns would pass
/// just as well if `cfg.zero-config` fired on every run — the pairing is
/// what pins it to the condition (ADR-112 § CLR-007).
#[test]
fn configured_root_stays_quiet_and_keeps_the_ok_trailer() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join("docs/adrs")).expect("mkdir docs/adrs");
    fs::write(
        tmp.path().join("ctxgrd.toml"),
        "[ADR]\npaths = [\"docs/adrs/**\"]\nowner = \"developer\"\nrules = [\"core.frontmatter\", \"core.id\"]\n",
    )
    .expect("write ctxgrd.toml");
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

    let stderr = String::from_utf8(lint.stderr).expect("stderr utf-8");
    assert_eq!(lint.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(
        !stderr.contains("cfg.zero-config"),
        "a configured run must not claim it ran zero-config; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("ok: 1 document"),
        "a configured clean run keeps the ok: trailer; stderr:\n{stderr}"
    );
}

/// The exit-2 path must put one conforming object on stdout under
/// `--format json` (ADR-086 § WIRE-005).
///
/// `exit_code` lives *inside* the object, so an error path that writes nothing
/// makes the documented value `2` unreachable by construction: the consumer
/// sees an empty stream and cannot tell a broken config from a binary that
/// never ran. Before the fix stdout was empty here, so the parse below is the
/// assertion that bites.
#[test]
fn a_config_error_still_emits_a_conforming_json_object() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), "this is not toml [[[\n").expect("write config");

    let lint = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["--root", tmp.path().to_str().unwrap(), "--format", "json"])
        .output()
        .expect("ctxgrd lint runs");

    let stdout = String::from_utf8(lint.stdout).expect("stdout utf-8");
    let stderr = String::from_utf8(lint.stderr).expect("stderr utf-8");

    assert_eq!(lint.status.code(), Some(2), "stderr:\n{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be one JSON object, got {stdout:?}: {e}"));

    assert_eq!(v["exit_code"], 2, "the in-payload code must match the process exit");
    assert_eq!(v["diagnostics"][0]["code"], "cfg.invalid");
    assert_eq!(v["diagnostics"][0]["file"], "ctxgrd.toml");
    assert_eq!(v["summary"]["errors"], 1);
    assert_eq!(v["summary"]["files"], 0, "nothing was linted");

    // ADR-038 § HINT-004, on the wire this time. The rich renderer already
    // suppressed the "fix the documents, not the config" nudge for a config
    // error; the JSON path gated on the array being non-empty instead, so it
    // would have emitted advice that is backwards for exactly this failure.
    assert!(
        v.get("hint").is_none(),
        "the document-fix hint must not ride a config error: {v}"
    );

    // WIRE-007: stdout is the machine stream, stderr keeps the human block.
    // Without this the fix could pass by writing the object to both.
    assert!(stderr.contains("cfg.invalid"), "stderr keeps the error block:\n{stderr}");
    assert!(!stderr.contains("\"exit_code\""), "stderr must not carry the object:\n{stderr}");
}

/// The paired negative: the same failure without `--format json` must leave
/// stdout empty. Asserted so the fix cannot be "always print JSON", which would
/// corrupt the rich rendering for every human caller.
#[test]
fn a_config_error_without_json_keeps_stdout_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), "this is not toml [[[\n").expect("write config");

    let lint = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["--root", tmp.path().to_str().unwrap()])
        .output()
        .expect("ctxgrd lint runs");

    assert_eq!(lint.status.code(), Some(2));
    let stdout = String::from_utf8(lint.stdout).expect("stdout utf-8");
    assert!(stdout.is_empty(), "the rich path must not emit the object: {stdout:?}");
}

/// `--harness` owns a different wire contract — the Stop hook's
/// `{"decision":"block",…}` — and already fails closed on a config error. The
/// ADR-086 object must not also appear there: two unrelated objects on one
/// stream is what `emits_json` excluding the harness axis prevents.
#[test]
fn the_harness_axis_keeps_its_own_contract_on_a_config_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("ctxgrd.toml"), "this is not toml [[[\n").expect("write config");

    let lint = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["--root", tmp.path().to_str().unwrap(), "--harness", "claude"])
        .output()
        .expect("ctxgrd lint runs");

    let stdout = String::from_utf8(lint.stdout).expect("stdout utf-8");
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be one JSON object, got {stdout:?}: {e}"));
    assert_eq!(v["decision"], "block", "the Stop gate must still fail closed");
    assert!(
        v.get("exit_code").is_none(),
        "the ADR-086 object must not ride the harness stream: {v}"
    );
}
