//! Integration tests for `ctxgrd changelog` (ADR-084). Each test builds a
//! real temporary git repository with annotated release tags, then drives
//! the built binary and asserts on the structured `--format json` output.
//!
//! Coverage:
//! - CHG-004 first-terminal-tag attribution: a document appears under the
//!   first tag whose frozen tree marks it terminal, not an earlier tag
//!   where it was still open;
//! - `## [Unreleased]` = terminal-at-HEAD minus already-shipped;
//! - CHG-006 cutover: `since` excludes everything terminal at the cutover
//!   tree from the generated output.

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
    String::from_utf8(out.stdout)
        .expect("git stdout utf-8")
        .trim()
        .to_owned()
}

fn git_init(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "ci@aktagon.com"]);
    git(root, &["config", "user.name", "ctxgrd CI"]);
    git(root, &["config", "commit.gpgsign", "false"]);
}

/// Write the config: a path-claimed BUG namespace and a `[changelog]`
/// whitelisting it (`fixed` → `### Fixed`). `since` is included verbatim
/// when non-empty.
fn write_config(root: &Path, since: &str) {
    let since_line = if since.is_empty() {
        String::new()
    } else {
        format!("since = \"{since}\"\n")
    };
    std::fs::write(
        root.join("ctxgrd.toml"),
        format!(
            r#"[BUG]
paths = ["docs/bugs/**"]
rules = ["core.frontmatter", "core.id"]

[changelog]
namespaces = ["BUG"]
{since_line}
[changelog.BUG]
when = "fixed"
section = "Fixed"
"#
        ),
    )
    .unwrap();
}

/// Write a BUG document with the given id, title, and status.
fn write_bug(root: &Path, id: &str, title: &str, status: &str) {
    std::fs::create_dir_all(root.join("docs/bugs")).unwrap();
    std::fs::write(
        root.join(format!("docs/bugs/{id}.md")),
        format!("---\nid: {id}\ntitle: {title}\nstatus: {status}\n---\n\n# {id}\n"),
    )
    .unwrap();
}

/// Drive `ctxgrd changelog --format json` and parse the result.
fn changelog_json(root: &Path) -> Value {
    let out = Command::cargo_bin("ctxgrd")
        .unwrap()
        .args(["changelog", "--format", "json", "--root"])
        .arg(root)
        .output()
        .expect("binary runs");
    assert!(
        out.status.success(),
        "changelog failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json parses")
}

/// The entry ids under a version section's given category.
fn ids(section: &Value, category: &str) -> Vec<String> {
    section["sections"][category]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|e| e["id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Find the version block whose `version` equals `ver` (or the Unreleased
/// block when `ver` is `None`).
fn block<'a>(cl: &'a Value, ver: Option<&str>) -> Option<&'a Value> {
    cl["versions"].as_array().unwrap().iter().find(|v| {
        v["version"].as_str() == ver
    })
}

#[test]
fn first_terminal_tag_attribution_and_unreleased() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_init(root);
    write_config(root, "");

    // v0.1.0: BUG-001 is still open.
    write_bug(root, "BUG-001", "Login form loses focus", "open");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "release 0.1.0"]);
    git(root, &["tag", "-a", "v0.1.0", "-m", "0.1.0"]);

    // v0.2.0: BUG-001 fixed. This is its first terminal tag.
    write_bug(root, "BUG-001", "Login form loses focus", "fixed");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "release 0.2.0"]);
    git(root, &["tag", "-a", "v0.2.0", "-m", "0.2.0"]);

    // HEAD (unreleased): BUG-002 fixed, not yet shipped in any tag.
    write_bug(root, "BUG-002", "Export drops trailing rows", "fixed");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "fix export"]);

    let cl = changelog_json(root);

    // BUG-001 attributes to v0.2.0 (its first terminal tag), NOT v0.1.0.
    let v020 = block(&cl, Some("0.2.0")).expect("0.2.0 block present");
    assert_eq!(ids(v020, "Fixed"), vec!["BUG-001"]);
    // v0.1.0 marks nothing terminal → no block (empty releases are skipped).
    assert!(block(&cl, Some("0.1.0")).is_none(), "0.1.0 must not appear");
    // BUG-002 is terminal-at-HEAD but unshipped → Unreleased.
    let unrel = block(&cl, None).expect("Unreleased block present");
    assert_eq!(ids(unrel, "Fixed"), vec!["BUG-002"]);
    // Unreleased is the first version block (CHG-005 order).
    assert!(cl["versions"][0]["version"].is_null());
}

#[test]
fn cutover_since_excludes_shipped_from_unreleased() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_init(root);
    write_config(root, "v0.1.0");

    // v0.1.0 (the cutover): BUG-001 already fixed → shipped, must not
    // reappear in the generated output.
    write_bug(root, "BUG-001", "Login form loses focus", "fixed");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "release 0.1.0"]);
    git(root, &["tag", "-a", "v0.1.0", "-m", "0.1.0"]);

    // HEAD: BUG-001 still fixed (shipped), BUG-003 newly fixed.
    write_bug(root, "BUG-003", "Sidebar overlaps content", "fixed");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "fix sidebar"]);

    let cl = changelog_json(root);

    // The cutover tag itself is not re-rendered; BUG-001 (shipped at the
    // cutover) is absent from Unreleased.
    assert!(block(&cl, Some("0.1.0")).is_none(), "cutover tag not regenerated");
    let unrel = block(&cl, None).expect("Unreleased present");
    let unrel_ids = ids(unrel, "Fixed");
    assert_eq!(unrel_ids, vec!["BUG-003"], "only the post-cutover fix appears");
    assert!(!unrel_ids.contains(&"BUG-001".to_string()));
}
