//! The default `lint` action: single-root, `--recursive`, and the Claude Code
//! `--harness claude` Stop-hook gate (ADR-062).
//!
//! Category-3 command (ADR-101): `lint` has three sub-behaviours, each with its
//! own wire shape (single-root outcome, recursive `{roots:[…]}`, Claude Stop
//! decision) and a verdict-driven exit. It renders its own output
//! (`SELF_RENDERS_JSON`) and maps its verdict onto [`Outcome`] so the exit code
//! still flows through the one central map (ENF-003) — `Ok` → `Did` (0),
//! `LintFailure` → `Findings` (1), and the already-rendered worst-of-2 recursive
//! case returns [`KernelError::Reported`] (2, no re-emit).

use std::path::Path;

use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::reporter;
use ctxgrd::run::{self, ExitStatus, ScopeSelector};

use super::command::{Command, Ctx, KernelError, Outcome, SelfRendered};
use super::{relative_display, Format, LintArgs};

/// The agent harnesses ctxgrd can emit a turn-end decision for, selected by
/// `--harness <name>`. A closed set — `claude` is the only member today
/// (ADR-062).
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

/// `ctxgrd lint` (also the default, no-subcommand action).
pub(super) struct LintCmd {
    pub(super) args: LintArgs,
}

impl Command for LintCmd {
    type Json = SelfRendered;
    const SELF_RENDERS_JSON: bool = true;

    /// `SELF_RENDERS_JSON` makes the dispatcher skip the *success*-path write,
    /// which is why this went unimplemented — and therefore silently answered
    /// "no" — until the failure path started reading it. It is the answer to
    /// "did this invocation request `--format json`", so it must be truthful
    /// regardless of who does the writing.
    ///
    /// `--harness` is excluded: that axis owns its own wire contract (the Stop
    /// hook's `{"decision":…}`), and its kernel-error path already emits a
    /// fail-closed block body. Adding the ADR-086 object there would put two
    /// unrelated objects on one stream.
    fn emits_json(&self) -> bool {
        self.args.format == Format::Json && self.args.harness.is_none()
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        if let Some(name) = self.args.harness.as_deref() {
            // The `--harness` axis: resolve the name to a known harness, then
            // emit its turn-end decision. An unknown name is misuse (exit 2).
            let Some(harness) = Harness::from_name(name) else {
                let d = Diagnostic::error("cli.bad-harness", "", 0, 0, format!("unknown harness '{name}'"))
                    .with_help("the only harness is `claude` — see `ctxgrd hooks claude`");
                return Err(KernelError::report(d));
            };
            // STOP-001/003: the Stop gate lints a single root; pairing it with
            // --recursive has no meaning, so reject rather than silently lint
            // only one.
            if self.args.recursive {
                let d = Diagnostic::error(
                    "cli.bad-harness",
                    "",
                    0,
                    0,
                    "`--harness` cannot combine with --recursive".to_string(),
                )
                .with_help("the harness gate lints a single root — drop --recursive");
                return Err(KernelError::report(d));
            }
            match harness {
                Harness::Claude => lint_claude_stop(root, &self.args.scope()),
            }
        } else if self.args.recursive {
            lint_recursive(root, self.args.format, &self.args.scope())
        } else {
            lint_single(root, self.args.format, &self.args.scope())
        }
    }
}

/// Render one root's lint outcome in the requested format. Shared by the
/// single-root and per-root recursive paths so both speak identical output.
/// `root: <path>` on stderr when the resolved root is not the working
/// directory (BUG-048 follow-up).
///
/// Diagnostic paths are relative to the root, which used to be the same
/// place the user was standing. The upward search broke that assumption:
/// `cd docs/adrs && ctxgrd lint` prints `docs/adrs/001-x.md`, which does
/// not open from `docs/adrs/`. Naming the root is the cheap half of the
/// fix — it makes the path reconstructible. Editor quickfix still wants
/// cwd-relative or absolute paths; that is BUG-056.
///
/// stderr, so `--format json | jq` is untouched, and silent in the common
/// case where root == cwd so nothing changes for anyone at the top level.
fn disclose_root(root: &Path) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    // Compare canonically: `--root .` and the cwd are the same directory
    // spelled differently, and reporting a difference there would be noise.
    let same = match (root.canonicalize(), cwd.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => root == cwd.as_path(),
    };
    if !same {
        eprintln!(
            "root: {} (diagnostic paths are relative to this, not to your working directory)",
            root.display()
        );
    }
}

fn render_outcome(outcome: &run::LintOutcome, root: &Path, format: Format) {
    match format {
        Format::Rich => {
            disclose_root(root);
            for msg in &outcome.kernel_messages {
                print!("{}", reporter::render_kernel_message_rich(msg));
            }
            let rendered =
                reporter::render_rich(&outcome.diagnostics, &outcome.kernel_messages, root);
            if !rendered.is_empty() {
                print!("{rendered}");
            }
            if let Some(summary) = reporter::ok_summary(
                outcome.documents_linted,
                outcome.rules_active,
                outcome.namespaces_undeclared,
                &outcome.diagnostics,
                &outcome.kernel_messages,
            ) {
                eprintln!("{summary}");
            }
            // ADR-038 § HINT-002/003: advisory on stderr, only when rule
            // diagnostics were reported.
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
            if !outcome.diagnostics.is_empty() {
                eprintln!("hint: {}", reporter::LINT_HINT);
            }
        }
        Format::Json => {
            println!("{}", run::render_json_outcome(outcome));
        }
    }
}

/// Map a single-root outcome's [`ExitStatus`] onto the central [`Outcome`].
/// `run::lint` yields only `Ok`/`LintFailure` on the success path.
fn outcome_verdict(exit: ExitStatus) -> Outcome<SelfRendered> {
    match exit {
        ExitStatus::Ok => Outcome::Did(SelfRendered),
        _ => Outcome::Findings(SelfRendered),
    }
}

fn lint_single(
    root: &Path,
    format: Format,
    scope: &ScopeSelector,
) -> Result<Outcome<SelfRendered>, KernelError> {
    match run::lint_scoped(root, scope) {
        Ok(outcome) => {
            render_outcome(&outcome, root, format);
            Ok(outcome_verdict(outcome.exit))
        }
        Err(e) => Err(KernelError::report(e.to_diagnostic(root))),
    }
}

/// `--harness claude`: the Claude Code `Stop`-hook gate (ADR-062).
///
/// Projects the lint into the Stop decision contract: a
/// `{"decision":"block","reason":…}` object on stdout when the run failed,
/// nothing when clean or warnings-only. Unlike every other path this **always
/// exits 0** (`Did`) — block-vs-allow is signalled by the stdout JSON, not the
/// exit code (STOP-001). A config/kernel error blocks too (fail-closed).
fn lint_claude_stop(
    root: &Path,
    scope: &ScopeSelector,
) -> Result<Outcome<SelfRendered>, KernelError> {
    // STOP-002: honour the re-entrant-stop guard before doing any work.
    if stop_hook_active() {
        return Ok(Outcome::Did(SelfRendered));
    }
    let decision = match run::lint_scoped(root, scope) {
        Ok(outcome) => run::render_claude_stop(&outcome),
        Err(e) => Some(run::claude_stop_block(&reporter::render(&[e.to_diagnostic(root)]))),
    };
    if let Some(json) = decision {
        println!("{json}");
    }
    Ok(Outcome::Did(SelfRendered))
}

/// Read the Claude Code `Stop`-hook payload from stdin and report whether this
/// is a re-entrant stop. A terminal stdin (manual invocation) is never read.
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

/// `--recursive`: discover every `ctxgrd.toml` under `root` and lint each as an
/// independent project. The aggregate exit is the worst across all roots
/// (RUN-001 buckets: 2 > 1 > 0). Per-root navigation goes to stderr so stdout
/// stays a clean diagnostic stream.
fn lint_recursive(
    root: &Path,
    format: Format,
    scope: &ScopeSelector,
) -> Result<Outcome<SelfRendered>, KernelError> {
    let config_roots = run::discover_config_roots(root);
    if config_roots.is_empty() {
        let d = Diagnostic::error(
            "cfg.no-configs",
            "ctxgrd.toml",
            0,
            0,
            format!("no ctxgrd.toml found under {}", root.display()),
        );
        return Err(KernelError::report(d));
    }

    let mut worst: u8 = 0;

    if matches!(format, Format::Json) {
        let mut roots_arr: Vec<serde_json::Value> = Vec::with_capacity(config_roots.len());
        for cr in &config_roots {
            let label = relative_display(cr, root);
            match run::lint_scoped(cr, scope) {
                Ok(outcome) => {
                    worst = worst.max(outcome.exit.code());
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
        return recursive_verdict(worst);
    }

    for cr in &config_roots {
        let label = relative_display(cr, root);
        eprintln!("== {label} ==");
        match run::lint_scoped(cr, scope) {
            Ok(outcome) => {
                worst = worst.max(outcome.exit.code());
                render_outcome(&outcome, cr, format);
            }
            Err(e) => {
                worst = worst.max(run::ExitStatus::KernelError.code());
                super::emit_error(&e.to_diagnostic(cr), cr);
            }
        }
    }
    recursive_verdict(worst)
}

/// Map the recursive worst-of-code onto the central [`Outcome`]. A worst of `2`
/// has already had its per-root errors rendered, so it returns
/// [`KernelError::Reported`] (exit 2, no re-emit).
fn recursive_verdict(worst: u8) -> Result<Outcome<SelfRendered>, KernelError> {
    match worst {
        0 => Ok(Outcome::Did(SelfRendered)),
        1 => Ok(Outcome::Findings(SelfRendered)),
        _ => Err(KernelError::Reported),
    }
}
