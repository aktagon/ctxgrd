//! Scaffolding commands that write files: `new`, `new rule`, `init`.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;

use ctxgrd::config;
use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::run::{self, LintError};
use ctxgrd::scaffold;
use ctxgrd::source::markdown;

use super::{emit_error, relative_display};

pub(super) fn new_cmd(
    root: &PathBuf,
    namespace: &str,
    title: &str,
    out: Option<&std::path::Path>,
    to_stdout: bool,
    id_override: Option<u32>,
) -> Result<ExitCode> {
    let config = match config::load(root) {
        Ok(c) => c,
        Err(e) => {
            emit_error(&LintError::Config(e).to_diagnostic(root), root);
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    };
    let path_claims = ctxgrd::path_claims::PathClaims::from_config(&config);
    let scan = match markdown::scan(root, config.ignore.as_ref(), Some(&path_claims)) {
        Ok(s) => s,
        Err(e) => {
            emit_error(&LintError::MarkdownScan(e).to_diagnostic(root), root);
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    };

    let ns_cfg = config.namespace_config(namespace);
    let scaffold = scaffold::scaffold(
        namespace,
        title,
        id_override,
        &ns_cfg,
        &scan.documents,
        root,
        out,
    );

    if to_stdout {
        print!("{}", scaffold.contents);
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
    }

    // Refuse to overwrite.
    if scaffold.target_path.exists() {
        let rel = relative_display(&scaffold.target_path, root);
        let d = Diagnostic::error("io.exists", &rel, 0, 0, format!("{rel} already exists"))
            .with_help("pass --stdout to preview, or delete the file first");
        emit_error(&d, root);
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }

    if let Some(parent) = scaffold.target_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            let rel = relative_display(parent, root);
            let d = Diagnostic::error(
                "io.mkdir",
                &rel,
                0,
                0,
                format!("could not create directory {rel}"),
            )
            .with_help("check file permissions on the parent directory")
            .with_note(format!("cause: {e}"));
            emit_error(&d, root);
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    }
    if let Err(e) = fs::write(&scaffold.target_path, scaffold.contents.as_bytes()) {
        let rel = relative_display(&scaffold.target_path, root);
        let d = Diagnostic::error("io.write", &rel, 0, 0, format!("could not write {rel}"))
            .with_help("check file permissions, or re-run with --stdout to preview")
            .with_note(format!("cause: {e}"));
        emit_error(&d, root);
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }

    // Print the target path relative to root where possible.
    let display_path = scaffold
        .target_path
        .strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| scaffold.target_path.display().to_string());
    println!("{display_path}");

    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

pub(super) fn new_rule_cmd(
    root: &PathBuf,
    code: &str,
    description: Option<&str>,
    out: Option<&std::path::Path>,
    to_stdout: bool,
) -> Result<ExitCode> {
    let scaffold = match scaffold::scaffold_rule(code, description, root, out) {
        Ok(s) => s,
        Err(msg) => {
            let d = Diagnostic::error("rule.invalid-code", "", 0, 0, msg)
                .with_help("rule code must be `<lowercase-namespace>.<kebab-name>`");
            emit_error(&d, root);
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    };

    if to_stdout {
        // stdout only renders the run script — the README is mostly
        // boilerplate the user can preview directly from disk.
        print!("{}", scaffold.run_contents);
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
    }

    // Lib-side materialisation: mkdir + write + chmod 0755, atomic
    // from the caller's perspective. The chmod policy lives in the
    // lib (ADR-002 § RUL-006) so a future LSP code-action that
    // creates a rule does not have to re-implement it.
    if let Err(e) = scaffold.write_run_script() {
        let rel = relative_display(&scaffold.run_path, root);
        let (code, help): (&str, &str) = match e.kind() {
            io::ErrorKind::AlreadyExists => (
                "io.exists",
                "delete the existing rule directory or pass --out to write elsewhere",
            ),
            io::ErrorKind::PermissionDenied => (
                "io.permission",
                "check file permissions on the parent directory and the run script",
            ),
            _ => (
                "io.write",
                "check file permissions on the parent, or re-run with --stdout to preview",
            ),
        };
        let d = Diagnostic::error(code, &rel, 0, 0, format!("could not write {rel}: {e}"))
            .with_help(help)
            .with_note(format!("cause: {}", e.kind()));
        emit_error(&d, root);
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }

    // README is non-fatal: a write failure here doesn't make the
    // rule unloadable, it just means the user has a directory
    // without a README.
    if let Err(e) = scaffold.write_readme() {
        let rel = relative_display(&scaffold.readme_path, root);
        eprintln!("warning: could not write README {rel}: {e}");
    }

    let display_path = relative_display(&scaffold.run_path, root);
    println!("{display_path}");
    println!();
    println!("Next steps:");
    println!(
        "  • Implement the check in {} (look for the `TODO:` line).",
        display_path
    );
    println!(
        "  • Add `\"{}\"` to the `rules` list of [{}] in ctxgrd.toml.",
        scaffold.code,
        scaffold.namespace.to_uppercase()
    );
    println!(
        "  • Verify wiring:                  ctxgrd rules {}",
        scaffold.code
    );
    println!();
    println!("Note: external rules only run against `.md` documents — see `ctxgrd docs rules`.");

    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

pub(super) fn init_cmd(
    root: &PathBuf,
    namespaces: &[String],
    force: bool,
    to_stdout: bool,
    packs: &[String],
) -> Result<ExitCode> {
    // ADR 006 § EXT-003 + ADR 007 § DOC-005: the body-header advisory
    // and the paths-pre-fill announcement must reach the user
    // regardless of whether ctxgrd.toml is written. Sniff up front so
    // the announcement is in hand before we decide on success/failure
    // paths, then print one combined stderr buffer on each return.
    let sniff = scaffold::scan_body_headers(root);
    let detected_paths = scaffold::detected_paths_for_namespaces(&sniff);
    let body_header_advisory = scaffold::render_body_header_advisory(&sniff);

    // When the user explicitly passed --namespaces, those are active
    // and nothing is commented. When they used the default (which is
    // `["ADR"]` — a single-element vec from clap's default_values_t),
    // fall back to the richer starter (ADR+PRD active, DDR/RFC/RUN/PMR
    // commented) so first-time users see the full catalogue.
    let user_specified = !(namespaces.len() == 1 && namespaces[0] == "ADR");
    let active_owned: Vec<&str> = namespaces.iter().map(String::as_str).collect();
    let (active, commented): (&[&str], &[&str]) = if user_specified {
        (&active_owned, &[])
    } else {
        (
            scaffold::DEFAULT_ACTIVE_NAMESPACES,
            scaffold::DEFAULT_COMMENTED_NAMESPACES,
        )
    };
    let paths_announcement = scaffold::render_paths_announcement(&detected_paths, active);
    // TODO(ADR-039 § DAG-007): seed `[NS."core.dep-shape"] requires=[...]`
    // params from `dag::infer_namespace_dag` over the existing docs, so an
    // existing-docs repo scaffolds a *declared* DAG (lifted edge PRD→SPEC ⇒
    // SPEC requires PRD). Deferred: `init_cmd` has no document-ingest path
    // today — it sniffs body headers and renders TOML from a fixed namespace
    // list via `render_init_toml`, which has no dep-shape param surface.
    // Wiring inference in means ingesting documents here, lifting edges, and
    // threading per-namespace `requires` into `render_namespace_block` — a
    // larger change than this ADR's runtime work. The runtime `inferred`
    // branch is already removed (status is declared-or-default); until this
    // lands, an existing-docs repo with no config reports `source: default`.
    let toml_text = scaffold::render_init_toml(active, commented, &detected_paths);

    // DOC-005: positive output (pre-fill announcement) sits above
    // EXT-003's body-header advisory so the user reads "here is what
    // is now linting" before "here is what still needs migration".
    let flush_advisory = || {
        if let Some(a) = &body_header_advisory {
            eprint!("{a}");
        }
    };
    let flush_stderr = || {
        if let Some(a) = &paths_announcement {
            eprint!("{a}");
        }
        if let Some(a) = &body_header_advisory {
            eprint!("{a}");
        }
    };

    if to_stdout {
        print!("{toml_text}");
        if !packs.is_empty() {
            eprintln!("note: --pack is ignored with --stdout; packs are applied only when writing the file");
        }
        flush_stderr();
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
    }

    let target = root.join("ctxgrd.toml");
    if target.exists() && !force {
        println!("ctxgrd.toml already exists — left unchanged");
        flush_advisory();
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
    }

    if let Err(e) = fs::create_dir_all(root) {
        let rel = relative_display(root, root);
        let d = Diagnostic::error(
            "io.mkdir",
            &rel,
            0,
            0,
            format!("could not create directory {rel}"),
        )
        .with_help("check file permissions on the parent directory")
        .with_note(format!("cause: {e}"));
        emit_error(&d, root);
        flush_stderr();
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }
    if let Err(e) = fs::write(&target, toml_text.as_bytes()) {
        let rel = relative_display(&target, root);
        let d = Diagnostic::error("io.write", &rel, 0, 0, format!("could not write {rel}"))
            .with_help("check file permissions")
            .with_note(format!("cause: {e}"));
        emit_error(&d, root);
        flush_stderr();
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
    }

    let display_path = target
        .strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| target.display().to_string());
    println!("Created  {display_path}");

    // PACK-006: `init --pack` is sugar for `init` then `pack add` per
    // pack. Apply after the base config is on disk so each pack appends
    // its missing blocks (never-clobbering the namespaces init wrote).
    for name in packs {
        match ctxgrd::pack::find(root, name) {
            Some(p) => {
                let plan = ctxgrd::pack::apply_add(&p, root)?;
                super::pack::report_pack_add(&p, &plan);
            }
            None => {
                let d =
                    Diagnostic::error("pack.unknown", "", 0, 0, format!("unknown pack '{name}'"))
                        .with_help("run `ctxgrd pack list` to see available packs");
                emit_error(&d, root);
                flush_stderr();
                return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
            }
        }
    }

    // PKD-003: advertise discoverable packs as an adoption on-ramp.
    // Suppressed when the user already applied one via --pack (PKD-004);
    // the --stdout path returned earlier, so it never reaches here.
    if packs.is_empty() {
        let discovered = ctxgrd::pack::discover(root);
        if !discovered.is_empty() {
            println!();
            println!("Available packs:");
            println!();
            print!("{}", ctxgrd::pack::render_init_packs(&discovered));
        }
    }

    println!();
    println!("Next steps:");
    println!("  • Apply a pack:             ctxgrd pack add <name>");
    if let Some(first) = active.first() {
        println!("  • Scaffold a document:      ctxgrd new {first} \"<title>\"");
    }
    println!("  • Run the linter:           ctxgrd check");
    println!("  • Install pre-commit hook:  ctxgrd hooks install");
    flush_stderr();
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}
