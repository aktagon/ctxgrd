//! `ctxgrd hooks` — pre-commit and Claude Code Stop-hook wiring (ADR-014, ADR-062).

use std::fs;
use std::path::{Path, PathBuf};

use ctxgrd::diagnostic::Diagnostic;

use super::command::{Command, Ctx, KernelError, Outcome};
use super::{relative_display, Format};

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

/// The fresh-clone bootstrap `scripts/setup-hooks.sh` (ADR-014 § HOOK-010).
/// `core.hooksPath` is *local* git config, not tracked in the repo, so a clone
/// that never runs this has the tracked `.githooks/` sitting inert. This script
/// activates it. Written only when absent (`write_setup_hooks_if_absent`), so it
/// never clobbers the byte-different bootstrap a sibling `*grd` tool (wrkgrd) may
/// already have committed — both do the same `git config`, so whichever is present
/// is correct. The echo names the `*grd` family, not ctxgrd specifically.
const SETUP_HOOKS: &str = "#!/bin/sh\n\
     set -eu\n\
     # Point this repo's git hooks at the tracked .githooks directory.\n\
     git -C \"$(git rev-parse --show-toplevel)\" config core.hooksPath .githooks\n\
     echo \"core.hooksPath -> .githooks (grd pre-commit hooks active)\"\n";

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

/// Write the composable `pre-commit.d/` drop-in into `hooks_path` (ADR-014
/// §§ HOOK-008, HOOK-009, HOOK-010). Ensures the shared `run-parts` runner exists
/// at `<hooks_path>/pre-commit` (byte-identical to wrkgrd's), then writes ctxgrd's
/// `10-ctxgrd` fragment (mode 0755). Does no printing — the caller composes the
/// success report so it can describe the surrounding install (`core.hooksPath`,
/// `setup-hooks.sh`) that this function knows nothing about.
///
/// Idempotent: re-running refreshes only the runner and `10-ctxgrd`, and NEVER
/// deletes or modifies a foreign fragment (e.g. `50-wrkgrd`). A fragment without
/// the execute bit is silently skipped by the runner, so the fragment is always
/// chmod 0755. On any I/O failure it returns the diagnostic for the dispatcher
/// to render (exit 2).
fn write_dropin(root: &Path, hooks_path: &Path) -> Result<(), KernelError> {
    let runner = hooks_path.join("pre-commit");
    let fragment_dir = hooks_path.join("pre-commit.d");
    let fragment = fragment_dir.join(CTXGRD_FRAGMENT_NAME);

    if let Err(e) = fs::create_dir_all(&fragment_dir) {
        let rel = relative_display(&fragment_dir, root);
        let d = Diagnostic::error("io.mkdir", &rel, 0, 0, format!("could not create {rel}"))
            .with_help("check file permissions on the hooks directory")
            .with_note(format!("cause: {e}"));
        return Err(KernelError::report(d));
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
            return Err(KernelError::report(d));
        }
        if let Err(e) = set_executable(&runner) {
            let rel = relative_display(&runner, root);
            let d = Diagnostic::error("io.chmod", &rel, 0, 0, format!("could not chmod {rel}"))
                .with_help("check file permissions")
                .with_note(format!("cause: {e}"));
            return Err(KernelError::report(d));
        }
    }

    // ctxgrd's own fragment is always (re-)written — foreign fragments in the
    // directory are left untouched.
    if let Err(e) = fs::write(&fragment, CTXGRD_FRAGMENT.as_bytes()) {
        let rel = relative_display(&fragment, root);
        let d = Diagnostic::error("io.write", &rel, 0, 0, format!("could not write {rel}"))
            .with_help("check file permissions")
            .with_note(format!("cause: {e}"));
        return Err(KernelError::report(d));
    }
    if let Err(e) = set_executable(&fragment) {
        let rel = relative_display(&fragment, root);
        let d = Diagnostic::error("io.chmod", &rel, 0, 0, format!("could not chmod {rel}"))
            .with_help("check file permissions")
            .with_note(format!("cause: {e}"));
        return Err(KernelError::report(d));
    }

    Ok(())
}

/// Write the fresh-clone bootstrap `scripts/setup-hooks.sh` (ADR-014 § HOOK-010),
/// but only when it does not already exist — never clobbering a sibling `*grd`
/// tool's committed bootstrap. Returns `Ok(true)` when it wrote the file,
/// `Ok(false)` when one was already present, and `Err` (exit 2) on an I/O failure.
fn write_setup_hooks_if_absent(root: &Path) -> Result<bool, KernelError> {
    let scripts = root.join("scripts");
    let setup = scripts.join("setup-hooks.sh");
    if setup.exists() {
        return Ok(false);
    }
    if let Err(e) = fs::create_dir_all(&scripts) {
        let rel = relative_display(&scripts, root);
        let d = Diagnostic::error("io.mkdir", &rel, 0, 0, format!("could not create {rel}"))
            .with_help("check file permissions")
            .with_note(format!("cause: {e}"));
        return Err(KernelError::report(d));
    }
    if let Err(e) = fs::write(&setup, SETUP_HOOKS.as_bytes()) {
        let rel = relative_display(&setup, root);
        let d = Diagnostic::error("io.write", &rel, 0, 0, format!("could not write {rel}"))
            .with_help("check file permissions")
            .with_note(format!("cause: {e}"));
        return Err(KernelError::report(d));
    }
    if let Err(e) = set_executable(&setup) {
        let rel = relative_display(&setup, root);
        let d = Diagnostic::error("io.chmod", &rel, 0, 0, format!("could not chmod {rel}"))
            .with_help("check file permissions")
            .with_note(format!("cause: {e}"));
        return Err(KernelError::report(d));
    }
    Ok(true)
}

/// `git -C <root> config core.hooksPath .githooks` (ADR-014 § HOOK-010). Shelling
/// out to git keeps the config write canonical (correct section, quoting) rather
/// than hand-editing the INI, and reuses the binary already required to use a hook
/// at all. Mirrors wrkgrd's `set_hooks_path`. On failure emits a `hooks.git-config`
/// diagnostic (exit 2).
fn set_hooks_path(root: &Path) -> Result<(), KernelError> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "core.hooksPath", ".githooks"])
        .output();
    let failed = match out {
        Ok(o) if o.status.success() => return Ok(()),
        Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
        Err(e) => e.to_string(),
    };
    let rel = relative_display(root, root);
    let d = Diagnostic::error(
        "hooks.git-config",
        &rel,
        0,
        0,
        "could not set core.hooksPath to .githooks".to_string(),
    )
    .with_help("run `git config core.hooksPath .githooks` manually")
    .with_note(format!("cause: {failed}"));
    Err(KernelError::report(d))
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

/// `ctxgrd hooks claude` — print the Claude Code Stop-hook wiring and report
/// whether it is already installed. Print-and-detect only — never mutates the
/// shared, user-global `settings.json` (ADR-062 § STOP-004).
pub(super) struct HooksClaudeCmd {
    pub(super) format: Format,
}

impl Command for HooksClaudeCmd {
    type Json = serde_json::Value;

    fn emits_json(&self) -> bool {
        matches!(self.format, Format::Json)
    }

    fn render_json(out: &Self::Json) -> String {
        serde_json::to_string_pretty(out).unwrap_or_else(|_| "{}".to_string())
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        // Detection: the project-local file and the user-global one — the two
        // places Claude Code reads Stop hooks from. Read-only (STOP-004).
        let project = root.join(".claude").join("settings.json");
        let global = std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|h| h.join(".claude").join("settings.json"));

        let mut wired_paths: Vec<String> = Vec::new();
        if claude_stop_hook_wired(&project) {
            wired_paths.push(relative_display(&project, root));
        }
        if let Some(global) = &global {
            if claude_stop_hook_wired(global) {
                wired_paths.push(global.display().to_string());
            }
        }
        let wired_anywhere = !wired_paths.is_empty();

        // ADR-096 § CMD-003: `--format json` emits `{installed, wiring}` on a
        // clean stdout (the dispatcher writes it) so an agent can branch on
        // install state without parsing the wiring block.
        if matches!(self.format, Format::Json) {
            let obj = serde_json::json!({
                "installed": wired_anywhere,
                "wiring": {
                    "command": CLAUDE_STOP_HOOK_COMMAND,
                    "settings_snippet": render_claude_stop_settings_snippet(),
                    "wired_paths": wired_paths,
                },
            });
            return Ok(Outcome::Did(obj));
        }

        println!("Claude Code Stop-hook — a turn-end lint gate (ADR-062).");
        println!("Add this to .claude/settings.json (project) or ~/.claude/settings.json (global):");
        println!();
        println!("{}", render_claude_stop_settings_snippet());
        println!();
        println!("It runs `ctxgrd lint --harness claude` when the agent ends a turn;");
        println!("an error-severity diagnostic blocks the turn until it is fixed. Warnings");
        println!("never block, and a clean run is silent.");
        println!();

        for path in &wired_paths {
            println!("wired: {path}");
        }
        if !wired_anywhere {
            println!("not wired: no ctxgrd `--harness claude` Stop hook found in project or global settings.");
        }
        Ok(Outcome::Did(serde_json::Value::Null))
    }
}

/// `ctxgrd hooks install` — install ctxgrd as a composable pre-commit fragment.
pub(super) struct HooksInstallCmd {
    pub(super) force: bool,
    pub(super) dry_run: bool,
    pub(super) format: Format,
}

/// ADR-096 § CMD-001 wire shape for `ctxgrd hooks install --format json`.
#[derive(serde::Serialize)]
pub(super) struct HooksInstallJson {
    status: &'static str,
    path: String,
}

impl Command for HooksInstallCmd {
    type Json = HooksInstallJson;

    fn emits_json(&self) -> bool {
        matches!(self.format, Format::Json)
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        // `--force` is retained for CLI back-compat only (ADR-014 § HOOK-010):
        // the composable drop-in never clobbers a foreign fragment, so there is
        // nothing to force. Kept so old invocations do not error.
        let _ = self.force;
        let dry_run = self.dry_run;

        // ADR-096 § CMD-001: `--format json` emits `{status, path}` on a clean
        // stdout; the human "Next steps" narration stays on stderr.
        let json = matches!(self.format, Format::Json);

        // HOOK-004: a repo managed by the pre-commit framework owns its hooks —
        // a tracked `.githooks/` gate would sit unused while the framework
        // drives `.git/hooks/`. Detect it and emit the framework's native
        // config instead. Takes precedence over everything, including --dry-run.
        if root.join(".pre-commit-config.yaml").exists() {
            let config_rel = relative_display(&root.join(".pre-commit-config.yaml"), root);
            if json {
                eprintln!("{config_rel} already exists — add ctxgrd to it rather than writing a raw hook:");
                eprintln!();
                eprint!("{}", render_precommit_framework_snippet());
            } else {
                println!("{config_rel} already exists — add ctxgrd to it rather than writing a raw hook:");
                println!();
                print!("{}", render_precommit_framework_snippet());
            }
            return Ok(Outcome::Did(HooksInstallJson {
                status: "framework",
                path: config_rel,
            }));
        }

        // HOOK-010: install ctxgrd as a composable fragment under a *tracked*
        // hooks directory (wrkgrd's convention), never an untracked
        // `.git/hooks/pre-commit`. Read `core.hooksPath`: a custom
        // (non-`.githooks`) value is composed into as-is; otherwise default to
        // `.githooks` and establish the convention.
        let resolved = active_hooks_path(root);
        let githooks = root.join(".githooks");
        let on_githooks = resolved.as_deref().map_or(true, |p| p == githooks.as_path());
        let target = resolved.clone().unwrap_or_else(|| githooks.clone());

        let frag_rel = relative_display(
            &target.join("pre-commit.d").join(CTXGRD_FRAGMENT_NAME),
            root,
        );
        let runner_rel = relative_display(&target.join("pre-commit"), root);
        let setup_path = root.join("scripts").join("setup-hooks.sh");

        // Whether ctxgrd's fragment is already on disk — distinguishes an
        // initial install from an idempotent refresh in the JSON status.
        let fragment_existed = target
            .join("pre-commit.d")
            .join(CTXGRD_FRAGMENT_NAME)
            .exists();

        // HOOK-006: --dry-run previews the plan and writes nothing. Allowed
        // outside a git repo — a harmless preview.
        if dry_run {
            if json {
                eprintln!("would install:");
                eprintln!("  {frag_rel}   (ctxgrd pre-commit gate)");
                eprintln!("  {runner_rel}   (shared run-parts runner)");
            } else {
                println!("would install:");
                println!("  {frag_rel}   (ctxgrd pre-commit gate)");
                println!("  {runner_rel}   (shared run-parts runner)");
                if on_githooks {
                    if !setup_path.exists() {
                        println!(
                            "  {}   (fresh-clone bootstrap)",
                            relative_display(&setup_path, root)
                        );
                    }
                    if resolved.is_none() {
                        println!("would run: git config core.hooksPath .githooks");
                    }
                }
            }
            return Ok(Outcome::Did(HooksInstallJson {
                status: "would-install",
                path: frag_rel,
            }));
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
            return Err(KernelError::report(d));
        }

        write_dropin(root, &target)?;

        // Establish the tracked convention only when we are on `.githooks` —
        // never override a user's custom `core.hooksPath`.
        let mut wrote_setup = false;
        let mut set_config = false;
        if on_githooks {
            wrote_setup = write_setup_hooks_if_absent(root)?;
            // Set `core.hooksPath` only when it is not already pointed at
            // `.githooks` (a sibling `*grd` tool may have set it).
            if resolved.is_none() {
                set_hooks_path(root)?;
                set_config = true;
            }
        }

        if json {
            let status = if fragment_existed { "exists" } else { "installed" };
            eprintln!("Installed ctxgrd as a composable pre-commit fragment at {frag_rel}.");
            if wrote_setup {
                eprintln!("Bootstrap a fresh clone with scripts/setup-hooks.sh.");
            }
            if set_config {
                eprintln!("Set core.hooksPath -> .githooks.");
            }
            return Ok(Outcome::Did(HooksInstallJson {
                status,
                path: frag_rel,
            }));
        }

        println!("{frag_rel}");
        println!();
        if on_githooks {
            if set_config {
                println!("Installed ctxgrd as a composable pre-commit fragment under the tracked");
                println!(".githooks/ directory and set core.hooksPath -> .githooks.");
            } else {
                println!("Installed ctxgrd as a composable pre-commit fragment under the tracked");
                println!(".githooks/ directory (core.hooksPath already -> .githooks).");
            }
        } else {
            println!(
                "Installed ctxgrd as a composable pre-commit fragment in {} (core.hooksPath",
                relative_display(&target, root)
            );
            println!("already points there); a sibling *grd gate is not shadowed.");
        }
        println!("The shared run-parts runner at {runner_rel} dispatches every executable");
        println!("fragment in pre-commit.d/ (10-ctxgrd before a sibling *grd's 50- gate) and");
        println!("aborts on the first failure.");
        if wrote_setup {
            println!("Bootstrap a fresh clone with scripts/setup-hooks.sh.");
        }
        println!("Remove ctxgrd's gate with `rm {frag_rel}`.");
        let status = if fragment_existed { "exists" } else { "installed" };
        Ok(Outcome::Did(HooksInstallJson {
            status,
            path: frag_rel,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{CTXGRD_FRAGMENT, CTXGRD_FRAGMENT_NAME, PRE_COMMIT_RUNNER, SETUP_HOOKS};

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

    /// HOOK-010: the fresh-clone bootstrap points `core.hooksPath` at the tracked
    /// `.githooks/` directory, so a clone that runs it activates the composed
    /// hooks. It is a POSIX sh script (git runs it as the setup step).
    #[test]
    fn setup_hooks_sets_the_tracked_hooks_path() {
        assert!(SETUP_HOOKS.starts_with("#!/bin/sh"));
        assert!(
            SETUP_HOOKS.contains("config core.hooksPath .githooks"),
            "bootstrap must set core.hooksPath: {SETUP_HOOKS}"
        );
    }
}
