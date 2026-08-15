//! `ctxgrd pack` — list/show/add/outdated/migrate reusable namespace bundles
//! (ADR-013, ADR-053).

use std::fs;
use std::io;
use std::path::Path;

use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::pack::MigratePlan;

use super::command::{Command, Ctx, KernelError, Outcome};
use super::{relative_display, Format};

/// `ctxgrd pack list` — read-only table of every discoverable pack (PACK-004).
pub(super) struct PackListCmd {
    pub(super) paid: bool,
    pub(super) format: Format,
}

impl Command for PackListCmd {
    type Json = serde_json::Value;

    fn emits_json(&self) -> bool {
        matches!(self.format, Format::Json)
    }

    fn render_json(out: &Self::Json) -> String {
        serde_json::to_string_pretty(out).unwrap_or_else(|_| "[]".to_string())
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        if matches!(self.format, Format::Json) {
            // ADR-086 § WIRE-001: the pack catalog as a structured array an
            // agent can act on — `[{"name":…,"namespaces":[…]}, …]`.
            let catalog: Vec<serde_json::Value> = if self.paid {
                ctxgrd::pack::paid_packs()
                    .iter()
                    .map(|p| serde_json::json!({ "name": p.name, "namespaces": p.namespaces }))
                    .collect()
            } else {
                ctxgrd::pack::discover(root)
                    .iter()
                    .map(|p| {
                        let namespaces: Vec<String> = ctxgrd::pack::namespace_views(p)
                            .into_iter()
                            .map(|v| v.name)
                            .collect();
                        serde_json::json!({ "name": p.name, "namespaces": namespaces })
                    })
                    .collect()
            };
            return Ok(Outcome::Did(serde_json::Value::Array(catalog)));
        }
        if self.paid {
            print!(
                "{}",
                ctxgrd::pack::render_paid_list(&ctxgrd::pack::paid_packs())
            );
        } else {
            let packs = ctxgrd::pack::discover(root);
            print!("{}", ctxgrd::pack::render_list(&packs));
        }
        Ok(Outcome::Did(serde_json::Value::Null))
    }
}

/// `ctxgrd pack show <name>` — read-only detail view of one pack (PACK-004).
pub(super) struct PackShowCmd {
    pub(super) name: String,
    pub(super) format: Format,
}

impl Command for PackShowCmd {
    type Json = serde_json::Value;

    fn emits_json(&self) -> bool {
        matches!(self.format, Format::Json)
    }

    fn render_json(out: &Self::Json) -> String {
        serde_json::to_string_pretty(out).unwrap_or_else(|_| "{}".to_string())
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        let Some(pack) = ctxgrd::pack::find(root, &self.name) else {
            return Err(pack_not_found(&self.name));
        };
        if matches!(self.format, Format::Json) {
            // ADR-096 § CMD-002: the pack detail as a structured object. Per-
            // namespace rules stay under `namespaces[]`; the top-level `rules`
            // is the flat aggregate of every bound code across namespaces.
            //
            // ADR-113 § PKJ-001 adds `params` (every declared `[NS."rule"]`
            // table), `depends`, and `scope`. `scope` is not decoration: this
            // is the **pack definition**, not the project's materialised
            // `ctxgrd.toml`. `pack add` copies blocks into the consumer's
            // config, so the two diverge as a pack evolves — that gap is
            // `pack outdated`'s to report (ADR-053 § PKM-004), and a consumer
            // comparing against the wrong one gets a false green.
            let views = ctxgrd::pack::namespace_views(&pack);
            let mut aggregate: Vec<String> = Vec::new();
            let namespaces: Vec<serde_json::Value> = views
                .iter()
                .map(|v| {
                    for code in &v.rules {
                        if !aggregate.contains(code) {
                            aggregate.push(code.clone());
                        }
                    }
                    serde_json::json!({
                        "namespace": v.name,
                        "rules": v.rules,
                        "paths": v.path_patterns,
                        "required_metadata": v.required_metadata,
                        "params": v.params,
                        "fingerprint": v.fingerprint,
                    })
                })
                .collect();
            let external_rules: Vec<serde_json::Value> = pack
                .rules
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "code": r.code(),
                        "path": format!("rules/{}/{}/run", r.ns, r.name),
                    })
                })
                .collect();
            let doc = serde_json::json!({
                "name": pack.name,
                "path": pack.source_label,
                "summary": pack.summary,
                "depends": pack.depends(),
                "scope": "pack-definition",
                "namespaces": namespaces,
                "external_rules": external_rules,
                "rules": aggregate,
            });
            return Ok(Outcome::Did(doc));
        }
        print!("{}", ctxgrd::pack::render_show(&pack));
        Ok(Outcome::Did(serde_json::Value::Null))
    }
}

/// `ctxgrd pack add <name>` — apply a pack, create-or-append, never clobber
/// (PACK-005). `--dry-run` prints the config it would write and touches no file.
pub(super) struct PackAddCmd {
    pub(super) name: String,
    pub(super) dry_run: bool,
    pub(super) format: Format,
}

/// ADR-096 § CMD-001 wire shape for `ctxgrd pack add <name> --format json`.
#[derive(serde::Serialize)]
pub(super) struct PackAddJson {
    status: &'static str,
    namespaces_added: Vec<String>,
    path: String,
}

impl Command for PackAddCmd {
    type Json = PackAddJson;

    fn emits_json(&self) -> bool {
        matches!(self.format, Format::Json)
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        let Some(pack) = ctxgrd::pack::find(root, &self.name) else {
            return Err(pack_not_found(&self.name));
        };

        // ADR-068 § PKD-002: apply the dependency closure (deps first, `pack`
        // last) so `pack add gdpr` also installs the `security` base.
        let chain = ctxgrd::pack::resolve_dependencies(root, &pack)
            .map_err(|e| pack_dependency_error(&e))?;

        let toml_rel = relative_display(&root.join("ctxgrd.toml"), root);

        // ADR-096 § CMD-001: `--format json` emits `{status, namespaces_added,
        // path}` — the dispatcher writes it; human progress stays on stderr.
        if matches!(self.format, Format::Json) {
            let mut added: Vec<String> = Vec::new();
            if self.dry_run {
                let mut existing = fs::read_to_string(root.join("ctxgrd.toml")).unwrap_or_default();
                for p in &chain {
                    let plan = ctxgrd::pack::plan_add(p, &existing, root);
                    if !plan.blocks_text.is_empty() {
                        existing.push_str(&plan.blocks_text);
                    }
                    added.extend(plan.added.iter().cloned());
                }
                let status = if added.is_empty() { "up-to-date" } else { "would-add" };
                return Ok(Outcome::Did(PackAddJson {
                    status,
                    namespaces_added: added,
                    path: toml_rel,
                }));
            }
            for p in &chain {
                let plan = ctxgrd::pack::apply_add(p, root).map_err(io_kernel)?;
                added.extend(plan.added.iter().cloned());
            }
            let status = if added.is_empty() { "up-to-date" } else { "added" };
            return Ok(Outcome::Did(PackAddJson {
                status,
                namespaces_added: added,
                path: toml_rel,
            }));
        }

        if self.dry_run {
            // Thread the growing config so a later pack in the chain skips a
            // namespace an earlier one already added.
            let mut existing = fs::read_to_string(root.join("ctxgrd.toml")).unwrap_or_default();
            let mut wrote_any = false;
            let mut added: Vec<String> = Vec::new();
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
                added.extend(plan.added.iter().cloned());
            }
            if !wrote_any {
                println!("# (nothing to add — every namespace is already present)");
            }
            let status = if added.is_empty() { "up-to-date" } else { "would-add" };
            return Ok(Outcome::Did(PackAddJson {
                status,
                namespaces_added: added,
                path: toml_rel,
            }));
        }

        let mut added: Vec<String> = Vec::new();
        for p in &chain {
            let plan = ctxgrd::pack::apply_add(p, root).map_err(io_kernel)?;
            report_pack_add(p, &plan);
            added.extend(plan.added.iter().cloned());
        }
        let status = if added.is_empty() { "up-to-date" } else { "added" };
        Ok(Outcome::Did(PackAddJson {
            status,
            namespaces_added: added,
            path: toml_rel,
        }))
    }
}

/// Shared success report for `pack add` and `init --pack` (PKC-003).
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

/// `ctxgrd pack outdated` — read-only drift report (ADR-053 § PKM-004,
/// ADR-126 § DRF-001). Reports blocks whose *pack* has moved since they were
/// stamped; consumer edits are not drift. Exit 0 = no pack moved (blocks with
/// no baseline are listed but do not count), 1 = a pack moved, 2 = config
/// error.
pub(super) struct PackOutdatedCmd {
    pub(super) format: Format,
}

impl Command for PackOutdatedCmd {
    type Json = MigratePlan;

    fn emits_json(&self) -> bool {
        matches!(self.format, Format::Json)
    }

    fn render_json(out: &Self::Json) -> String {
        serde_json::to_string_pretty(out).unwrap_or_else(|_| "{}".to_string())
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        let toml_path = root.join("ctxgrd.toml");
        let config_toml =
            fs::read_to_string(&toml_path).map_err(|e| pack_config_read_error(root, &toml_path, &e))?;
        let plan = ctxgrd::pack::plan_migrate(&config_toml, root);
        // A stamp-only rewrite is a block that is already at its pack's
        // current shape and merely lacks a baseline; it is housekeeping
        // `pack migrate` performs, not drift, and must not fail the gate
        // (ADR-126 § DRF-008).
        let drift =
            plan.rewrites.iter().any(|r| !r.stamp_only) || !plan.diffs.is_empty();

        match self.format {
            // JSON: the dispatcher writes the plan (via `render_json`).
            Format::Json => {}
            Format::Rich | Format::Simple => {
                if !drift && plan.unknown.is_empty() {
                    println!("ctxgrd.toml is up to date with the installed packs.");
                } else {
                    // Baseline-less blocks are reported but are not drift:
                    // the pack-moved question cannot be asked of them, so
                    // they must not set the exit code (ADR-126).
                    print!("{}", ctxgrd::pack::render_migrate_report(&plan, true));
                }
            }
        }

        if drift {
            Ok(Outcome::Findings(plan))
        } else {
            Ok(Outcome::Did(plan))
        }
    }
}

/// `ctxgrd pack migrate` — rewrite blocks that still match their pack
/// byte-for-byte, give v1 stamps a baseline, and emit a diff for the rest
/// (ADR-053 § PKM-002, ADR-126 § DRF-007). A block the consumer edited is
/// never overwritten. Exit 0 = nothing to do or only clean rewrites applied,
/// 1 = blocks remain to reconcile, 2 = config error.
pub(super) struct PackMigrateCmd {
    pub(super) dry_run: bool,
    pub(super) format: Format,
}

impl Command for PackMigrateCmd {
    type Json = MigratePlan;

    fn emits_json(&self) -> bool {
        matches!(self.format, Format::Json)
    }

    fn render_json(out: &Self::Json) -> String {
        serde_json::to_string_pretty(out).unwrap_or_else(|_| "{}".to_string())
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        let toml_path = root.join("ctxgrd.toml");
        // Read first so a missing/unreadable config is a clean exit-2, not an
        // io panic inside apply_migrate.
        if let Err(e) = fs::read_to_string(&toml_path) {
            return Err(pack_config_read_error(root, &toml_path, &e));
        }
        let plan = ctxgrd::pack::apply_migrate(root, self.dry_run).map_err(io_kernel)?;

        match self.format {
            Format::Json => {
                // The dispatcher writes the plan; progress goes to stderr.
                if !self.dry_run && !plan.rewrites.is_empty() {
                    eprintln!("migrated {} block(s) in ctxgrd.toml", plan.rewrites.len());
                }
            }
            Format::Rich | Format::Simple => {
                print!("{}", ctxgrd::pack::render_migrate_report(&plan, self.dry_run));
            }
        }

        if plan.diffs.is_empty() {
            Ok(Outcome::Did(plan))
        } else {
            Ok(Outcome::Findings(plan))
        }
    }
}

/// An `io::Error` that escaped a pack apply/migrate — mirrors the pre-refactor
/// anyhow bubble to `main`'s `internal` diagnostic (exit 2).
fn io_kernel(e: io::Error) -> KernelError {
    KernelError::report(Diagnostic::error("internal", "", 0, 0, format!("{e}")))
}

/// Emit a `pack.config-unreadable` kernel error (exit 2) for a config
/// `pack migrate`/`pack outdated` cannot read.
fn pack_config_read_error(root: &Path, path: &Path, e: &io::Error) -> KernelError {
    let rel = relative_display(path, root);
    KernelError::report(
        Diagnostic::error(
            "pack.config-unreadable",
            &rel,
            0,
            0,
            format!("could not read {rel}: {e}"),
        )
        .with_help("run `ctxgrd init` to create a ctxgrd.toml, or check file permissions"),
    )
}

/// Emit the `pack.dependency` error (exit 2) for an unresolvable dependency
/// closure (ADR-068 § PKD-003).
fn pack_dependency_error(e: &ctxgrd::pack::DependencyError) -> KernelError {
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
    KernelError::report(Diagnostic::error("pack.dependency", "", 0, 0, e.to_string()).with_help(help))
}

/// Emit the `pack.unknown` error (exit 2).
fn pack_not_found(name: &str) -> KernelError {
    KernelError::report(
        Diagnostic::error("pack.unknown", "", 0, 0, format!("unknown pack '{name}'"))
            .with_help("run `ctxgrd pack list` to see available packs"),
    )
}
