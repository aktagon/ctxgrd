//! Rule packs (ADR-013): named, reusable bundles of namespace config
//! plus the external rule scripts they need.
//!
//! A pack is a **generator, not a dependency** (PACK-001). `pack add`
//! writes the pack's namespace blocks into `ctxgrd.toml` verbatim, each
//! prefixed with a `# pack: <name>` provenance comment, then walks away.
//! ctxgrd never reads packs at lint time, so behaviour is identical
//! whether or not the pack is still discoverable after application.
//!
//! On disk a pack is a directory (PACK-002): a `pack.toml` carrying the
//! `[<NS>]` blocks it contributes (same grammar as `ctxgrd.toml`) plus an
//! optional `rules/<ns>/<name>/run` subtree of external rule scripts. No
//! new file format is introduced.
//!
//! Discovery is layered (PACK-003): built-in packs compiled into the
//! binary, then `<global>/packs/*`, then `<root>/packs/*`. On a name
//! collision the more-local source wins.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use toml::Value;

/// Built-in pack definitions, embedded at compile time so they ship in
/// the binary with no filesystem dependency (PACK-003, PACK-009).
const PROJECT_DOCS_TOML: &str = include_str!("../packs/project-docs/pack.toml");
const OPS_TOML: &str = include_str!("../packs/ops/pack.toml");
const AGENTS_TOML: &str = include_str!("../packs/agents/pack.toml");
const DESIGN_TOML: &str = include_str!("../packs/design/pack.toml");
const PERSONA_TOML: &str = include_str!("../packs/persona/pack.toml");

/// A discovered pack, normalised across the three discovery sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pack {
    /// Pack name — the directory name on disk, or the built-in key.
    pub name: String,
    /// One-line summary, read from the `# summary:` comment in `pack.toml`.
    pub summary: String,
    /// Human label for the resolved source, e.g. `built-in` or
    /// `./packs/<name>` (PACK-003 "report the source").
    pub source_label: String,
    /// Collision rank: 0 built-in, 1 global, 2 local. Higher wins.
    rank: u8,
    /// Raw `pack.toml` text. Segmented verbatim on `pack add`.
    pub toml_text: String,
    /// External rule scripts the pack ships (PACK-002).
    pub rules: Vec<PackRule>,
}

/// One bundled external rule script, addressed by its `<ns>/<name>`
/// directory under the pack's `rules/` tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRule {
    /// Rule namespace directory, e.g. `llm`.
    pub ns: String,
    /// Rule name directory, e.g. `agents`. Together they form the rule
    /// code `<ns>.<name>` (e.g. `llm.agents`).
    pub name: String,
    /// The `run` script contents, copied verbatim on `pack add`.
    pub contents: String,
}

impl PackRule {
    /// The rule code this script resolves to, `<ns>.<name>`.
    pub fn code(&self) -> String {
        format!("{}.{}", self.ns, self.name)
    }
}

/// A single namespace a pack defines, for the `pack show` view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceView {
    pub name: String,
    pub rules: Vec<String>,
    pub required_metadata: Vec<String>,
    pub path_patterns: Vec<String>,
}

/// The result of planning a `pack add` against an existing config.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AddPlan {
    /// Text to append to `ctxgrd.toml` (provenance comments included).
    /// Empty when every namespace is already present.
    pub blocks_text: String,
    /// Namespaces that will be added.
    pub added: Vec<String>,
    /// Namespaces skipped because the config already defines them
    /// (PACK-005 never-clobber).
    pub skipped: Vec<String>,
    /// Rule scripts to copy — only those not already on disk.
    pub rules_to_copy: Vec<PackRule>,
}

/// The built-in packs (PACK-009). Four packs: `project-docs`, `ops`,
/// `agents`, `design`, and `persona` (ADR-023 § PKC-001, ADR-027, ADR-034,
/// ADR-035).
pub fn builtin_packs() -> Vec<Pack> {
    vec![
        Pack {
            name: "project-docs".to_string(),
            summary: summary_of(PROJECT_DOCS_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: PROJECT_DOCS_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // RUN/PMR — the incident-management doc lifecycle (runbooks +
            // postmortems), grounded in Google's SRE Book. Split from
            // project-docs because the adoption decision differs: teams
            // adopt ADRs without an incident process and vice versa.
            name: "ops".to_string(),
            summary: summary_of(OPS_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: OPS_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // AGENTS/SKILLS/SPEC/TASK/PROMPT namespaces. Mixed-claim: AGENTS
            // and SKILLS are path-claimed; SPEC/TASK/PROMPT are id-claimed.
            // Rules are all builtin-compiled (ADR-023 § PKC-003), so `rules`
            // is empty (no external `run` scripts shipped).
            name: "agents".to_string(),
            summary: summary_of(AGENTS_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: AGENTS_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // DESIGN namespace — path-claimed on DESIGN.md. Structural checks
            // only (section order, token refs). Semantic checks (WCAG, orphaned
            // tokens) are delegated to @google/design.md lint (ADR-027 § DES-005).
            name: "design".to_string(),
            summary: summary_of(DESIGN_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: DESIGN_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // SOUL/STYLE namespaces — path-claimed on SOUL.md / STYLE.md.
            // Structural checks only: SOUL.md high-signal section presence
            // (ADR-035), STYLE.md section order and SOUL.md pairing (ADR-034).
            // The semantic quality tests (STYLE rules-vs-adjectives, SOUL
            // prediction test) are declined — there is no companion linter to
            // delegate them to (ADR-034 § STY-005, ADR-035 § SOUL-005). Kept
            // standalone, not folded into `agents`, because persona/voice is
            // orthogonal to the coding-agent workflow (ADR-034 § Context,
            // following the ADR-027 precedent for `design`).
            name: "persona".to_string(),
            summary: summary_of(PERSONA_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: PERSONA_TOML.to_string(),
            rules: Vec::new(),
        },
    ]
}

/// Discover every pack visible from `root`, resolving the layered search
/// order (PACK-003): built-in, then `<global>/packs/*`, then
/// `<root>/packs/*`, with the more-local source winning on name
/// collision. Returned sorted by name.
pub fn discover_packs(root: &Path, global_dir: Option<&Path>) -> Vec<Pack> {
    let mut by_name: BTreeMap<String, Pack> = BTreeMap::new();
    for p in builtin_packs() {
        by_name.insert(p.name.clone(), p);
    }
    if let Some(global) = global_dir {
        for p in load_packs_in(&global.join("packs"), false) {
            insert_if_wins(&mut by_name, p);
        }
    }
    for p in load_packs_in(&root.join("packs"), true) {
        insert_if_wins(&mut by_name, p);
    }
    by_name.into_values().collect()
}

/// Convenience wrapper resolving the global dir the same way the config
/// loader does.
pub fn discover(root: &Path) -> Vec<Pack> {
    discover_packs(root, crate::config::global_ctxgrd_dir().as_deref())
}

/// Find one pack by name among the discovered set.
pub fn find(root: &Path, name: &str) -> Option<Pack> {
    discover(root).into_iter().find(|p| p.name == name)
}

/// Names of discoverable packs that provide rule `code` in any of their
/// namespaces' rule lists (ADR-025 § PKD-001). Sorted, deduplicated.
///
/// Reads `namespace_views`, which lists both builtin-compiled rules a pack
/// bundles (e.g. `skills.frontmatter` → `agents`) and external-script pack
/// rules — so a `cfg.rule-unknown` for either kind can point at the pack
/// that installs it. Empty when no discoverable pack provides the code.
pub fn providers_of(root: &Path, code: &str) -> Vec<String> {
    let mut names: Vec<String> = discover(root)
        .iter()
        .filter(|p| {
            namespace_views(p)
                .iter()
                .any(|v| v.rules.iter().any(|r| r == code))
        })
        .map(|p| p.name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn insert_if_wins(by_name: &mut BTreeMap<String, Pack>, pack: Pack) {
    match by_name.get(&pack.name) {
        Some(existing) if existing.rank > pack.rank => {}
        _ => {
            by_name.insert(pack.name.clone(), pack);
        }
    }
}

/// Read every `<dir>/<name>/pack.toml` into a [`Pack`]. `local` selects
/// the source label / rank. Missing directory → empty (silent).
fn load_packs_in(dir: &Path, local: bool) -> Vec<Pack> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Ok(toml_text) = fs::read_to_string(path.join("pack.toml")) else {
            continue;
        };
        let (source_label, rank) = if local {
            (format!("./packs/{name}"), 2)
        } else {
            (path.display().to_string(), 1)
        };
        out.push(Pack {
            name: name.to_string(),
            summary: summary_of(&toml_text),
            source_label,
            rank,
            toml_text,
            rules: load_pack_rules(&path.join("rules")),
        });
    }
    out
}

/// Walk a pack's `rules/<ns>/<name>/run` subtree into [`PackRule`]s.
fn load_pack_rules(rules_dir: &Path) -> Vec<PackRule> {
    let mut out = Vec::new();
    let Ok(ns_entries) = fs::read_dir(rules_dir) else {
        return out;
    };
    for ns_entry in ns_entries.flatten() {
        if !ns_entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(ns) = ns_entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if ns.starts_with('.') {
            continue;
        }
        let Ok(name_entries) = fs::read_dir(ns_entry.path()) else {
            continue;
        };
        for name_entry in name_entries.flatten() {
            if !name_entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Some(name) = name_entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let run = name_entry.path().join("run");
            if let Ok(contents) = fs::read_to_string(&run) {
                out.push(PackRule {
                    ns: ns.clone(),
                    name,
                    contents,
                });
            }
        }
    }
    out.sort_by(|a, b| (a.ns.as_str(), a.name.as_str()).cmp(&(b.ns.as_str(), b.name.as_str())));
    out
}

/// The namespaces a pack defines, in `pack.toml` declaration order, with
/// their rule list, required-metadata keys, and path patterns resolved
/// from the parsed TOML. Order comes from text segmentation so it matches
/// what `pack add` would append.
pub fn namespace_views(pack: &Pack) -> Vec<NamespaceView> {
    let table = pack.toml_text.parse::<Value>().ok();
    namespace_blocks(&pack.toml_text)
        .into_iter()
        .map(|(name, _)| {
            let ns_tbl = table
                .as_ref()
                .and_then(|v| v.get(&name))
                .and_then(Value::as_table);
            let rules = ns_tbl
                .and_then(|t| t.get("rules"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let required_metadata = ns_tbl
                .and_then(|t| t.get("core.required-metadata"))
                .and_then(Value::as_table)
                .and_then(|t| t.get("keys"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let path_patterns = ns_tbl
                .and_then(|t| t.get("paths"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            NamespaceView {
                name,
                rules,
                required_metadata,
                path_patterns,
            }
        })
        .collect()
}

/// Plan a `pack add` against the current `ctxgrd.toml` text, without
/// touching any file. Namespaces already present are skipped
/// (PACK-005); rule scripts already on disk are not re-copied.
pub fn plan_add(pack: &Pack, existing_toml: &str, root: &Path) -> AddPlan {
    let existing = existing_namespaces(existing_toml);
    let mut plan = AddPlan::default();
    for (ns, block) in namespace_blocks(&pack.toml_text) {
        if existing.contains(&ns) {
            plan.skipped.push(ns);
            continue;
        }
        plan.blocks_text
            .push_str(&format!("\n# pack: {}\n", pack.name));
        plan.blocks_text.push_str(&block);
        plan.blocks_text.push('\n');
        plan.added.push(ns);
    }
    plan.rules_to_copy = pack
        .rules
        .iter()
        .filter(|r| {
            !root
                .join("rules")
                .join(&r.ns)
                .join(&r.name)
                .join("run")
                .exists()
        })
        .cloned()
        .collect();
    plan
}

/// Apply a pack to `root`: append its missing namespace blocks to
/// `ctxgrd.toml` (creating the file if absent) and copy any bundled rule
/// scripts not already present (PACK-005). Returns the executed plan.
pub fn apply_add(pack: &Pack, root: &Path) -> io::Result<AddPlan> {
    let toml_path = root.join("ctxgrd.toml");
    let existed = toml_path.exists();
    let existing_toml = fs::read_to_string(&toml_path).unwrap_or_default();
    let plan = plan_add(pack, &existing_toml, root);

    if !plan.blocks_text.is_empty() {
        let mut content = if existed {
            let mut c = existing_toml;
            while c.ends_with('\n') {
                c.pop();
            }
            c.push('\n');
            c
        } else {
            fs::create_dir_all(root)?;
            "# ctxgrd.toml — generated by `ctxgrd pack add`.\n".to_string()
        };
        content.push_str(&plan.blocks_text);
        fs::write(&toml_path, content)?;
    }

    for rule in &plan.rules_to_copy {
        let dir = root.join("rules").join(&rule.ns).join(&rule.name);
        fs::create_dir_all(&dir)?;
        let run = dir.join("run");
        fs::write(&run, &rule.contents)?;
        set_executable(&run)?;
    }
    Ok(plan)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// The one-line `# summary:` comment from a `pack.toml`, or empty.
fn summary_of(toml: &str) -> String {
    toml.lines()
        .find_map(|l| l.trim_start().strip_prefix("# summary:"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// A string list from a built-in doc pack's param table for
/// `namespace` (e.g. `core.required-headings` / `headings`), or `None`
/// when no pack defines it. Used by `ctxgrd init` to render the pack's
/// shape as the active default, keeping the embedded `pack.toml` files
/// the single source of truth.
///
/// Scans `project-docs` and `ops` only — the `agents` and `design`
/// packs use builtin-compiled rules, not the core nine that init
/// renders, so their namespaces are out of init's catalogue.
fn builtin_pack_list(namespace: &str, table: &str, key: &str) -> Option<Vec<String>> {
    [PROJECT_DOCS_TOML, OPS_TOML]
        .into_iter()
        .find_map(|toml_text| {
            let value: Value = toml_text.parse().expect("built-in pack.toml is valid TOML");
            Some(
                value
                    .get(namespace)?
                    .get(table)?
                    .get(key)?
                    .as_array()?
                    .iter()
                    .filter_map(|h| h.as_str().map(str::to_string))
                    .collect(),
            )
        })
}

/// `core.required-headings.headings` from a built-in doc pack.
pub(crate) fn builtin_pack_headings(namespace: &str) -> Option<Vec<String>> {
    builtin_pack_list(namespace, "core.required-headings", "headings")
}

/// `core.required-metadata.keys` from a built-in doc pack.
pub(crate) fn builtin_pack_metadata_keys(namespace: &str) -> Option<Vec<String>> {
    builtin_pack_list(namespace, "core.required-metadata", "keys")
}

/// `core.allowed-values.status` from a built-in doc pack.
pub(crate) fn builtin_pack_status_values(namespace: &str) -> Option<Vec<String>> {
    builtin_pack_list(namespace, "core.allowed-values", "status")
}

/// Namespace keys already defined in a `ctxgrd.toml`. Parse failure
/// yields an empty set (append-only never loses existing bytes).
pub(crate) fn existing_namespaces(toml: &str) -> std::collections::BTreeSet<String> {
    let Ok(Value::Table(table)) = toml.parse::<Value>() else {
        return std::collections::BTreeSet::new();
    };
    table
        .keys()
        .filter(|k| is_namespace_key(k))
        .cloned()
        .collect()
}

/// Segment `pack.toml` text into `(namespace, verbatim_block)` pairs in
/// declaration order. A namespace block is the contiguous run of lines
/// from its `[<NS>]` header through the table headers nested under it
/// (`[<NS>."core.required-metadata"]` etc.), up to the next top-level
/// namespace header. Preamble before the first namespace header (the
/// `# summary:` line) is dropped. Trailing blank lines are trimmed.
pub fn namespace_blocks(toml: &str) -> Vec<(String, String)> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut current: Option<String> = None;
    for line in toml.lines() {
        if let Some(ns) = header_namespace(line) {
            if current.as_deref() != Some(ns.as_str()) {
                blocks.push((ns.clone(), String::new()));
                current = Some(ns);
            }
        }
        if current.is_some() {
            if let Some((_, buf)) = blocks.last_mut() {
                buf.push_str(line);
                buf.push('\n');
            }
        }
    }
    for (_, buf) in &mut blocks {
        *buf = buf.trim_end().to_string();
    }
    blocks
}

/// The namespace named by a table header line, if it is a top-level
/// namespace header. `[ADR]` and `[ADR."core.x"]` both yield `ADR`;
/// `[ignore]`, `[sources.foo]`, comments and blanks yield `None`.
fn header_namespace(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix('[')?;
    if rest.starts_with('[') {
        return None; // array-of-tables, not a namespace
    }
    let ns: String = rest
        .chars()
        .take_while(|c| *c != ']' && *c != '.' && *c != '"' && *c != ' ')
        .collect();
    if !ns.is_empty() && is_namespace_key(&ns) {
        Some(ns)
    } else {
        None
    }
}

/// A namespace key starts with an uppercase ASCII letter — mirrors
/// `config::is_namespace_key` (kept local to avoid widening its
/// visibility for one predicate).
fn is_namespace_key(key: &str) -> bool {
    key.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

// -- rendering ----------------------------------------------------------

/// Render the adoption receipt for `pack add` (ADR-023 § PKC-003).
///
/// Splits the added namespaces into two groups:
/// - "Linting now" — path-claimed namespaces (have `paths` set), which
///   start firing immediately if their claimed files exist.
/// - "Activates when you create a document" — id-claimed namespaces
///   (no `paths`), which are dormant until a file with a matching `id:`
///   is created.
///
/// Only includes ADDED namespaces (not skipped). Returns an empty string
/// when no namespaces were added.
pub fn render_add_receipt(pack: &Pack, plan: &AddPlan) -> String {
    if plan.added.is_empty() {
        return String::new();
    }

    let views = namespace_views(pack);
    let added_set: std::collections::BTreeSet<&str> =
        plan.added.iter().map(String::as_str).collect();

    let path_claimed: Vec<&NamespaceView> = views
        .iter()
        .filter(|v| added_set.contains(v.name.as_str()) && !v.path_patterns.is_empty())
        .collect();
    let id_claimed: Vec<&NamespaceView> = views
        .iter()
        .filter(|v| added_set.contains(v.name.as_str()) && v.path_patterns.is_empty())
        .collect();

    let ns_count = plan.added.len();
    let rule_families: std::collections::BTreeSet<String> = views
        .iter()
        .filter(|v| added_set.contains(v.name.as_str()))
        .flat_map(|v| {
            v.rules
                .iter()
                .filter_map(|r| r.split('.').next().map(str::to_string))
        })
        .filter(|prefix| prefix != "core")
        .collect();
    let family_count = rule_families.len();

    let mut out = String::new();
    out.push_str(&format!(
        "Added pack '{}' — {}, {} rule {}.\n",
        pack.name,
        plural_ns(ns_count),
        family_count,
        if family_count == 1 {
            "family"
        } else {
            "families"
        }
    ));

    if !path_claimed.is_empty() {
        out.push('\n');
        out.push_str("  Linting now (path-claimed — these files already exist):\n");
        for view in &path_claimed {
            out.push_str(&format!(
                "    • {:<8} {}\n",
                view.name,
                view.path_patterns.join(", ")
            ));
        }
    }

    if !id_claimed.is_empty() {
        out.push('\n');
        out.push_str("  Activates when you create a document (id-claimed):\n");
        for view in &id_claimed {
            out.push_str(&format!("    • {:<8} id: {}-<n>\n", view.name, view.name));
        }
    }

    out.push('\n');
    // CTA must name an id-claimed namespace (path-claimed ones like AGENTS
    // cannot be created with `ctxgrd new`). Use the first id-claimed
    // namespace from the added set, falling back to "SPEC" (finding #2).
    let cta_ns = id_claimed
        .first()
        .map(|v| v.name.as_str())
        .unwrap_or("SPEC");
    out.push_str(&format!(
        "  Run `ctxgrd` to lint your instruction files now,\n  or `ctxgrd new {cta_ns} \"<title>\"` to start the build loop.\n",
    ));

    out
}

fn plural_ns(n: usize) -> String {
    if n == 1 {
        "1 namespace".to_string()
    } else {
        format!("{n} namespaces")
    }
}

/// Grep-friendly table for `pack list` (PACK-004): name, namespace
/// count, shipped-script count, source, summary.
pub fn render_list(packs: &[Pack]) -> String {
    let rows: Vec<[String; 5]> = packs
        .iter()
        .map(|p| {
            [
                p.name.clone(),
                namespace_blocks(&p.toml_text).len().to_string(),
                p.rules.len().to_string(),
                p.source_label.clone(),
                p.summary.clone(),
            ]
        })
        .collect();
    let headers = ["NAME", "NS", "RULES", "SOURCE", "SUMMARY"];
    let mut widths = headers.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let mut out = String::new();
    for (i, h) in headers.iter().enumerate() {
        if i == headers.len() - 1 {
            out.push_str(h); // last column unpadded
        } else {
            out.push_str(&format!("{:<width$}  ", h, width = widths[i]));
        }
    }
    out.push('\n');
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if i == row.len() - 1 {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{:<width$}  ", cell, width = widths[i]));
            }
        }
        out.push('\n');
    }
    out
}

/// Compact available-packs table for `ctxgrd init` (ADR-025 § PKD-003):
/// pack name and the namespaces it defines (by name). Lists namespace names
/// rather than the count `render_list` shows — at init the user is choosing
/// which bundle to adopt, so the names are the signal. SOURCE is omitted
/// (always "built-in" for new users; redundant at init time).
/// Uses a gh-style per-column `─` separator under the header row.
pub fn render_init_packs(packs: &[Pack]) -> String {
    let rows: Vec<[String; 2]> = packs
        .iter()
        .map(|p| {
            let namespaces = namespace_views(p)
                .iter()
                .map(|v| v.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            [p.name.clone(), namespaces]
        })
        .collect();
    let headers = ["NAME", "NAMESPACES"];
    let mut name_width = headers[0].len();
    let mut ns_width = headers[1].len();
    for row in &rows {
        name_width = name_width.max(row[0].len());
        ns_width = ns_width.max(row[1].len());
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{:<width$}  {}\n",
        headers[0],
        headers[1],
        width = name_width
    ));
    out.push_str(&format!(
        "{}  {}\n",
        "─".repeat(name_width),
        "─".repeat(ns_width)
    ));
    for row in &rows {
        out.push_str(&format!(
            "{:<width$}  {}\n",
            row[0],
            row[1],
            width = name_width
        ));
    }
    out
}

/// Detail view for `pack show <name>` (PACK-004): the namespaces it
/// defines, each namespace's rules and required-metadata keys, and any
/// external rule scripts it ships.
pub fn render_show(pack: &Pack) -> String {
    let mut out = String::new();
    out.push_str(&format!("Pack: {}  ({})\n", pack.name, pack.source_label));
    if !pack.summary.is_empty() {
        out.push_str(&format!("{}\n", pack.summary));
    }
    out.push('\n');
    out.push_str("Namespaces:\n");
    for view in namespace_views(pack) {
        out.push_str(&format!("  [{}]\n", view.name));
        out.push_str(&format!(
            "    rules:             {}\n",
            view.rules.join(", ")
        ));
        if !view.path_patterns.is_empty() {
            out.push_str(&format!(
                "    paths:             {}\n",
                view.path_patterns.join(", ")
            ));
        }
        out.push_str(&format!(
            "    required-metadata: {}\n",
            view.required_metadata.join(", ")
        ));
    }
    out.push('\n');
    out.push_str("External rule scripts:\n");
    if pack.rules.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for rule in &pack.rules {
            out.push_str(&format!(
                "  rules/{}/{}/run  →  {}\n",
                rule.ns,
                rule.name,
                rule.code()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn summary_is_read_from_comment() {
        let toml = "# summary: Five doc types.\n[ADR]\nrules = []\n";
        assert_eq!(summary_of(toml), "Five doc types.");
    }

    #[test]
    fn header_namespace_recognizes_top_level_and_subtables() {
        assert_eq!(header_namespace("[ADR]"), Some("ADR".to_string()));
        assert_eq!(
            header_namespace("[ADR.\"core.required-metadata\"]"),
            Some("ADR".to_string())
        );
        assert_eq!(header_namespace("[ignore]"), None);
        assert_eq!(header_namespace("[sources.foo]"), None);
        assert_eq!(header_namespace("rules = []"), None);
        assert_eq!(header_namespace("# comment"), None);
    }

    #[test]
    fn namespace_blocks_preserve_declaration_order_and_drop_preamble() {
        let toml = "# summary: x\n\n[ADR]\nrules = [\"core.id\"]\n\n[ADR.\"core.allowed-values\"]\nstatus = [\"draft\"]\n\n[PRD]\nrules = [\"core.id\"]\n";
        let blocks = namespace_blocks(toml);
        let names: Vec<&str> = blocks.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["ADR", "PRD"]);
        // The ADR block carries both its header and its sub-table, no preamble.
        assert!(blocks[0].1.starts_with("[ADR]"));
        assert!(blocks[0].1.contains("[ADR.\"core.allowed-values\"]"));
        assert!(!blocks[0].1.contains("# summary"));
    }

    #[test]
    fn builtin_project_docs_defines_its_doc_types() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "project-docs")
            .unwrap();
        let names: Vec<String> = namespace_views(&pack).into_iter().map(|v| v.name).collect();
        assert_eq!(names, vec!["ADR", "PRD", "RFC", "BUG", "TODO"]);
    }

    #[test]
    fn builtin_ops_defines_run_and_pmr() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "ops")
            .unwrap();
        let names: Vec<String> = namespace_views(&pack).into_iter().map(|v| v.name).collect();
        assert_eq!(names, vec!["RUN", "PMR"]);
        // PMR follows the SRE Book Appendix D postmortem template and
        // requires the incident date.
        let pmr_headings = builtin_pack_headings("PMR").unwrap();
        assert_eq!(
            pmr_headings,
            vec![
                "Summary",
                "Impact",
                "Root Causes",
                "Trigger",
                "Resolution",
                "Detection",
                "Action Items",
                "Lessons Learned",
                "Timeline"
            ]
        );
        let pmr = namespace_views(&pack)
            .into_iter()
            .find(|v| v.name == "PMR")
            .unwrap();
        assert!(pmr.required_metadata.contains(&"incident_date".to_string()));
    }

    #[test]
    fn plan_add_skips_existing_namespace_and_adds_the_rest() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "project-docs")
            .unwrap();
        let existing = "[ADR]\nrules = [\"core.id\"]\n";
        let plan = plan_add(&pack, existing, Path::new("/nonexistent-root"));
        assert_eq!(plan.skipped, vec!["ADR".to_string()]);
        assert_eq!(
            plan.added,
            vec![
                "PRD".to_string(),
                "RFC".to_string(),
                "BUG".to_string(),
                "TODO".to_string()
            ]
        );
        // Every added block carries a provenance comment.
        assert_eq!(plan.blocks_text.matches("# pack: project-docs").count(), 4);
        assert!(plan.blocks_text.contains("[PRD]"));
        assert!(!plan.blocks_text.contains("[ADR]"));
    }

    #[test]
    fn plan_add_into_empty_config_adds_everything() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "project-docs")
            .unwrap();
        let plan = plan_add(&pack, "", Path::new("/nonexistent-root"));
        assert!(plan.skipped.is_empty());
        assert_eq!(plan.added.len(), 5);
    }

    #[test]
    fn apply_add_appends_without_modifying_existing_block() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let original = "[ADR]\nrules = [\"core.id\"]\n";
        fs::write(root.join("ctxgrd.toml"), original).unwrap();
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "project-docs")
            .unwrap();
        let plan = apply_add(&pack, root).unwrap();
        assert_eq!(plan.skipped, vec!["ADR".to_string()]);

        let result = fs::read_to_string(root.join("ctxgrd.toml")).unwrap();
        // The original [ADR] block survives verbatim at the head.
        assert!(result.starts_with(original));
        assert!(result.contains("# pack: project-docs"));
        assert!(result.contains("[TODO]"));
    }

    #[test]
    fn agents_pack_lists_five_namespaces_with_claim_split() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "agents")
            .unwrap();
        let views = namespace_views(&pack);
        let names: Vec<&str> = views.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["AGENTS", "SKILLS", "SPEC", "TASK", "PROMPT"]);
        // Path-claimed: AGENTS and SKILLS have path_patterns
        let agents = views.iter().find(|v| v.name == "AGENTS").unwrap();
        assert!(!agents.path_patterns.is_empty(), "AGENTS is path-claimed");
        let skills = views.iter().find(|v| v.name == "SKILLS").unwrap();
        assert!(!skills.path_patterns.is_empty(), "SKILLS is path-claimed");
        // Id-claimed: SPEC, TASK, PROMPT have no path_patterns
        for ns in ["SPEC", "TASK", "PROMPT"] {
            let v = views.iter().find(|v| v.name == ns).unwrap();
            assert!(
                v.path_patterns.is_empty(),
                "{ns} must be id-claimed (no paths)"
            );
        }
    }

    #[test]
    fn providers_of_maps_rule_code_to_bundling_pack() {
        // ADR-025 § PKD-001: a builtin-compiled rule a pack bundles is
        // discoverable by code. `skills.frontmatter` lives only in the
        // `agents` pack's [SKILLS] block.
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            providers_of(tmp.path(), "skills.frontmatter"),
            vec!["agents".to_string()]
        );
    }

    #[test]
    fn providers_of_is_empty_for_an_unprovided_code() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(providers_of(tmp.path(), "fizz.buzz").is_empty());
    }

    #[test]
    fn project_docs_drops_cr_and_task_adds_todo() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "project-docs")
            .unwrap();
        let names: Vec<String> = namespace_views(&pack).into_iter().map(|v| v.name).collect();
        assert!(
            names.contains(&"TODO".to_string()),
            "project-docs gains TODO"
        );
        assert!(!names.contains(&"CR".to_string()), "project-docs drops CR");
        assert!(
            !names.contains(&"TASK".to_string()),
            "project-docs drops TASK"
        );
        let todo = namespace_views(&pack)
            .into_iter()
            .find(|v| v.name == "TODO")
            .unwrap();
        assert!(todo.rules.contains(&"todo.freshness".to_string()));
        assert!(!todo.path_patterns.is_empty(), "TODO is path-claimed");
    }

    #[test]
    fn agents_paths_include_gemini_md() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "agents")
            .unwrap();
        let agents_view = namespace_views(&pack)
            .into_iter()
            .find(|v| v.name == "AGENTS")
            .unwrap();
        assert!(agents_view.path_patterns.contains(&"GEMINI.md".to_string()));
    }

    #[test]
    fn skills_namespace_claims_claude_and_codex_skill_md() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "agents")
            .unwrap();
        let skills = namespace_views(&pack)
            .into_iter()
            .find(|v| v.name == "SKILLS")
            .unwrap();
        assert!(skills
            .path_patterns
            .iter()
            .any(|p| p.contains(".claude/skills")));
        assert!(skills
            .path_patterns
            .iter()
            .any(|p| p.contains(".codex/skills")));
    }

    #[test]
    fn pack_add_receipt_splits_path_and_id_claims() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "agents")
            .unwrap();
        let plan = plan_add(&pack, "", Path::new("/nonexistent"));
        let receipt = render_add_receipt(&pack, &plan);
        assert!(
            receipt.contains("Linting now"),
            "has path-claim section:\n{receipt}"
        );
        assert!(
            receipt.contains("AGENTS"),
            "AGENTS in linting-now:\n{receipt}"
        );
        assert!(
            receipt.contains("SKILLS"),
            "SKILLS in linting-now:\n{receipt}"
        );
        assert!(
            receipt.contains("Activates when you create"),
            "has id-claim section:\n{receipt}"
        );
        assert!(receipt.contains("SPEC"), "SPEC in activates:\n{receipt}");
        assert!(receipt.contains("TASK"), "TASK in activates:\n{receipt}");
        assert!(
            receipt.contains("PROMPT"),
            "PROMPT in activates:\n{receipt}"
        );
    }

    /// Finding #2: render_add_receipt CTA must name the first id-claimed
    /// namespace, never a path-claimed one. AGENTS is declared first in
    /// agents/pack.toml but is path-claimed — the CTA must read
    /// `ctxgrd new SPEC`, never `ctxgrd new AGENTS`.
    #[test]
    fn render_add_receipt_cta_names_first_id_claimed_namespace() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "agents")
            .unwrap();
        let plan = plan_add(&pack, "", Path::new("/nonexistent"));
        let receipt = render_add_receipt(&pack, &plan);
        assert!(
            receipt.contains("ctxgrd new SPEC"),
            "CTA must name first id-claimed namespace SPEC, got:\n{receipt}"
        );
        assert!(
            !receipt.contains("ctxgrd new AGENTS"),
            "CTA must not name path-claimed namespace AGENTS:\n{receipt}"
        );
    }

    /// Finding #4: render_show must print a `paths:` line for path-claimed
    /// namespaces (AGENTS and SKILLS in the agents pack).
    #[test]
    fn render_show_includes_paths_for_path_claimed_namespaces() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "agents")
            .unwrap();
        let output = render_show(&pack);
        assert!(
            output.contains("paths:"),
            "render_show must print paths: for AGENTS/SKILLS:\n{output}"
        );
        assert!(
            output.contains("CLAUDE.md") || output.contains(".claude/skills"),
            "paths line must show actual claim patterns:\n{output}"
        );
    }

    #[test]
    fn no_builtin_pack_ships_an_external_script() {
        // ADR-013 § PACK-009 (amended): built-in pack rules are all
        // builtin-compiled, so no built-in pack ships a `run` script.
        for pack in builtin_packs() {
            assert!(
                pack.rules.is_empty(),
                "built-in pack `{}` must not ship external rule scripts",
                pack.name
            );
        }
    }

    #[test]
    fn apply_add_copies_executable_rule_script_from_local_pack() {
        // Local/global packs may still ship external scripts; only
        // built-in packs are script-free. Use an on-disk local pack here.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let pack_dir = root.join("packs/team-rules");
        let rule_dir = pack_dir.join("rules/team/freshness");
        fs::create_dir_all(&rule_dir).unwrap();
        fs::write(
            pack_dir.join("pack.toml"),
            "# summary: Team rules.\n[NOTE]\nrules = [\"team.freshness\"]\n",
        )
        .unwrap();
        fs::write(rule_dir.join("run"), "#!/usr/bin/env bash\nexit 0\n").unwrap();

        let pack = discover_packs(root, None)
            .into_iter()
            .find(|p| p.name == "team-rules")
            .expect("local pack discovered");
        apply_add(&pack, root).unwrap();
        let run = root.join("rules/team/freshness/run");
        assert!(run.is_file(), "rule script copied");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&run).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "script is executable");
        }
    }

    #[test]
    fn design_pack_produces_design_block_with_four_rules() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "design")
            .unwrap();
        let plan = plan_add(&pack, "", Path::new("/nonexistent-root"));
        assert!(plan.added.contains(&"DESIGN".to_string()));
        assert!(plan.blocks_text.contains("[DESIGN]"));
        assert!(plan.blocks_text.contains("DESIGN.md"));
        assert!(plan.blocks_text.contains("design.section-order"));
        assert!(plan.blocks_text.contains("design.token-ref"));
        assert!(plan.blocks_text.contains("core.frontmatter"));
        assert!(plan.blocks_text.contains("core.required-metadata"));
    }

    #[test]
    fn providers_of_design_section_order_names_design_pack() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            providers_of(tmp.path(), "design.section-order"),
            vec!["design".to_string()]
        );
    }

    #[test]
    fn persona_pack_produces_soul_block_with_soul_sections() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "persona")
            .unwrap();
        let plan = plan_add(&pack, "", Path::new("/nonexistent-root"));
        assert!(plan.added.contains(&"SOUL".to_string()));
        assert!(plan.added.contains(&"STYLE".to_string()));
        assert!(plan.blocks_text.contains("[SOUL]"));
        assert!(plan.blocks_text.contains("SOUL.md"));
        assert!(plan.blocks_text.contains("soul.sections"));
    }

    #[test]
    fn providers_of_soul_sections_names_persona_pack() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            providers_of(tmp.path(), "soul.sections"),
            vec!["persona".to_string()]
        );
    }

    #[test]
    fn local_pack_overrides_builtin_of_same_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = root.join("packs/project-docs");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("pack.toml"),
            "# summary: Local override.\n[ADR]\nrules = [\"core.id\"]\n",
        )
        .unwrap();
        let packs = discover_packs(root, None);
        let pd = packs.iter().find(|p| p.name == "project-docs").unwrap();
        assert_eq!(pd.source_label, "./packs/project-docs");
        assert_eq!(pd.summary, "Local override.");
    }
}
