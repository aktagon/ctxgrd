//! Read-only introspection commands: `docs`, `refs`, `status`, `rules`, `list`.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;

use ctxgrd::config;
use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::introspect;
use ctxgrd::list;
use ctxgrd::run::{self, LintError};

use super::{
    emit_error, Format, ListFormat, StatusFormat, DOC_NAMESPACES, DOC_PACKS, DOC_REFERENCES,
    DOC_RULES, DOC_SOURCES,
};

pub(super) fn docs_cmd(topic: Option<&str>) -> Result<ExitCode> {
    let Some(topic) = topic else {
        println!("Available topics:");
        println!("  namespaces  Configure namespaces and core rules in ctxgrd.toml");
        println!("  rules       Write external rule scripts (rules/<ns>/<name>/run)");
        println!("  sources     Write external source scripts (sources/<name>/run)");
        println!("  references  Scan non-markdown files for pointer mentions");
        println!("  packs       Apply reusable namespace bundles (ctxgrd pack)");
        println!();
        println!("Usage: ctxgrd docs <topic>");
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
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
            emit_error(&d, std::path::Path::new("."));
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    };
    print!("{body}");
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}

pub(super) fn refs_cmd(root: &PathBuf, id: &str, format: Format) -> Result<ExitCode> {
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

/// `ctxgrd status` — resolve the namespace DAG, optionally scope to a
/// `--lineage <ID>` feature, compute per-stage verdicts, sweep the BUG
/// tripwire, and render the result (SPEC-002, SPEC-003).
///
/// Exit-code matrix: WITHOUT `--exit-code`, a successful computation exits 0
/// regardless of pipeline position — early, blocked, or complete is data,
/// not failure (EARS-05.1). WITH `--exit-code`, the empty-frontier +
/// no-blocker done-signal is projected onto the exit: 0 when complete, 1
/// otherwise (ADR-056 § EARS-04). A config error, a namespace cycle, or an
/// unresolved `--lineage` id exits 2 (EARS-05.2, EARS-04.5).
pub(super) fn status_cmd(
    root: &PathBuf,
    format: StatusFormat,
    lineage: Option<&str>,
    exit_code: bool,
) -> Result<ExitCode> {
    match ctxgrd::status::report_scoped(root, lineage) {
        Ok(report) => {
            match format {
                StatusFormat::Text => print!("{}", ctxgrd::status::render_report(&report)),
                StatusFormat::Json => println!("{}", ctxgrd::status::render_json(&report)),
                StatusFormat::Mermaid => print!("{}", ctxgrd::status::render_mermaid(&report)),
                StatusFormat::Dot => print!("{}", ctxgrd::status::render_dot(&report)),
            }
            if exit_code {
                // ADR-056 § EARS-04: done iff the selected frontier is empty
                // AND no blocker is present. A pure projection of the same
                // read that produced the report (EARS-02.2) — no second
                // definition of done, no file touched (EARS-02.4).
                let done = report.frontier.is_empty() && report.blockers.is_empty();
                let status = if done {
                    run::ExitStatus::Ok
                } else {
                    run::ExitStatus::LintFailure
                };
                return Ok(ExitCode::from(status.code()));
            }
            // EARS-05.1: stage position is data — exit 0.
            Ok(ExitCode::from(run::ExitStatus::Ok.code()))
        }
        // EARS-05.2: an invalid configuration is a kernel error (exit 2).
        Err(ctxgrd::status::StatusError::Lint(e)) => {
            emit_error(&e.to_diagnostic(root), root);
            Ok(ExitCode::from(run::ExitStatus::KernelError.code()))
        }
        // EARS-01.5/05.2: a cyclic namespace graph is reported and
        // exits non-zero (kernel error, exit 2).
        Err(cycle @ ctxgrd::status::StatusError::Cycle { .. }) => {
            let d = Diagnostic::error("pipeline.namespace-cycle", "", 0, 0, cycle.to_string())
                .with_help(
                    "break the loop by removing one of the cross-namespace depends_on \
                     edges between these namespaces",
                );
            emit_error(&d, root);
            Ok(ExitCode::from(run::ExitStatus::KernelError.code()))
        }
        // EARS-04.5: a `--lineage <ID>` that resolves to no document in the
        // run is a kernel error (exit 2).
        Err(nf @ ctxgrd::status::StatusError::LineageNotFound { .. }) => {
            let d = Diagnostic::error("pipeline.lineage-not-found", "", 0, 0, nf.to_string())
                .with_help(
                    "pass an id present in this run (see `ctxgrd list`); --lineage scopes \
                     by the depends_on graph, not the filesystem",
                );
            emit_error(&d, root);
            Ok(ExitCode::from(run::ExitStatus::KernelError.code()))
        }
    }
}

pub(super) fn rules_cmd(
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

pub(super) fn list_cmd(root: &PathBuf, namespace: Option<&str>, format: ListFormat) -> Result<ExitCode> {
    let entries = match list::inventory(root, namespace) {
        Ok(e) => e,
        Err(e) => {
            let d = match e {
                run::LintError::Config(ce) => run::config_error_to_diagnostic(&ce, root),
                other => Diagnostic::error("internal", "", 0, 0, format!("{other}")),
            };
            emit_error(&d, root);
            return Ok(ExitCode::from(run::ExitStatus::KernelError.code()));
        }
    };

    // An empty inventory would render `rich` as a lonely header row and
    // `markdown` as nothing at all — both read as "did it work?". JSON
    // keeps the valid empty array so machine consumers are unaffected.
    if entries.is_empty() && !matches!(format, ListFormat::Json) {
        match namespace {
            Some(ns) => println!("No {ns} documents found."),
            None => println!("No documents found."),
        }
        return Ok(ExitCode::from(run::ExitStatus::Ok.code()));
    }

    match format {
        ListFormat::Rich => print!("{}", list::render_table(&entries)),
        ListFormat::Markdown => print!("{}", list::render_markdown(&entries)),
        ListFormat::Json => println!("{}", list::render_json(&entries)),
    }
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}
