//! Integration tests for `core.commit-freshness` and `ctxgrd pin --bless`
//! (ADR-040). Each test builds a real temporary git repository — init,
//! commit, branch — then drives the built binary against it and asserts
//! on the JSON diagnostic stream.
//!
//! Coverage (PIN-002/003/004/005):
//! - stale on a committed scoped change after the pin;
//! - stale on an *uncommitted* scoped edit (working-tree sensitivity);
//! - green on an unrelated (out-of-scope) change;
//! - exit-1 hard error when the pin is not an ancestor of HEAD;
//! - skip-with-warning on a shallow clone (PIN-004);
//! - `pin --bless` rewrites the commit and the document then lints green
//!   on a clean tree.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::Value;

/// Run a git command in `root`, asserting success, returning trimmed stdout.
fn git(root: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git executes");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("git stdout utf-8").trim().to_owned()
}

/// Initialise a deterministic git repo at `root` with identity set so
/// commits succeed in CI without global git config.
fn git_init(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "ci@aktagon.com"]);
    git(root, &["config", "user.name", "ctxgrd CI"]);
    git(root, &["config", "commit.gpgsign", "false"]);
}

/// Write the ctxgrd.toml that opts the SECREV namespace into
/// `core.commit-freshness` and path-claims the review docs.
fn write_config(root: &Path) {
    std::fs::write(
        root.join("ctxgrd.toml"),
        r#"[SECREV]
paths = ["docs/secrevs/**"]
rules = ["core.commit-freshness"]

[SECREV."core.commit-freshness"]
require-pin = true
"#,
    )
    .unwrap();
}

/// Write a SECREV review document pinned to `commit` with `scope`.
fn write_review(root: &Path, commit: &str, scope: &str) {
    std::fs::create_dir_all(root.join("docs/secrevs")).unwrap();
    std::fs::write(
        root.join("docs/secrevs/SECREV-001-auth.md"),
        format!(
            "---\nid: SECREV-001\ntitle: Auth boundary review\npin:\n  commit: {commit}\n  scope:\n    - {scope}\n---\n\n# Auth boundary review\n"
        ),
    )
    .unwrap();
}

fn lint_json(root: &Path) -> Value {
    let out = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap(), "lint", "--format", "json"])
        .output()
        .expect("ctxgrd executes");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "lint --format json did not emit JSON: {e}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// All `core.commit-freshness` diagnostics from a lint run.
fn freshness_diags(report: &Value) -> Vec<&Value> {
    report["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .filter(|d| d["code"] == "core.commit-freshness")
        .collect()
}

#[test]
fn stale_on_committed_scoped_change() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_init(root);
    write_config(root);
    std::fs::create_dir_all(root.join("src/auth")).unwrap();
    std::fs::write(root.join("src/auth/token.rs"), "fn verify() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial auth"]);
    let pinned = git(root, &["rev-parse", "HEAD"]);

    // Advance src/auth/ past the pin with a committed change.
    std::fs::write(root.join("src/auth/token.rs"), "fn verify() { /* v2 */ }\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "rework auth"]);

    write_review(root, &pinned, "src/auth/**");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add review"]);

    let report = lint_json(root);
    let diags = freshness_diags(&report);
    assert_eq!(diags.len(), 1, "exactly one stale diagnostic: {report}");
    assert_eq!(diags[0]["severity"], "error");
    let msg = diags[0]["message"].as_str().unwrap();
    assert!(
        msg.contains("SECREV-001") && msg.contains("src/auth/token.rs"),
        "message names the doc and the stale path: {msg}"
    );
    assert_eq!(report["exit_code"], 1);
}

#[test]
fn stale_on_uncommitted_scoped_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_init(root);
    write_config(root);
    std::fs::create_dir_all(root.join("src/auth")).unwrap();
    std::fs::write(root.join("src/auth/token.rs"), "fn verify() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial auth"]);
    let pinned = git(root, &["rev-parse", "HEAD"]);

    write_review(root, &pinned, "src/auth/**");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add review"]);

    // Edit a scoped file WITHOUT committing — working-tree sensitivity.
    std::fs::write(root.join("src/auth/token.rs"), "fn verify() { /* dirty */ }\n").unwrap();

    let report = lint_json(root);
    let diags = freshness_diags(&report);
    assert_eq!(
        diags.len(),
        1,
        "uncommitted scoped edit is stale (PIN-003): {report}"
    );
    let msg = diags[0]["message"].as_str().unwrap();
    assert!(msg.contains("src/auth/token.rs"), "names the dirty path: {msg}");
}

#[test]
fn green_on_unrelated_change() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_init(root);
    write_config(root);
    std::fs::create_dir_all(root.join("src/auth")).unwrap();
    std::fs::create_dir_all(root.join("src/billing")).unwrap();
    std::fs::write(root.join("src/auth/token.rs"), "fn verify() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial"]);
    let pinned = git(root, &["rev-parse", "HEAD"]);

    // Change a file OUTSIDE the pin scope.
    std::fs::write(root.join("src/billing/invoice.rs"), "fn charge() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "billing work"]);

    write_review(root, &pinned, "src/auth/**");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add review"]);

    let report = lint_json(root);
    let diags = freshness_diags(&report);
    assert_eq!(
        diags.len(),
        0,
        "out-of-scope change stays green: {report}"
    );
}

#[test]
fn non_ancestor_pin_is_hard_error() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_init(root);
    write_config(root);
    std::fs::create_dir_all(root.join("src/auth")).unwrap();
    std::fs::write(root.join("src/auth/token.rs"), "fn verify() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "main commit"]);

    // Create a sibling branch with a commit that is NOT an ancestor of
    // the branch we lint on.
    git(root, &["checkout", "-q", "-b", "sibling"]);
    std::fs::write(root.join("src/auth/token.rs"), "fn verify() { /* sibling */ }\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "sibling commit"]);
    let sibling = git(root, &["rev-parse", "HEAD"]);
    git(root, &["checkout", "-q", "master"]);

    write_review(root, &sibling, "src/auth/**");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add review"]);

    let report = lint_json(root);
    let diags = freshness_diags(&report);
    assert_eq!(diags.len(), 1, "non-ancestor pin errors: {report}");
    assert_eq!(diags[0]["severity"], "error");
    let msg = diags[0]["message"].as_str().unwrap();
    assert!(
        msg.contains("not an ancestor of HEAD"),
        "names the non-ancestor failure: {msg}"
    );
    assert_eq!(report["exit_code"], 1);
}

#[test]
fn shallow_clone_skips_with_warning() {
    // Build an origin repo, then a shallow clone of it; pin to a commit
    // the shallow clone cannot reach.
    let origin_tmp = tempfile::tempdir().unwrap();
    let origin = origin_tmp.path();
    git_init(origin);
    std::fs::create_dir_all(origin.join("src/auth")).unwrap();
    std::fs::write(origin.join("src/auth/token.rs"), "fn v1() {}\n").unwrap();
    git(origin, &["add", "-A"]);
    git(origin, &["commit", "-q", "-m", "c1"]);
    let first = git(origin, &["rev-parse", "HEAD"]);
    std::fs::write(origin.join("src/auth/token.rs"), "fn v2() {}\n").unwrap();
    git(origin, &["add", "-A"]);
    git(origin, &["commit", "-q", "-m", "c2"]);

    let clone_tmp = tempfile::tempdir().unwrap();
    let root = clone_tmp.path().join("shallow");
    // A plain-path local clone hardlinks the full object store, ignoring
    // `--depth`; a `file://` URL forces a real shallow fetch (PIN-004).
    let origin_url = format!("file://{}", origin.to_str().unwrap());
    let out = StdCommand::new("git")
        .args([
            "clone",
            "-q",
            "--depth",
            "1",
            &origin_url,
            root.to_str().unwrap(),
        ])
        .output()
        .expect("git clone executes");
    assert!(out.status.success(), "shallow clone: {}", String::from_utf8_lossy(&out.stderr));
    git(&root, &["config", "user.email", "ci@aktagon.com"]);
    git(&root, &["config", "user.name", "ctxgrd CI"]);

    write_config(&root);
    // Pin to the FIRST commit — unreachable in a depth-1 clone.
    write_review(&root, &first, "src/auth/**");

    let report = lint_json(&root);
    let diags = freshness_diags(&report);
    assert_eq!(diags.len(), 1, "shallow clone warns: {report}");
    assert_eq!(
        diags[0]["severity"], "warning",
        "shallow degradation is a warning, not an error (PIN-004): {report}"
    );
    let msg = diags[0]["message"].as_str().unwrap();
    assert!(
        msg.contains("commit-freshness skipped"),
        "names the skip: {msg}"
    );
    // The skip must not push the exit code to 1.
    assert_eq!(report["exit_code"], 0, "skip-with-warning keeps exit 0");
}

#[test]
fn bless_updates_commit_and_lints_green() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_init(root);
    write_config(root);
    std::fs::create_dir_all(root.join("src/auth")).unwrap();
    std::fs::write(root.join("src/auth/token.rs"), "fn verify() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial auth"]);
    let stale_pin = git(root, &["rev-parse", "HEAD"]);

    // Advance scoped code so the original pin is stale.
    std::fs::write(root.join("src/auth/token.rs"), "fn verify() { /* v2 */ }\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "rework auth"]);

    write_review(root, &stale_pin, "src/auth/**");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add review"]);

    // Sanity: stale before blessing.
    let before = lint_json(root);
    assert_eq!(freshness_diags(&before).len(), 1, "stale before bless");

    let head = git(root, &["rev-parse", "HEAD"]);
    let bless = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap(), "pin", "--bless", "SECREV-001"])
        .output()
        .expect("ctxgrd executes");
    assert_eq!(
        bless.status.code(),
        Some(0),
        "bless exits 0: {}",
        String::from_utf8_lossy(&bless.stderr)
    );

    // The pin.commit line is rewritten to HEAD; nothing else changes.
    let doc = std::fs::read_to_string(root.join("docs/secrevs/SECREV-001-auth.md")).unwrap();
    assert!(
        doc.contains(&format!("commit: {head}")),
        "pin.commit rewritten to HEAD ({head}):\n{doc}"
    );
    assert!(doc.contains("title: Auth boundary review"), "title untouched:\n{doc}");
    assert!(doc.contains("- src/auth/**"), "scope untouched:\n{doc}");

    // After blessing, on a clean tree, the doc lints green.
    let after = lint_json(root);
    assert_eq!(
        freshness_diags(&after).len(),
        0,
        "green after bless on a clean tree: {after}"
    );
}

#[test]
fn bless_refuses_dirty_scoped_tree_without_force() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_init(root);
    write_config(root);
    std::fs::create_dir_all(root.join("src/auth")).unwrap();
    std::fs::write(root.join("src/auth/token.rs"), "fn verify() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial auth"]);
    let pinned = git(root, &["rev-parse", "HEAD"]);

    write_review(root, &pinned, "src/auth/**");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add review"]);

    // Dirty a scoped file without committing.
    std::fs::write(root.join("src/auth/token.rs"), "fn verify() { /* WIP */ }\n").unwrap();

    let bless = Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap(), "pin", "--bless", "SECREV-001"])
        .output()
        .expect("ctxgrd executes");
    assert_ne!(
        bless.status.code(),
        Some(0),
        "bless must refuse over a dirty scoped tree (PIN-005)"
    );
    let stderr = String::from_utf8_lossy(&bless.stderr);
    assert!(
        stderr.contains("uncommitted") || stderr.contains("--force"),
        "names the dirty-tree refusal: {stderr}"
    );
}
