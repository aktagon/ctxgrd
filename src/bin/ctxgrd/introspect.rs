//! Read-only introspection commands: `docs`, `refs`, `status`, `rules`, `list`.

use ctxgrd::config;
use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::introspect::{self, RuleEntry};
use ctxgrd::list::{self, DocEntry};
use ctxgrd::run::{self, LintError};

use super::command::{Command, Ctx, KernelError, Outcome, Prose, SelfRendered};
use super::{
    Format, Granularity, ListFormat, StatusFormat, DOC_NAMESPACES, DOC_PACKS, DOC_REFERENCES,
    DOC_RULES, DOC_SOURCES,
};

/// `ctxgrd docs [topic]` — print a bundled end-user guide.
///
/// ENF-002 exemption: guides are prose, with no `--format json` — so [`Prose`].
pub(super) struct DocsCmd {
    pub(super) topic: Option<String>,
}

impl Command for DocsCmd {
    type Json = Prose;

    fn run(self, _ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let Some(topic) = self.topic.as_deref() else {
            println!("Available topics:");
            println!("  namespaces  Configure namespaces and core rules in ctxgrd.toml");
            println!("  rules       Write external rule scripts (rules/<ns>/<name>/run)");
            println!("  sources     Write external source scripts (sources/<name>/run)");
            println!("  references  Scan non-markdown files for pointer mentions");
            println!("  packs       Apply reusable namespace bundles (ctxgrd pack)");
            println!();
            println!("Usage: ctxgrd docs <topic>");
            return Ok(Outcome::Did(Prose));
        };
        let body = match topic {
            "namespaces" => DOC_NAMESPACES,
            "rules" => DOC_RULES,
            "packs" => DOC_PACKS,
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
                return Err(KernelError::report(d));
            }
        };
        print!("{body}");
        Ok(Outcome::Did(Prose))
    }
}

/// `ctxgrd refs <ID>` — list every location pointing at a document ID.
pub(super) struct RefsCmd {
    pub(super) id: String,
    pub(super) format: Format,
}

/// Owned wire shape for `ctxgrd refs <ID> --format json`.
///
/// A separate type from [`run::ReferenceHit`] so the JSON contract is pinned at
/// the CLI boundary, not coupled to internal renames.
#[derive(serde::Serialize)]
pub(super) struct RefHitWire {
    file: String,
    line: u32,
    col: u32,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
}

impl From<&run::ReferenceHit> for RefHitWire {
    fn from(hit: &run::ReferenceHit) -> Self {
        let (kind, from) = match &hit.kind {
            run::ReferenceHitKind::SelfDoc => ("self", None),
            run::ReferenceHitKind::DependsOn { from } => ("depends_on", Some(from.clone())),
            run::ReferenceHitKind::BodyCrossRef { from } => ("body_cross_ref", Some(from.clone())),
            run::ReferenceHitKind::ScannerHit => ("scanner_hit", None),
            // ReferenceHitKind is #[non_exhaustive]; JSON consumers should
            // tolerate "unknown" gracefully rather than break the schema.
            _ => ("unknown", None),
        };
        RefHitWire {
            file: hit.file.clone(),
            line: hit.line,
            col: hit.col,
            kind,
            from,
        }
    }
}

impl Command for RefsCmd {
    type Json = Vec<RefHitWire>;

    fn emits_json(&self) -> bool {
        matches!(self.format, Format::Json)
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        let hits = run::find_references(root, &self.id).map_err(|e| {
            let d = match e {
                run::LintError::Config(ce) => run::config_error_to_diagnostic(&ce, root),
                other => Diagnostic::error("internal", "", 0, 0, format!("{other}")),
            };
            KernelError::report(d)
        })?;

        match self.format {
            // JSON: the dispatcher writes the wire array; run stays silent.
            Format::Json => {}
            // `rich` annotates each hit with its kind; `simple` is the
            // grep-friendly one-line `<file>:<line>:<col>` shape.
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

        let wire: Vec<RefHitWire> = hits.iter().map(RefHitWire::from).collect();
        Ok(Outcome::Did(wire))
    }
}

/// `ctxgrd status` — resolve the namespace DAG, optionally scope to a
/// `--lineage <ID>` feature, compute per-stage verdicts, sweep the BUG
/// tripwire, and render the result (SPEC-002, SPEC-003).
///
/// Category-3 command (ADR-101): its verdict-driven exit is preserved by
/// mapping the same verdict onto [`Outcome`] (empty-frontier + no-blocker under
/// `--exit-code` → `Did`/exit 0, otherwise `Findings`/exit 1), and its
/// multi-shape wire (`json`/`mermaid`/`dot`) is rendered here
/// (`SELF_RENDERS_JSON`).
pub(super) struct StatusCmd {
    pub(super) format: StatusFormat,
    pub(super) granularity: Granularity,
    pub(super) lineage: Option<String>,
    pub(super) exit_code: bool,
    /// Whether rows name their document (`BUG-046`). The CLI's `--no-titles`
    /// inverted at the parse boundary, so nothing downstream reasons about a
    /// negative flag.
    pub(super) titles: bool,
}

/// ADR-118 § STG-005: document granularity is the only granularity, so
/// `--granularity namespace` names a view that no longer exists and MUST be
/// refused rather than silently drawing the document graph — the same
/// posture STG-002 takes on `[pipeline]`.
///
/// `--granularity doc` is accepted and is now a no-op, which keeps every
/// existing script that passes it working. This replaces the ADR-108 §
/// GRN-003 misuse check, which rejected `doc` alongside `text`/`json`
/// because neither had a document-granular shape; both do now.
fn granularity_conflict(granularity: Granularity) -> Option<Diagnostic> {
    if granularity != Granularity::Namespace {
        return None;
    }
    Some(
        Diagnostic::error(
            "cli.bad-granularity",
            "",
            0,
            0,
            "`--granularity namespace` was removed in ADR-118 — namespace stages no longer exist"
                .to_string(),
        )
        .with_help(
            "drop the flag: status now reports per document. For namespace-level edge \
             constraints see core.dep-shape",
        ),
    )
}

impl Command for StatusCmd {
    type Json = SelfRendered;
    const SELF_RENDERS_JSON: bool = true;

    /// See [`super::lint::LintCmd::emits_json`]: `SELF_RENDERS_JSON` only
    /// suppresses the dispatcher's *success*-path write, so this must still
    /// answer truthfully for the failure path to know the caller wanted JSON.
    fn emits_json(&self) -> bool {
        self.format == StatusFormat::Json
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        // ADR-108 § GRN-003: `--granularity doc` describes a diagram, so
        // pairing it with a non-diagram format is misuse, not a no-op.
        // Validated before the report runs and routed through the kernel
        // error path, so `--format json` still yields the ADR-086 object.
        if let Some(d) = granularity_conflict(self.granularity) {
            return Err(KernelError::report(d));
        }
        match ctxgrd::status::report_scoped(root, self.lineage.as_deref()) {
            Ok(report) => {
                match self.format {
                    StatusFormat::Text => {
                        print!("{}", ctxgrd::status::render_report(&report, self.titles))
                    }
                    StatusFormat::Json => {
                        println!("{}", ctxgrd::status::render_json(&report, self.titles))
                    }
                    StatusFormat::Mermaid => {
                        print!("{}", ctxgrd::status::render_mermaid(&report))
                    }
                    StatusFormat::Dot => print!("{}", ctxgrd::status::render_dot(&report)),
                }
                if self.exit_code {
                    // ADR-056 § EARS-04 as redefined by ADR-118 § STG-004:
                    // done iff no document in scope is held by a non-terminal
                    // dependency. A pure projection of the same read that
                    // produced the report — no second definition of done, no
                    // file touched.
                    if report.unblocked() {
                        return Ok(Outcome::Did(SelfRendered));
                    }
                    return Ok(Outcome::Findings(SelfRendered));
                }
                // EARS-05.1: stage position is data — exit 0.
                Ok(Outcome::Did(SelfRendered))
            }
            // EARS-05.2: an invalid configuration is a kernel error (exit 2).
            Err(ctxgrd::status::StatusError::Lint(e)) => {
                Err(KernelError::report(e.to_diagnostic(root)))
            }
            // EARS-04.5: a `--lineage <ID>` that resolves to no document in the
            // run is a kernel error (exit 2).
            Err(nf @ ctxgrd::status::StatusError::LineageNotFound { .. }) => {
                let d = Diagnostic::error("pipeline.lineage-not-found", "", 0, 0, nf.to_string())
                    .with_help(
                        "pass an id present in this run (see `ctxgrd list`); --lineage scopes \
                         by the depends_on graph, not the filesystem",
                    );
                Err(KernelError::report(d))
            }
        }
    }
}

/// `ctxgrd rules` — introspect the resolved rule set.
pub(super) struct RulesCmd {
    pub(super) namespace: Option<String>,
    pub(super) rule_code: Option<String>,
    pub(super) format: Format,
}

impl Command for RulesCmd {
    type Json = Vec<RuleEntry>;

    fn emits_json(&self) -> bool {
        matches!(self.format, Format::Json)
    }

    fn render_json(out: &Self::Json) -> String {
        introspect::render_json(out)
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        let config = config::load(root)
            .map_err(|e| KernelError::report(LintError::Config(e).to_diagnostic(root)))?;
        let discovered = config::discover_external_rules(root)
            .map_err(|e| KernelError::report(LintError::Config(e).to_diagnostic(root)))?;
        // BUG-049: report the rule set that would actually dispatch, not
        // the one `ctxgrd.toml` happens to name. A namespace claimed by a
        // document but never declared is really linted with the
        // zero-config six; deriving this from the config alone answered
        // `[]` for a tree `lint` was governing with 12 rules.
        let governed = run::governed_namespaces(root, &config)
            .map_err(|e| KernelError::report(e.to_diagnostic(root)))?;

        // BUG-049 follow-up: `governed_namespaces` walks markdown only,
        // because running `sources/<name>/run` would give introspection
        // subprocess side effects. A namespace that exists solely in an
        // external source's envelopes is therefore invisible here while
        // `lint` governs it — the original defect, narrowed but not gone.
        // Say so rather than imply the list is complete; stderr, so
        // `--format json | jq` is unaffected.
        if !config.sources.is_empty() {
            let mut names: Vec<&str> = config.sources.keys().map(String::as_str).collect();
            names.sort_unstable();
            eprintln!(
                "note: {} external source(s) configured ({}); namespaces that only appear in \
                 their envelopes are not listed — sources are not run for introspection",
                names.len(),
                names.join(", ")
            );
        }

        let entries =
            introspect::list_rules(&config, &discovered, self.namespace.as_deref(), &governed);

        match self.format {
            // JSON bypasses the detail view — machine consumers want the full
            // array. The dispatcher writes it (via `render_json`).
            Format::Json => {}
            // `rich` and `simple` share the same text rendering for `rules`.
            Format::Rich | Format::Simple => {
                if let Some(code) = self.rule_code.as_deref() {
                    print!("{}", introspect::render_detail(&entries, code, &discovered));
                } else {
                    print!("{}", introspect::render_table(&entries));
                }
            }
        }
        Ok(Outcome::Did(entries))
    }
}

/// `ctxgrd list` — list ingested documents grouped by namespace (ADR-015).
pub(super) struct ListCmd {
    pub(super) namespace: Option<String>,
    pub(super) format: ListFormat,
}

impl Command for ListCmd {
    type Json = Vec<DocEntry>;

    fn emits_json(&self) -> bool {
        matches!(self.format, ListFormat::Json)
    }

    fn render_json(out: &Self::Json) -> String {
        list::render_json(out)
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        let entries = list::inventory(root, self.namespace.as_deref()).map_err(|e| {
            let d = match e {
                run::LintError::Config(ce) => run::config_error_to_diagnostic(&ce, root),
                other => Diagnostic::error("internal", "", 0, 0, format!("{other}")),
            };
            KernelError::report(d)
        })?;

        let json = matches!(self.format, ListFormat::Json);
        // An empty inventory would render `rich` as a lonely header row and
        // `markdown` as nothing at all — both read as "did it work?". JSON keeps
        // the valid empty array so machine consumers are unaffected.
        if entries.is_empty() && !json {
            match self.namespace.as_deref() {
                Some(ns) => println!("No {ns} documents found."),
                None => println!("No documents found."),
            }
            return Ok(Outcome::Did(entries));
        }

        match self.format {
            ListFormat::Rich => print!("{}", list::render_table(&entries)),
            ListFormat::Markdown => print!("{}", list::render_markdown(&entries)),
            // JSON: the dispatcher writes the array (via `render_json`).
            ListFormat::Json => {}
        }
        Ok(Outcome::Did(entries))
    }
}
