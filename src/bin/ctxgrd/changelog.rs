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

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;

use ctxgrd::changelog::{self, ChangelogError};
use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::run;

use super::{emit_error, Format};

pub(super) fn changelog_cmd(
    root: &Path,
    write: bool,
    check: bool,
    format: Format,
) -> Result<ExitCode> {
    // `--format json` is introspection: emit the structured model and stop,
    // regardless of --write/--check.
    if format == Format::Json {
        return match changelog::render_json(root) {
            Ok(json) => {
                println!("{json}");
                Ok(ok())
            }
            Err(e) => Ok(fail(&e, root)),
        };
    }

    if check {
        return match changelog::check(root) {
            Ok(outcome) if outcome.fresh => {
                eprintln!("changelog: up to date");
                Ok(ok())
            }
            Ok(_) => {
                eprintln!(
                    "changelog: CHANGELOG.md is stale — run `ctxgrd changelog --write` to regenerate"
                );
                Ok(ExitCode::from(run::ExitStatus::LintFailure.code()))
            }
            Err(e) => Ok(fail(&e, root)),
        };
    }

    if write {
        return match changelog::write(root) {
            Ok(outcome) => {
                if outcome.changed {
                    eprintln!("changelog: wrote CHANGELOG.md");
                } else {
                    eprintln!("changelog: CHANGELOG.md already up to date");
                }
                Ok(ok())
            }
            Err(e) => Ok(fail(&e, root)),
        };
    }

    // No mode flag: print the generated markdown to stdout (a preview).
    match changelog::generate(root) {
        Ok(content) => {
            print!("{content}");
            Ok(ok())
        }
        Err(e) => Ok(fail(&e, root)),
    }
}

fn ok() -> ExitCode {
    ExitCode::from(run::ExitStatus::Ok.code())
}

/// Render a [`ChangelogError`] as a kernel-style diagnostic on stderr and
/// return exit code 2.
fn fail(err: &ChangelogError, root: &Path) -> ExitCode {
    let d = Diagnostic::error(err.code(), "", 0, 0, format!("{err}"));
    emit_error(&d, root);
    ExitCode::from(run::ExitStatus::KernelError.code())
}
