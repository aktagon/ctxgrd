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
fn dry_run_previews_the_plan_and_writes_nothing() {
    let tmp = git_repo_tempdir();

    let output = run_hooks_install(tmp.path(), &["--dry-run"]);
    assert_eq!(output.status.code(), Some(0), "dry-run exits 0");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("would install"),
        "dry-run previews the plan; got:\n{stdout}"
    );
    assert!(
        stdout.contains("10-ctxgrd"),
        "dry-run names the composable fragment; got:\n{stdout}"
    );
    assert!(
        stdout.contains("core.hooksPath .githooks"),
        "dry-run says it would set core.hooksPath; got:\n{stdout}"
    );
    assert!(
        !tmp.path().join(".githooks").exists(),
        "dry-run writes no .githooks tree"
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

/// A real git repo with `core.hooksPath` left unset — the fresh-repo default the
/// installer establishes. Returns `None` (test skips) when `git` is unavailable.
fn git_repo_default() -> Option<tempfile::TempDir> {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return None;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q"]);
    Some(tmp)
}

/// Read `git config --get core.hooksPath`, trimmed; empty string when unset.
fn read_hooks_path(root: &Path) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--get", "core.hooksPath"])
        .output()
        .expect("git config runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// HOOK-010: the fresh-repo default installs the tracked `.githooks/` composable
// hook and sets `core.hooksPath` — wrkgrd's posture — never an untracked
// `.git/hooks/pre-commit`.

#[test]
fn default_install_writes_tracked_githooks_and_sets_hooks_path() {
    let Some(tmp) = git_repo_default() else {
        return;
    };

    let output = run_hooks_install(tmp.path(), &[]);
    assert_eq!(output.status.code(), Some(0), "default install exits 0");

    let fragment = tmp.path().join(".githooks/pre-commit.d/10-ctxgrd");
    let runner = tmp.path().join(".githooks/pre-commit");
    let setup = tmp.path().join("scripts/setup-hooks.sh");
    assert!(fragment.is_file(), "ctxgrd installed as 10-ctxgrd fragment");
    assert!(runner.is_file(), "shared run-parts runner written");
    assert!(setup.is_file(), "fresh-clone bootstrap written");
    assert!(
        !tmp.path().join(".git/hooks/pre-commit").exists(),
        "never writes an untracked .git/hooks/pre-commit"
    );
    assert_eq!(
        read_hooks_path(tmp.path()),
        ".githooks",
        "core.hooksPath is set to the tracked directory"
    );

    let body = fs::read_to_string(&fragment).expect("fragment readable");
    assert!(
        body.contains("exec ctxgrd --root \".\""),
        "fragment ends in the ctxgrd invocation; got:\n{body}"
    );
    let setup_body = fs::read_to_string(&setup).expect("setup readable");
    assert!(
        setup_body.contains("config core.hooksPath .githooks"),
        "bootstrap sets core.hooksPath; got:\n{setup_body}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in [&fragment, &runner, &setup] {
            let mode = fs::metadata(f).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "{} is executable (0o755)", f.display());
        }
    }
}

#[test]
fn default_install_leaves_a_preexisting_git_hooks_precommit_untouched() {
    let Some(tmp) = git_repo_default() else {
        return;
    };
    // A stale hand-written hook under the *untracked* .git/hooks — the installer
    // moves to .githooks and must not touch it (it clobbers nothing).
    let stale = tmp.path().join(".git/hooks/pre-commit");
    let original = "#!/bin/sh\necho pre-existing hook\n";
    fs::write(&stale, original).unwrap();

    let output = run_hooks_install(tmp.path(), &[]);
    assert_eq!(output.status.code(), Some(0), "install exits 0");
    assert_eq!(
        fs::read_to_string(&stale).unwrap(),
        original,
        "the pre-existing .git/hooks/pre-commit is left byte-identical"
    );
    assert!(
        tmp.path().join(".githooks/pre-commit.d/10-ctxgrd").is_file(),
        "ctxgrd's gate lands in the tracked .githooks tree"
    );
}

#[test]
fn force_is_a_noop_and_reinstall_is_idempotent() {
    let Some(tmp) = git_repo_default() else {
        return;
    };
    assert_eq!(run_hooks_install(tmp.path(), &[]).status.code(), Some(0));
    let fragment = tmp.path().join(".githooks/pre-commit.d/10-ctxgrd");
    let first = fs::read_to_string(&fragment).unwrap();

    // Re-running with --force is accepted and changes nothing (the drop-in never
    // clobbers, so there is nothing to force).
    assert_eq!(
        run_hooks_install(tmp.path(), &["--force"]).status.code(),
        Some(0),
        "--force re-install exits 0 (no refuse-without-force in the drop-in path)"
    );
    assert_eq!(
        fs::read_to_string(&fragment).unwrap(),
        first,
        "re-install leaves ctxgrd's fragment byte-identical"
    );
    assert_eq!(
        read_hooks_path(tmp.path()),
        ".githooks",
        "core.hooksPath stays .githooks"
    );
}

#[test]
fn custom_hookspath_is_respected_not_overridden() {
    let Some(tmp) = git_repo_with_hooks_path(".myhooks") else {
        return;
    };

    let output = run_hooks_install(tmp.path(), &[]);
    assert_eq!(output.status.code(), Some(0), "install exits 0");

    assert!(
        tmp.path().join(".myhooks/pre-commit.d/10-ctxgrd").is_file(),
        "ctxgrd composes into the user's custom hooksPath"
    );
    assert!(
        !tmp.path().join(".githooks").exists(),
        "does not create a .githooks tree beside the custom hooksPath"
    );
    assert!(
        !tmp.path().join("scripts/setup-hooks.sh").exists(),
        "does not write a bootstrap that would fight the custom hooksPath"
    );
    assert_eq!(
        read_hooks_path(tmp.path()),
        ".myhooks",
        "the user's custom core.hooksPath is left unchanged"
    );
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
