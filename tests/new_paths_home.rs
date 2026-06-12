//! Integration test for ADR-010 NEW-004 / BUG-002: `ctxgrd new <NS>` on a
//! greenfield path-claimed namespace must scaffold into the directory the
//! namespace's `[<NS>].paths` glob declares, not the hardcoded `<ns>s/`
//! fallback.
//!
//! The unit tests in `src/scaffold.rs` cover `target_dir`'s ladder; this
//! exercises the real binary end-to-end, the regression guard for the
//! reported "file lands in `runs/`, fails the path-claim, has to be moved
//! by hand" behavior.

use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

fn run_new(root: &Path, namespace: &str, title: &str) -> Output {
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap(), "new", namespace, title])
        .output()
        .expect("ctxgrd executes")
}

const RUN_CONFIG: &str = r#"
[RUN]
paths = ["docs/runbooks/**"]
rules = ["core.frontmatter", "core.id"]
"#;

#[test]
fn new_lands_in_declared_paths_home_on_greenfield_namespace() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("ctxgrd.toml"), RUN_CONFIG).unwrap();

    let out = run_new(tmp.path(), "RUN", "Fetch Kanta service events");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(0), "new should succeed\n{stderr}");

    let expected = tmp
        .path()
        .join("docs/runbooks/001-fetch-kanta-service-events.md");
    assert!(
        expected.exists(),
        "file must land in the declared docs/runbooks/ home\n{stderr}"
    );
    // The old hardcoded fallback must NOT be used.
    assert!(
        !tmp.path().join("runs").exists(),
        "the hardcoded runs/ fallback must not be created\n{stderr}"
    );
}
