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
const SECURITY_TOML: &str = include_str!("../packs/security/pack.toml");
const CLAUDE_TOML: &str = include_str!("../packs/claude/pack.toml");
const WORKFLOW_TOML: &str = include_str!("../packs/workflow/pack.toml");
const CODEX_TOML: &str = include_str!("../packs/codex/pack.toml");
const GEMINI_TOML: &str = include_str!("../packs/gemini/pack.toml");
const OPENCODE_TOML: &str = include_str!("../packs/opencode/pack.toml");
const GUIDE_TOML: &str = include_str!("../packs/guide/pack.toml");
const C4_TOML: &str = include_str!("../packs/c4/pack.toml");
const GDPR_TOML: &str = include_str!("../packs/gdpr/pack.toml");
const HIPAA_TOML: &str = include_str!("../packs/hipaa/pack.toml");
const SOC2_TOML: &str = include_str!("../packs/soc2/pack.toml");
const ISO27001_TOML: &str = include_str!("../packs/iso-27001/pack.toml");
const NIST80053_TOML: &str = include_str!("../packs/nist-800-53/pack.toml");

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

impl Pack {
    /// The pack's declared dependencies (ADR-068 § PKD-001), parsed from
    /// the `# depends: <name>[, <name>…]` comment in `pack.toml`. A method,
    /// not a stored field: it is read only during `pack add` resolution, so
    /// parsing on demand avoids threading a field through every Pack
    /// constructor. Empty when no `# depends:` line is present (a base pack).
    pub fn depends(&self) -> Vec<String> {
        depends_of(&self.toml_text)
    }
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

/// A paid (non-built-in) pack the public binary *advertises* but does not
/// ship (ADR-045 § ENT-001). It carries storefront metadata only — name,
/// the namespaces it would define, a one-line summary, and an availability
/// note — never the pack's heading/rule content. That content lives in the
/// private repo and is delivered through the licensed channel; embedding it
/// here would both give the paid bytes away and break the open-core
/// boundary. This is a listing, not a `Pack`: it is not discoverable, not
/// applicable, and never appears in `discover`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaidPack {
    /// Pack name, e.g. `arc42`.
    pub name: String,
    /// Namespaces the pack defines once installed, e.g. `["ARC42"]`.
    pub namespaces: Vec<String>,
    /// Hand-authored one-line summary. Deliberately not the pack's own
    /// `# summary:` — reading that would require embedding the pack.
    pub summary: String,
    /// Availability note shown in place of an install command while the
    /// licensed distribution channel (ADR-045 § ENT-003/ENT-005) is unbuilt.
    pub status: String,
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

/// The built-in packs (PACK-009). Seventeen packs: `project-docs`, `ops`,
/// `agents`, `design`, `persona`, `security`, `claude`, `workflow`, `codex`,
/// `gemini`, `opencode`, `guide`, `c4`, `gdpr`, `hipaa`, `soc2`, `iso-27001`,
/// and `nist-800-53` (ADR-023 § PKC-001, ADR-027, ADR-034, ADR-035, ADR-041,
/// ADR-050, ADR-051, ADR-055, ADR-066, ADR-069, ADR-070, ADR-071, ADR-075).
/// ADR-051 split the old catch-all `agents` pack into the AGENTS.md-only
/// `agents` pack, the harness-neutral `workflow` pack (SPEC/TASK/PROMPT), and
/// the per-harness packs. ADR-055 added `guide` (Diátaxis-typed end-user
/// documentation) plus a [README] namespace on `project-docs`. ADR-075 added
/// `c4` (architecture-diagram docs typed by the C4 model level, path-claimed at
/// docs/diagrams/**, one builtin `c4.frontmatter` rule). ADR-066 added
/// `gdpr` (ROPA/DPIA/DPA statutory document namespaces over the security base,
/// generated from a canonical regulation extract) and `hipaa` (the Security
/// Rule safeguard register SAFEGUARD plus the BAA register, same generated
/// shape). ADR-069 added `soc2` (the SOC2 control-to-evidence register over
/// the Trust Services Criteria — the thinnest family member, no statutory
/// namespace of its own, reusing the hipaa.safeguard-evidence machinery).
/// ADR-070 added `iso-27001` (the ISO27001 register over the ISO/IEC
/// 27001:2022 Annex A catalog) and ADR-071 added `nist-800-53` (the NIST80053
/// register over the SP 800-53 Rev 5 family catalog) as further register-only
/// family members, each with its own conditional-evidence rule delegating to
/// the shared evidence_gap core.
/// ADR-052's `machine-learning` pack was reverted in 0.31.0 (see its
/// Change log).
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
        Pack {
            // THREAT/VULN/RISK/SECREV/DEPAUDIT lifecycle namespaces plus
            // POLICY/ASSET governance — framework-neutral security
            // evidence discipline, no certification intent (ADR-041).
            // The three security.* rules are builtin-compiled, so `rules`
            // is empty (no external `run` scripts shipped).
            name: "security".to_string(),
            summary: summary_of(SECURITY_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: SECURITY_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // CLAUDEAGENTS namespace — path-claimed on .claude/agents/*.md (Claude
            // Code subagent definitions). Lints via the builtin-compiled
            // `agent.frontmatter` rule, so `rules` is empty. First of the
            // per-provider agent packs (ADR-050); the full carve of CLAUDE.md /
            // .claude skills out of `agents`, and opencode/gemini/codex packs,
            // are deferred. AGENTS.md (the agents.md standard) stays shared in
            // `agents`.
            name: "claude".to_string(),
            summary: summary_of(CLAUDE_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: CLAUDE_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // SPEC/TASK/PROMPT — ctxgrd's harness-neutral agent-development doc
            // pipeline, split out of the old `agents` pack by ADR-051 so the
            // neutral docs no longer ride a harness-named pack.
            name: "workflow".to_string(),
            summary: summary_of(WORKFLOW_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: WORKFLOW_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // CODEXSKILLS — OpenAI Codex SKILL.md definitions. Codex's
            // instruction file is AGENTS.md (the `agents` pack); it has no
            // proprietary agent-definition format (ADR-051).
            name: "codex".to_string(),
            summary: summary_of(CODEX_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: CODEX_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // GEMINI — the GEMINI.md instruction file. Gemini's custom commands
            // are TOML (out of ctxgrd's markdown scope), so the pack carries
            // only the instruction file (ADR-051).
            name: "gemini".to_string(),
            summary: summary_of(GEMINI_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: GEMINI_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // OPENCODEAGENTS — opencode agent definitions (.opencode/agent/*.md),
            // linted by the builtin-compiled `opencode.frontmatter` rule
            // (no `name` field; name is the filename). ADR-051.
            name: "opencode".to_string(),
            summary: summary_of(OPENCODE_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: OPENCODE_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // GUIDE namespace — path-claimed on docs/guides/**. End-user
            // documentation typed by the Diátaxis taxonomy (tutorial/how-to/
            // reference/explanation). Every guardrail is a core.* rule, so
            // `rules` is empty (no external scripts). Adds a [README] namespace
            // to project-docs so the front door links the entry guide (ADR-055).
            name: "guide".to_string(),
            summary: summary_of(GUIDE_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: GUIDE_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // C4 namespace — path-claimed on docs/diagrams/**. Architecture
            // diagrams typed by Simon Brown's C4 model (context/container/
            // component/code + supplementary deployment/dynamic/landscape).
            // id-less: the filename is the slug. The only guardrail is the
            // builtin-compiled `c4.frontmatter` rule (title + a valid c4.level),
            // so `rules` (external scripts) is empty. ctxgrd lints the markdown
            // envelope only; the diagram stays a ```mermaid```/```dot``` block
            // inside the same .md file — no `.mmd` source support (ADR-075).
            name: "c4".to_string(),
            summary: summary_of(C4_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: C4_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // ROPA/DPIA/DPA — the GDPR documentary spine (Regulation (EU)
            // 2016/679 Arts. 30/35/28), path-claimed under
            // docs/compliance/gdpr/. A thin pack on `security` (POLICY/RISK/VULN
            // reused from there): adds only statutory document namespaces, no
            // cross-edges. Every guardrail is a core.* rule, so `rules` is empty.
            // pack.toml is generated from packs/gdpr/regulation.json, not
            // hand-authored (ADR-066 § CMP-005).
            name: "gdpr".to_string(),
            summary: summary_of(GDPR_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: GDPR_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // SR-MAP/BAA — the HIPAA Security Rule safeguard register and the
            // Business Associate Agreement register (45 CFR 164.308/310/312),
            // path-claimed under docs/compliance/hipaa/. A thin pack on
            // `security` (POLICY/RISK/VULN reused from there): adds only the
            // statutory document namespaces, no cross-edges. Every guardrail is
            // a core.* rule, so `rules` is empty. pack.toml is generated from
            // packs/hipaa/regulation.json, not hand-authored (ADR-066 § CMP-005,
            // HIPAA-001/002/003).
            name: "hipaa".to_string(),
            summary: summary_of(HIPAA_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: HIPAA_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // SOC2 — the SOC 2 control-to-evidence register over the AICPA
            // Trust Services Criteria (2017, rev. 2022), path-claimed under
            // docs/compliance/soc2/. The thinnest compliance-family member: a
            // thin pack on `security` (POLICY/RISK/VULN reused from there) that
            // adds no statutory document namespace of its own — only the SOC2
            // register over the TSC catalog. soc2.control-evidence reuses the
            // hipaa.safeguard-evidence conditional machinery. Every guardrail
            // is a core.*/soc2.* rule, so `rules` is empty. pack.toml is
            // generated from packs/soc2/regulation.json, not hand-authored
            // (ADR-069 § SOC-004, SOC-001/002/003).
            name: "soc2".to_string(),
            summary: summary_of(SOC2_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: SOC2_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // ISO27001 — the ISO 27001 control-to-evidence register over the
            // ISO/IEC 27001:2022 Annex A catalog (93 controls across four
            // themes), path-claimed under docs/compliance/iso-27001/. A
            // register-only family member like soc2: a thin pack on `security`
            // (POLICY/RISK/VULN reused) that adds no statutory document
            // namespace of its own — only the ISO27001 register over the Annex
            // A catalog. iso27001.control-evidence reuses the shared
            // conditional-evidence core. Every guardrail is a core.*/iso27001.*
            // rule, so `rules` is empty. pack.toml is generated from
            // packs/iso-27001/regulation.json, not hand-authored (ADR-070 §
            // ISO-004, ISO-001/002/003).
            name: "iso-27001".to_string(),
            summary: summary_of(ISO27001_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: ISO27001_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // NIST80053 — the NIST 800-53 control-to-evidence register over the
            // SP 800-53 Rev 5 family catalog (20 families, family-level grain),
            // path-claimed under docs/compliance/nist-800-53/. A register-only
            // family member like soc2/iso-27001: a thin pack on `security`
            // (POLICY/RISK/VULN reused) that adds no statutory document
            // namespace of its own — only the NIST80053 register. The one
            // divergence is a single closed vocabulary (the family catalog) with
            // no category alongside it, because families are the top-level
            // grouping. nist.control-evidence reuses the shared
            // conditional-evidence core. Every guardrail is a core.*/nist.* rule,
            // so `rules` is empty. pack.toml is generated from
            // packs/nist-800-53/regulation.json, not hand-authored (ADR-071 §
            // NIST-004, NIST-001/002/003).
            name: "nist-800-53".to_string(),
            summary: summary_of(NIST80053_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: NIST80053_TOML.to_string(),
            rules: Vec::new(),
        },
    ]
}

/// The paid-pack storefront the public binary advertises (ADR-045 § ENT-001).
///
/// arc42 (ADR-049) is the first entry. These packs are deliberately absent
/// from `builtin_packs()` and ship no content in the MIT binary; this
/// catalog exists only so `pack list --paid` and the marketing site can
/// name them. The `status` line is verb-neutral because the install path
/// (ADR-045 § ENT-005) is not yet built — no command is implied.
pub fn paid_packs() -> Vec<PaidPack> {
    vec![PaidPack {
        name: "arc42".to_string(),
        namespaces: vec!["ARC42".to_string()],
        summary:
            "arc42 architecture documentation — the 12 canonical sections as required headings."
                .to_string(),
        // ASCII only: STATUS is a padded (non-last) column, and the table's
        // width math counts bytes while formatting pads by chars — a multibyte
        // glyph here would misalign the row (the em-dash summary is safe only
        // because it is the unpadded last column).
        status: "commercial license, coming soon".to_string(),
    }]
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
            .push_str(&format!("\n{}\n", provenance_comment(&pack.name, &block)));
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

/// The pack names listed in the `# depends: a, b` comment of a `pack.toml`
/// (ADR-068 § PKD-001), in declared order. Empty when absent. Parsed like
/// `summary_of` — a comment, never a TOML key, so it cannot be copied into
/// a consumer's `ctxgrd.toml` by the namespace-block segmenter.
fn depends_of(toml: &str) -> Vec<String> {
    toml.lines()
        .find_map(|l| l.trim_start().strip_prefix("# depends:"))
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Why a pack's dependency closure could not be resolved (ADR-068 § PKD-003).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyError {
    /// A `depends` edge points at a pack that is not discoverable.
    Missing { pack: String, missing: String },
    /// A `depends` edge points at a pack that itself declares a `depends`
    /// — a non-base target (no compliance-on-compliance, no intermediate
    /// layer; ADR-066 § CMP-002).
    NonBaseTarget { pack: String, target: String },
    /// The `depends` graph contains a cycle; `path` is the offending chain.
    Cycle { path: Vec<String> },
}

impl std::fmt::Display for DependencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DependencyError::Missing { pack, missing } => write!(
                f,
                "pack '{pack}' depends on '{missing}', which is not a discoverable pack"
            ),
            DependencyError::NonBaseTarget { pack, target } => write!(
                f,
                "pack '{pack}' depends on '{target}', which itself declares a dependency — \
                 a dependency target must be a base pack (ADR-068 § PKD-003)"
            ),
            DependencyError::Cycle { path } => {
                write!(f, "pack dependency cycle: {}", path.join(" -> "))
            }
        }
    }
}

/// Resolve `pack`'s transitive dependency closure into apply order —
/// dependencies first, `pack` last, each pack once (ADR-068 § PKD-002).
/// Enforces the DAG-with-base-targets invariant (PKD-003): a cycle, an
/// undiscoverable dependency, or a dependency on a non-base pack is an error.
pub fn resolve_dependencies(root: &Path, pack: &Pack) -> Result<Vec<Pack>, DependencyError> {
    let mut order: Vec<Pack> = Vec::new();
    let mut done: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    visit_dependencies(root, pack.clone(), &mut order, &mut done, &mut stack)?;
    Ok(order)
}

fn visit_dependencies(
    root: &Path,
    pack: Pack,
    order: &mut Vec<Pack>,
    done: &mut std::collections::BTreeSet<String>,
    stack: &mut Vec<String>,
) -> Result<(), DependencyError> {
    if done.contains(&pack.name) {
        return Ok(());
    }
    if stack.contains(&pack.name) {
        let mut path = stack.clone();
        path.push(pack.name.clone());
        return Err(DependencyError::Cycle { path });
    }
    stack.push(pack.name.clone());
    for dep_name in pack.depends() {
        let Some(dep) = find(root, &dep_name) else {
            return Err(DependencyError::Missing {
                pack: pack.name.clone(),
                missing: dep_name,
            });
        };
        if !dep.depends().is_empty() {
            return Err(DependencyError::NonBaseTarget {
                pack: pack.name.clone(),
                target: dep_name,
            });
        }
        visit_dependencies(root, dep, order, done, stack)?;
    }
    stack.pop();
    done.insert(pack.name.clone());
    order.push(pack);
    Ok(())
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

// -- provenance (ADR-053 § PKM-001) -------------------------------------

/// The parsed contents of a `# pack: ...` provenance comment.
///
/// Three input forms are accepted (PKM-001), in increasing richness:
/// - `# pack: claude` — the original ADR-013 free-form label (no
///   version, no fingerprint). `version`/`sha` are `None`.
/// - `# pack: claude@0.35.0` — pack name plus the binary version that
///   stamped the block. `sha` is `None`.
/// - `# pack: claude@0.35.0 sha:ab0123...` — the full v2 form carrying
///   both the version label and the block content fingerprint.
///
/// The suffix is an inert comment to any binary that does not parse it,
/// so older binaries keep linting a v2 config unchanged (PKM-006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The pack name the block was generated from.
    pub pack: String,
    /// The binary version that stamped the block (provenance label
    /// only — never used for resolution). `None` for the bare form.
    pub version: Option<String>,
    /// The content fingerprint of the block as generated (PKM-001).
    /// `None` for the bare and `@version`-only forms.
    pub sha: Option<String>,
}

/// Parse a `# pack: <name>[@<version>[ sha:<hash>]]` provenance line.
/// Returns `None` for any line that is not a provenance comment.
/// Surrounding whitespace is tolerated.
pub fn parse_provenance(line: &str) -> Option<Provenance> {
    let rest = line.trim().strip_prefix("# pack:")?.trim();
    if rest.is_empty() {
        return None;
    }
    // The first whitespace-delimited token is `<name>` or `<name>@<version>`;
    // a following `sha:<hash>` token, if present, carries the fingerprint.
    let mut tokens = rest.split_whitespace();
    let name_token = tokens.next()?;
    let (pack, version) = match name_token.split_once('@') {
        Some((name, ver)) if !name.is_empty() && !ver.is_empty() => {
            (name.to_string(), Some(ver.to_string()))
        }
        Some(_) => return None, // malformed `@` token
        None => (name_token.to_string(), None),
    };
    let sha = tokens
        .next()
        .and_then(|t| t.strip_prefix("sha:"))
        .filter(|h| !h.is_empty())
        .map(str::to_string);
    Some(Provenance { pack, version, sha })
}

/// The v2 provenance comment line for a freshly generated block:
/// `# pack: <name>@<version> sha:<fingerprint(block)>`. The version is
/// the binary's own version, stamped as a provenance label (PKM-001).
fn provenance_comment(pack: &str, block: &str) -> String {
    format!(
        "# pack: {pack}@{version} sha:{sha}",
        version = env!("CARGO_PKG_VERSION"),
        sha = fingerprint(block),
    )
}

/// A stable, NON-cryptographic content digest of a namespace block
/// (ADR-053 § PKM-001). The threat model is accidental edits (a user or
/// formatter touching a generated block), not adversarial collisions —
/// the `sha:` token is the ADR-fixed grammar label, not a claim of
/// SHA-family hashing.
///
/// The block is normalized first so that line-ending and trailing-
/// whitespace churn does not flip the digest: each line is stripped of
/// trailing whitespace, the lines are rejoined with `\n`, and trailing
/// blank lines / the trailing newline are removed. The result is hashed
/// with FNV-1a (64-bit) and formatted as 16-char zero-padded lowercase
/// hex. The fingerprint is computed over the verbatim block text (the
/// `[NS] ...` lines), never including the provenance comment line.
pub fn fingerprint(block: &str) -> String {
    let normalized = block
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    let normalized = normalized.trim_end_matches(['\n', ' ', '\t']);

    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in normalized.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

// -- migration recipes (ADR-053 § PKM-002/006/008) ----------------------

/// A compiled-in pointer describing how an old namespace block maps to
/// the pack's current shape (ADR-053 § PKM-002). Recipes are minimal
/// rename/move pointers, never block internals — the clean swap re-renders
/// from the current pack definition, and a hand-edited block is surfaced
/// as a diff rather than transformed (PKM-003).
struct MigrationRecipe {
    /// Provenance pack name the source block belongs to.
    pack: &'static str,
    /// Old namespace name as it appears on disk.
    from_ns: &'static str,
    /// New namespace name(s) the current packs render. One entry is a
    /// rename; more than one is a split.
    to_ns: &'static [&'static str],
    /// For 1->N splits only: fingerprints of known-clean historical
    /// renderings of the old block (a split cannot be clean-detected by
    /// reverse substitution because the shape changes). Empty for pure
    /// 1->1 renames, which are clean-detected by reverse substitution.
    clean_fingerprints: &'static [&'static str],
}

/// The compiled-in migration recipes (ADR-053 § PKM-008).
///
/// - ADR-061 renames: `CLAUDECODE`→`CLAUDEAGENTS` (pack `claude`),
///   `OPENCODE`→`OPENCODEAGENTS` (pack `opencode`).
/// - ADR-051 splits: the old catch-all `agents` pack's `AGENTS` block
///   (`CLAUDE.md`/`AGENTS.md`/`GEMINI.md`) splits into `CLAUDE` +
///   `GEMINI` + `AGENTS`; its `SKILLS` block splits into `CLAUDESKILLS`
///   + `CODEXSKILLS`.
///
/// The split `clean_fingerprints` are the digests of the pre-split
/// canonical block text (recovered from the parent of the split commit,
/// 680b40c); a test re-derives them so they cannot rot.
fn migration_recipes() -> &'static [MigrationRecipe] {
    &[
        MigrationRecipe {
            pack: "claude",
            from_ns: "CLAUDECODE",
            to_ns: &["CLAUDEAGENTS"],
            clean_fingerprints: &[],
        },
        MigrationRecipe {
            pack: "opencode",
            from_ns: "OPENCODE",
            to_ns: &["OPENCODEAGENTS"],
            clean_fingerprints: &[],
        },
        MigrationRecipe {
            pack: "agents",
            from_ns: "AGENTS",
            to_ns: &["CLAUDE", "GEMINI", "AGENTS"],
            // fingerprint of the pre-split [AGENTS] block (680b40c).
            clean_fingerprints: &["7800619f29dbf307"],
        },
        MigrationRecipe {
            pack: "agents",
            from_ns: "SKILLS",
            to_ns: &["CLAUDESKILLS", "CODEXSKILLS"],
            // fingerprint of the pre-split [SKILLS] block (680b40c).
            clean_fingerprints: &["f66294963c421057"],
        },
    ]
}

/// Replace the namespace identifier `from` with `to` wherever it appears
/// as a TOML table key — the `[from]` and `[from.` forms — including in
/// the commented-out example sub-table headers a pack block carries
/// (`# [from."core.x"]`). A pure rename renames every textual occurrence
/// of the key, comments included, so reverse-substituting an untouched
/// old block must reproduce the current canonical block byte-for-byte;
/// gating to header-only lines would leave the comment references stale
/// and mis-classify a clean block as hand-edited. The `]`/`.` delimiters
/// keep the match prefix-safe — `[OPENCODE]`/`[OPENCODE.` never touch an
/// `[OPENCODEAGENTS]` token. Values would only collide if a path glob
/// literally embedded `[<NS>]`, which no namespace claim does.
fn substitute_namespace_name(block: &str, from: &str, to: &str) -> String {
    block
        .replace(&format!("[{from}]"), &format!("[{to}]"))
        .replace(&format!("[{from}."), &format!("[{to}."))
}

// -- config segmentation for migrate ------------------------------------

/// One segment of a `ctxgrd.toml`, paired with its preceding `# pack:`
/// provenance comment (if any). A block runs from its `[NS]` header
/// through its nested sub-tables, up to the next top-level namespace
/// header or the next provenance comment. Non-namespace text (preamble,
/// `[ignore]`, blank runs) is carried as `Other` so the file can be
/// rewritten in place without losing bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigSegment {
    /// A namespace block with its provenance line, if it carried one.
    Namespace {
        ns: String,
        /// The verbatim `# pack: ...` line (no trailing newline), if any.
        provenance: Option<String>,
        /// The verbatim block text (`[NS] ...` lines), trailing-trimmed.
        block: String,
    },
    /// Verbatim text not part of a namespace block (preamble, blank
    /// lines, `[ignore]`/`[sources.*]` tables, etc.).
    Other(String),
}

/// Segment a full `ctxgrd.toml` into ordered [`ConfigSegment`]s,
/// preserving every byte across `Namespace`/`Other` segments. A
/// provenance comment immediately preceding a namespace header is paired
/// with that block; an orphan provenance comment (not followed by a
/// header) stays in `Other`.
fn config_segments(toml: &str) -> Vec<ConfigSegment> {
    let lines: Vec<&str> = toml.lines().collect();
    let mut segments: Vec<ConfigSegment> = Vec::new();
    let mut other = String::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(ns) = header_namespace(line) {
            // A `# pack:` comment on the immediately preceding line (with
            // no blank line between) is this block's provenance.
            let provenance = match segments_pending_provenance(&other) {
                Some((before, prov)) => {
                    other = before;
                    Some(prov)
                }
                None => None,
            };
            if !other.is_empty() {
                segments.push(ConfigSegment::Other(std::mem::take(&mut other)));
            }
            // Collect the block: this header through its nested sub-tables
            // (`[NS."core.x"]`, same namespace) up to the next *different*
            // namespace header or the next provenance comment. A same-namespace
            // sub-table stays in the block — mirrors `namespace_blocks`, which
            // only starts a new block when the namespace changes.
            //
            // Interior blank lines (between sub-tables) belong to the block;
            // *trailing* blank lines are the separator to the next block, so
            // they are held back into `other` rather than eaten — otherwise
            // re-emitting a block would collapse the blank line that separated
            // it from its neighbour, churning the whole file.
            let mut block = String::new();
            block.push_str(line);
            block.push('\n');
            i += 1;
            let mut pending_blanks = String::new();
            while i < lines.len() {
                let l = lines[i];
                if let Some(h) = header_namespace(l) {
                    if h != ns {
                        break;
                    }
                } else if parse_provenance(l).is_some() {
                    break;
                }
                if l.trim().is_empty() {
                    // Hold blanks: flushed into the block only if a content
                    // line follows (interior), else left trailing.
                    pending_blanks.push_str(l);
                    pending_blanks.push('\n');
                } else {
                    block.push_str(&pending_blanks);
                    pending_blanks.clear();
                    block.push_str(l);
                    block.push('\n');
                }
                i += 1;
            }
            segments.push(ConfigSegment::Namespace {
                ns,
                provenance,
                block: block.trim_end().to_string(),
            });
            // Trailing blanks separate this block from the next — re-queue
            // them so the next segment (or its provenance) keeps its spacing.
            other.push_str(&pending_blanks);
            continue;
        }
        other.push_str(line);
        other.push('\n');
        i += 1;
    }
    if !other.is_empty() {
        segments.push(ConfigSegment::Other(other));
    }
    segments
}

/// If the tail of `other` is a lone `# pack:` provenance comment (the
/// provenance for the namespace block that follows), split it off:
/// returns `(other_without_provenance, provenance_line)`.
fn segments_pending_provenance(other: &str) -> Option<(String, String)> {
    // Find the last non-empty line; it must be a provenance comment, and
    // every line after it must be blank for it to bind to the next block.
    let trimmed_end = other.trim_end_matches('\n');
    let last_line = trimmed_end.lines().next_back()?;
    parse_provenance(last_line)?;
    // Only bind when there is no blank line between the comment and the
    // header (i.e. the provenance is the very last line before the header).
    if other.trim_end().ends_with(last_line) {
        let before_len = trimmed_end.len() - last_line.len();
        let before = trimmed_end[..before_len].to_string();
        Some((before, last_line.to_string()))
    } else {
        None
    }
}

// -- migrate engine (ADR-053 § PKM-002/003/004) -------------------------

/// One clean block rewrite the migrate applied.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BlockRewrite {
    /// The on-disk namespace that was rewritten.
    pub from_ns: String,
    /// The namespace(s) it was rewritten to (>1 for a split).
    pub to_ns: Vec<String>,
    /// The provenance pack the block belongs to.
    pub pack: String,
}

/// One dirty (hand-edited) block migrate left untouched, with the diff a
/// human/agent resolves (ADR-053 § PKM-003).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BlockDiff {
    /// The on-disk namespace.
    pub namespace: String,
    /// The provenance pack.
    pub pack: String,
    /// `"rename"`, `"split"`, or `"internals"`.
    pub kind: String,
    /// The block as it sits on disk (left intact).
    pub on_disk: String,
    /// The canonical block(s) migrate would produce if the block were
    /// clean.
    pub proposed: Vec<String>,
}

/// The result of planning a `pack migrate` (ADR-053 § PKM-002/004).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct MigratePlan {
    /// Clean blocks that were (or would be) rewritten.
    pub rewrites: Vec<BlockRewrite>,
    /// Dirty blocks left intact, surfaced as diffs.
    pub diffs: Vec<BlockDiff>,
    /// Blocks already at their current shape (nothing to do).
    pub noop_count: usize,
    /// The full migrated config text (clean rewrites applied; dirty and
    /// no-op blocks left as-is). Omitted from the JSON wire — the
    /// structured `rewrites`/`diffs` are the contract; the migrated text
    /// is written to disk, not streamed.
    #[serde(skip)]
    pub new_config: String,
}

/// Find the current canonical block text for namespace `ns` among the
/// discovered packs (a namespace may have moved packs, so search all).
/// Returns `(owning_pack_name, canonical_block_text)`.
fn canonical_block(packs: &[Pack], ns: &str) -> Option<(String, String)> {
    for pack in packs {
        if let Some((_, block)) = namespace_blocks(&pack.toml_text)
            .into_iter()
            .find(|(name, _)| name == ns)
        {
            return Some((pack.name.clone(), block));
        }
    }
    None
}

/// Plan a `pack migrate` from the config text and the discovered packs
/// (ADR-053 § PKM-002). Filesystem-free w.r.t. the working tree: `root`
/// is used only to discover packs (the same discovery `find` performs),
/// never to inspect the documents being linted.
pub fn plan_migrate(config_toml: &str, root: &Path) -> MigratePlan {
    let packs = discover(root);
    let mut plan = MigratePlan::default();
    let mut new_config = String::new();

    for segment in config_segments(config_toml) {
        match segment {
            ConfigSegment::Other(text) => new_config.push_str(&text),
            ConfigSegment::Namespace {
                ns,
                provenance,
                block,
            } => {
                let prov = provenance.as_deref().and_then(parse_provenance);
                let pack_name = prov
                    .as_ref()
                    .map(|p| p.pack.clone())
                    .unwrap_or_else(|| owning_pack_name(&packs, &ns));
                let stored_sha = prov.as_ref().and_then(|p| p.sha.clone());

                let recipe = migration_recipes()
                    .iter()
                    .find(|r| r.pack == pack_name && r.from_ns == ns);

                let outcome = classify_block(&packs, &ns, &pack_name, &block, stored_sha, recipe);
                apply_outcome(
                    &mut plan,
                    &mut new_config,
                    &ns,
                    &pack_name,
                    provenance.as_deref(),
                    &block,
                    outcome,
                );
            }
        }
    }

    plan.new_config = new_config;
    plan
}

/// The pack that currently owns namespace `ns` by discovery, or the
/// namespace name itself when no pack defines it (a local/unknown
/// namespace migrate leaves alone).
fn owning_pack_name(packs: &[Pack], ns: &str) -> String {
    canonical_block(packs, ns)
        .map(|(pack, _)| pack)
        .unwrap_or_else(|| ns.to_string())
}

/// The decision for one block, before it is rendered into the plan.
enum BlockOutcome {
    /// Already current — emit verbatim, count as a no-op.
    NoOp,
    /// Clean — replace with the listed `(pack, canonical_block)` targets,
    /// each freshly provenance-stamped. `to_ns` records the target names.
    Rewrite {
        to_ns: Vec<String>,
        targets: Vec<(String, String)>,
    },
    /// Dirty — leave intact, surface a diff.
    Diff { kind: String, proposed: Vec<String> },
    /// No recipe and no canonical definition (unknown/local namespace) —
    /// leave verbatim, do not count as anything.
    Untouched,
}

/// Decide what happens to one provenance block (ADR-053 § PKM-002/003).
fn classify_block(
    packs: &[Pack],
    ns: &str,
    pack_name: &str,
    block: &str,
    stored_sha: Option<String>,
    recipe: Option<&MigrationRecipe>,
) -> BlockOutcome {
    match recipe {
        Some(r) if r.clean_fingerprints.is_empty() => {
            // 1->1 rename: clean iff reverse-substitution matches canonical.
            let new = r.to_ns[0];
            let Some((owner, canon)) = canonical_block(packs, new) else {
                return BlockOutcome::Untouched;
            };
            if substitute_namespace_name(block, ns, new) == canon {
                BlockOutcome::Rewrite {
                    to_ns: vec![new.to_string()],
                    targets: vec![(owner, canon)],
                }
            } else {
                BlockOutcome::Diff {
                    kind: "rename".to_string(),
                    proposed: vec![canon],
                }
            }
        }
        Some(r) => {
            // 1->N split. A split's source name can also be one of its
            // targets (AGENTS -> CLAUDE+GEMINI+AGENTS), so the recipe keeps
            // matching the *already-split* block. If this namespace is itself
            // a target and the on-disk block already equals its current
            // canonical, the split has happened — nothing to do (else
            // `outdated` would nag forever after a successful migrate).
            if r.to_ns.contains(&ns) {
                if let Some((_, canon)) = canonical_block(packs, ns) {
                    if block == canon {
                        return BlockOutcome::NoOp;
                    }
                }
            }
            // Otherwise: clean iff the block matches a known-clean historical
            // (pre-split) fingerprint.
            let fp = fingerprint(block);
            let mut targets: Vec<(String, String)> = Vec::new();
            for target in r.to_ns {
                match canonical_block(packs, target) {
                    Some(t) => targets.push(t),
                    None => return BlockOutcome::Untouched,
                }
            }
            if r.clean_fingerprints.contains(&fp.as_str()) {
                BlockOutcome::Rewrite {
                    to_ns: r.to_ns.iter().map(|s| s.to_string()).collect(),
                    targets,
                }
            } else {
                BlockOutcome::Diff {
                    kind: "split".to_string(),
                    proposed: targets.into_iter().map(|(_, b)| b).collect(),
                }
            }
        }
        None => {
            // Identity: same namespace, possibly relabel the pack provenance
            // (SPEC/TASK/PROMPT agents->workflow, PKM-008) on a clean swap.
            let Some((owner, canon)) = canonical_block(packs, ns) else {
                return BlockOutcome::Untouched;
            };
            let provenance_current = pack_name == owner;
            match stored_sha {
                Some(sha) if sha == fingerprint(block) => {
                    if block == canon && provenance_current {
                        BlockOutcome::NoOp
                    } else {
                        // Clean (sha matches) — restamp to canonical + current
                        // owning pack. Covers the agents->workflow relabel.
                        BlockOutcome::Rewrite {
                            to_ns: vec![ns.to_string()],
                            targets: vec![(owner, canon)],
                        }
                    }
                }
                Some(_) => BlockOutcome::Diff {
                    kind: "internals".to_string(),
                    proposed: vec![canon],
                },
                None => {
                    // Bare provenance (no fingerprint): only safe to treat as
                    // current when the block already equals canonical AND the
                    // provenance pack is current. Otherwise conservative-dirty.
                    if block == canon && provenance_current {
                        BlockOutcome::NoOp
                    } else if block == canon {
                        // Block is canonical but provenance pack is stale
                        // (agents->workflow relabel for SPEC/TASK/PROMPT).
                        BlockOutcome::Rewrite {
                            to_ns: vec![ns.to_string()],
                            targets: vec![(owner, canon)],
                        }
                    } else {
                        BlockOutcome::Diff {
                            kind: "internals".to_string(),
                            proposed: vec![canon],
                        }
                    }
                }
            }
        }
    }
}

/// Render one block's outcome into the running plan and config text.
/// `provenance` is the block's original provenance line (verbatim), which
/// is preserved untouched for every non-rewrite outcome so migrate never
/// churns a block it is not actually changing.
fn apply_outcome(
    plan: &mut MigratePlan,
    new_config: &mut String,
    ns: &str,
    pack_name: &str,
    provenance: Option<&str>,
    block: &str,
    outcome: BlockOutcome,
) {
    // Re-emit a block (and its original provenance, if any) verbatim.
    let emit_verbatim = |out: &mut String| {
        if let Some(prov) = provenance {
            out.push_str(prov);
            out.push('\n');
        }
        out.push_str(block);
        out.push('\n');
    };
    match outcome {
        BlockOutcome::NoOp => {
            plan.noop_count += 1;
            emit_verbatim(new_config);
        }
        BlockOutcome::Rewrite { to_ns, targets } => {
            plan.rewrites.push(BlockRewrite {
                from_ns: ns.to_string(),
                to_ns,
                pack: pack_name.to_string(),
            });
            for (i, (owner, canon)) in targets.iter().enumerate() {
                if i > 0 {
                    new_config.push('\n');
                }
                new_config.push_str(&provenance_comment(owner, canon));
                new_config.push('\n');
                new_config.push_str(canon);
                new_config.push('\n');
            }
        }
        BlockOutcome::Diff { kind, proposed } => {
            plan.diffs.push(BlockDiff {
                namespace: ns.to_string(),
                pack: pack_name.to_string(),
                kind,
                on_disk: block.to_string(),
                proposed,
            });
            // Leave the dirty block (and its provenance) intact.
            emit_verbatim(new_config);
        }
        BlockOutcome::Untouched => emit_verbatim(new_config),
    }
}

/// Apply a `pack migrate` to `root`: rewrite `ctxgrd.toml` in place with
/// the migrated config (clean swaps applied, dirty blocks left intact).
/// Returns the plan. When `dry_run` is set, nothing is written.
pub fn apply_migrate(root: &Path, dry_run: bool) -> io::Result<MigratePlan> {
    let toml_path = root.join("ctxgrd.toml");
    let config_toml = fs::read_to_string(&toml_path)?;
    let plan = plan_migrate(&config_toml, root);
    if !dry_run && !plan.rewrites.is_empty() {
        fs::write(&toml_path, &plan.new_config)?;
    }
    Ok(plan)
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

/// Render the paid-pack storefront for `ctxgrd pack list --paid`
/// (ADR-045 § ENT-001). Mirrors `render_list`'s column layout; a STATUS
/// column stands in for an install command while the licensed distribution
/// channel (ADR-045 § ENT-005) is unbuilt, so no command is implied.
pub fn render_paid_list(packs: &[PaidPack]) -> String {
    let rows: Vec<[String; 4]> = packs
        .iter()
        .map(|p| {
            [
                p.name.clone(),
                p.namespaces.join(", "),
                p.status.clone(),
                p.summary.clone(),
            ]
        })
        .collect();
    let headers = ["NAME", "NAMESPACES", "STATUS", "SUMMARY"];
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

/// Human-readable migrate/outdated report (ADR-053 § PKM-002/004).
/// `dry_run` shapes the verb: a `pack migrate --dry-run` and `pack
/// outdated` describe what *would* happen; a real `pack migrate`
/// describes what was done. Dirty blocks always show their diff for a
/// human/agent to resolve.
pub fn render_migrate_report(plan: &MigratePlan, dry_run: bool) -> String {
    let mut out = String::new();
    if plan.rewrites.is_empty() && plan.diffs.is_empty() {
        out.push_str("Nothing to migrate — every provenance block is at its pack's current shape.\n");
        return out;
    }

    if !plan.rewrites.is_empty() {
        let verb = if dry_run { "Would migrate" } else { "Migrated" };
        out.push_str(&format!("{verb} {} block(s):\n", plan.rewrites.len()));
        for r in &plan.rewrites {
            if r.to_ns.len() == 1 && r.to_ns[0] == r.from_ns {
                out.push_str(&format!("  • [{}] (pack {})\n", r.from_ns, r.pack));
            } else {
                out.push_str(&format!(
                    "  • [{}] → {} (pack {})\n",
                    r.from_ns,
                    r.to_ns
                        .iter()
                        .map(|n| format!("[{n}]"))
                        .collect::<Vec<_>>()
                        .join(" + "),
                    r.pack
                ));
            }
        }
    }

    if !plan.diffs.is_empty() {
        if !plan.rewrites.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "{} block(s) hand-edited — resolve manually (left untouched):\n",
            plan.diffs.len()
        ));
        for d in &plan.diffs {
            out.push_str(&format!(
                "\n  [{}] (pack {}, {} change)\n",
                d.namespace, d.pack, d.kind
            ));
            out.push_str("    on disk:\n");
            for line in d.on_disk.lines() {
                out.push_str(&format!("      {line}\n"));
            }
            out.push_str("    proposed:\n");
            for block in &d.proposed {
                for line in block.lines() {
                    out.push_str(&format!("      {line}\n"));
                }
            }
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
    fn paid_catalog_advertises_arc42() {
        let catalog = paid_packs();
        assert_eq!(catalog.len(), 1);
        let arc42 = &catalog[0];
        assert_eq!(arc42.name, "arc42");
        assert_eq!(arc42.namespaces, vec!["ARC42".to_string()]);
        assert_eq!(arc42.status, "commercial license, coming soon");
        assert!(
            arc42
                .summary
                .starts_with("arc42 architecture documentation"),
            "summary was: {}",
            arc42.summary
        );
    }

    #[test]
    fn paid_packs_are_not_built_in() {
        // ENT-001: a paid pack must ship no content in the MIT binary, so its
        // name must never resolve as a built-in (discoverable, applicable) pack.
        let builtin: Vec<String> = builtin_packs().into_iter().map(|p| p.name).collect();
        for p in paid_packs() {
            assert!(
                !builtin.contains(&p.name),
                "paid pack `{}` must not be built in",
                p.name
            );
        }
    }

    #[test]
    fn render_paid_list_shows_arc42_row_and_status() {
        let out = render_paid_list(&paid_packs());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "header + one paid pack");
        // Header column order. Padding widths are data-derived, so assert the
        // column names in order rather than exact whitespace.
        assert!(lines[0].starts_with("NAME   NAMESPACES  STATUS"));
        assert!(lines[0].ends_with("SUMMARY"));
        // Row: name and namespace adjacent, then the verb-neutral status, then
        // the summary as the unpadded final column.
        assert!(lines[1].starts_with("arc42  ARC42 "));
        assert!(lines[1].contains("commercial license, coming soon"));
        assert!(lines[1].ends_with(
            "arc42 architecture documentation — the 12 canonical sections as required headings."
        ));
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
        assert_eq!(names, vec!["ADR", "PRD", "RFC", "BUG", "TODO", "README"]);
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
    fn security_pack_defines_seven_namespaces() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "security")
            .unwrap();
        let views = namespace_views(&pack);
        let names: Vec<&str> = views.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["THREAT", "VULN", "RISK", "SECREV", "DEPAUDIT", "POLICY", "ASSET"]
        );

        // THREAT carries the commit pin (SEC-003) and the six STRIDE
        // categories as required headings.
        let threat = views.iter().find(|v| v.name == "THREAT").unwrap();
        assert!(threat.rules.contains(&"core.commit-freshness".to_string()));
        let stride = builtin_pack_headings_for(&pack, "THREAT").unwrap();
        assert_eq!(
            stride,
            vec![
                "Spoofing",
                "Tampering",
                "Repudiation",
                "Information Disclosure",
                "Denial of Service",
                "Elevation of Privilege"
            ]
        );

        // VULN wires the three security.* rules (SEC-004/005/006) and a
        // severity/status allowed-values vocabulary.
        let vuln = views.iter().find(|v| v.name == "VULN").unwrap();
        for rule in [
            "security.vuln-sla",
            "security.risk-expiry",
            "security.remediation-link",
        ] {
            assert!(
                vuln.rules.contains(&rule.to_string()),
                "VULN must wire {rule}"
            );
        }
        let allowed = pack
            .toml_text
            .parse::<Value>()
            .unwrap()
            .get("VULN")
            .and_then(|v| v.get("core.allowed-values"))
            .cloned()
            .unwrap();
        let severities: Vec<String> = allowed
            .get("severity")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        assert_eq!(
            severities,
            vec!["critical", "high", "medium", "low", "info"]
        );
        let statuses: Vec<String> = allowed
            .get("status")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        assert_eq!(
            statuses,
            vec!["open", "mitigated", "accepted", "false-positive"]
        );

        // ASSET requires a data classification (SEC-002).
        let asset = views.iter().find(|v| v.name == "ASSET").unwrap();
        assert!(asset
            .required_metadata
            .contains(&"data_classification".to_string()));
    }

    /// Read a `core.required-headings.headings` list directly from a
    /// pack's parsed TOML (the `builtin_pack_headings` helper only scans
    /// project-docs/ops, so it is not usable for the security pack here).
    fn builtin_pack_headings_for(pack: &Pack, namespace: &str) -> Option<Vec<String>> {
        Some(
            pack.toml_text
                .parse::<Value>()
                .ok()?
                .get(namespace)?
                .get("core.required-headings")?
                .get("headings")?
                .as_array()?
                .iter()
                .filter_map(|h| h.as_str().map(str::to_string))
                .collect(),
        )
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
                "TODO".to_string(),
                "README".to_string()
            ]
        );
        // Every added block carries a provenance comment.
        assert_eq!(plan.blocks_text.matches("# pack: project-docs").count(), 5);
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
        assert_eq!(plan.added.len(), 6);
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

    fn pack_namespace_names(name: &str) -> Vec<String> {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == name)
            .unwrap();
        namespace_views(&pack)
            .iter()
            .map(|v| v.name.clone())
            .collect()
    }

    #[test]
    fn agents_pack_owns_only_agents_md() {
        // ADR-051: after the split, the `agents` pack carries a single
        // path-claimed namespace AGENTS over AGENTS.md (the agents.md standard).
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "agents")
            .unwrap();
        let views = namespace_views(&pack);
        let names: Vec<&str> = views.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["AGENTS"]);
        let agents = views.iter().find(|v| v.name == "AGENTS").unwrap();
        assert_eq!(agents.path_patterns, vec!["AGENTS.md".to_string()]);
    }

    #[test]
    fn workflow_pack_holds_id_claimed_spec_task_prompt() {
        // ADR-051: SPEC/TASK/PROMPT moved out of `agents` into `workflow`.
        let names = pack_namespace_names("workflow");
        assert_eq!(names, vec!["SPEC", "TASK", "PROMPT"]);
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "workflow")
            .unwrap();
        for v in namespace_views(&pack) {
            assert!(
                v.path_patterns.is_empty(),
                "{} must be id-claimed (no paths)",
                v.name
            );
        }
    }

    #[test]
    fn claude_pack_lists_three_namespaces() {
        // ADR-051: the `claude` pack carries the Claude-proprietary files —
        // CLAUDE.md, .claude/skills, .claude/agents.
        let names = pack_namespace_names("claude");
        assert_eq!(names, vec!["CLAUDE", "CLAUDESKILLS", "CLAUDEAGENTS"]);
    }

    #[test]
    fn per_harness_packs_carry_expected_namespaces() {
        // ADR-051: codex/gemini/opencode per-harness packs.
        assert_eq!(pack_namespace_names("codex"), vec!["CODEXSKILLS"]);
        assert_eq!(pack_namespace_names("gemini"), vec!["GEMINI"]);
        assert_eq!(pack_namespace_names("opencode"), vec!["OPENCODEAGENTS"]);
    }

    #[test]
    fn providers_of_maps_rule_code_to_bundling_pack() {
        // ADR-025 § PKD-001 + ADR-051: a builtin-compiled rule is discoverable
        // by code. After the split `skills.frontmatter` is bundled by both the
        // `claude` and `codex` packs; `agent.frontmatter` only by `claude`;
        // `opencode.frontmatter` only by `opencode`.
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            providers_of(tmp.path(), "skills.frontmatter"),
            vec!["claude".to_string(), "codex".to_string()]
        );
        assert_eq!(
            providers_of(tmp.path(), "agent.frontmatter"),
            vec!["claude".to_string()]
        );
        assert_eq!(
            providers_of(tmp.path(), "opencode.frontmatter"),
            vec!["opencode".to_string()]
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
    fn gemini_pack_claims_gemini_md() {
        // ADR-051: GEMINI.md moved out of the agents pack into its own `gemini`
        // pack; the agents pack's AGENTS namespace now claims only AGENTS.md.
        let agents = builtin_packs()
            .into_iter()
            .find(|p| p.name == "agents")
            .unwrap();
        let agents_view = namespace_views(&agents)
            .into_iter()
            .find(|v| v.name == "AGENTS")
            .unwrap();
        assert_eq!(agents_view.path_patterns, vec!["AGENTS.md".to_string()]);

        let gemini = builtin_packs()
            .into_iter()
            .find(|p| p.name == "gemini")
            .unwrap();
        let gemini_view = namespace_views(&gemini)
            .into_iter()
            .find(|v| v.name == "GEMINI")
            .unwrap();
        assert!(gemini_view.path_patterns.contains(&"GEMINI.md".to_string()));
    }

    #[test]
    fn skill_md_claims_split_per_harness() {
        // ADR-051: SKILL.md claims split — .claude/skills → claude pack
        // (CLAUDESKILLS), .codex/skills → codex pack (CODEXSKILLS).
        let claude = builtin_packs()
            .into_iter()
            .find(|p| p.name == "claude")
            .unwrap();
        let cs = namespace_views(&claude)
            .into_iter()
            .find(|v| v.name == "CLAUDESKILLS")
            .unwrap();
        assert!(cs
            .path_patterns
            .iter()
            .any(|p| p.contains(".claude/skills")));

        let codex = builtin_packs()
            .into_iter()
            .find(|p| p.name == "codex")
            .unwrap();
        let xs = namespace_views(&codex)
            .into_iter()
            .find(|v| v.name == "CODEXSKILLS")
            .unwrap();
        assert!(xs.path_patterns.iter().any(|p| p.contains(".codex/skills")));
    }

    /// A synthetic pack with one path-claimed namespace (GUIDE, has `paths`)
    /// and one id-claimed namespace (NOTE, no `paths`) — exercises the receipt's
    /// two-section split without depending on a single builtin pack carrying
    /// both claim kinds (after ADR-051 none does).
    fn synthetic_split_pack() -> Pack {
        Pack {
            name: "synthetic".to_string(),
            summary: "synthetic".to_string(),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: "[GUIDE]\npaths = [\"GUIDE.md\"]\nrules = [\"agents.context-budget\"]\n\n\
                        [NOTE]\nrules = [\"core.frontmatter\"]\n"
                .to_string(),
            rules: Vec::new(),
        }
    }

    #[test]
    fn pack_add_receipt_splits_path_and_id_claims() {
        let pack = synthetic_split_pack();
        let plan = plan_add(&pack, "", Path::new("/nonexistent"));
        let receipt = render_add_receipt(&pack, &plan);
        assert!(
            receipt.contains("Linting now"),
            "has path-claim section:\n{receipt}"
        );
        assert!(
            receipt.contains("GUIDE"),
            "GUIDE in linting-now:\n{receipt}"
        );
        assert!(
            receipt.contains("Activates when you create"),
            "has id-claim section:\n{receipt}"
        );
        assert!(receipt.contains("NOTE"), "NOTE in activates:\n{receipt}");
    }

    /// Finding #2: render_add_receipt CTA must name the first id-claimed
    /// namespace, never a path-claimed one. GUIDE is declared first but is
    /// path-claimed — the CTA must read `ctxgrd new NOTE`.
    #[test]
    fn render_add_receipt_cta_names_first_id_claimed_namespace() {
        let pack = synthetic_split_pack();
        let plan = plan_add(&pack, "", Path::new("/nonexistent"));
        let receipt = render_add_receipt(&pack, &plan);
        assert!(
            receipt.contains("ctxgrd new NOTE"),
            "CTA must name first id-claimed namespace NOTE, got:\n{receipt}"
        );
        assert!(
            !receipt.contains("ctxgrd new GUIDE"),
            "CTA must not name path-claimed namespace GUIDE:\n{receipt}"
        );
    }

    /// Finding #4: render_show must print a `paths:` line for path-claimed
    /// namespaces (AGENTS in the agents pack claims AGENTS.md).
    #[test]
    fn render_show_includes_paths_for_path_claimed_namespaces() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "agents")
            .unwrap();
        let output = render_show(&pack);
        assert!(
            output.contains("paths:"),
            "render_show must print paths: for AGENTS:\n{output}"
        );
        assert!(
            output.contains("AGENTS.md"),
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
    fn providers_of_agent_assigned_names_workflow_pack() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            providers_of(tmp.path(), "agent.assigned"),
            vec!["workflow".to_string()]
        );
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

    // -- ADR-053 provenance v2, fingerprint, migrate ------------------

    #[test]
    fn parse_provenance_reads_all_three_forms() {
        // Bare form (ADR-013): no version, no fingerprint.
        assert_eq!(
            parse_provenance("# pack: claude"),
            Some(Provenance {
                pack: "claude".to_string(),
                version: None,
                sha: None,
            })
        );
        // @version form: provenance label only.
        assert_eq!(
            parse_provenance("# pack: claude@0.35.0"),
            Some(Provenance {
                pack: "claude".to_string(),
                version: Some("0.35.0".to_string()),
                sha: None,
            })
        );
        // Full v2 form: version label + content fingerprint.
        assert_eq!(
            parse_provenance("# pack: claude@0.35.0 sha:7800619f29dbf307"),
            Some(Provenance {
                pack: "claude".to_string(),
                version: Some("0.35.0".to_string()),
                sha: Some("7800619f29dbf307".to_string()),
            })
        );
        // Surrounding whitespace is tolerated.
        assert_eq!(
            parse_provenance("   # pack:  project-docs@0.35.0  sha:abcd  "),
            Some(Provenance {
                pack: "project-docs".to_string(),
                version: Some("0.35.0".to_string()),
                sha: Some("abcd".to_string()),
            })
        );
        // Non-provenance lines yield None.
        assert_eq!(parse_provenance("[ADR]"), None);
        assert_eq!(parse_provenance("# summary: x"), None);
        assert_eq!(parse_provenance("rules = []"), None);
    }

    #[test]
    fn fingerprint_is_stable_and_normalization_insensitive() {
        // A small known block, with the expected hex pinned (FNV-1a 64-bit
        // over the normalized text — verified against the recipe constants).
        let block = "[NOTE]\nrules = [\"core.frontmatter\"]\n";
        assert_eq!(fingerprint(block), "c1cea673c7ee5346");

        // Trailing whitespace per line does not change the digest.
        let trailing_ws = "[NOTE]   \nrules = [\"core.frontmatter\"]\t\n";
        assert_eq!(fingerprint(trailing_ws), fingerprint(block));

        // CRLF line endings normalize to LF (the \r is trailing whitespace).
        let crlf = "[NOTE]\r\nrules = [\"core.frontmatter\"]\r\n";
        assert_eq!(fingerprint(crlf), fingerprint(block));

        // Trailing blank lines do not change the digest.
        let trailing_blanks = "[NOTE]\nrules = [\"core.frontmatter\"]\n\n\n";
        assert_eq!(fingerprint(trailing_blanks), fingerprint(block));

        // A real content change DOES change the digest.
        let changed = "[NOTE]\nrules = [\"core.frontmatter\", \"core.id\"]\n";
        assert_ne!(fingerprint(changed), fingerprint(block));
    }

    #[test]
    fn split_recipe_fingerprints_match_pre_split_blocks() {
        // Guard against silent rot of the embedded split fingerprints: the
        // pre-split (680b40c) `agents` pack [AGENTS]/[SKILLS] blocks, with
        // their fingerprints re-derived here and checked against both the
        // embedded recipe constants and a pinned hex.
        const OLD_AGENTS_PACK: &str = "\
[AGENTS]
paths = [\"CLAUDE.md\", \"AGENTS.md\", \"GEMINI.md\"]
rules = [\"agents.context-headings\", \"agents.context-budget\", \"agents.context-cache\"]

# [AGENTS.\"agents.context-budget\"]
# max_words = 4000        # instruction-file size budget (default 4000)
# [AGENTS.\"agents.context-cache\"]
# churn_min_hours = 0     # >0 enables the commit-context churn warning

# SKILLS claims SKILL.md files used by Claude Code and Codex (agentskills.io
# convention). Path-claimed. Uses a file-level compiled rule (not core.*) because
# SKILL.md has name/description but no id: — core.* would error with IdMissing.
[SKILLS]
paths = [\".claude/skills/**/SKILL.md\", \".codex/skills/**/SKILL.md\"]
rules = [\"skills.frontmatter\"]
";
        let blocks = namespace_blocks(OLD_AGENTS_PACK);
        let agents_block = &blocks.iter().find(|(n, _)| n == "AGENTS").unwrap().1;
        let skills_block = &blocks.iter().find(|(n, _)| n == "SKILLS").unwrap().1;

        assert_eq!(fingerprint(agents_block), "7800619f29dbf307");
        assert_eq!(fingerprint(skills_block), "f66294963c421057");

        // The recipe constants must equal the re-derived fingerprints.
        let agents_recipe = migration_recipes()
            .iter()
            .find(|r| r.pack == "agents" && r.from_ns == "AGENTS")
            .unwrap();
        assert_eq!(
            agents_recipe.clean_fingerprints,
            &[fingerprint(agents_block).as_str()]
        );
        let skills_recipe = migration_recipes()
            .iter()
            .find(|r| r.pack == "agents" && r.from_ns == "SKILLS")
            .unwrap();
        assert_eq!(
            skills_recipe.clean_fingerprints,
            &[fingerprint(skills_block).as_str()]
        );
    }

    #[test]
    fn plan_add_emits_v2_provenance_with_matching_fingerprint() {
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "claude")
            .unwrap();
        let plan = plan_add(&pack, "", Path::new("/nonexistent-root"));
        let version = env!("CARGO_PKG_VERSION");
        // Every emitted provenance line is the v2 form for this pack.
        let marker = format!("# pack: claude@{version} sha:");
        assert!(
            plan.blocks_text.contains(&marker),
            "v2 provenance present:\n{}",
            plan.blocks_text
        );
        // The stamped sha equals fingerprint(block) for the CLAUDEAGENTS block.
        let canon = canonical_block(&[pack.clone()], "CLAUDEAGENTS").unwrap().1;
        let expected = format!(
            "# pack: claude@{version} sha:{}",
            fingerprint(&canon)
        );
        assert!(
            plan.blocks_text.contains(&expected),
            "stamped sha matches fingerprint(block):\n{}",
            plan.blocks_text
        );
        // The substring assertions older tests rely on still hold.
        assert!(plan.blocks_text.contains("# pack: claude"));
    }

    #[test]
    fn config_segments_pairs_provenance_with_block() {
        let toml = "\
# ctxgrd.toml

# pack: claude@0.35.0 sha:abc
[CLAUDEAGENTS]
paths = [\".claude/agents/**/*.md\"]
rules = [\"agent.frontmatter\"]

[ADR]
rules = [\"core.id\"]
";
        let segs = config_segments(toml);
        let ns_segs: Vec<&ConfigSegment> = segs
            .iter()
            .filter(|s| matches!(s, ConfigSegment::Namespace { .. }))
            .collect();
        assert_eq!(ns_segs.len(), 2);
        match ns_segs[0] {
            ConfigSegment::Namespace {
                ns, provenance, ..
            } => {
                assert_eq!(ns, "CLAUDEAGENTS");
                assert_eq!(
                    provenance.as_deref(),
                    Some("# pack: claude@0.35.0 sha:abc")
                );
            }
            _ => unreachable!(),
        }
        match ns_segs[1] {
            ConfigSegment::Namespace {
                ns, provenance, ..
            } => {
                assert_eq!(ns, "ADR");
                assert_eq!(provenance.as_deref(), None);
            }
            _ => unreachable!(),
        }
    }

    /// A clean `[CLAUDECODE]` block: the current CLAUDEAGENTS canonical with
    /// the namespace token reverse-substituted to the old name.
    fn clean_claudecode_block() -> String {
        let claude = builtin_packs()
            .into_iter()
            .find(|p| p.name == "claude")
            .unwrap();
        let canon = canonical_block(&[claude], "CLAUDEAGENTS").unwrap().1;
        substitute_namespace_name(&canon, "CLAUDEAGENTS", "CLAUDECODE")
    }

    #[test]
    fn migrate_renames_clean_claudecode_to_claudeagents() {
        let tmp = tempfile::tempdir().unwrap();
        let config = format!(
            "# pack: claude\n{}\n",
            clean_claudecode_block()
        );
        let plan = plan_migrate(&config, tmp.path());
        assert_eq!(plan.diffs, Vec::new(), "clean block is not dirty");
        assert_eq!(plan.rewrites.len(), 1);
        assert_eq!(plan.rewrites[0].from_ns, "CLAUDECODE");
        assert_eq!(plan.rewrites[0].to_ns, vec!["CLAUDEAGENTS".to_string()]);
        // The migrated config carries the new namespace and v2 provenance.
        assert!(plan.new_config.contains("[CLAUDEAGENTS]"));
        assert!(!plan.new_config.contains("[CLAUDECODE]"));
        assert!(plan
            .new_config
            .contains(&format!("# pack: claude@{} sha:", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn migrate_is_idempotent_after_clean_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let config = format!("# pack: claude\n{}\n", clean_claudecode_block());
        let first = plan_migrate(&config, tmp.path());
        assert_eq!(first.rewrites.len(), 1);

        // Second run over the migrated text must be a no-op.
        let second = plan_migrate(&first.new_config, tmp.path());
        assert_eq!(second.rewrites, Vec::new(), "second run rewrites nothing");
        assert_eq!(second.diffs, Vec::new(), "second run has no diffs");
        assert_eq!(
            second.new_config, first.new_config,
            "second migrate is byte-identical"
        );
    }

    #[test]
    fn migrate_leaves_hand_edited_block_as_a_dirty_diff() {
        let tmp = tempfile::tempdir().unwrap();
        // A hand-edited CLAUDECODE block: an extra rule line breaks the clean
        // reverse-substitution match.
        let edited = clean_claudecode_block().replace(
            "rules = [\"agent.frontmatter\"]",
            "rules = [\"agent.frontmatter\", \"core.min-docs\"]",
        );
        let config = format!("# pack: claude\n{edited}\n");
        let plan = plan_migrate(&config, tmp.path());
        assert_eq!(plan.rewrites, Vec::new(), "dirty block not rewritten");
        assert_eq!(plan.diffs.len(), 1);
        assert_eq!(plan.diffs[0].namespace, "CLAUDECODE");
        assert_eq!(plan.diffs[0].kind, "rename");
        assert!(plan.diffs[0].on_disk.contains("core.min-docs"));
        // The dirty block is left intact in the migrated text.
        assert!(plan.new_config.contains("[CLAUDECODE]"));
        assert!(plan.new_config.contains("core.min-docs"));
    }

    #[test]
    fn migrate_block_with_subtables_is_not_falsely_dirty() {
        // Regression: a namespace whose canonical block carries nested
        // sub-tables (project-docs [ADR] has core.required-headings/-metadata/
        // -values) must segment as ONE block. An earlier config_segments split
        // it at the first sub-table, leaving a truncated on-disk block that
        // mismatched canonical and was falsely reported as an `internals` diff.
        let tmp = tempfile::tempdir().unwrap();
        let project_docs = builtin_packs()
            .into_iter()
            .find(|p| p.name == "project-docs")
            .unwrap();
        let adr = canonical_block(&[project_docs], "ADR").unwrap().1;
        assert!(
            adr.contains("[ADR.\"core.required-headings\"]"),
            "ADR canonical must carry sub-tables for this regression to bite"
        );
        let config = format!("# pack: project-docs\n{adr}\n");
        let plan = plan_migrate(&config, tmp.path());
        assert_eq!(plan.diffs, Vec::new(), "block with sub-tables is not dirty");
        assert_eq!(plan.rewrites, Vec::new());
        assert_eq!(plan.noop_count, 1);
    }

    #[test]
    fn migrate_preserves_blank_line_separators_between_blocks() {
        // Regression from fleet dogfooding: re-emitting blocks must keep the
        // single blank line that separates them — eating it churned the whole
        // file even for blocks migrate left unchanged. A clean CLAUDECODE
        // rename followed by an unrelated block must keep the blank between.
        let tmp = tempfile::tempdir().unwrap();
        let claude = builtin_packs()
            .into_iter()
            .find(|p| p.name == "claude")
            .unwrap();
        let canon = canonical_block(&[claude], "CLAUDEAGENTS").unwrap().1;
        let claudecode = substitute_namespace_name(&canon, "CLAUDEAGENTS", "CLAUDECODE");
        // Two blocks separated by exactly one blank line; the second is an
        // unknown local namespace migrate leaves untouched.
        let config = format!("# pack: claude\n{claudecode}\n\n[LOCAL]\nrules = [\"x.y\"]\n");
        let plan = plan_migrate(&config, tmp.path());
        assert_eq!(plan.rewrites.len(), 1, "the rename is the only rewrite");
        // Exactly one blank line between the migrated block and [LOCAL] — no
        // collapse (would be `]\n[LOCAL]`) and no doubling (`]\n\n\n[LOCAL]`).
        assert!(
            plan.new_config.contains("\n\n[LOCAL]"),
            "blank separator preserved:\n{}",
            plan.new_config
        );
        assert!(
            !plan.new_config.contains("\n\n\n[LOCAL]"),
            "separator not doubled:\n{}",
            plan.new_config
        );
    }

    #[test]
    fn migrate_already_split_agents_block_is_a_noop() {
        // Regression from fleet dogfooding: the AGENTS split recipe has
        // `from_ns: AGENTS` and AGENTS among its `to_ns`, so it keeps matching
        // the *already-split* [AGENTS] block. After a successful migrate that
        // block is the current canonical — it must classify as a NoOp, not a
        // perpetual dirty "split" diff.
        let tmp = tempfile::tempdir().unwrap();
        let agents = builtin_packs()
            .into_iter()
            .find(|p| p.name == "agents")
            .unwrap();
        let canon = canonical_block(&[agents], "AGENTS").unwrap().1;
        // v2 provenance carrying the canonical fingerprint, as migrate stamps.
        let config = format!("# pack: agents@{}\n{canon}\n", env!("CARGO_PKG_VERSION"));
        let plan = plan_migrate(&config, tmp.path());
        assert_eq!(plan.diffs, Vec::new(), "already-split AGENTS is not dirty");
        assert_eq!(plan.rewrites, Vec::new());
        assert_eq!(plan.noop_count, 1);
    }

    #[test]
    fn migrate_bare_already_current_block_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        // A bare-form provenance on a block that already equals the current
        // canonical shape: nothing to migrate.
        let claude = builtin_packs()
            .into_iter()
            .find(|p| p.name == "claude")
            .unwrap();
        let canon = canonical_block(&[claude], "CLAUDEAGENTS").unwrap().1;
        let config = format!("# pack: claude\n{canon}\n");
        let plan = plan_migrate(&config, tmp.path());
        assert_eq!(plan.rewrites, Vec::new());
        assert_eq!(plan.diffs, Vec::new());
        assert_eq!(plan.noop_count, 1);
    }

    // -- pack-to-pack dependencies (ADR-068) ----------------------------

    fn write_local_pack(root: &Path, name: &str, body: &str) {
        let dir = root.join("packs").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pack.toml"), body).unwrap();
    }

    #[test]
    fn depends_parses_comma_list_and_defaults_empty() {
        // PKD-001: parsed from the comment, like summary.
        assert_eq!(
            depends_of("# summary: x\n# depends: security, ops\n[A]\nrules = []\n"),
            vec!["security".to_string(), "ops".to_string()]
        );
        assert!(depends_of("# summary: x\n[A]\nrules = []\n").is_empty());
    }

    #[test]
    fn resolve_gdpr_pulls_security_first() {
        // PKD-002: the closure is [dependency, …, pack], dependencies first.
        let tmp = tempfile::tempdir().unwrap();
        let gdpr = find(tmp.path(), "gdpr").expect("gdpr is a builtin pack");
        let chain = resolve_dependencies(tmp.path(), &gdpr).unwrap();
        let names: Vec<&str> = chain.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["security", "gdpr"]);
    }

    #[test]
    fn resolve_base_pack_is_just_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let security = find(tmp.path(), "security").expect("security is a builtin pack");
        let chain = resolve_dependencies(tmp.path(), &security).unwrap();
        let names: Vec<&str> = chain.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["security"]);
    }

    #[test]
    fn every_builtin_pack_resolves_against_the_base_target_rule() {
        // PKD-003 property: every built-in pack's dependency closure
        // resolves — no cycle, no missing dependency, every target a base
        // pack. Catches a future pack that declares an illegal edge.
        let tmp = tempfile::tempdir().unwrap();
        for pack in builtin_packs() {
            assert!(
                resolve_dependencies(tmp.path(), &pack).is_ok(),
                "builtin pack '{}' has an unresolvable dependency closure",
                pack.name
            );
        }
    }

    #[test]
    fn resolve_rejects_missing_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        write_local_pack(
            tmp.path(),
            "needs-ghost",
            "# summary: x\n# depends: ghost\n[NG]\nrules = []\n",
        );
        let pack = find(tmp.path(), "needs-ghost").unwrap();
        let err = resolve_dependencies(tmp.path(), &pack).unwrap_err();
        assert_eq!(
            err,
            DependencyError::Missing {
                pack: "needs-ghost".to_string(),
                missing: "ghost".to_string(),
            }
        );
    }

    #[test]
    fn resolve_rejects_non_base_target() {
        // PKD-003 / CMP-002: a dependency must itself be dependency-free.
        let tmp = tempfile::tempdir().unwrap();
        write_local_pack(tmp.path(), "leaf", "# summary: leaf\n[LF]\nrules = []\n");
        write_local_pack(
            tmp.path(),
            "mid",
            "# summary: mid\n# depends: leaf\n[MD]\nrules = []\n",
        );
        write_local_pack(
            tmp.path(),
            "top",
            "# summary: top\n# depends: mid\n[TP]\nrules = []\n",
        );
        let top = find(tmp.path(), "top").unwrap();
        let err = resolve_dependencies(tmp.path(), &top).unwrap_err();
        assert_eq!(
            err,
            DependencyError::NonBaseTarget {
                pack: "top".to_string(),
                target: "mid".to_string(),
            }
        );
    }

    #[test]
    fn resolve_rejects_mutual_dependency() {
        // A two-cycle is rejected: the base-target rule (PKD-003) fires
        // first — each pack names a non-base target — which subsumes the
        // acyclicity requirement.
        let tmp = tempfile::tempdir().unwrap();
        write_local_pack(
            tmp.path(),
            "ping",
            "# summary: ping\n# depends: pong\n[PI]\nrules = []\n",
        );
        write_local_pack(
            tmp.path(),
            "pong",
            "# summary: pong\n# depends: ping\n[PO]\nrules = []\n",
        );
        let ping = find(tmp.path(), "ping").unwrap();
        assert!(resolve_dependencies(tmp.path(), &ping).is_err());
    }
}
