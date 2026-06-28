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

// --- HOOK-008 / HOOK-009: hooksPath-aware composable drop-in (BUG-012) ---
//
// These tests need a real git repo so `git config core.hooksPath` is readable.
// They are skipped when `git` is not on PATH rather than failing the suite.

fn git(root: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} succeeded");
}

/// A real git repo with `core.hooksPath` set to `.githooks`. Returns `None`
/// (test skips) when `git` is unavailable.
fn git_repo_with_hooks_path(dir: &str) -> Option<tempfile::TempDir> {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return None;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q"]);
    git(tmp.path(), &["config", "core.hooksPath", dir]);
    Some(tmp)
}

#[test]
fn nondefault_hookspath_installs_dropin_fragment_not_git_hooks() {
    let Some(tmp) = git_repo_with_hooks_path(".githooks") else {
        return;
    };

    let output = run_hooks_install(tmp.path(), &[]);
    assert_eq!(output.status.code(), Some(0), "drop-in install exits 0");

    let fragment = tmp.path().join(".githooks/pre-commit.d/10-ctxgrd");
    assert!(fragment.is_file(), "ctxgrd installed as 10-ctxgrd fragment");
    assert!(
        !tmp.path().join(".git/hooks/pre-commit").exists(),
        "never writes the dead .git/hooks/pre-commit under a non-default hooksPath"
    );

    let body = fs::read_to_string(&fragment).expect("fragment readable");
    assert!(body.contains("command -v ctxgrd"), "fragment hard-gates");
    assert!(
        body.contains("exit 1") && !body.contains("exit 0"),
        "hard-gate aborts (exit 1), never fails open (exit 0); got:\n{body}"
    );
    assert!(
        body.contains("export CTXGRD_COMMIT_CONTEXT=1"),
        "fragment keeps the commit-context export; got:\n{body}"
    );
    assert!(
        body.contains("exec ctxgrd --root \".\""),
        "fragment ends in the ctxgrd invocation; got:\n{body}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&fragment).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "fragment is executable (0o755)");
        let runner = tmp.path().join(".githooks/pre-commit");
        let rmode = fs::metadata(&runner).unwrap().permissions().mode();
        assert_eq!(rmode & 0o777, 0o755, "runner is executable (0o755)");
    }
}

#[test]
fn dropin_runner_dispatches_fragments_and_aborts_on_nonzero() {
    let Some(tmp) = git_repo_with_hooks_path(".githooks") else {
        return;
    };
    assert_eq!(run_hooks_install(tmp.path(), &[]).status.code(), Some(0));

    let runner = tmp.path().join(".githooks/pre-commit");
    let body = fs::read_to_string(&runner).expect("runner readable");
    assert!(body.contains("pre-commit.d"), "runner sources pre-commit.d/");
    assert!(
        body.contains("for fragment in") && body.contains("|| exit $?"),
        "runner dispatches fragments and aborts on first non-zero; got:\n{body}"
    );

    // Drive the runner under sh with a failing fragment ahead of a sentinel; the
    // runner must abort with the fragment's code and never reach the sentinel.
    let frag_dir = tmp.path().join(".githooks/pre-commit.d");
    fs::write(frag_dir.join("00-fail"), "#!/bin/sh\nexit 7\n").unwrap();
    fs::write(frag_dir.join("99-sentinel"), "#!/bin/sh\ntouch reached\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in ["00-fail", "99-sentinel"] {
            fs::set_permissions(frag_dir.join(f), fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    let status = std::process::Command::new("sh")
        .arg(&runner)
        .current_dir(tmp.path())
        .status()
        .expect("sh runs the runner");
    assert_eq!(status.code(), Some(7), "runner propagates the failing code");
    assert!(
        !tmp.path().join("reached").exists(),
        "a non-zero fragment stops the runner before later fragments"
    );
}

#[test]
fn reinstall_preserves_a_foreign_fragment() {
    let Some(tmp) = git_repo_with_hooks_path(".githooks") else {
        return;
    };
    assert_eq!(run_hooks_install(tmp.path(), &[]).status.code(), Some(0));

    // A sibling tool's fragment lands in the same directory.
    let foreign = tmp.path().join(".githooks/pre-commit.d/50-wrkgrd");
    let foreign_body = "#!/bin/sh\nexec wrkgrd verify\n";
    fs::write(&foreign, foreign_body).unwrap();

    // Re-running ctxgrd install is idempotent and must not touch the foreign one.
    assert_eq!(
        run_hooks_install(tmp.path(), &[]).status.code(),
        Some(0),
        "re-install is idempotent (no refuse-without-force in the drop-in path)"
    );
    assert_eq!(
        fs::read_to_string(&foreign).unwrap(),
        foreign_body,
        "the foreign 50-wrkgrd fragment is left byte-identical"
    );
    assert!(
        tmp.path().join(".githooks/pre-commit.d/10-ctxgrd").is_file(),
        "ctxgrd's own fragment is still present after re-install"
    );
}

#[test]
fn dropin_runner_skips_a_non_executable_fragment() {
    let Some(tmp) = git_repo_with_hooks_path(".githooks") else {
        return;
    };
    assert_eq!(run_hooks_install(tmp.path(), &[]).status.code(), Some(0));

    let runner = tmp.path().join(".githooks/pre-commit");
    let frag_dir = tmp.path().join(".githooks/pre-commit.d");

    // A fragment that would fail *if run*, but without the execute bit — the
    // runner's `[ -x ]` guard must skip it, so the commit is not aborted.
    let inert = frag_dir.join("20-inert");
    fs::write(&inert, "#!/bin/sh\nexit 9\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&inert, fs::Permissions::from_mode(0o644)).unwrap();
        // Keep ctxgrd's own fragment from failing the run (no ctxgrd on the
        // test PATH would `exit 1`): strip its execute bit too so only the
        // skip behaviour is under test.
        let ctx = frag_dir.join("10-ctxgrd");
        fs::set_permissions(&ctx, fs::Permissions::from_mode(0o644)).unwrap();
    }

    let status = std::process::Command::new("sh")
        .arg(&runner)
        .current_dir(tmp.path())
        .status()
        .expect("sh runs the runner");
    assert_eq!(
        status.code(),
        Some(0),
        "a non-executable fragment is silently skipped, so the runner exits clean"
    );
}
