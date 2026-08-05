//! Scaffolding commands that write files: `new`, `new rule`, `init`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use ctxgrd::config;
use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::run::LintError;
use ctxgrd::scaffold;
use ctxgrd::source::markdown;

use super::command::{Command, Ctx, KernelError, Outcome, Prose};
use super::{relative_display, Format};

/// `ctxgrd new <NS> <title>` — scaffold a new document. The `new rule` sub-mode
/// is a distinct, prose-exempt command ([`NewRuleCmd`]).
pub(super) struct NewDocCmd {
    pub(super) namespace: String,
    pub(super) title: String,
    pub(super) out: Option<PathBuf>,
    pub(super) to_stdout: bool,
    pub(super) id_override: Option<u32>,
    pub(super) format: Format,
}

/// ADR-096 § CMD-001 wire shape for `ctxgrd new <NS> <title> --format json`.
///
/// ADR-094 § AXI-007: `help` carries next-step command templates (AXI-004
/// disclosure). Omitted on the refuse-to-act and preview paths — an agent that
/// created nothing gets no next step to fabricate against.
#[derive(serde::Serialize, Default)]
pub(super) struct NewJson {
    status: &'static str,
    id: String,
    path: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    help: Vec<String>,
}

impl Command for NewDocCmd {
    type Json = NewJson;

    fn emits_json(&self) -> bool {
        // `--stdout` prints the scaffold body and wins over `--format json`
        // (the preview has no structured shape), mirroring the pre-refactor
        // early return.
        matches!(self.format, Format::Json) && !self.to_stdout
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        let json = matches!(self.format, Format::Json);
        let config = config::load(root)
            .map_err(|e| KernelError::report(LintError::Config(e).to_diagnostic(root)))?;
        let path_claims = ctxgrd::path_claims::PathClaims::from_config(&config);
        let scan = markdown::scan(root, config.ignore.as_ref(), Some(&path_claims))
            .map_err(|e| KernelError::report(LintError::MarkdownScan(e).to_diagnostic(root)))?;

        let ns_cfg = config.namespace_config(&self.namespace);
        let scaffold = scaffold::scaffold(
            &self.namespace,
            &self.title,
            self.id_override,
            &ns_cfg,
            &scan.documents,
            root,
            self.out.as_deref(),
        );

        if self.to_stdout {
            print!("{}", scaffold.contents);
            let rel = relative_display(&scaffold.target_path, root);
            return Ok(Outcome::Did(NewJson {
                status: "created",
                id: scaffold.id.to_string(),
                path: rel,
                ..Default::default()
            }));
        }

        // Refuse to overwrite. Mirroring init's WIRE-007: refusing to touch an
        // existing target is a benign refuse-to-act, not an error — exit 0
        // (`Outcome::Noop`) in both renderings, disambiguated by the `exists`
        // status, not the exit code. In JSON mode the dispatcher writes the
        // `{status:"exists"}` result to a clean stdout; in text mode a one-line
        // note goes to stderr.
        if scaffold.target_path.exists() {
            let rel = relative_display(&scaffold.target_path, root);
            if !json {
                eprintln!("{rel} already exists — left unchanged");
            }
            return Ok(Outcome::Noop(NewJson {
                status: "exists",
                id: scaffold.id.to_string(),
                path: rel,
                ..Default::default()
            }));
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
                return Err(KernelError::report(d));
            }
        }
        if let Err(e) = fs::write(&scaffold.target_path, scaffold.contents.as_bytes()) {
            let rel = relative_display(&scaffold.target_path, root);
            let d = Diagnostic::error("io.write", &rel, 0, 0, format!("could not write {rel}"))
                .with_help("check file permissions, or re-run with --stdout to preview")
                .with_note(format!("cause: {e}"));
            return Err(KernelError::report(d));
        }

        // Print the target path relative to root where possible.
        let display_path = scaffold
            .target_path
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| scaffold.target_path.display().to_string());
        if !json {
            println!("{display_path}");
        }

        Ok(Outcome::Did(NewJson {
            status: "created",
            id: scaffold.id.to_string(),
            path: display_path,
            // AXI-004: the created doc is a stub with placeholders — the honest
            // next steps are to lint it and locate it on the graph.
            help: vec!["ctxgrd check".to_string(), "ctxgrd status".to_string()],
        }))
    }
}

/// `ctxgrd new rule <code> [desc]` — scaffold an external rule script.
///
/// ENF-002 exemption: the rule scaffold emits plain-text guidance (the run
/// script path + next steps), never a `--format json` payload — so it declares
/// [`Prose`]. A named decision, not a silent gap.
pub(super) struct NewRuleCmd {
    pub(super) code: String,
    pub(super) description: Option<String>,
    pub(super) out: Option<PathBuf>,
    pub(super) to_stdout: bool,
}

impl Command for NewRuleCmd {
    type Json = Prose;

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        let scaffold =
            scaffold::scaffold_rule(&self.code, self.description.as_deref(), root, self.out.as_deref())
                .map_err(|msg| {
                    KernelError::report(
                        Diagnostic::error("rule.invalid-code", "", 0, 0, msg)
                            .with_help("rule code must be `<lowercase-namespace>.<kebab-name>`"),
                    )
                })?;

        if self.to_stdout {
            // stdout only renders the run script — the README is mostly
            // boilerplate the user can preview directly from disk.
            print!("{}", scaffold.run_contents);
            return Ok(Outcome::Did(Prose));
        }

        // Lib-side materialisation: mkdir + write + chmod 0755, atomic from the
        // caller's perspective. The chmod policy lives in the lib (ADR-002 §
        // RUL-006) so a future LSP code-action that creates a rule does not
        // have to re-implement it.
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
            return Err(KernelError::report(d));
        }

        // README is non-fatal: a write failure here doesn't make the rule
        // unloadable, it just means the user has a directory without a README.
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

        Ok(Outcome::Did(Prose))
    }
}

/// `ctxgrd init` — write a starter `ctxgrd.toml`.
pub(super) struct InitCmd {
    pub(super) namespaces: Vec<String>,
    pub(super) force: bool,
    pub(super) to_stdout: bool,
    pub(super) packs: Vec<String>,
    pub(super) format: Format,
}

/// ADR-086 § WIRE-001 wire shape for `ctxgrd init --format json`.
///
/// ADR-094 § AXI-007: beyond the `{status, path}` mutation baseline, the
/// created-config object advertises what an agent would otherwise re-parse the
/// file to learn — the live namespace split (AXI-002 aggregate), which paths
/// were auto-detected, which packs were applied, which remain available, and
/// the next-step command templates (AXI-004 disclosure). Every added field is
/// `skip_serializing_if` empty, so the `exists` refuse-to-act and the `--stdout`
/// preview keep the byte-identical `{status, path}` shape they emitted before.
#[derive(serde::Serialize, Default)]
pub(super) struct InitJson {
    status: &'static str,
    path: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    namespaces_active: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    namespaces_commented: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    paths_prefilled: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    packs_applied: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    packs_available: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    help: Vec<String>,
}

impl Command for InitCmd {
    type Json = InitJson;

    fn emits_json(&self) -> bool {
        // `--stdout` prints the TOML and wins over `--format json`.
        matches!(self.format, Format::Json) && !self.to_stdout
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        let json = matches!(self.format, Format::Json);
        // ADR 006 § EXT-003 + ADR 007 § DOC-005: the body-header advisory and
        // the paths-pre-fill announcement must reach the user regardless of
        // whether ctxgrd.toml is written. Sniff up front so the announcement is
        // in hand before we decide on success/failure paths.
        let sniff = scaffold::scan_body_headers(root);
        let detected_paths = scaffold::detected_paths_for_namespaces(&sniff);
        let body_header_advisory = scaffold::render_body_header_advisory(&sniff);

        // When the user explicitly passed --namespaces, those are active and
        // nothing is commented. When they used the default (`["ADR"]`), fall
        // back to the richer starter (ADR+PRD active, DDR/RFC/RUN/PMR
        // commented) so first-time users see the full catalogue.
        let user_specified = !(self.namespaces.len() == 1 && self.namespaces[0] == "ADR");
        let active_owned: Vec<&str> = self.namespaces.iter().map(String::as_str).collect();
        let (active, commented): (&[&str], &[&str]) = if user_specified {
            (&active_owned, &[])
        } else {
            (
                scaffold::DEFAULT_ACTIVE_NAMESPACES,
                scaffold::DEFAULT_COMMENTED_NAMESPACES,
            )
        };
        let paths_announcement = scaffold::render_paths_announcement(&detected_paths, active);
        let toml_text = scaffold::render_init_toml(active, commented, &detected_paths);

        // DOC-005: positive output (pre-fill announcement) sits above EXT-003's
        // body-header advisory so the user reads "here is what is now linting"
        // before "here is what still needs migration".
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

        if self.to_stdout {
            print!("{toml_text}");
            if !self.packs.is_empty() {
                eprintln!("note: --pack is ignored with --stdout; packs are applied only when writing the file");
            }
            flush_stderr();
            return Ok(Outcome::Did(InitJson {
                status: "created",
                path: String::new(),
                ..Default::default()
            }));
        }

        let target = root.join("ctxgrd.toml");
        let abs_path = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
        if target.exists() && !self.force {
            // WIRE-007: a benign refuse-to-act is not an error — exit 0
            // (`Outcome::Noop`) in both renderings, disambiguated by the
            // `exists` status, not the exit code.
            if !json {
                println!("ctxgrd.toml already exists — left unchanged");
            }
            flush_advisory();
            return Ok(Outcome::Noop(InitJson {
                status: "exists",
                path: abs_path.display().to_string(),
                ..Default::default()
            }));
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
            super::emit_error(&d, root);
            flush_stderr();
            return Err(KernelError::Reported);
        }
        if let Err(e) = fs::write(&target, toml_text.as_bytes()) {
            let rel = relative_display(&target, root);
            let d = Diagnostic::error("io.write", &rel, 0, 0, format!("could not write {rel}"))
                .with_help("check file permissions")
                .with_note(format!("cause: {e}"));
            super::emit_error(&d, root);
            flush_stderr();
            return Err(KernelError::Reported);
        }

        let display_path = target
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| target.display().to_string());
        if !json {
            println!("Created  {display_path}");
        }

        // PACK-006: `init --pack` is sugar for `init` then `pack add` per pack.
        // Apply after the base config is on disk so each pack appends its
        // missing blocks (never-clobbering the namespaces init wrote). In JSON
        // mode the human pack report is suppressed to keep stdout clean.
        for name in &self.packs {
            match ctxgrd::pack::find(root, name) {
                Some(p) => {
                    let plan = ctxgrd::pack::apply_add(&p, root).map_err(|e| {
                        KernelError::report(Diagnostic::error("internal", "", 0, 0, format!("{e}")))
                    })?;
                    if !json {
                        super::pack::report_pack_add(&p, &plan);
                    }
                }
                None => {
                    let d =
                        Diagnostic::error("pack.unknown", "", 0, 0, format!("unknown pack '{name}'"))
                            .with_help("run `ctxgrd pack list` to see available packs");
                    super::emit_error(&d, root);
                    flush_stderr();
                    return Err(KernelError::Reported);
                }
            }
        }

        // WIRE-001 + AXI-007: the created-config object is the whole stdout
        // payload in JSON mode. The absolute path is re-resolved now that the
        // file exists, and the object mirrors — as structured fields — the
        // namespace split, path pre-fills, applied/available packs, and
        // next-step templates the text rendering prints to stderr below. An
        // agent then never re-parses the file to learn what init just did.
        if json {
            let abs = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
            // Path pre-fills, restricted to active namespaces — the same subset
            // the human announcement (`render_paths_announcement`) reports.
            let mut paths_prefilled: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for ns in active {
                if let Some(globs) = detected_paths.get(*ns) {
                    if !globs.is_empty() {
                        paths_prefilled.insert((*ns).to_string(), globs.clone());
                    }
                }
            }
            // Available packs mirror the human "Available packs" block: shown
            // only when the user applied none via --pack (PKD-004 suppression).
            let packs_available: Vec<String> = if self.packs.is_empty() {
                ctxgrd::pack::discover(root)
                    .iter()
                    .map(|p| p.name.clone())
                    .collect()
            } else {
                Vec::new()
            };
            // Next-step templates mirror the stderr "Next steps" block below,
            // in the same order. The namespace is a known value (resolved);
            // `<name>`/`<title>` stay literal placeholders (AXI-004).
            let mut help: Vec<String> = vec!["ctxgrd pack add <name>".to_string()];
            if let Some(first) = active.first() {
                help.push(format!("ctxgrd new {first} \"<title>\""));
            }
            help.push("ctxgrd check".to_string());
            help.push("ctxgrd serve".to_string());
            help.push("ctxgrd hooks install".to_string());
            flush_stderr();
            return Ok(Outcome::Did(InitJson {
                status: "created",
                path: abs.display().to_string(),
                namespaces_active: active.iter().map(|s| (*s).to_string()).collect(),
                namespaces_commented: commented.iter().map(|s| (*s).to_string()).collect(),
                paths_prefilled,
                packs_applied: self.packs.clone(),
                packs_available,
                help,
            }));
        }

        // WIRE-007: the adoption on-ramp (available packs) and the Next-steps
        // hint block are guidance, not result data, so they go to stderr —
        // stdout carries only the `Created  <path>` line. PKD-003: advertise
        // discoverable packs. Suppressed when the user already applied one via
        // --pack (PKD-004); the --stdout path returned earlier.
        if self.packs.is_empty() {
            let discovered = ctxgrd::pack::discover(root);
            if !discovered.is_empty() {
                eprintln!();
                eprintln!("Available packs:");
                eprintln!();
                eprint!("{}", ctxgrd::pack::render_init_packs(&discovered));
            }
        }

        eprintln!();
        eprintln!("Next steps:");
        eprintln!("  • Apply a pack:             ctxgrd pack add <name>");
        if let Some(first) = active.first() {
            eprintln!("  • Scaffold a document:      ctxgrd new {first} \"<title>\"");
        }
        eprintln!("  • Run the linter:           ctxgrd check");
        eprintln!("  • Browse the docs:          ctxgrd serve");
        eprintln!("  • Install pre-commit hook:  ctxgrd hooks install");
        flush_stderr();
        // Text mode: emits_json() is false, so the dispatcher never serializes
        // this — the enrichment above is JSON-only. The human rendering already
        // printed the same information to stderr.
        Ok(Outcome::Did(InitJson {
            status: "created",
            path: display_path,
            ..Default::default()
        }))
    }
}
