//! Integration coverage for the Claude Code `Stop`-hook mode
//! (`ctxgrd lint --harness claude`, ADR-062) and the `hooks claude`
//! guidance subcommand. The unit tests in `run.rs` pin the decision JSON
//! shape; these pin the binary's wiring: stdin guard, always-exit-0, the
//! lint-only `--harness` restriction, and the `--recursive` rejection.

use assert_cmd::Command;
use predicates::prelude::*;

/// A failing root (`examples` carries 8 seeded errors) blocks the turn:
/// a decision object on stdout, yet exit 0 — the block is the JSON, not
/// the exit code (STOP-001).
#[test]
fn dirty_root_emits_block_decision_and_exits_zero() {
    Command::cargo_bin("ctxgrd")
        .unwrap()
        .args(["lint", "--root", "examples", "--harness", "claude"])
        .write_stdin(r#"{"stop_hook_active":false}"#)
        .assert()
        .success() // STOP-001: always exit 0
        .stdout(
            predicate::str::contains(r#""decision":"block""#)
                .and(predicate::str::contains("Fix before completing.")),
        );
}

/// `stop_hook_active: true` is the re-entrant fire after a block: the gate
/// runs no rules and stays silent, so a never-passing check cannot trap the
/// agent in an infinite block loop (STOP-002). Even over the failing
/// `examples` root, stdout is empty.
#[test]
fn reentrant_stop_runs_nothing_and_is_silent() {
    Command::cargo_bin("ctxgrd")
        .unwrap()
        .args(["lint", "--root", "examples", "--harness", "claude"])
        .write_stdin(r#"{"stop_hook_active":true}"#)
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

/// A clean root allows the turn: nothing on stdout, exit 0. The repo root
/// lints clean (it ignores `examples/`).
#[test]
fn clean_root_allows_silently() {
    Command::cargo_bin("ctxgrd")
        .unwrap()
        .args(["lint", "--root", ".", "--harness", "claude"])
        .write_stdin(r#"{"stop_hook_active":false}"#)
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

/// STOP-003: the Stop contract is lint-only. `--harness` is defined only on
/// `lint`, so clap rejects it on `rules` as an unexpected argument (usage
/// error, exit 2) with no runtime check in ctxgrd.
#[test]
fn harness_is_rejected_on_non_lint_commands() {
    Command::cargo_bin("ctxgrd")
        .unwrap()
        .args(["rules", "--harness", "claude"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--harness"));
}

/// An unknown harness name is misuse (kernel error, exit 2) — not a silent
/// fallback to serialising — and points at the capability catalog.
#[test]
fn unknown_harness_is_rejected() {
    Command::cargo_bin("ctxgrd")
        .unwrap()
        .args(["lint", "--root", "examples", "--harness", "codex"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown harness 'codex'"));
}

/// STOP-001/003: the harness gate lints a single root; pairing it with
/// `--recursive` is meaningless and is rejected (kernel error, exit 2).
#[test]
fn harness_rejects_recursive() {
    Command::cargo_bin("ctxgrd")
        .unwrap()
        .args(["lint", "--root", "examples", "--harness", "claude", "--recursive"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot combine with --recursive"));
}

/// `hooks claude` prints the settings.json wiring and detects state; it is
/// read-only and exits 0. From the repo root (no Stop hook wired) it
/// reports "not wired".
#[test]
fn hooks_claude_prints_snippet_and_detects_state() {
    Command::cargo_bin("ctxgrd")
        .unwrap()
        .args(["hooks", "claude", "--root", "examples"])
        // Pin HOME away from the developer's real ~/.claude so detection
        // is deterministic — examples/ has no project settings either.
        .env("HOME", "/nonexistent-ctxgrd-claude-stop-test")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("lint --harness claude")
                .and(predicate::str::contains("not wired")),
        );
}
