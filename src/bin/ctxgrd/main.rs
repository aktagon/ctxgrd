//! `ctxgrd` CLI shell.
//!
//! Thin argv-to-library binding. This module holds the clap types,
//! `main`, and `dispatch`; per-command-family handlers live in the
//! sibling submodules (`lint`, `pack`, `hooks`, `scaffold`, `introspect`,
//! `pin`) — ADR-063. `anyhow`/`ExitCode`/`println!` are confined to this
//! binary's modules and never enter the library. Output order for `lint`
//! mirrors the brief's acceptance transcripts:
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

use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::reporter;
use ctxgrd::run;

mod hooks;
mod introspect;
mod lint;
mod pack;
mod pin;
mod scaffold;

/// Render a synthetic diagnostic to stderr — used for every
/// kernel / config / IO / usage error so failures speak the same
/// cargo-style language as rule diagnostics.
pub(crate) fn emit_error(d: &Diagnostic, root: &std::path::Path) {
    eprint!("{}", reporter::render_error_block(d, root));
}

/// Best-effort relative path for display. Falls back to the absolute
/// form when the path is outside `root`. Keeps diagnostic `location`
/// strings tidy.
pub(crate) fn relative_display(path: &std::path::Path, root: &std::path::Path) -> String {
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
  ctxgrd -r                    # lint every ctxgrd.toml under --root (monorepo)
  ctxgrd rules                 # list resolved rules
  ctxgrd rules core.cross-ref  # show details for one rule
  ctxgrd refs ADR-001          # list every pointer to a document
  ctxgrd status                # show pipeline position: stages, blockers, next action
  ctxgrd status --format json  # same, as a JSON object for agent routers
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

    /// Flags for the default `lint` action, usable with no subcommand
    /// (`ctxgrd -r --format json`). Ignored when a subcommand is named.
    //
    // NOTE: `args_conflicts_with_subcommands` is deliberately NOT set here.
    // It is command-level (all-or-nothing) and clap 4 does not exempt global
    // args from it, so it makes the global `--root` conflict with subcommands
    // when `--root` is given *before* the subcommand (`ctxgrd --root X lint`) —
    // a position CI scripts rely on. Bare-lint dispatch does not need it:
    // `command` is `Option<Cmd>` and the dispatcher runs the default lint when
    // it is `None`. The only thing the attr added was erroring on default-lint
    // flags placed before a subcommand (`ctxgrd --format json status`); those
    // are now parsed into `lint` and ignored when a subcommand is named.
    #[command(flatten)]
    lint: LintArgs,

    /// Project root for all commands.
    #[arg(long, default_value = ".", global = true)]
    root: PathBuf,
}

/// Output format for `lint`, `rules`, `refs`, and `pack` — the diagnostic
/// *serialisation* axis. The Claude Code Stop-hook decision is deliberately
/// not a value here: it rides the orthogonal `--harness` axis (ADR-062), so
/// "serialise as a Stop decision" is unrepresentable.
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
pub(crate) enum Format {
    Rich,
    Simple,
    Json,
}

/// Output format for `ctxgrd list`.
///
/// Separate from [`Format`] because the inventory has no `simple`
/// one-line diagnostic form, but does add `markdown` — an H2-per-
/// namespace pipe table for pasting into docs or an LLM prompt.
/// `rich` is the column-aligned terminal table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub(crate) enum ListFormat {
    Rich,
    Markdown,
    Json,
}

/// Output format for `ctxgrd status`.
///
/// Dedicated enum (like [`ListFormat`]) because `status` has no
/// `simple` one-line form: `text` is the human table, `json` the
/// SPEC-002 § Data model object for agent routers (ADR-032), and
/// `mermaid`/`dot` emit DAG diagram *source* (output only — never
/// rendered) for embedding in markdown or piping to Graphviz.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub(crate) enum StatusFormat {
    Text,
    Json,
    Mermaid,
    Dot,
}

/// Flags for the default `lint` action. Flattened into both the
/// top-level [`Cli`] (so `ctxgrd -r --format json` works with no
/// subcommand) and the explicit [`Cmd::Lint`] variant (so `ctxgrd lint
/// -r --format json` is identical). One definition, two spellings.
#[derive(Debug, Clone, clap::Args)]
struct LintArgs {
    /// Output format. `rich` is the REP-001 human rendering; `json` emits
    /// a `{exit_code, diagnostics, kernel_messages}` object. The Claude
    /// Code Stop-hook decision is selected with `--harness claude`, not a
    /// format value (ADR-062) — see `ctxgrd hooks claude`.
    #[arg(long, value_enum, default_value_t = Format::Rich)]
    format: Format,

    /// Emit a turn-end decision for the named agent harness instead of
    /// serialising diagnostics (ADR-062). The only harness is `claude`,
    /// which renders the Claude Code Stop-hook object and always exits 0 —
    /// see `ctxgrd hooks claude`. Orthogonal to `--format`; incompatible
    /// with `--recursive`.
    #[arg(long)]
    harness: Option<String>,

    /// Lint every `ctxgrd.toml` found under `--root`, each as its own
    /// project, instead of just the one at the root. Diagnostics from
    /// all configs are reported; the exit code is the worst across them
    /// (2 if any config errored, 1 if any had diagnostics, else 0).
    /// `--format json` emits one labelled `{recursive, exit_code,
    /// roots:[…]}` object attributing each finding to its config.
    #[arg(long, short)]
    recursive: bool,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Lint the tree (default).
    Lint {
        #[command(flatten)]
        args: LintArgs,
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
        /// Apply one or more packs after writing the base config — sugar
        /// for `init` then `pack add <name>` for each (ADR-013 § PACK-006).
        #[arg(long, value_delimiter = ',')]
        pack: Vec<String>,
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
    /// List ingested documents grouped by namespace (ADR-015).
    List {
        /// Filter to a single namespace.
        #[arg(long)]
        namespace: Option<String>,
        /// Output format. `rich` is the column-aligned table;
        /// `markdown` emits an H2 heading + pipe table per namespace;
        /// `json` emits the full document array.
        #[arg(long, value_enum, default_value_t = ListFormat::Rich)]
        format: ListFormat,
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
    /// Report the project's pipeline position (SPEC-002): the resolved
    /// namespace DAG, per-stage verdicts, the current position, any
    /// open-BUG blockers, and the next action.
    Status {
        /// Output format. `text` is the human table; `json` emits the
        /// SPEC-002 § Data model object for agent routers; `mermaid` and
        /// `dot` emit DAG diagram source (output only, not rendered).
        #[arg(long, value_enum, default_value_t = StatusFormat::Text)]
        format: StatusFormat,
        /// Scope to one feature's lineage: the transitive dependents of
        /// `<ID>` over the `depends_on` graph (every document that
        /// transitively depends on `<ID>`, plus `<ID>` itself). A graph
        /// scope, distinct from the filesystem `--root`. Without it, the
        /// global view is reported (SPEC-003 ADR-059).
        #[arg(long, value_name = "ID")]
        lineage: Option<String>,
        /// Project the done-signal onto the process exit (ADR-056): exit
        /// `0` only when the selected frontier is empty and there are no
        /// blockers, `1` otherwise, `2` on config/cycle error. The report
        /// body is still printed; no file is modified. Composes with
        /// `--lineage` for a per-feature done-gate.
        #[arg(long)]
        exit_code: bool,
    },
    /// Start the Language Server Protocol (LSP) server over stdio.
    Lsp,
    /// Manage git hooks that gate commits on ctxgrd (ADR-014).
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Inspect and apply rule packs — reusable namespace bundles (ADR-013).
    Pack {
        #[command(subcommand)]
        action: PackAction,
    },
    /// Manage commit pins on documents (ADR-040).
    Pin {
        /// Re-pin the named document's `pin.commit` to the current HEAD,
        /// recording a freshly re-validated green commit (PIN-005).
        #[arg(long)]
        bless: String,
        /// Bless even when scoped paths have uncommitted changes. Without
        /// this the bless refuses, since HEAD would exclude those edits.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum HooksAction {
    /// Install a pre-commit hook that runs `ctxgrd` before each commit.
    Install {
        /// Overwrite an existing `.git/hooks/pre-commit`.
        #[arg(long)]
        force: bool,
        /// Print the hook script instead of writing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the Claude Code `Stop`-hook wiring (turn-end lint gate,
    /// ADR-062) and report whether it is already installed. Print-and-
    /// detect only: it never writes settings.json — that file is shared,
    /// user-global agent config, and clobbering it could drop unrelated
    /// hooks (STOP-004).
    Claude,
}

#[derive(Debug, Subcommand)]
enum PackAction {
    /// List every discoverable pack (built-in, global, local).
    List {
        /// List the paid (commercial, non-built-in) packs the binary
        /// advertises instead of the discoverable free ones (ADR-045).
        #[arg(long)]
        paid: bool,
    },
    /// Show the namespaces, rules, and scripts a pack defines.
    Show {
        /// Pack name, e.g. `project-docs`.
        name: String,
    },
    /// Apply a pack: append its blocks to ctxgrd.toml, never clobbering
    /// an existing namespace. Copies any bundled rule scripts.
    Add {
        /// Pack name, e.g. `project-docs`.
        name: String,
        /// Print the config that would be written and exit, touching no file.
        #[arg(long)]
        dry_run: bool,
    },
    /// Report pack-definition drift: blocks whose pack shape has evolved
    /// since they were generated (ADR-053 § PKM-004). Read-only; exit 1
    /// when drift is present, 0 when clean.
    Outdated {
        /// Output format. `json` emits the structured drift plan.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
    },
    /// Migrate provenance-stamped blocks to their pack's current shape
    /// (ADR-053 § PKM-002): rewrite fingerprint-clean blocks in place and
    /// emit a diff for hand-edited ones to resolve. Exit 1 when dirty
    /// blocks remain, 0 otherwise.
    Migrate {
        /// Compute and print the plan without writing ctxgrd.toml.
        #[arg(long)]
        dry_run: bool,
        /// Output format. `json` emits the structured migrate plan.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
    },
}

/// End-user guides embedded at compile time so `ctxgrd docs <topic>`
/// works after install without the source tree.
pub(crate) const DOC_NAMESPACES: &str = include_str!("../../../docs/namespaces.md");
pub(crate) const DOC_RULES: &str = include_str!("../../../docs/rules.md");
pub(crate) const DOC_SOURCES: &str = include_str!("../../../docs/sources.md");
pub(crate) const DOC_REFERENCES: &str = include_str!("../../../docs/references.md");
pub(crate) const DOC_PACKS: &str = include_str!("../../../docs/packs.md");

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
    // No subcommand → the top-level flattened lint flags ARE the
    // command (clap's `args_conflicts_with_subcommands` default).
    match cli.command.unwrap_or(Cmd::Lint { args: cli.lint }) {
        Cmd::Lint { args } => {
            if let Some(name) = args.harness.as_deref() {
                // The `--harness` axis: resolve the name to a known harness,
                // then emit its turn-end decision. An unknown name is misuse
                // (exit 2), not a silent fallback to serialising.
                let Some(harness) = lint::Harness::from_name(name) else {
                    let d = Diagnostic::error(
                        "cli.bad-harness",
                        "",
                        0,
                        0,
                        format!("unknown harness '{name}'"),
                    )
                    .with_help("the only harness is `claude` — see `ctxgrd hooks claude`");
                    emit_error(&d, &cli.root);
                    return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
                };
                // STOP-001/003: the Stop gate lints a single root. Pairing it
                // with --recursive (multi-config) has no meaning, so reject
                // rather than silently lint only one.
                if args.recursive {
                    let d = Diagnostic::error(
                        "cli.bad-harness",
                        "",
                        0,
                        0,
                        "`--harness` cannot combine with --recursive".to_string(),
                    )
                    .with_help("the harness gate lints a single root — drop --recursive");
                    emit_error(&d, &cli.root);
                    return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
                }
                match harness {
                    lint::Harness::Claude => lint::lint_claude_stop_cmd(&cli.root),
                }
            } else if args.recursive {
                lint::lint_recursive_cmd(&cli.root, args.format)
            } else {
                lint::lint_cmd(&cli.root, args.format)
            }
        }
        Cmd::Init {
            namespaces,
            force,
            stdout,
            pack,
        } => scaffold::init_cmd(&cli.root, &namespaces, force, stdout, &pack),
        Cmd::New {
            namespace,
            title,
            description,
            out,
            stdout,
            id,
        } => {
            if namespace.eq_ignore_ascii_case("rule") {
                scaffold::new_rule_cmd(
                    &cli.root,
                    &title,
                    description.as_deref(),
                    out.as_deref(),
                    stdout,
                )
            } else {
                scaffold::new_cmd(&cli.root, &namespace, &title, out.as_deref(), stdout, id)
            }
        }
        Cmd::Rules {
            namespace,
            format,
            rule_code,
        } => introspect::rules_cmd(
            &cli.root,
            namespace.as_deref(),
            rule_code.as_deref(),
            format,
        ),
        Cmd::List { namespace, format } => {
            introspect::list_cmd(&cli.root, namespace.as_deref(), format)
        }
        Cmd::Docs { topic } => introspect::docs_cmd(topic.as_deref()),
        Cmd::Refs { id, format } => introspect::refs_cmd(&cli.root, &id, format),
        Cmd::Status {
            format,
            lineage,
            exit_code,
        } => introspect::status_cmd(&cli.root, format, lineage.as_deref(), exit_code),
        Cmd::Lsp => pin::lsp_cmd(),
        Cmd::Hooks { action } => match action {
            HooksAction::Install { force, dry_run } => {
                hooks::hooks_install_cmd(&cli.root, force, dry_run)
            }
            HooksAction::Claude => hooks::hooks_claude_cmd(&cli.root),
        },
        Cmd::Pack { action } => match action {
            PackAction::List { paid } => pack::pack_list_cmd(&cli.root, paid),
            PackAction::Show { name } => pack::pack_show_cmd(&cli.root, &name),
            PackAction::Add { name, dry_run } => pack::pack_add_cmd(&cli.root, &name, dry_run),
            PackAction::Outdated { format } => pack::pack_outdated_cmd(&cli.root, format),
            PackAction::Migrate { dry_run, format } => {
                pack::pack_migrate_cmd(&cli.root, dry_run, format)
            }
        },
        Cmd::Pin { bless, force } => pin::pin_bless_cmd(&cli.root, &bless, force),
    }
}
