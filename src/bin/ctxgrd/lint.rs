//! The default `lint` action: single-root, `--recursive`, and the
//! Claude Code `--harness claude` Stop-hook gate (ADR-062).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;

use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::reporter;
use ctxgrd::run;

use super::{emit_error, relative_display, Format};

/// The agent harnesses ctxgrd can emit a turn-end decision for, selected by
/// `--harness <name>`. A closed set — `claude` is the only member today
/// (ADR-062). Modelled as an enum rather than a bare string match so a
/// second harness slots in here and an unknown name is one actionable
/// error, mirroring wrkgrd's `Harness::from_name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Harness {
    Claude,
}

impl Harness {
    /// Resolve the `--harness <name>` selector; `None` for an unknown name.
    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Harness::Claude),
            _ => None,
        }
    }
}

/// Render one root's lint outcome in the requested format. Shared by
/// the single-root [`lint_cmd`] and the per-root loop in
/// [`lint_recursive_cmd`] so both speak identical output. The caller
/// owns the exit code (`outcome.exit`) and, for recursive runs, any
/// per-root section header.
fn render_outcome(outcome: &run::LintOutcome, root: &Path, format: Format) {
    match format {
        Format::Rich => {
            for msg in &outcome.kernel_messages {
                print!("{}", reporter::render_kernel_message_rich(msg));
            }
            let rendered = reporter::render_rich(&outcome.diagnostics, root);
            if !rendered.is_empty() {
                print!("{rendered}");
            }
            if let Some(summary) = reporter::ok_summary(
                outcome.documents_linted,
                outcome.rules_active,
                &outcome.diagnostics,
            ) {
                eprintln!("{summary}");
            }
            // ADR-038 § HINT-002/003: advisory on stderr, only
            // when rule diagnostics were reported.
            if !outcome.diagnostics.is_empty() {
                eprintln!("hint: {}", reporter::LINT_HINT);
            }
        }
        Format::Simple => {
            for msg in &outcome.kernel_messages {
                print!("{}", reporter::render_kernel_message_simple(msg));
            }
            let rendered = reporter::render(&outcome.diagnostics);
            if !rendered.is_empty() {
                print!("{rendered}");
            }
            // ADR-038 § HINT-003: stderr keeps Simple's stdout a
            // pure REP-001 diagnostic stream for grep consumers.
            if !outcome.diagnostics.is_empty() {
                eprintln!("hint: {}", reporter::LINT_HINT);
            }
        }
        Format::Json => {
            println!("{}", run::render_json_outcome(outcome));
        }
    }
}

pub(super) fn lint_cmd(root: &PathBuf, format: Format) -> Result<ExitCode> {
    match run::lint(root) {
        Ok(outcome) => {
            render_outcome(&outcome, root, format);
            Ok(ExitCode::from(outcome.exit.code()))
        }
        Err(e) => {
            emit_error(&e.to_diagnostic(root), root);
            Ok(ExitCode::from(run::ExitStatus::KernelError.code()))
        }
    }
}

/// `--harness claude`: the Claude Code `Stop`-hook gate (ADR-062).
///
/// Re-runs the in-process lint and projects the result into the Stop
/// decision contract: a `{"decision":"block","reason":…}` object on stdout
/// when the run failed, nothing when it is clean or warnings-only. Unlike
/// every other path this **always exits 0** — block-vs-allow is signalled
/// by the stdout JSON, not the exit code (STOP-001). A config/kernel error
/// blocks too (fail-closed), so a broken setup never lets the turn pass.
pub(super) fn lint_claude_stop_cmd(root: &Path) -> Result<ExitCode> {
    // STOP-002: honour the re-entrant-stop guard before doing any work. On
    // the second fire after a block, the payload carries
    // `stop_hook_active: true`; running the lint again would re-block a
    // never-passing check forever, so do nothing and allow.
    if stop_hook_active() {
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
    }
    let decision = match run::lint(root) {
        Ok(outcome) => run::render_claude_stop(&outcome),
        Err(e) => Some(run::claude_stop_block(&reporter::render(&[e.to_diagnostic(root)]))),
    };
    if let Some(json) = decision {
        println!("{json}");
    }
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

/// Read the Claude Code `Stop`-hook payload from stdin and report whether
/// this is a re-entrant stop (`stop_hook_active: true`). A terminal stdin
/// (a manual `--harness claude` invocation with no piped payload) is
/// never read — it returns `false` and proceeds, so manual use cannot
/// hang. Any read or parse failure is also `false`: an absent signal means
/// a first, normal stop, not a re-entrant one (STOP-002).
fn stop_hook_active() -> bool {
    use std::io::{IsTerminal, Read};
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return false;
    }
    let mut buf = String::new();
    if stdin.read_to_string(&mut buf).is_err() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&buf)
        .ok()
        .and_then(|v| v.get("stop_hook_active").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// `--recursive`: discover every `ctxgrd.toml` under `root` and lint
/// each as an independent project. The aggregate exit code is the
/// worst across all roots (RUN-001 buckets: 2 > 1 > 0), so a single
/// kernel error or any diagnostics fails the whole run.
///
/// Per-root navigation (`== <path> ==` headers, summaries, hints) goes
/// to stderr so stdout stays a clean diagnostic stream. `--format json`
/// emits one `{recursive, exit_code, roots:[…]}` object — each entry is
/// the single-root wire shape plus a `root` key — so an agent can
/// attribute every finding to its config without screen-scraping.
pub(super) fn lint_recursive_cmd(root: &Path, format: Format) -> Result<ExitCode> {
    let config_roots = run::discover_config_roots(root);
    if config_roots.is_empty() {
        // Mirror the single-root NothingLinted philosophy: fail loudly
        // rather than report a false-confidence clean exit.
        let d = Diagnostic::error(
            "cfg.no-configs",
            "ctxgrd.toml",
            0,
            0,
            format!("no ctxgrd.toml found under {}", root.display()),
        );
        emit_error(&d, root);
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }

    let mut worst: u8 = 0;

    if matches!(format, Format::Json) {
        let mut roots_arr: Vec<serde_json::Value> = Vec::with_capacity(config_roots.len());
        for cr in &config_roots {
            let label = relative_display(cr, root);
            match run::lint(cr) {
                Ok(outcome) => {
                    worst = worst.max(outcome.exit.code());
                    // Reuse the single-root wire shape as the source of
                    // truth, then label it with its directory.
                    let mut v: serde_json::Value =
                        serde_json::from_str(&run::render_json_outcome(&outcome))
                            .unwrap_or_else(|_| serde_json::json!({}));
                    if let serde_json::Value::Object(ref mut m) = v {
                        m.insert("root".into(), serde_json::json!(label));
                    }
                    roots_arr.push(v);
                }
                Err(e) => {
                    worst = worst.max(run::ExitStatus::KernelError.code());
                    let d = e.to_diagnostic(cr);
                    roots_arr.push(serde_json::json!({
                        "root": label,
                        "exit_code": run::ExitStatus::KernelError.code(),
                        "error": { "code": d.code, "message": d.message },
                    }));
                }
            }
        }
        let out = serde_json::json!({
            "recursive": true,
            "exit_code": worst,
            "roots": roots_arr,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".to_string())
        );
        return Ok(ExitCode::from(worst));
    }

    for cr in &config_roots {
        let label = relative_display(cr, root);
        eprintln!("== {label} ==");
        match run::lint(cr) {
            Ok(outcome) => {
                worst = worst.max(outcome.exit.code());
                render_outcome(&outcome, cr, format);
            }
            Err(e) => {
                worst = worst.max(run::ExitStatus::KernelError.code());
                emit_error(&e.to_diagnostic(cr), cr);
            }
        }
    }
    Ok(ExitCode::from(worst))
}
