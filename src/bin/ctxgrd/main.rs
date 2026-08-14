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

mod changelog;
mod command;
mod hooks;
mod introspect;
mod lint;
mod pack;
mod pin;
mod scaffold;
mod serve;

use command::{emit, Ctx};

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
  ctxgrd status                # the work queue: what is ready, what is blocked, and by what
  ctxgrd status --format json  # same, as a JSON object for agent routers
  ctxgrd serve                 # browse the governed docs + work queue at a localhost URL
  ctxgrd docs rules            # learn how to write your own rules

Exit codes:  0 clean · 1 diagnostics · 2 kernel/config error
Config:      finds `ctxgrd.toml` in the nearest parent directory, like git or cargo
             (run `ctxgrd init` to create one, or --root <dir> to pin it exactly).";

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

    /// Project root for all commands. Defaults to the nearest ancestor
    /// directory containing a `ctxgrd.toml`, or the working directory
    /// when no ancestor has one.
    ///
    /// Deliberately not `default_value = "."`: the absence of the flag
    /// is what licenses the upward search (BUG-048), and a clap default
    /// is indistinguishable from the user typing `--root .`.
    #[arg(long, global = true)]
    root: Option<PathBuf>,
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
    /// The default human rendering. `text` is accepted as an alias so a
    /// family-wide caller can pass the shared `--format text` value
    /// (ADR-086 § WIRE-006); it renders byte-identically to `rich`.
    #[value(alias = "text")]
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

/// Which graph the `status` diagram formats draw (ADR-108 § GRN-001).
///
/// `namespace` (the default) is the resolved namespace DAG the renderers
/// have always drawn; `doc` is the document `depends_on` graph, one node
/// per counted document. The axis is explicit at the call site because
/// `--lineage` scopes documents while the renderers drew namespaces —
/// two features that silently disagreed about which graph was meant.
///
/// ADR-118 § STG-005 left `Doc` as the only real value. `Namespace` is
/// kept in the enum on purpose: removing the variant would make
/// `--granularity namespace` a generic clap parse error, where keeping it
/// lets the command answer with a diagnostic that names the ADR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub(crate) enum Granularity {
    Namespace,
    Doc,
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

    /// Scope the run to the namespaces this project stamped `# pack:
    /// <name>` in its own ctxgrd.toml (ADR-080). Repeatable and
    /// comma-separable; intersects with `--namespace`. Resolves through
    /// the config's provenance, not the built-in pack definition, so it
    /// never scopes to a namespace the project never adopted. A name
    /// matching nothing exits 2; a hand-written block carries no stamp
    /// and is reachable only with `--namespace`.
    #[arg(long, value_delimiter = ',', value_name = "NAME")]
    pack: Vec<String>,

    /// Scope the run to one or more namespaces of the resolved config
    /// (ADR-080), e.g. `--namespace ADR`. Repeatable and comma-separable;
    /// intersects with `--pack`. Files outside the scope are skipped, not
    /// errored; a name the config does not declare exits 2.
    #[arg(long, value_delimiter = ',', value_name = "NS")]
    namespace: Vec<String>,
}

impl LintArgs {
    /// The ADR-080 scope selectors as the library sees them.
    fn scope(&self) -> ctxgrd::run::ScopeSelector {
        ctxgrd::run::ScopeSelector {
            packs: self.pack.clone(),
            namespaces: self.namespace.clone(),
        }
    }
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
        /// Output format. `json` emits `{"status":"created|exists","path":…}`
        /// on a clean stdout (ADR-086 § WIRE-001); the default renders the
        /// human summary with hints on stderr.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
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
        /// Output format. `json` emits `{"status":"created|exists","id":…,"path":…}`
        /// on a clean stdout (ADR-096 § CMD-001, mirroring init's WIRE-001 shape);
        /// the default prints the created path with hints on stderr.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
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
    /// Report the work queue (ADR-118): per-document readiness over the
    /// `depends_on` graph — what can be picked up now, what is waiting on
    /// what, and what is finished.
    Status {
        /// Output format. `text` is the census plus the ready/blocked
        /// lists; `json` emits one row per document for agent routers;
        /// `mermaid` and `dot` emit graph source (output only, not
        /// rendered).
        #[arg(long, value_enum, default_value_t = StatusFormat::Text)]
        format: StatusFormat,
        /// Which graph the `mermaid` and `dot` formats draw (ADR-108).
        /// Since ADR-118 there is only one: `doc`, the document
        /// `depends_on` graph, one node per counted document coloured by
        /// readiness. The flag is retained as a no-op so existing
        /// `--granularity doc` invocations keep working; `namespace` is
        /// rejected (exit 2) rather than silently redirected, because it
        /// named the deleted stage DAG.
        #[arg(long, value_enum, default_value_t = Granularity::Doc)]
        granularity: Granularity,
        /// Scope to one feature's lineage: the transitive dependents of
        /// `<ID>` over the `depends_on` graph (every document that
        /// transitively depends on `<ID>`, plus `<ID>` itself). A graph
        /// scope, distinct from the filesystem `--root`. Without it, the
        /// global view is reported (SPEC-003 ADR-059).
        #[arg(long, value_name = "ID")]
        lineage: Option<String>,
        /// Project the done-signal onto the process exit (ADR-056, redefined
        /// by ADR-118 § STG-004): exit `0` when no document in scope is
        /// blocked by a non-terminal dependency, `1` otherwise, `2` on
        /// config error. The report body is still printed; no file is
        /// modified. Composes with `--lineage` for a per-feature gate —
        /// which under the removed stage layer could not pass at all
        /// (`BUG-036` R2).
        #[arg(long)]
        exit_code: bool,
        /// Drop the document titles from the report (`BUG-046`), leaving
        /// the bare `<ID>  <status>` rows the queue printed before 2.2.0.
        ///
        /// Titles are on by default: an ID identifies a document without
        /// saying anything about it, so a queue keyed only by ID is
        /// unreadable to anyone not already holding the corpus in their
        /// head. This is the opt-out for a cost-sensitive injector that
        /// pays for the report on every session start.
        ///
        /// Applies to `text` and `json`; the diagram formats label their
        /// nodes by id and status and are unaffected.
        #[arg(long)]
        no_titles: bool,
    },
    /// Start the Language Server Protocol (LSP) server over stdio.
    Lsp,
    /// Serve a read-only, graph-aware web view of the governed docs (ADR-097).
    ///
    /// Starts a localhost HTTP server rendering the namespace index,
    /// per-doc pages (server-side markdown), clickable `depends_on`
    /// edges, and the `status` pipeline. Read-only, loopback only. Prints
    /// one `{"url":…}` line to stdout (SRV-006) so an agent can discover
    /// the bound port; logs go to stderr.
    Serve {
        /// TCP port to bind on `127.0.0.1`. The default `0` lets the OS
        /// assign a free port, reported in the stdout `{"url":…}` line.
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
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
    /// Generate CHANGELOG.md from the document graph (ADR-084).
    ///
    /// Reads the whitelisted namespace documents at each release tag and
    /// attributes each to the first release whose frozen tree marks it
    /// terminal; released sections are immutable because they are read from
    /// tags. With no flag, prints the generated markdown to stdout.
    Changelog {
        /// Regenerate CHANGELOG.md in place.
        #[arg(long)]
        write: bool,
        /// Regenerate to memory and diff against the file on disk; exit 1
        /// if they differ (the `cargo fmt --check` contract). Writes nothing.
        #[arg(long)]
        check: bool,
        /// Output format. `json` emits the structured changelog (versions →
        /// sections → entries) on a clean stdout for agent drivers.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
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
        /// Output format. `json` emits `{"status":"blessed","id":…,"pin":…}`
        /// on a clean stdout (ADR-096 § CMD-001); the default prints the
        /// human summary.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
    },
}

#[derive(Debug, Subcommand)]
enum HooksAction {
    /// Install a pre-commit hook that runs `ctxgrd` before each commit.
    ///
    /// Installs a composable `.githooks/pre-commit.d/10-ctxgrd` fragment and
    /// sets `core.hooksPath .githooks` (ADR-014 § HOOK-010), composing with a
    /// sibling `*grd` tool's gate rather than claiming the single hook slot.
    Install {
        /// Retained for back-compat; the composable drop-in never clobbers a
        /// foreign fragment, so this is a no-op.
        #[arg(long)]
        force: bool,
        /// Print what would be installed instead of writing it.
        #[arg(long)]
        dry_run: bool,
        /// Output format. `json` emits `{"status":"installed|exists|would-install",
        /// "path":…}` on a clean stdout (ADR-096 § CMD-001); the default prints
        /// the human summary.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
    },
    /// Print the Claude Code `Stop`-hook wiring (turn-end lint gate,
    /// ADR-062) and report whether it is already installed. Print-and-
    /// detect only: it never writes settings.json — that file is shared,
    /// user-global agent config, and clobbering it could drop unrelated
    /// hooks (STOP-004).
    Claude {
        /// Output format. `json` emits `{"installed":bool,"wiring":…}` on a
        /// clean stdout (ADR-096 § CMD-003); the default prints the wiring
        /// block and detect result for a human.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
    },
}

#[derive(Debug, Subcommand)]
enum PackAction {
    /// List every discoverable pack (built-in, global, local).
    List {
        /// List the paid (commercial, non-built-in) packs the binary
        /// advertises instead of the discoverable free ones (ADR-045).
        #[arg(long)]
        paid: bool,
        /// Output format. `json` emits the pack catalog as an array of
        /// `{"name":…,"namespaces":[…]}` objects (ADR-086 § WIRE-001);
        /// the default is the human table.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
    },
    /// Show the namespaces, rules, and scripts a pack defines.
    Show {
        /// Pack name, e.g. `project-docs`.
        name: String,
        /// Output format. `json` emits the pack detail object — namespaces,
        /// per-namespace rules, and the aggregate `rules` array of every bound
        /// code (ADR-096 § CMD-002); the default is the human view.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
    },
    /// Apply a pack: append its blocks to ctxgrd.toml, never clobbering
    /// an existing namespace. Copies any bundled rule scripts.
    Add {
        /// Pack name, e.g. `project-docs`.
        name: String,
        /// Print the config that would be written and exit, touching no file.
        #[arg(long)]
        dry_run: bool,
        /// Output format. `json` emits `{"status":…,"namespaces_added":[…],
        /// "path":…}` on a clean stdout (ADR-096 § CMD-001); the default prints
        /// the human receipt.
        #[arg(long, value_enum, default_value_t = Format::Rich)]
        format: Format,
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

/// Resolve the root every command runs against (BUG-048).
///
/// `--root <dir>` means exactly that directory and never searches. With
/// no flag, walk up from the working directory to the nearest
/// `ctxgrd.toml`; with none anywhere above, fall back to the working
/// directory, where the zero-config path takes over and announces itself.
///
/// `init` is exempt and always gets the working directory. It *writes*
/// `<root>/ctxgrd.toml` and `--force` overwrites without prompting, so an
/// upward search would turn `ctxgrd init --force` in `docs/adrs/` into a
/// silent overwrite of the repository's config. Every other command reads
/// the config, where finding the real one is the whole point.
///
/// `--recursive` composes on top rather than against: the upward search
/// picks the root, then `-r` descends from there. So `ctxgrd -r` in a
/// monorepo subdirectory now lints the monorepo, not the subtree.
fn resolve_root(explicit: Option<PathBuf>, is_init: bool) -> PathBuf {
    let cwd = || std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match explicit {
        Some(dir) => dir,
        None if is_init => cwd(),
        None => {
            let here = cwd();
            ctxgrd::config::find_project_root(&here).unwrap_or(here)
        }
    }
}

fn dispatch() -> Result<ExitCode> {
    let cli = Cli::parse();
    let is_init = matches!(cli.command, Some(Cmd::Init { .. }));
    let ctx = Ctx {
        root: resolve_root(cli.root, is_init),
    };
    // No subcommand → the top-level flattened lint flags ARE the command.
    //
    // Every arm builds its command type and routes through the single
    // `command::emit`, which owns the machine-stream JSON render (ENF-004) and
    // the one `Outcome`/`KernelError` → exit-code map (ENF-003). No arm returns
    // a bare exit code.
    let exit = match cli.command.unwrap_or(Cmd::Lint { args: cli.lint }) {
        Cmd::Lint { args } => emit(lint::LintCmd { args }, &ctx),
        Cmd::Init {
            namespaces,
            force,
            stdout,
            pack,
            format,
        } => emit(
            scaffold::InitCmd {
                namespaces,
                force,
                to_stdout: stdout,
                packs: pack,
                format,
            },
            &ctx,
        ),
        Cmd::New {
            namespace,
            title,
            description,
            out,
            stdout,
            id,
            format,
        } => {
            if namespace.eq_ignore_ascii_case("rule") {
                emit(
                    scaffold::NewRuleCmd {
                        code: title,
                        description,
                        out,
                        to_stdout: stdout,
                    },
                    &ctx,
                )
            } else {
                emit(
                    scaffold::NewDocCmd {
                        namespace,
                        title,
                        out,
                        to_stdout: stdout,
                        id_override: id,
                        format,
                    },
                    &ctx,
                )
            }
        }
        Cmd::Rules {
            namespace,
            format,
            rule_code,
        } => emit(
            introspect::RulesCmd {
                namespace,
                rule_code,
                format,
            },
            &ctx,
        ),
        Cmd::List { namespace, format } => {
            emit(introspect::ListCmd { namespace, format }, &ctx)
        }
        Cmd::Docs { topic } => emit(introspect::DocsCmd { topic }, &ctx),
        Cmd::Refs { id, format } => emit(introspect::RefsCmd { id, format }, &ctx),
        Cmd::Status {
            format,
            granularity,
            lineage,
            exit_code,
            no_titles,
        } => emit(
            introspect::StatusCmd {
                format,
                granularity,
                lineage,
                exit_code,
                titles: !no_titles,
            },
            &ctx,
        ),
        Cmd::Lsp => emit(pin::LspCmd, &ctx),
        Cmd::Serve { port } => emit(serve::ServeCmd { port }, &ctx),
        Cmd::Hooks { action } => match action {
            HooksAction::Install {
                force,
                dry_run,
                format,
            } => emit(
                hooks::HooksInstallCmd {
                    force,
                    dry_run,
                    format,
                },
                &ctx,
            ),
            HooksAction::Claude { format } => emit(hooks::HooksClaudeCmd { format }, &ctx),
        },
        Cmd::Pack { action } => match action {
            PackAction::List { paid, format } => emit(pack::PackListCmd { paid, format }, &ctx),
            PackAction::Show { name, format } => emit(pack::PackShowCmd { name, format }, &ctx),
            PackAction::Add {
                name,
                dry_run,
                format,
            } => emit(
                pack::PackAddCmd {
                    name,
                    dry_run,
                    format,
                },
                &ctx,
            ),
            PackAction::Outdated { format } => emit(pack::PackOutdatedCmd { format }, &ctx),
            PackAction::Migrate { dry_run, format } => {
                emit(pack::PackMigrateCmd { dry_run, format }, &ctx)
            }
        },
        Cmd::Changelog {
            write,
            check,
            format,
        } => emit(
            changelog::ChangelogCmd {
                write,
                check,
                format,
            },
            &ctx,
        ),
        Cmd::Pin {
            bless,
            force,
            format,
        } => emit(
            pin::PinCmd {
                target_id: bless,
                force,
                format,
            },
            &ctx,
        ),
    };
    Ok(exit)
}
