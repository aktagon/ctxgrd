//! `ctxgrd changelog` — generate `CHANGELOG.md` from the document graph
//! (ADR-084).
//!
//! Three modes over one generator (`ctxgrd::changelog`):
//! - `--write` regenerates `CHANGELOG.md` in place;
//! - `--check` regenerates to memory and diffs against disk (the
//!   `cargo fmt --check` contract: exit 1 when stale);
//! - no flag prints the generated markdown to stdout (a preview).
//!
//! `--format json` emits the structured changelog (versions → sections →
//! entries) on a clean stdout, so an agent can drive it without scraping.
//! Exit codes honour the documented contract: 0 clean / 1 stale / 2
//! kernel-or-config error.
//!
//! Category-3 command (ADR-101): its JSON is produced by a root-based library
//! String renderer and its `--check` verdict exits 1, so it renders its own
//! output (`SELF_RENDERS_JSON`) and maps its verdict onto [`Outcome`] for the
//! central exit code.

use ctxgrd::changelog::{self, ChangelogError};
use ctxgrd::diagnostic::Diagnostic;

use super::command::{Command, Ctx, KernelError, Outcome, SelfRendered};
use super::Format;

/// `ctxgrd changelog` — generate/check/preview `CHANGELOG.md`.
pub(super) struct ChangelogCmd {
    pub(super) write: bool,
    pub(super) check: bool,
    pub(super) format: Format,
}

impl ChangelogCmd {
    /// Render a [`ChangelogError`] as a kernel-style diagnostic (exit 2).
    fn kernel(err: &ChangelogError) -> KernelError {
        KernelError::report(Diagnostic::error(err.code(), "", 0, 0, format!("{err}")))
    }
}

impl Command for ChangelogCmd {
    type Json = SelfRendered;
    const SELF_RENDERS_JSON: bool = true;

    /// See [`super::lint::LintCmd::emits_json`]: `SELF_RENDERS_JSON` only
    /// suppresses the dispatcher's *success*-path write, so this must still
    /// answer truthfully for the failure path to know the caller wanted JSON.
    fn emits_json(&self) -> bool {
        self.format == Format::Json
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;

        // `--format json` is introspection: emit the structured model and stop,
        // regardless of --write/--check.
        if self.format == Format::Json {
            let json = changelog::render_json(root).map_err(|e| Self::kernel(&e))?;
            println!("{json}");
            return Ok(Outcome::Did(SelfRendered));
        }

        if self.check {
            return match changelog::check(root) {
                Ok(outcome) if outcome.fresh => {
                    eprintln!("changelog: up to date");
                    Ok(Outcome::Did(SelfRendered))
                }
                Ok(_) => {
                    eprintln!(
                        "changelog: CHANGELOG.md is stale — run `ctxgrd changelog --write` to regenerate"
                    );
                    Ok(Outcome::Findings(SelfRendered))
                }
                Err(e) => Err(Self::kernel(&e)),
            };
        }

        if self.write {
            return match changelog::write(root) {
                Ok(outcome) => {
                    if outcome.changed {
                        eprintln!("changelog: wrote CHANGELOG.md");
                    } else {
                        eprintln!("changelog: CHANGELOG.md already up to date");
                    }
                    Ok(Outcome::Did(SelfRendered))
                }
                Err(e) => Err(Self::kernel(&e)),
            };
        }

        // No mode flag: print the generated markdown to stdout (a preview).
        match changelog::generate(root) {
            Ok(content) => {
                print!("{content}");
                Ok(Outcome::Did(SelfRendered))
            }
            Err(e) => Err(Self::kernel(&e)),
        }
    }
}
