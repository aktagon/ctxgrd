//! BUG-024 regression guards: one file that cannot be read (permission
//! denied, vanished between walk and read) must be a per-file outcome —
//! mirroring the BUG-011 decode fix — not an abort of the whole lint
//! with exit 2 anchored on the root.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

fn run(root: &Path) -> Output {
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap()])
        .output()
        .expect("ctxgrd executes")
}

const ADR_CONFIG: &str = r#"
[ADR]
owner = "developer"
paths = ["docs/adrs/**"]
rules = ["core.frontmatter", "core.id"]
"#;

fn lock(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
}

#[test]
fn unreadable_claimed_file_is_a_per_file_diagnostic_not_an_abort() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join("docs/adrs")).unwrap();
    fs::write(tmp.path().join("ctxgrd.toml"), ADR_CONFIG).unwrap();
    fs::write(
        tmp.path().join("docs/adrs/001-good.md"),
        "---\nid: ADR-1\ntitle: ok\n---\n## Status\n",
    )
    .unwrap();
    let locked = tmp.path().join("docs/adrs/002-locked.md");
    fs::write(&locked, "---\nid: ADR-2\ntitle: locked\n---\n## Status\n").unwrap();
    lock(&locked);

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "a claimed-but-unreadable file is one finding (exit 1), \
         not a kernel abort (exit 2):\n{stdout}"
    );
    assert!(
        stdout.contains("002-locked.md") && stdout.contains("src.markdown-read"),
        "the diagnostic must anchor on the unreadable file:\n{stdout}"
    );
    assert!(
        !stdout.contains("could not walk markdown tree"),
        "the whole-walk abort must not fire for a per-file read error:\n{stdout}"
    );
}

#[test]
fn unreadable_unclaimed_file_is_skipped_silently() {
    // BUG-011 precedent: an unclaimed file that cannot be read cannot
    // carry an id-claim we could check — skip it, exactly as an
    // unclaimed undecodable file is skipped.
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join("docs/adrs")).unwrap();
    fs::write(tmp.path().join("ctxgrd.toml"), ADR_CONFIG).unwrap();
    fs::write(
        tmp.path().join("docs/adrs/001-good.md"),
        "---\nid: ADR-1\ntitle: ok\n---\n## Status\n",
    )
    .unwrap();
    let scratch = tmp.path().join("notes.md");
    fs::write(&scratch, "scratch notes, no claim\n").unwrap();
    lock(&scratch);

    let out = run(tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "an unclaimed unreadable file must not fail the run:\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("ok: 1 document ·"),
        "the readable claimed document is still linted:\n{stderr}"
    );
}
