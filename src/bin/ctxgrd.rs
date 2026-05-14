//! `ctxgrd` CLI shell.
//!
//! Thin argv-to-library binding. `anyhow` lives only in this file —
//! the library never imports it. Output order for `lint` mirrors the
//! brief's acceptance transcripts:
//!
//! - warnings (if any) to stderr as `warning: <message>`;
//! - kernel-level runtime messages to stdout as
//!   `<severity>: [<code>] <message>`;
//! - sorted REP-001 diagnostics to stdout.
//!
//! Exit code: 0 / 1 / 2 per RUN-001. Config/kernel errors surface as
//! a single-line `error: [cfg.*] ...` on stderr with exit 2.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use std::fs;
use std::io;

use ctxgrd::config;
use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::introspect;
use ctxgrd::reporter;
use ctxgrd::run::{self, LintError};
use ctxgrd::scaffold;
use ctxgrd::source::markdown;

/// Render a synthetic diagnostic to stderr — used for every
/// kernel / config / IO / usage error so failures speak the same
/// cargo-style language as rule diagnostics.
fn emit_error(d: &Diagnostic, root: &std::path::Path) {
    eprint!("{}", reporter::render_error_block(d, root));
}

/// Best-effort relative path for display. Falls back to the absolute
/// form when the path is outside `root`. Keeps diagnostic `location`
/// strings tidy.
fn relative_display(path: &std::path::Path, root: &std::path::Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// Post-synopsis text appended to `ctxgrd --help`. Task-oriented
/// examples first, then the two facts users actually need when wiring
/// `ctxgrd` into CI: exit codes and config location.
const AFTER_HELP: &str = "\
Examples:
  ctxgrd init                  # create ctxgrd.toml with ADR + PRD defaults
  ctxgrd new ADR \"Use Rust\"    # scaffold a new ADR
  ctxgrd new rule design.foo \"check X\"  # scaffold a new external rule
  ctxgrd lint                  # lint the tree (rich output)
  ctxgrd lint --format json    # machine-readable, for CI pipelines
  ctxgrd rules                 # list resolved rules
  ctxgrd rules core.cross-ref  # show details for one rule
  ctxgrd refs ADR-001          # list every pointer to a document
  ctxgrd docs rules            # learn how to write your own rules

Exit codes:  0 clean · 1 diagnostics · 2 kernel/config error
Config:      reads `ctxgrd.toml` from --root (run `ctxgrd init` to create one).";

#[derive(Debug, Parser)]
#[command(
    name = "ctxgrd",
    about = "contextguard — document linter",
    version,
    after_help = AFTER_HELP,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Project root for all commands.
    #[arg(long, default_value = ".", global = true)]
    root: PathBuf,
}

/// Output format for `lint` and `rules`.
///
/// `rich` is the default — cargo-style multi-line diagnostics with
/// source snippets, carets, `help:` suggestions, and `note:` context.
/// Optimised for LLM and human readers.
///
/// `simple` is the grep-friendly REP-001 one-line form —
/// `<loc>:<line>:<col>: <sev>: [<code>] <msg>`. Use it when piping
/// through shell tooling that expects one diagnostic per line.
///
/// `json` is the structured wire format — the intended substrate
/// for LSP adapters, CI dashboards, and programmatic consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum Format {
    Rich,
    Simple,
    Json,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Lint the tree (default).
    Lint {
        /// Output format. `text` is the REP-001 human rendering;
        /// `json` emits a `{exit_code, diagnostics, kernel_messages,
        /// warnings}` object.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
    },
    /// Write a starter ctxgrd.toml.
    Init {
        /// Comma-separated list of namespaces (e.g. `ADR,PRD,DDR`).
        #[arg(long, value_delimiter = ',', default_values_t = vec!["ADR".to_string()])]
        namespaces: Vec<String>,
        /// Overwrite an existing ctxgrd.toml.
        #[arg(long)]
        force: bool,
        /// Print to stdout instead of writing the file.
        #[arg(long)]
        stdout: bool,
    },
    /// Scaffold a new document, or a new external rule when namespace is `rule`.
    New {
        /// Namespace (e.g. ADR, PRD), or the literal `rule` to scaffold an external rule.
        namespace: String,
        /// Document title (when namespace is a real namespace),
        /// or rule code like `design.foo` (when namespace is `rule`).
        title: String,
        /// Rule description — only used when namespace is `rule`. One-line summary
        /// of what the rule checks; lands in the generated script and README.
        description: Option<String>,
        /// Target directory override. Default for documents:
        /// `<root>/<lowercase-ns>s/`. Default for rules:
        /// `<root>/rules/<rule-namespace>/<rule-name>/`.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print the scaffolded content to stdout instead of writing files.
        #[arg(long)]
        stdout: bool,
        /// Explicit document number (documents only); defaults to `max(existing) + 1`.
        #[arg(long)]
        id: Option<u32>,
    },
    /// Introspect the resolved rule set.
    Rules {
        /// Filter to a single namespace.
        #[arg(long)]
        namespace: Option<String>,
        /// Output format. `json` emits the full rule array (including
        /// descriptions); `text` emits the column-aligned table +
        /// optional detail view.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
        /// Show a detail block for one rule code instead of the table.
        rule_code: Option<String>,
    },
    /// Print an end-user guide bundled with the binary.
    Docs {
        /// Topic name. Omit to list available topics.
        topic: Option<String>,
    },
    /// List every location pointing at a document ID (ADR-001 § REF-008).
    ///
    /// Prints the document itself (if file-backed), every other
    /// document whose `depends_on:` lists it, every body cross-ref
    /// token to it, and every reference-scanner hit. Output is
    /// deterministic so callers can diff across runs.
    Refs {
        /// Document ID, e.g. `ADR-001`.
        id: String,
        /// Output format. `text` is one `<file>:<line>:<col>` per
        /// line; `json` emits the structured array.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
    },
    /// Start the Language Server Protocol (LSP) server over stdio.
    Lsp,
}

/// End-user guides embedded at compile time so `ctxgrd docs <topic>`
/// works after install without the source tree.
const DOC_NAMESPACES: &str = include_str!("../../docs/namespaces.md");
const DOC_RULES: &str = include_str!("../../docs/rules.md");
const DOC_SOURCES: &str = include_str!("../../docs/sources.md");
const DOC_REFERENCES: &str = include_str!("../../docs/references.md");

fn main() -> ExitCode {
    match dispatch() {
        Ok(exit) => exit,
        Err(e) => {
            let d = Diagnostic::error("internal", "", 0, 0, format!("{e:#}"));
            emit_error(&d, std::path::Path::new("."));
            ExitCode::from(run::ExitStatus::KernelError.code())
        }
    }
}

fn dispatch() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Cmd::Lint {
        format: Format::Rich,
    }) {
        Cmd::Lint { format } => lint_cmd(&cli.root, format),
        Cmd::Init {
            namespaces,
            force,
            stdout,
        } => init_cmd(&cli.root, &namespaces, force, stdout),
        Cmd::New {
            namespace,
            title,
            description,
            out,
            stdout,
            id,
        } => {
            if namespace.eq_ignore_ascii_case("rule") {
                new_rule_cmd(
                    &cli.root,
                    &title,
                    description.as_deref(),
                    out.as_deref(),
                    stdout,
                )
            } else {
                new_cmd(&cli.root, &namespace, &title, out.as_deref(), stdout, id)
            }
        }
        Cmd::Rules {
            namespace,
            format,
            rule_code,
        } => rules_cmd(
            &cli.root,
            namespace.as_deref(),
            rule_code.as_deref(),
            format,
        ),
        Cmd::Docs { topic } => docs_cmd(topic.as_deref()),
        Cmd::Refs { id, format } => refs_cmd(&cli.root, &id, format),
        Cmd::Lsp => lsp_cmd(),
    }
}

#[tokio::main]
async fn lsp_cmd() -> Result<ExitCode> {
    ctxgrd::lsp::run_server().await;
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

fn docs_cmd(topic: Option<&str>) -> Result<ExitCode> {
    let Some(topic) = topic else {
        println!("Available topics:");
        println!("  namespaces  Configure namespaces and core rules in ctxgrd.toml");
        println!("  rules       Write external rule scripts (rules/<ns>/<name>/run)");
        println!("  sources     Write external source scripts (sources/<name>/run)");
        println!("  references  Scan non-markdown files for pointer mentions");
        println!();
        println!("Usage: ctxgrd docs <topic>");
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
    };
    let body = match topic {
        "namespaces" => DOC_NAMESPACES,
        "rules" => DOC_RULES,
        "sources" => DOC_SOURCES,
        "references" => DOC_REFERENCES,
        unknown => {
            let d = Diagnostic::error(
                "docs.unknown-topic",
                "",
                0,
                0,
                format!("unknown docs topic '{unknown}'"),
            )
            .with_help("run `ctxgrd docs` to list available topics");
            emit_error(&d, std::path::Path::new("."));
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    };
    print!("{body}");
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

fn refs_cmd(root: &PathBuf, id: &str, format: Format) -> Result<ExitCode> {
    let hits = match run::find_references(root, id) {
        Ok(h) => h,
        Err(e) => {
            let d = match e {
                run::LintError::Config(ce) => run::config_error_to_diagnostic(&ce, root),
                other => Diagnostic::error("internal", "", 0, 0, format!("{other}")),
            };
            emit_error(&d, root);
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    };

    match format {
        Format::Json => {
            let wire: Vec<WireRefHit<'_>> = hits.iter().map(WireRefHit::from).collect();
            let rendered =
                serde_json::to_string(&wire).expect("WireRefHit only contains serializable fields");
            println!("{rendered}");
        }
        // `rich` annotates each hit with its kind; `simple` is the
        // grep-friendly one-line `<file>:<line>:<col>` shape with no
        // kind suffix, so it pipes cleanly into `xargs`, `awk`, an
        // editor's quickfix list, etc.
        Format::Rich => {
            for hit in &hits {
                let kind_label = match &hit.kind {
                    run::ReferenceHitKind::SelfDoc => "(self)".to_string(),
                    run::ReferenceHitKind::DependsOn { from } => {
                        format!("(depends_on from {from})")
                    }
                    run::ReferenceHitKind::BodyCrossRef { from } => {
                        format!("(body ref from {from})")
                    }
                    run::ReferenceHitKind::ScannerHit => "(scanner)".to_string(),
                    // ReferenceHitKind is #[non_exhaustive]; new variants
                    // surface as "(unknown)" until the renderer is taught
                    // about them. Better than failing to compile when the
                    // library adds a kind we haven't styled yet.
                    _ => "(unknown)".to_string(),
                };
                println!("{}:{}:{}: {kind_label}", hit.file, hit.line, hit.col);
            }
        }
        Format::Simple => {
            for hit in &hits {
                println!("{}:{}:{}", hit.file, hit.line, hit.col);
            }
        }
    }

    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

/// Wire shape for `ctxgrd refs <ID> --format json`.
///
/// A separate type from [`run::ReferenceHit`] so the JSON contract is
/// pinned at the CLI boundary, not coupled to internal renames.
/// Serde's untagged adjacent representation puts `kind` and the
/// optional `from` next to each other for easy `jq` consumption.
#[derive(serde::Serialize)]
struct WireRefHit<'a> {
    file: &'a str,
    line: u32,
    col: u32,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<&'a str>,
}

impl<'a> From<&'a run::ReferenceHit> for WireRefHit<'a> {
    fn from(hit: &'a run::ReferenceHit) -> Self {
        let (kind, from) = match &hit.kind {
            run::ReferenceHitKind::SelfDoc => ("self", None),
            run::ReferenceHitKind::DependsOn { from } => ("depends_on", Some(from.as_str())),
            run::ReferenceHitKind::BodyCrossRef { from } => ("body_cross_ref", Some(from.as_str())),
            run::ReferenceHitKind::ScannerHit => ("scanner_hit", None),
            // See ReferenceHitKind comment above. JSON consumers should
            // tolerate "unknown" gracefully rather than break the schema.
            _ => ("unknown", None),
        };
        WireRefHit {
            file: hit.file.as_str(),
            line: hit.line,
            col: hit.col,
            kind,
            from,
        }
    }
}

fn lint_cmd(root: &PathBuf, format: Format) -> Result<ExitCode> {
    match run::lint(root) {
        Ok(outcome) => {
            match format {
                Format::Rich => {
                    for msg in &outcome.kernel_messages {
                        print!("{}", reporter::render_kernel_message_rich(msg));
                    }
                    let rendered = reporter::render_rich(&outcome.diagnostics, root);
                    if !rendered.is_empty() {
                        print!("{rendered}");
                    }
                    if outcome.exit == run::ExitStatus::Ok {
                        eprintln!(
                            "ok: {} · {} · 0 diagnostics",
                            reporter::plural(outcome.documents_linted, "document"),
                            reporter::plural(outcome.rules_active, "rule"),
                        );
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
                }
                Format::Json => {
                    println!("{}", run::render_json_outcome(&outcome));
                }
            }
            Ok(ExitCode::from(outcome.exit.code()))
        }
        Err(e) => {
            emit_error(&e.to_diagnostic(root), root);
            Ok(ExitCode::from(run::ExitStatus::KernelError.code()))
        }
    }
}

fn rules_cmd(
    root: &PathBuf,
    namespace: Option<&str>,
    rule_code: Option<&str>,
    format: Format,
) -> Result<ExitCode> {
    let config = match config::load(root) {
        Ok(c) => c,
        Err(e) => {
            emit_error(&LintError::Config(e).to_diagnostic(root), root);
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    };
    let discovered = match config::discover_external_rules(root) {
        Ok(d) => d,
        Err(e) => {
            emit_error(&LintError::Config(e).to_diagnostic(root), root);
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    };

    let entries = introspect::list_rules(&config, &discovered, namespace);

    match format {
        Format::Json => {
            // JSON bypasses the detail view — machine consumers want
            // the full array regardless of whether a specific code
            // was named. Callers who want one entry can filter on
            // `rule` themselves.
            println!("{}", introspect::render_json(&entries));
        }
        // `rich` and `simple` share the same text rendering for
        // `ctxgrd rules` — the table IS the compact human form.
        // Rich-vs-simple only matters for `lint` diagnostics.
        Format::Rich | Format::Simple => {
            if let Some(code) = rule_code {
                print!("{}", introspect::render_detail(&entries, code, &discovered));
            } else {
                print!("{}", introspect::render_table(&entries));
            }
        }
    }
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

fn new_cmd(
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

fn new_rule_cmd(
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

fn init_cmd(
    root: &PathBuf,
    namespaces: &[String],
    force: bool,
    to_stdout: bool,
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
    let toml_text = scaffold::render_init_toml(active, commented, &detected_paths);

    // DOC-005: positive output (pre-fill announcement) sits above
    // EXT-003's body-header advisory so the user reads "here is what
    // is now linting" before "here is what still needs migration".
    let stderr_extras = {
        let mut s = String::new();
        if let Some(a) = &paths_announcement {
            s.push_str(a);
        }
        if let Some(a) = &body_header_advisory {
            s.push_str(a);
        }
        s
    };
    let flush_stderr = || {
        if !stderr_extras.is_empty() {
            eprint!("{stderr_extras}");
        }
    };

    if to_stdout {
        print!("{toml_text}");
        flush_stderr();
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
    }

    let target = root.join("ctxgrd.toml");
    if target.exists() && !force {
        let rel = relative_display(&target, root);
        let d = Diagnostic::error("io.exists", &rel, 0, 0, format!("{rel} already exists"))
            .with_help("re-run with --force to overwrite, or --stdout to print");
        emit_error(&d, root);
        flush_stderr();
        return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
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
    println!("{display_path}");
    println!();
    println!("Next steps:");
    println!("  • Review the generated sections and customise headings / allowed values.");
    if let Some(first) = active.first() {
        println!("  • Scaffold your first document:  ctxgrd new {first} \"<title>\"");
    }
    println!("  • List resolved rules:            ctxgrd rules");
    flush_stderr();
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}
