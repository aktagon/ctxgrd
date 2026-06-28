//! `ctxgrd pack` — list/show/add/outdated/migrate reusable namespace bundles
//! (ADR-013, ADR-053).

use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;

use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::run;

use super::{emit_error, relative_display, Format};

/// `ctxgrd pack list` — read-only table of every discoverable pack
/// (PACK-004). Touches no file.
pub(super) fn pack_list_cmd(root: &Path, paid: bool) -> Result<ExitCode> {
    if paid {
        print!(
            "{}",
            ctxgrd::pack::render_paid_list(&ctxgrd::pack::paid_packs())
        );
    } else {
        let packs = ctxgrd::pack::discover(root);
        print!("{}", ctxgrd::pack::render_list(&packs));
    }
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

/// `ctxgrd pack show <name>` — read-only detail view of one pack
/// (PACK-004). Touches no file.
pub(super) fn pack_show_cmd(root: &Path, name: &str) -> Result<ExitCode> {
    let Some(pack) = ctxgrd::pack::find(root, name) else {
        return pack_not_found(root, name);
    };
    print!("{}", ctxgrd::pack::render_show(&pack));
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

/// `ctxgrd pack add <name>` — apply a pack, create-or-append, never
/// clobber (PACK-005). `--dry-run` prints the config it would write and
/// exits without touching any file.
pub(super) fn pack_add_cmd(root: &Path, name: &str, dry_run: bool) -> Result<ExitCode> {
    let Some(pack) = ctxgrd::pack::find(root, name) else {
        return pack_not_found(root, name);
    };

    // ADR-068 § PKD-002: apply the dependency closure (deps first, `pack`
    // last) so `pack add gdpr` also installs the `security` base.
    let chain = match ctxgrd::pack::resolve_dependencies(root, &pack) {
        Ok(c) => c,
        Err(e) => return pack_dependency_error(root, &e),
    };

    if dry_run {
        // Thread the growing config so a later pack in the chain skips a
        // namespace an earlier one already added.
        let mut existing = fs::read_to_string(root.join("ctxgrd.toml")).unwrap_or_default();
        let mut wrote_any = false;
        for p in &chain {
            let plan = ctxgrd::pack::plan_add(p, &existing, root);
            if !plan.blocks_text.is_empty() {
                print!("{}", plan.blocks_text);
                existing.push_str(&plan.blocks_text);
                wrote_any = true;
            }
            for ns in &plan.skipped {
                eprintln!("would skip [{ns}] — already defined in ctxgrd.toml");
            }
            for rule in &plan.rules_to_copy {
                eprintln!("would copy rules/{}/{}/run", rule.ns, rule.name);
            }
        }
        if !wrote_any {
            println!("# (nothing to add — every namespace is already present)");
        }
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
    }

    for p in &chain {
        let plan = ctxgrd::pack::apply_add(p, root)?;
        report_pack_add(p, &plan);
    }
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

/// Shared success report for `pack add` and `init --pack` (PKC-003).
/// Uses `render_add_receipt` to split path-claimed vs id-claimed namespaces;
/// reports any skipped or copied items separately.
pub(crate) fn report_pack_add(pack: &ctxgrd::pack::Pack, plan: &ctxgrd::pack::AddPlan) {
    let receipt = ctxgrd::pack::render_add_receipt(pack, plan);
    if !receipt.is_empty() {
        print!("{receipt}");
    }
    for ns in &plan.skipped {
        println!("skipped [{ns}] — already defined in ctxgrd.toml");
    }
    for rule in &plan.rules_to_copy {
        println!("copied rules/{}/{}/run", rule.ns, rule.name);
    }
    if plan.added.is_empty() && plan.skipped.is_empty() {
        println!("pack '{}': nothing to add", pack.name);
    }
}

/// `ctxgrd pack outdated` — read-only drift report (ADR-053 § PKM-004).
/// Exit 0 = no drift, 1 = drift present (clean rewrites and/or dirty
/// diffs), 2 = config error.
pub(super) fn pack_outdated_cmd(root: &Path, format: Format) -> Result<ExitCode> {
    let toml_path = root.join("ctxgrd.toml");
    let config_toml = match fs::read_to_string(&toml_path) {
        Ok(c) => c,
        Err(e) => return pack_config_read_error(root, &toml_path, &e),
    };
    let plan = ctxgrd::pack::plan_migrate(&config_toml, root);
    let drift = !plan.rewrites.is_empty() || !plan.diffs.is_empty();

    match format {
        Format::Json => {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        }
        Format::Rich | Format::Simple => {
            if !drift {
                println!("ctxgrd.toml is up to date with the installed packs.");
            } else {
                print!("{}", ctxgrd::pack::render_migrate_report(&plan, true));
            }
        }
    }

    let exit = if drift {
        run::ExitStatus::LintFailure
    } else {
        run::ExitStatus::Ok
    };
    Ok(ExitCode::from(exit.code()))
}

/// `ctxgrd pack migrate` — rewrite fingerprint-clean blocks in place and
/// emit a diff for hand-edited ones (ADR-053 § PKM-002). Exit 0 = nothing
/// to do or only clean rewrites applied, 1 = dirty blocks need manual
/// resolution, 2 = config error.
pub(super) fn pack_migrate_cmd(root: &Path, dry_run: bool, format: Format) -> Result<ExitCode> {
    let toml_path = root.join("ctxgrd.toml");
    // Read first so a missing/unreadable config is a clean exit-2, not an
    // io panic inside apply_migrate.
    if let Err(e) = fs::read_to_string(&toml_path) {
        return pack_config_read_error(root, &toml_path, &e);
    }
    let plan = ctxgrd::pack::apply_migrate(root, dry_run)?;

    match format {
        Format::Json => {
            // Clean JSON stream on stdout; progress goes to stderr.
            println!("{}", serde_json::to_string_pretty(&plan)?);
            if !dry_run && !plan.rewrites.is_empty() {
                eprintln!("migrated {} block(s) in ctxgrd.toml", plan.rewrites.len());
            }
        }
        Format::Rich | Format::Simple => {
            print!("{}", ctxgrd::pack::render_migrate_report(&plan, dry_run));
        }
    }

    let exit = if plan.diffs.is_empty() {
        run::ExitStatus::Ok
    } else {
        run::ExitStatus::LintFailure
    };
    Ok(ExitCode::from(exit.code()))
}

/// Emit a `pack.config-unreadable` kernel error (exit 2) for a config
/// `pack migrate`/`pack outdated` cannot read.
fn pack_config_read_error(root: &Path, path: &Path, e: &io::Error) -> Result<ExitCode> {
    let rel = relative_display(path, root);
    let d = Diagnostic::error(
        "pack.config-unreadable",
        &rel,
        0,
        0,
        format!("could not read {rel}: {e}"),
    )
    .with_help("run `ctxgrd init` to create a ctxgrd.toml, or check file permissions");
    emit_error(&d, root);
    Ok(ExitCode::from(run::ExitStatus::KernelError.code()))
}

/// Emit the `pack.dependency` error and return the kernel-error exit code
/// (ADR-068 § PKD-003: an unresolvable dependency closure is a config error).
fn pack_dependency_error(
    root: &Path,
    e: &ctxgrd::pack::DependencyError,
) -> Result<ExitCode> {
    use ctxgrd::pack::DependencyError;
    let help = match e {
        DependencyError::Missing { .. } => {
            "install the missing dependency pack, or run `ctxgrd pack list` to see what is available"
        }
        DependencyError::NonBaseTarget { .. } => {
            "a dependency target must be a base pack — one with no `# depends:` line of its own"
        }
        DependencyError::Cycle { .. } => "break the cycle in the packs' `# depends:` lines",
    };
    let d = Diagnostic::error("pack.dependency", "", 0, 0, e.to_string()).with_help(help);
    emit_error(&d, root);
    Ok(ExitCode::from(run::ExitStatus::KernelError.code()))
}

/// Emit the `pack.unknown` error and return the kernel-error exit code.
fn pack_not_found(root: &Path, name: &str) -> Result<ExitCode> {
    let d = Diagnostic::error("pack.unknown", "", 0, 0, format!("unknown pack '{name}'"))
        .with_help("run `ctxgrd pack list` to see available packs");
    emit_error(&d, root);
    Ok(ExitCode::from(run::ExitStatus::KernelError.code()))
}
