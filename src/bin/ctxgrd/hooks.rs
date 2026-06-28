//! `ctxgrd hooks` — pre-commit and Claude Code Stop-hook wiring (ADR-014, ADR-062).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;

use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::run;

use super::{emit_error, relative_display};

/// Render the pre-commit hook script. The hook is intentionally
/// minimal (ADR-014 § HOOK-005): it delegates entirely to `ctxgrd` and
/// lets the three-valued exit code decide the commit — a non-zero exit
/// (lint failure `1` or kernel error `2`) aborts it. The
/// `command -v ctxgrd` guard hard-gates: a machine that has activated the
/// hook but lacks ctxgrd on PATH aborts the commit with an actionable
/// message, rather than silently skipping the lint (ADR-014 § HOOK-005).
fn render_precommit_hook(root: &std::path::Path) -> String {
    let root_arg = root.display();
    format!(
        "#!/bin/sh\n\
         # Installed by `ctxgrd hooks install` (ADR-014).\n\
         # Gates commits on ctxgrd; a non-zero exit aborts the commit.\n\
         command -v ctxgrd >/dev/null 2>&1 || {{\n\
         \techo 'ctxgrd not found on PATH — install it or remove this hook (unset core.hooksPath)' >&2\n\
         \texit 1\n\
         }}\n\
         # Signals commit context so the agents.context-cache rule can warn\n\
         # on cache-busting edits to CLAUDE.md/AGENTS.md (ADR-020).\n\
         export CTXGRD_COMMIT_CONTEXT=1\n\
         exec ctxgrd --root \"{root_arg}\"\n"
    )
}

/// The shared `run-parts`-style pre-commit *runner* (ADR-014 § HOOK-008).
/// When `core.hooksPath` points away from `.git/hooks/`, ctxgrd composes with
/// sibling `*grd` tools by dropping a fragment into `<hooksPath>/pre-commit.d/`
/// rather than claiming the single pre-commit slot. This runner dispatches every
/// executable fragment in that directory in lexical order and aborts the commit
/// on the first non-zero exit; it owns no checks of its own.
///
/// These bytes are **byte-identical** to the runner the sibling wrkgrd installer
/// writes (`PRE_COMMIT` in wrkgrd's `src/hooks.rs`, ADR-016 § WRK-065 in the
/// wrkgrd repo). Whichever `*grd` tool writes the runner first, the other's
/// re-install reproduces the same bytes, so the two installers are idempotent
/// against each other and never churn the shared file. Do not edit one side
/// without the other.
const PRE_COMMIT_RUNNER: &str = "#!/bin/sh
# Tracked pre-commit runner — installed by `wrkgrd hooks install`, shared via
# git under .githooks. Enable with scripts/setup-hooks.sh, or:
#   git config core.hooksPath .githooks
# Runs every executable fragment in pre-commit.d/ (lexical order); a non-zero
# fragment aborts the commit. Drop a sibling gate in as its own fragment.
set -eu
dir=\"$(dirname \"$0\")/pre-commit.d\"
[ -d \"$dir\" ] || exit 0
for fragment in \"$dir\"/*; do
\t[ -f \"$fragment\" ] || continue
\t[ -x \"$fragment\" ] || continue
\t\"$fragment\" \"$@\" || exit $?
done
";

/// The fragment filename ctxgrd owns inside `pre-commit.d/`. The `10-` prefix
/// orders ctxgrd's doc gate *before* wrkgrd's `50-wrkgrd` code gate, so the
/// cheap document lint fails fast ahead of the build/test gate.
const CTXGRD_FRAGMENT_NAME: &str = "10-ctxgrd";

/// ctxgrd's own pre-commit fragment, carrying the gate that lives in the
/// monolithic hook: the BUG-010 hard-gate (a missing `ctxgrd` aborts the commit
/// with `exit 1`, never `exit 0`), the `CTXGRD_COMMIT_CONTEXT=1` export
/// (ADR-020), and `exec ctxgrd --root "."`. Refreshed on every (re-)install;
/// sibling fragments in this directory are never touched (ADR-014 § HOOK-009).
const CTXGRD_FRAGMENT: &str = "#!/bin/sh\n\
     # ctxgrd pre-commit fragment — installed by `ctxgrd hooks install` (ADR-014).\n\
     # Refreshed on every (re-)install; sibling fragments here are never touched.\n\
     command -v ctxgrd >/dev/null 2>&1 || {\n\
     \techo 'ctxgrd not found on PATH — install it or remove this fragment' >&2\n\
     \texit 1\n\
     }\n\
     # Signals commit context so the agents.context-cache rule can warn\n\
     # on cache-busting edits to CLAUDE.md/AGENTS.md (ADR-020).\n\
     export CTXGRD_COMMIT_CONTEXT=1\n\
     exec ctxgrd --root \".\"\n";

/// Resolve the active hooks directory from `git config core.hooksPath`.
///
/// Returns `Ok(None)` when `core.hooksPath` is unset or names the default
/// `.git/hooks` — the legacy single-slot path is then taken unchanged. Returns
/// `Ok(Some(dir))` (resolved against `root` when relative) when it points
/// elsewhere — the composable drop-in path is taken so a sibling `*grd` gate is
/// not shadowed (ADR-014 § HOOK-008). Shelling out to `git` keeps the lookup
/// canonical rather than hand-parsing the INI; a missing `git` or an unset key
/// is simply "default".
fn active_hooks_path(root: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--get", "core.hooksPath"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout);
    let value = value.trim();
    if value.is_empty() || value == ".git/hooks" {
        return None;
    }
    let path = Path::new(value);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    })
}

#[cfg(unix)]
fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Install the composable `pre-commit.d/` drop-in into `hooks_path` (ADR-014
/// §§ HOOK-008, HOOK-009). Ensures the shared `run-parts` runner exists at
/// `<hooks_path>/pre-commit` (byte-identical to wrkgrd's), then writes ctxgrd's
/// `10-ctxgrd` fragment (mode 0755).
///
/// Idempotent: re-running refreshes only the runner and `10-ctxgrd`, and NEVER
/// deletes or modifies a foreign fragment (e.g. `50-wrkgrd`). `--force` is not
/// consulted here — the drop-in never clobbers another tool's work, so there is
/// nothing to force. A fragment without the execute bit is silently skipped by
/// the runner, so the fragment is always chmod 0755.
fn install_dropin(root: &PathBuf, hooks_path: &Path) -> Result<ExitCode> {
    let runner = hooks_path.join("pre-commit");
    let fragment_dir = hooks_path.join("pre-commit.d");
    let fragment = fragment_dir.join(CTXGRD_FRAGMENT_NAME);

    if let Err(e) = fs::create_dir_all(&fragment_dir) {
        let rel = relative_display(&fragment_dir, root);
        let d = Diagnostic::error("io.mkdir", &rel, 0, 0, format!("could not create {rel}"))
            .with_help("check file permissions on the hooks directory")
            .with_note(format!("cause: {e}"));
        emit_error(&d, root);
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }

    // The shared runner: write it only when absent or stale, so a re-install
    // never churns a runner a sibling already wrote with identical bytes.
    let runner_current = fs::read_to_string(&runner).ok();
    if runner_current.as_deref() != Some(PRE_COMMIT_RUNNER) {
        if let Err(e) = fs::write(&runner, PRE_COMMIT_RUNNER.as_bytes()) {
            let rel = relative_display(&runner, root);
            let d = Diagnostic::error("io.write", &rel, 0, 0, format!("could not write {rel}"))
                .with_help("check file permissions")
                .with_note(format!("cause: {e}"));
            emit_error(&d, root);
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
        if let Err(e) = set_executable(&runner) {
            let rel = relative_display(&runner, root);
            let d = Diagnostic::error("io.chmod", &rel, 0, 0, format!("could not chmod {rel}"))
                .with_help("check file permissions")
                .with_note(format!("cause: {e}"));
            emit_error(&d, root);
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    }

    // ctxgrd's own fragment is always (re-)written — foreign fragments in the
    // directory are left untouched.
    if let Err(e) = fs::write(&fragment, CTXGRD_FRAGMENT.as_bytes()) {
        let rel = relative_display(&fragment, root);
        let d = Diagnostic::error("io.write", &rel, 0, 0, format!("could not write {rel}"))
            .with_help("check file permissions")
            .with_note(format!("cause: {e}"));
        emit_error(&d, root);
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }
    if let Err(e) = set_executable(&fragment) {
        let rel = relative_display(&fragment, root);
        let d = Diagnostic::error("io.chmod", &rel, 0, 0, format!("could not chmod {rel}"))
            .with_help("check file permissions")
            .with_note(format!("cause: {e}"));
        emit_error(&d, root);
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }

    let frag_rel = relative_display(&fragment, root);
    let runner_rel = relative_display(&runner, root);
    println!("{frag_rel}");
    println!();
    println!(
        "core.hooksPath points at {} — installed ctxgrd as a composable",
        relative_display(hooks_path, root)
    );
    println!("pre-commit fragment so a sibling *grd gate is not shadowed.");
    println!("The shared run-parts runner is at {runner_rel}; it dispatches every");
    println!("executable fragment in pre-commit.d/ and aborts on the first failure.");
    println!("Remove ctxgrd's gate with `rm {frag_rel}`.");
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

/// The pre-commit-framework snippet printed when `.pre-commit-config.yaml`
/// is present (ADR-014 § HOOK-004). The `rev` tracks the installed
/// binary's version so the printed pin matches what the user has.
fn render_precommit_framework_snippet() -> String {
    format!(
        "repos:\n\
         \x20\x20- repo: https://github.com/aktagon/ctxgrd\n\
         \x20\x20\x20\x20rev: v{version}\n\
         \x20\x20\x20\x20hooks:\n\
         \x20\x20\x20\x20\x20\x20- id: ctxgrd\n",
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// The guarded Stop-hook command (ADR-062 § STOP-004). Mirrors the
/// pre-commit hook's `command -v` guard: if `ctxgrd` is not on PATH the
/// hook exits 0 (allow) rather than blocking the turn — a gate that cannot
/// find its tool must never trap the agent. Otherwise it `exec`s the lint
/// in `--harness claude` mode, which reads the Stop payload from stdin.
const CLAUDE_STOP_HOOK_COMMAND: &str =
    "command -v ctxgrd >/dev/null 2>&1 || exit 0; exec ctxgrd lint --harness claude";

/// The `settings.json` Stop-hook block a user pastes to wire the gate.
fn render_claude_stop_settings_snippet() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "hooks": {
            "Stop": [
                { "hooks": [ { "type": "command", "command": CLAUDE_STOP_HOOK_COMMAND } ] }
            ]
        }
    }))
    .expect("static JSON serialises")
}

/// True when `path`'s parsed settings JSON already wires a ctxgrd
/// `--harness claude` Stop hook. Best-effort and read-only: a missing or
/// unparseable file is simply "not wired".
fn claude_stop_hook_wired(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    json.get("hooks")
        .and_then(|h| h.get("Stop"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|groups| {
            groups.iter().any(|g| {
                g.get("hooks")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|c| c.contains("ctxgrd") && c.contains("--harness claude"))
                        })
                    })
            })
        })
}

/// `hooks claude`: print the Claude Code Stop-hook wiring and report
/// whether it is already installed. Print-and-detect only — never mutates
/// the shared, user-global `settings.json` (ADR-062 § STOP-004).
pub(super) fn hooks_claude_cmd(root: &Path) -> Result<ExitCode> {
    println!("Claude Code Stop-hook — a turn-end lint gate (ADR-062).");
    println!("Add this to .claude/settings.json (project) or ~/.claude/settings.json (global):");
    println!();
    println!("{}", render_claude_stop_settings_snippet());
    println!();
    println!("It runs `ctxgrd lint --harness claude` when the agent ends a turn;");
    println!("an error-severity diagnostic blocks the turn until it is fixed. Warnings");
    println!("never block, and a clean run is silent.");
    println!();

    // Detection: the project-local file and the user-global one — the two
    // places Claude Code reads Stop hooks from.
    let project = root.join(".claude").join("settings.json");
    let global = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".claude").join("settings.json"));

    let mut wired_anywhere = false;
    if claude_stop_hook_wired(&project) {
        println!("wired: {}", relative_display(&project, root));
        wired_anywhere = true;
    }
    if let Some(global) = &global {
        if claude_stop_hook_wired(global) {
            println!("wired: {}", global.display());
            wired_anywhere = true;
        }
    }
    if !wired_anywhere {
        println!("not wired: no ctxgrd `--harness claude` Stop hook found in project or global settings.");
    }
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

pub(super) fn hooks_install_cmd(root: &PathBuf, force: bool, dry_run: bool) -> Result<ExitCode> {
    // HOOK-004: a repo managed by the pre-commit framework owns its
    // hooks — writing a raw `.git/hooks/pre-commit` would be clobbered
    // on the framework's next `pre-commit install`. Detect it and emit
    // the framework's native config instead. This takes precedence over
    // everything else, including --dry-run: the "what would I do" answer
    // here is simply "print this snippet".
    if root.join(".pre-commit-config.yaml").exists() {
        println!(
            "{} already exists — add ctxgrd to it rather than writing a raw hook:",
            relative_display(&root.join(".pre-commit-config.yaml"), root)
        );
        println!();
        print!("{}", render_precommit_framework_snippet());
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
    }

    let script = render_precommit_hook(root);

    // HOOK-006: --dry-run previews the script and writes nothing. Allowed
    // outside a git repo too — it is a harmless preview.
    if dry_run {
        print!("{script}");
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
    }

    // Git-repo precondition: a `.git` directory must be present. Worktrees
    // and submodules (where `.git` is a file) are deferred per ADR-014's
    // Open Questions — report rather than guess.
    let git_dir = root.join(".git");
    if !git_dir.is_dir() {
        let rel = relative_display(root, root);
        let d = Diagnostic::error(
            "hooks.not-a-repo",
            &rel,
            0,
            0,
            "not a git repository (no .git directory)".to_string(),
        )
        .with_help("run `ctxgrd hooks install` from a git repository root")
        .with_note("pass --root to point at the repository containing .git");
        emit_error(&d, root);
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }

    // HOOK-008: when `core.hooksPath` points away from `.git/hooks/`, the raw
    // `.git/hooks/pre-commit` this command used to write would be ignored by
    // git — silently dead on arrival, and a sibling `*grd` gate that owns the
    // active hooks dir would shadow ctxgrd entirely (BUG-012). Install ctxgrd
    // as a composable `pre-commit.d/10-ctxgrd` fragment in the *active*
    // directory instead, beside the shared run-parts runner. This path is
    // idempotent and `--force`-free — it never clobbers another tool's work.
    if let Some(hooks_path) = active_hooks_path(root) {
        return install_dropin(root, &hooks_path);
    }

    let hook_path = git_dir.join("hooks").join("pre-commit");

    // HOOK-003: never clobber an existing hook without --force. Mirrors
    // init_cmd's ctxgrd.toml guard.
    if hook_path.exists() && !force {
        let rel = relative_display(&hook_path, root);
        let d = Diagnostic::error("io.exists", &rel, 0, 0, format!("{rel} already exists"))
            .with_help("re-run with --force to overwrite, or --dry-run to preview");
        emit_error(&d, root);
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }

    let hooks_dir = git_dir.join("hooks");
    if let Err(e) = fs::create_dir_all(&hooks_dir) {
        let rel = relative_display(&hooks_dir, root);
        let d = Diagnostic::error("io.mkdir", &rel, 0, 0, format!("could not create {rel}"))
            .with_help("check file permissions on the .git directory")
            .with_note(format!("cause: {e}"));
        emit_error(&d, root);
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }
    if let Err(e) = fs::write(&hook_path, script.as_bytes()) {
        let rel = relative_display(&hook_path, root);
        let d = Diagnostic::error("io.write", &rel, 0, 0, format!("could not write {rel}"))
            .with_help("check file permissions")
            .with_note(format!("cause: {e}"));
        emit_error(&d, root);
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)) {
            let rel = relative_display(&hook_path, root);
            let d = Diagnostic::error("io.chmod", &rel, 0, 0, format!("could not chmod {rel}"))
                .with_help("check file permissions")
                .with_note(format!("cause: {e}"));
            emit_error(&d, root);
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    }

    let rel = relative_display(&hook_path, root);
    println!("{rel}");
    println!();
    println!("Installed a pre-commit hook. It runs `ctxgrd` before each commit;");
    println!("a lint failure aborts the commit. Remove it with `rm {rel}`.");
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

#[cfg(test)]
mod tests {
    use super::{CTXGRD_FRAGMENT, CTXGRD_FRAGMENT_NAME, PRE_COMMIT_RUNNER};

    /// The shared runner ctxgrd writes MUST be byte-identical to the runner the
    /// sibling wrkgrd installer writes (`PRE_COMMIT` in wrkgrd's `src/hooks.rs`),
    /// so the two installers are idempotent against each other (ADR-014 §
    /// HOOK-008). This is the wrkgrd literal inlined; if wrkgrd's runner changes,
    /// this guard fails and forces both sides to be updated together.
    #[test]
    fn runner_is_byte_identical_to_wrkgrd() {
        // Verbatim copy of wrkgrd's `PRE_COMMIT` constant.
        let wrkgrd = "#!/bin/sh
# Tracked pre-commit runner — installed by `wrkgrd hooks install`, shared via
# git under .githooks. Enable with scripts/setup-hooks.sh, or:
#   git config core.hooksPath .githooks
# Runs every executable fragment in pre-commit.d/ (lexical order); a non-zero
# fragment aborts the commit. Drop a sibling gate in as its own fragment.
set -eu
dir=\"$(dirname \"$0\")/pre-commit.d\"
[ -d \"$dir\" ] || exit 0
for fragment in \"$dir\"/*; do
\t[ -f \"$fragment\" ] || continue
\t[ -x \"$fragment\" ] || continue
\t\"$fragment\" \"$@\" || exit $?
done
";
        assert_eq!(
            PRE_COMMIT_RUNNER, wrkgrd,
            "the shared run-parts runner must match wrkgrd's byte-for-byte"
        );
    }

    /// BUG-010 / BUG-012 invariant: ctxgrd's drop-in fragment hard-gates — a
    /// missing `ctxgrd` aborts the commit (`exit 1`), never fails open
    /// (`exit 0`) — and carries the `CTXGRD_COMMIT_CONTEXT=1` export and the
    /// `ctxgrd --root "."` invocation.
    #[test]
    fn fragment_hard_gates_and_keeps_commit_context() {
        assert!(CTXGRD_FRAGMENT.contains("command -v ctxgrd"));
        assert!(
            CTXGRD_FRAGMENT.contains("exit 1"),
            "fragment must block, not skip"
        );
        assert!(
            !CTXGRD_FRAGMENT.contains("exit 0"),
            "fragment must not fail open"
        );
        assert!(CTXGRD_FRAGMENT.contains("export CTXGRD_COMMIT_CONTEXT=1"));
        assert!(CTXGRD_FRAGMENT.contains("exec ctxgrd --root \".\""));
    }

    /// The `10-` prefix orders ctxgrd's doc gate before wrkgrd's `50-wrkgrd`.
    #[test]
    fn fragment_name_orders_before_wrkgrd() {
        assert_eq!(CTXGRD_FRAGMENT_NAME, "10-ctxgrd");
        assert!(CTXGRD_FRAGMENT_NAME < "50-wrkgrd");
    }
}
