//! Integration tests for `ctxgrd hooks install` (ADR-014).
//!
//! Each test runs the binary against a tempdir. A bare `.git/`
//! directory is created by hand so the tests stay hermetic — they do
//! not depend on the `git` binary being present, only on the guard
//! that `.git` exists (HOOK-002 / the git-repo precondition).

use std::fs;
use std::path::Path;

use assert_cmd::Command;

/// Create a tempdir that looks like a git repo to `hooks install`:
/// a `.git/` directory present, no hooks installed yet.
fn git_repo_tempdir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join(".git")).expect("create .git");
    tmp
}

fn run_hooks_install(root: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["--root", root.to_str().unwrap(), "hooks", "install"];
    args.extend_from_slice(extra);
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(&args)
        .output()
        .expect("ctxgrd executes")
}

#[test]
fn install_writes_executable_precommit_hook_that_runs_ctxgrd() {
    let tmp = git_repo_tempdir();

    let output = run_hooks_install(tmp.path(), &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "install exits 0 in a git repo"
    );

    let hook = tmp.path().join(".git/hooks/pre-commit");
    assert!(hook.is_file(), "pre-commit hook written");

    let body = fs::read_to_string(&hook).expect("hook readable");
    assert!(body.starts_with("#!/bin/sh"), "hook is a POSIX sh script");
    assert!(
        body.contains("exec ctxgrd --root"),
        "hook delegates to ctxgrd; got:\n{body}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&hook).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "hook is executable (0o755)");
    }
}

#[test]
fn install_refuses_to_clobber_existing_hook_without_force() {
    let tmp = git_repo_tempdir();
    let hooks_dir = tmp.path().join(".git/hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-commit");
    let original = "#!/bin/sh\necho existing\n";
    fs::write(&hook, original).unwrap();

    let output = run_hooks_install(tmp.path(), &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "refusing to clobber exits 2 (kernel error)"
    );
    assert_eq!(
        fs::read_to_string(&hook).unwrap(),
        original,
        "existing hook is left byte-identical"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--force"), "error names the --force remedy");
}

#[test]
fn install_force_overwrites_existing_hook() {
    let tmp = git_repo_tempdir();
    let hooks_dir = tmp.path().join(".git/hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-commit");
    fs::write(&hook, "#!/bin/sh\necho existing\n").unwrap();

    let output = run_hooks_install(tmp.path(), &["--force"]);
    assert_eq!(output.status.code(), Some(0), "--force install exits 0");
    let body = fs::read_to_string(&hook).unwrap();
    assert!(
        body.contains("exec ctxgrd --root"),
        "hook overwritten with the ctxgrd hook"
    );
}

#[test]
fn dry_run_prints_script_and_writes_nothing() {
    let tmp = git_repo_tempdir();

    let output = run_hooks_install(tmp.path(), &["--dry-run"]);
    assert_eq!(output.status.code(), Some(0), "dry-run exits 0");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("exec ctxgrd --root"),
        "dry-run prints the hook script; got:\n{stdout}"
    );
    assert!(
        !tmp.path().join(".git/hooks/pre-commit").exists(),
        "dry-run writes no hook file"
    );
}

#[test]
fn precommit_framework_present_prints_snippet_and_writes_no_hook() {
    let tmp = git_repo_tempdir();
    fs::write(tmp.path().join(".pre-commit-config.yaml"), "repos: []\n").unwrap();

    let output = run_hooks_install(tmp.path(), &[]);
    assert_eq!(output.status.code(), Some(0), "framework path exits 0");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("- id: ctxgrd"),
        "prints a pre-commit hook snippet referencing ctxgrd; got:\n{stdout}"
    );
    assert!(
        !tmp.path().join(".git/hooks/pre-commit").exists(),
        "no raw hook written when the framework manages hooks"
    );
}

#[test]
fn install_outside_git_repo_exits_2() {
    let tmp = tempfile::tempdir().expect("tempdir"); // no .git

    let output = run_hooks_install(tmp.path(), &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "install outside a git repo is a kernel error"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.to_lowercase().contains("git"),
        "error explains the missing git repo; got:\n{stderr}"
    );
}
