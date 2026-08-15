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

use std::collections::{BTreeMap, BTreeSet};
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
const CHECKLIST_TOML: &str = include_str!("../packs/checklist/pack.toml");
const INTAKE_TOML: &str = include_str!("../packs/intake/pack.toml");
const GDPR_TOML: &str = include_str!("../packs/gdpr/pack.toml");
const HIPAA_TOML: &str = include_str!("../packs/hipaa/pack.toml");
const SOC2_TOML: &str = include_str!("../packs/soc2/pack.toml");
const ISO27001_TOML: &str = include_str!("../packs/iso-27001/pack.toml");
const NIST80053_TOML: &str = include_str!("../packs/nist-800-53/pack.toml");
const NIS2_TOML: &str = include_str!("../packs/nis2/pack.toml");
const EU_AI_ACT_TOML: &str = include_str!("../packs/eu-ai-act/pack.toml");
const CCPA_TOML: &str = include_str!("../packs/ccpa/pack.toml");
const GITHUB_TOML: &str = include_str!("../packs/github/pack.toml");
const GITLAB_TOML: &str = include_str!("../packs/gitlab/pack.toml");
const DDD_TOML: &str = include_str!("../packs/ddd/pack.toml");
const GOVERNANCE_TOML: &str = include_str!("../packs/governance/pack.toml");
const RESEARCH_TOML: &str = include_str!("../packs/research/pack.toml");
const QA_TOML: &str = include_str!("../packs/qa/pack.toml");
const MARKETING_TOML: &str = include_str!("../packs/marketing/pack.toml");
const PORT_TOML: &str = include_str!("../packs/port/pack.toml");
const STRIPE_INTEGRATION_WEB_TOML: &str =
    include_str!("../packs/stripe-integration-web/pack.toml");

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
    /// Every `[NS."rule.code"]` param table the namespace declares, keyed by
    /// rule code (ADR-113 § PKJ-001).
    ///
    /// **Complete by construction, not by allow-list.** Every sub-table of a
    /// namespace block *is* a rule's param table, so this collects "every
    /// table present" rather than a curated selection. A withheld param is a
    /// param a downstream consumer must transcribe, which ADR-076 § OWN-001
    /// forbids — so there is deliberately no filter here that could fall out
    /// of sync with the packs.
    ///
    /// Includes `core.required-metadata`, which [`NamespaceView::required_metadata`]
    /// also hoists. The duplication is deliberate: removing the hoisted field
    /// would break the `pack show --format json` shape ADR-096 § CMD-002 ships.
    pub params: BTreeMap<String, serde_json::Value>,
    /// The digest a `# pack:` stamp must carry for this namespace to read as
    /// current — [`fingerprint`] of the pack's canonical block, the exact
    /// value the drift classifier compares a stored `sha:` against
    /// (ADR-126 § DRF-008).
    ///
    /// This is the escape hatch for a block with no baseline. Nothing can
    /// recover which pack revision such a block was copied from, so no verb
    /// adopts today's pack on the consumer's behalf; publishing the digest
    /// puts the assertion where it belongs, with the person who can actually
    /// judge whether their block is current.
    pub fingerprint: String,
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

/// The built-in packs (PACK-009). This list narrates the major additions,
/// not every entry. Core set: `project-docs`, `ops`,
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
/// the shared evidence_gap core. ADR-082 added `ddd` (strategic Domain-Driven
/// Design: the id-claimed BOUNDEDCONTEXT and CONTEXTMAP namespaces typed on the
/// dependency graph, with one new builtin `ddd.context-map-shape` rule).
/// ADR-085 added `stripe-integration-web` (the INTSTRIPE Stripe
/// web-integration checklist profile ADR-078 § CHK-006 deferred: an
/// [INTSTRIPE] namespace on docs/integrations/stripe/** binding the
/// checklist.* rules plus core.required-headings/anchors for the seven
/// Stripe phases and twelve inline-tier @stripe.* markers — the ctxgrd
/// presence leg of the Stripe verification triad with wrkgrd and cmplgrd).
/// ADR-092 added `governance` (the DEC program/governance decision register on
/// docs/decisions/[0-9]*.md — the first RAID+ register family member, kept out of
/// project-docs as an opt-in authority/impact overlay, the same placement the
/// intake pack's CR change register already set).
/// ADR-100 added `marketing` (the CAMPAIGN/PERSONA/POSITIONING/ICP
/// marketing-strategy namespaces — id-less path-claims with framework-sourced
/// required-headings and one monotonic builtin `marketing.frontmatter`, kept on
/// the docs side of the CRM line).
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
            // RESEARCH namespace — path-claimed on docs/research/**. Deep-research
            // reports (a multi-source cited synthesis genre) kept evidence-honest
            // by the builtin-compiled `research.evidence` rule: an evidence/sources
            // section is required (error) and a limitations/data-gaps section is
            // nudged (configurable warning), with an optional monotonic
            // `research.type` field routing a report into its academic/market/
            // deep-research skeleton. It also binds core.min-docs (a non-empty
            // research folder once adopted). id-less: the filename is the slug. No
            // dep rules, no core.allowed-values (the rule self-validates the type),
            // and no external scripts. ADR-093.
            name: "research".to_string(),
            summary: summary_of(RESEARCH_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: RESEARCH_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // CHECKLIST namespace — path-claimed on docs/checklists/**. Turns a
            // markdown checklist into an auditable, commit-pinned sign-off:
            // `checklist.structure` (title + living|sealed status + a pin when
            // sealed + ≥1 box), `checklist.complete` (sealed ⇒ 0 unchecked), and
            // `checklist.pinned` (sealed ⇒ pinned_commit resolves + ancestor of
            // HEAD). It also binds the generic, config-driven
            // `core.required-headings` (shipped with no section set — a project
            // supplies its own, e.g. the seven-phase Stripe shape). id-less: the
            // filename is the slug. No external scripts. ADR-078.
            name: "checklist".to_string(),
            summary: summary_of(CHECKLIST_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: CHECKLIST_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // INTSTRIPE — the Stripe web-integration checklist profile ADR-078
            // § CHK-006 deferred, unblocked once core.required-anchors shipped.
            // A dedicated [INTSTRIPE] namespace on docs/integrations/stripe/**
            // (its own path, so it coexists with the generic `checklist` pack)
            // binding the checklist.* sign-off rules plus core.required-headings
            // (the seven Stripe phases) and core.required-anchors (twelve
            // inline-tier @stripe.* markers). The ctxgrd "presence" leg of the
            // Stripe verification triad: wrkgrd's anchor-coverage proves the
            // tier=code half, cmplgrd orchestrates by tier. id-less, no external
            // scripts. ADR-085.
            name: "stripe-integration-web".to_string(),
            summary: summary_of(STRIPE_INTEGRATION_WEB_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: STRIPE_INTEGRATION_WEB_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // CR/FEEDBACK — the inbound-request intake pack (ADR-079). CR is a
            // JSM-Change-shaped, id-claimed namespace (CR-NNN) under docs/cr/**:
            // core.frontmatter/id/id-unique/cross-ref/required-headings
            // (Summary + References) plus core.required-metadata (id, title,
            // status, date, and the new `source` reporter key) and
            // core.allowed-values gating the status vocabulary. FEEDBACK is the
            // deliberately ceremony-light channel (INT-003), path-claimed on
            // docs/feedback/** with a single core.min-docs presence nudge and no
            // id / metadata / heading schema, so the existing frontmatter-less
            // feedback notes stay clean. Every guardrail is a core.* rule, so
            // `rules` (external scripts) is empty. Supersedes the fold-CR-into-
            // project-docs proposal; amends ADR-023 § PKC-002 for external intake.
            name: "intake".to_string(),
            summary: summary_of(INTAKE_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: INTAKE_TOML.to_string(),
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
        Pack {
            // NIS2 / NIS2INC — Directive (EU) 2022/2555. Unlike soc2/iso-27001/
            // nist-800-53 this is a LAW, not a control framework, so it earns two
            // namespaces (ADR-066 § Context): the Art. 21(2) measures register and
            // the Art. 23 significant-incident register. Art. 21(2)'s ten points
            // (a)-(j) are the cleanest closed vocabulary in the family. NIS2INC
            // deliberately binds no clock rule and no core.min-docs — ctxgrd's
            // dates are day-granular and cannot score a 24h/72h statutory deadline,
            // and an incident namespace with a minimum-document count would
            // pressure an entity to invent a report (ADR-115 § NIS-002/003).
            // Generated from packs/nis2/regulation.json.
            name: "nis2".to_string(),
            summary: summary_of(NIS2_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: NIS2_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // AIACT / FRIA — Regulation (EU) 2024/1689. A law with two namespaces:
            // the obligation register, where `risk_tier` and `role` are
            // load-bearing because the Act's requirement set is *selected* by tier
            // and role (Art. 25 lets one entity hold provider and deployer duties
            // over one system), and the Art. 27 fundamental rights impact
            // assessment, modelled separately for the same reason gdpr's DPIA is —
            // a dated assessment of a specific deployment, not a table row.
            // Generated from packs/eu-ai-act/regulation.json (ADR-116).
            name: "eu-ai-act".to_string(),
            summary: summary_of(EU_AI_ACT_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: EU_AI_ACT_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // CCPA / SPA — California Civil Code Title 1.81.5 (CCPA as amended by
            // the CPRA). A separate pack, NOT a gdpr extension: ADR-066 § CMP-001
            // requires one pack per regulation so a GDPR-subject project is not
            // forced to carry US state privacy namespaces (ADR-117 § CCP-001).
            // ROPA-shaped, but the pivot is a sale/sharing determination rather
            // than a lawful basis — and `sold`/`shared` are separate statutory
            // definitions (§ 1798.140(ad)/(ah)), so `both` is a real value.
            // Generated from packs/ccpa/regulation.json.
            name: "ccpa".to_string(),
            summary: summary_of(CCPA_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: CCPA_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // CONTRIBUTING/CODEOFCONDUCT/SECURITYDOC/SUPPORT — the markdown
            // community-health files GitHub recognizes (root, .github/, or
            // docs/), path-claimed and id-less with a single warning-severity
            // core.min-docs existence nudge. README stays in project-docs;
            // LICENSE is excluded (host detection needs verbatim SPDX text).
            // Shares the [CONTRIBUTING] namespace name with the gitlab pack, so
            // the two compose via never-clobber `pack add` rather than conflict
            // (ADR-077 § CHF-005).
            name: "github".to_string(),
            summary: summary_of(GITHUB_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: GITHUB_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // CONTRIBUTING/CHANGELOG — the markdown files GitLab's repository UI
            // surfaces beyond README/LICENSE (CHANGELOG is first-class on GitLab,
            // generated from git trailers). Path-claimed, id-less, warning-severity
            // core.min-docs existence nudge. CODE_OF_CONDUCT/SECURITY/SUPPORT are
            // GitHub-only, so they live in the github pack, not here. Shares the
            // [CONTRIBUTING] namespace name with the github pack, so the two compose
            // via never-clobber `pack add` rather than conflict (ADR-077 § CHF-005).
            name: "gitlab".to_string(),
            summary: summary_of(GITLAB_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: GITLAB_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // BOUNDEDCONTEXT/CONTEXTMAP — strategic Domain-Driven Design docs
            // typed on the dependency graph (ADR-082). Both namespaces are
            // id-claimed: a BOUNDEDCONTEXT is the anchor context artifact
            // (Ubiquitous Language, aggregates, and domain events folded in as
            // headings for the MVP, DDD-005), and a CONTEXTMAP models one Evans
            // strategic-pattern relationship edge per file via
            // depends_on: [BOUNDEDCONTEXT-x, BOUNDEDCONTEXT-y]. Every guardrail
            // is a core.*/ddd.* rule, so `rules` (external scripts) is empty.
            // The one new builtin, ddd.context-map-shape, adds the "exactly two
            // endpoints" cardinality and the pattern-conditional upstream/
            // downstream direction check core.dep-shape cannot express — the
            // soc2.control-evidence if/then precedent. DDD is deliberately NOT
            // path-claimed like c4/guide (DDD-004) and stays a sibling of the c4
            // pack (a Bounded Context is not a C4 Container, DDD-006).
            name: "ddd".to_string(),
            summary: summary_of(DDD_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: DDD_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // DEC — a program/governance decision register (ADR-092). One
            // id-claimed namespace on docs/decisions/[0-9]*.md: pure core.*
            // (frontmatter/id/id-unique/dep-resolved/dep-cycle/cross-ref/
            // required-headings [Decision/Rationale/Impact/Approval]/
            // required-metadata [+ the DEC-distinguishing `decision-maker`
            // authority key]/allowed-values [the governance lifecycle
            // proposed→pending→approved|rejected|deferred→superseded]/
            // successor-link), so `rules` (external scripts) is empty. NOT the
            // ADR namespace: an ADR is a technical decision owned by
            // engineering; a DEC is a program/strategic decision emphasizing
            // authority and cross-cutting impact (scope/cost/schedule/benefits/
            // risk/stakeholders). Its own opt-in pack, not project-docs, because
            // a governance register is an authority/impact overlay (like ops/
            // security/compliance), and DEC is the first member of the RAID+
            // register family — the change register (CR) already lives outside
            // project-docs in `intake`, the same precedent (CR-004).
            name: "governance".to_string(),
            summary: summary_of(GOVERNANCE_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: GOVERNANCE_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // TEST — a pinned Test Completion Report register (ADR-098). One
            // id-claimed namespace on docs/tests/TEST-*.md: the durable,
            // human-authored milestone artifact (IEEE 829 / ISO-29119-3) that
            // records whether a release cleared its exit gate, against which
            // tree and which contract revision, its outstanding defects, and
            // sign-off. Pure core.* plus one new builtin, `test.completion`,
            // which enforces the two sealed-record invariants core presence
            // rules cannot express: the `tested_commit`/`spec_commit` pins
            // (shape-only, cloned from checklist.pinned) and the non-empty
            // Outstanding Defects section a `conditional-pass` waiver requires.
            // The pack is named `qa` but the namespace is [TEST], since a
            // document's namespace is its id prefix (`id: TEST-<N>`), the same
            // pack-name/namespace-name split as governance -> [DEC]. Its own
            // opt-in pack, not project-docs, because a completion report is
            // QA-lifecycle-specific and will grow siblings (test plans, status
            // reports) — the governance-pack precedent (ADR-092).
            name: "qa".to_string(),
            summary: summary_of(QA_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: QA_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // marketing — the markdown-documentable marketing-strategy artifacts:
            // CAMPAIGN briefs, buyer PERSONAs, POSITIONING, and the ICP. Four
            // path-claimed, id-less namespaces (the filename is the slug, like
            // GUIDE/RESEARCH — strategy singletons, not an id-keyed graph). The
            // scope boundary is the CRM line: individual prospect/lead records,
            // lead scores, and UTM tables live in Salesforce/HubSpot, never in
            // markdown, so they get no namespace. Everything is core.* except one
            // builtin, `marketing.frontmatter`: a monotonic opt-in validating a
            // nested `marketing.type` genre discriminator (nested to dodge the
            // SSG-reserved top-level `type:`, BUG-015 — a check no core primitive
            // can express). Greenfield default globs are docs/marketing/**; a
            // project overrides `paths` (this repo points them at docs/strategy/,
            // as governance points [DEC] at docs/strategy/decisions/). The
            // PERSONA/POSITIONING/ICP required-headings are framework-sourced
            // (Revella's 5 Rings; Moore ∩ Dunford; the B2B ICP standard); CAMPAIGN
            // stays deliberately minimal until a real brief format exists. ADR-100.
            name: "marketing".to_string(),
            summary: summary_of(MARKETING_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: MARKETING_TOML.to_string(),
            rules: Vec::new(),
        },
        Pack {
            // A per-subsystem porting-parity tracker: one document per subsystem
            // of the software being ported, citing `original_source` and the
            // `original_ref` it was ported against, carrying a parity status and
            // proving equivalence under ## Verification. Reusable across any port,
            // so the namespace is generic rather than named for one project.
            name: "port".to_string(),
            summary: summary_of(PORT_TOML),
            source_label: "built-in".to_string(),
            rank: 0,
            toml_text: PORT_TOML.to_string(),
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
pub(crate) fn discover_packs(root: &Path, global_dir: Option<&Path>) -> Vec<Pack> {
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
pub(crate) fn providers_of(root: &Path, code: &str) -> Vec<String> {
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

/// Names of discoverable packs that define namespace `ns` (ADR-025 §
/// PKD-001, applied to namespaces rather than rule codes). Sorted,
/// deduplicated, empty when no pack ships it.
///
/// Lets `cfg.namespace-undeclared` (ADR-076 § OWN-004) name the exact
/// `pack add` that would declare the namespace a document already claims,
/// instead of leaving the reader to hunt through `ctxgrd pack list`.
pub(crate) fn providers_of_namespace(root: &Path, ns: &str) -> Vec<String> {
    let mut names: Vec<String> = discover(root)
        .iter()
        .filter(|p| namespace_views(p).iter().any(|v| v.name == ns))
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
        .map(|(name, block)| {
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
            // ADR-113 § PKJ-001: every sub-table of the namespace block is a
            // rule's param table. Scalars and arrays at this level are the
            // namespace's own keys (`paths`, `rules`, `owner`), already
            // surfaced above.
            let params: BTreeMap<String, serde_json::Value> = ns_tbl
                .map(|t| {
                    t.iter()
                        .filter_map(|(code, v)| {
                            v.as_table().map(|tbl| (code.clone(), toml_table_to_json(tbl)))
                        })
                        .collect()
                })
                .unwrap_or_default();
            // Hashed from the same `namespace_blocks` slice `canonical_block`
            // returns, so this is the classifier's operand and not a parallel
            // derivation of it — the divergence that would make the published
            // remedy silently produce a wrong value.
            let fingerprint = fingerprint(&block);
            NamespaceView {
                name,
                rules,
                required_metadata,
                path_patterns,
                params,
                fingerprint,
            }
        })
        .collect()
}

/// One TOML value as JSON, for the machine-readable pack contract
/// (ADR-113 § PKJ-002).
///
/// Hand-written rather than routed through `serde` because the target must be
/// **canonical**: `serde_json::Map` is a `BTreeMap` in this build (no
/// `preserve_order` feature), so object keys sort and the same pack always
/// serialises to the same bytes. That is what lets a consumer pin a digest it
/// computes itself. A TOML datetime has no JSON counterpart and becomes its
/// RFC 3339 string, which is how every pack already spells dates anyway.
fn toml_value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Integer(i) => serde_json::Value::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        Value::Array(a) => serde_json::Value::Array(a.iter().map(toml_value_to_json).collect()),
        Value::Table(t) => toml_table_to_json(t),
    }
}

fn toml_table_to_json(t: &toml::Table) -> serde_json::Value {
    serde_json::Value::Object(
        t.iter()
            .map(|(k, v)| (k.clone(), toml_value_to_json(v)))
            .collect(),
    )
}

/// Plan a `pack add` against the current `ctxgrd.toml` text, without
/// touching any file. Namespaces already present are skipped
/// (PACK-005); rule scripts already on disk are not re-copied.
pub fn plan_add(pack: &Pack, existing_toml: &str, root: &Path) -> AddPlan {
    let existing = existing_namespaces(existing_toml);
    let mut plan = AddPlan::default();
    for seg in namespace_segments(&pack.toml_text) {
        if existing.contains(&seg.name) {
            plan.skipped.push(seg.name);
            continue;
        }
        // Stamp first, then the pack's introduction to the namespace, then
        // the block — the stamp fingerprints the block alone (BUG-069).
        plan.blocks_text
            .push_str(&format!("\n{}\n", provenance_comment(&pack.name, &seg.block)));
        plan.blocks_text.push_str(&seg.lead);
        plan.blocks_text.push_str(&seg.block);
        plan.blocks_text.push('\n');
        plan.added.push(seg.name);
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

/// What the owning pack binds for `namespace`, for `ctxgrd init` to render
/// instead of a parallel hardcoded copy (BUG-052, BUG-071).
pub(crate) struct InitBlockSource {
    /// The owning pack's name, for the provenance stamp.
    pub pack: String,
    /// The rule list the pack binds. `init` hardcoded its own, so a rule
    /// bound in a pack never reached a new project — `core.acceptance-complete`
    /// was missing from every generated config for thirteen months.
    pub rules: Vec<String>,
    /// The pack's path claim, used when `init` detected no directory of its
    /// own for this namespace (ADR-007 § DOC-005 gives the detected one
    /// precedence: it describes the repo in front of us).
    pub paths: Vec<String>,
    /// The digest the generated block's stamp must carry. Pack-side, so it
    /// stays correct however much `init` adds on top (ADR-126 § DRF-001).
    pub fingerprint: String,
}

/// The pack that owns `namespace`, among the doc packs `init` renders from.
///
/// Same two-pack scan as [`builtin_pack_list`], for the same reason: the
/// `agents`/`design` packs bind builtin-compiled rules that are not in init's
/// catalogue. Returns `None` for a namespace no pack covers — `init` still
/// generates those, from the conventional defaults, and leaves them unstamped
/// because no pack authored them.
pub(crate) fn builtin_pack_block_source(namespace: &str) -> Option<InitBlockSource> {
    builtin_packs()
        .iter()
        .filter(|p| p.name == "project-docs" || p.name == "ops")
        .find_map(|p| {
            let view = namespace_views(p).into_iter().find(|v| v.name == namespace)?;
            Some(InitBlockSource {
                pack: p.name.clone(),
                rules: view.rules,
                paths: view.path_patterns,
                fingerprint: view.fingerprint,
            })
        })
}

/// The v2 provenance line for a block `init` generated, stamped with the
/// owning pack's current digest (ADR-126 § DRF-001).
///
/// Deliberately not `fingerprint` of the text `init` writes: `init` adds an
/// `owner`, a commented heading alternative and possibly a detected `paths`,
/// none of which any pack renders. Under the byte-equality model that made
/// the block "hand-edited" the moment it was written (BUG-071); under the
/// pack-moved model the stamp is a claim about the *pack*, so a generated
/// block may differ from canonical text and still be honestly current.
pub(crate) fn init_provenance_comment(pack: &str, fingerprint: &str) -> String {
    format!(
        "# pack: {pack}@{version} sha:{fingerprint}",
        version = env!("CARGO_PKG_VERSION"),
    )
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

/// One `# pack: <name>` provenance comment and the run of namespace
/// blocks that follows it, in file order (ADR-080 § AVS-004).
///
/// A run ends at the next `# pack:` comment or the next non-namespace
/// top-level table (`[pipeline]`, `[ignore]`). `pack add` writes one
/// comment immediately before each block it appends (`plan_add`), so the
/// generated form is a one-block run; hands routinely consolidate a
/// pack's blocks under a single comment (see this repo's `# pack:
/// marketing` run), so a run can be longer.
///
/// Segmentation reuses [`header_namespace`], the same predicate
/// [`namespace_blocks`] segments on; `namespace_blocks` itself cannot be
/// used here because it drops the preamble (where the first block's stamp
/// lives) and rejoins lines, losing the comment's position.
pub(crate) fn stamped_runs(config_toml: &str) -> Vec<(String, Vec<String>)> {
    let mut runs: Vec<(String, Vec<String>)> = Vec::new();
    let mut open = false;
    let mut current_ns: Option<String> = None;
    for line in config_toml.lines() {
        if let Some(prov) = parse_provenance(line) {
            runs.push((prov.pack, Vec::new()));
            open = true;
            current_ns = None;
            continue;
        }
        match header_namespace(line) {
            Some(ns) => {
                if current_ns.as_deref() != Some(ns.as_str()) {
                    if open {
                        if let Some((_, namespaces)) = runs.last_mut() {
                            namespaces.push(ns.clone());
                        }
                    }
                    current_ns = Some(ns);
                }
            }
            // A non-namespace top-level table ends the stamped run.
            None if line.trim_start().starts_with('[') => {
                open = false;
                current_ns = None;
            }
            None => {}
        }
    }
    runs
}

/// The namespaces `--pack <name>` selects: those stamped with that pack's
/// ADR-053 provenance comment in the project's own `ctxgrd.toml` (ADR-080
/// § AVS-004). Never the namespace list the built-in pack *declares* —
/// packs are applied by copy, so a project routinely adopts a subset, and
/// scoping to an unadopted namespace would lint nothing and report a
/// false clean.
///
/// Within a stamped run, the block the comment directly introduces is
/// always the pack's. A *trailing* block in the same run is the pack's
/// only when the pack's own definition declares it — that is what
/// separates a hand-consolidated `# pack: marketing` run (PERSONA,
/// POSITIONING, ICP all come from the pack) from a hand-written block
/// appended after someone else's stamped block. The definition is used
/// here as a filter on what the project actually stamped, never as the
/// source of the namespace list. A pack this binary cannot resolve (an
/// external or removed one) has no definition to filter with, so its
/// whole run counts.
pub(crate) fn stamped_namespaces(root: &Path, name: &str, config_toml: &str) -> BTreeSet<String> {
    let declared: Option<BTreeSet<String>> = find(root, name).map(|p| {
        namespace_blocks(&p.toml_text)
            .into_iter()
            .map(|(ns, _)| ns)
            .collect()
    });
    let mut selected: BTreeSet<String> = BTreeSet::new();
    for (pack, namespaces) in stamped_runs(config_toml) {
        if pack != name {
            continue;
        }
        for (i, ns) in namespaces.iter().enumerate() {
            let is_head = i == 0;
            if is_head || declared.as_ref().is_none_or(|d| d.contains(ns)) {
                selected.insert(ns.clone());
            }
        }
    }
    selected
}

/// One namespace as it appears in a `pack.toml`: the comment run that
/// introduces it, and its own table text.
pub(crate) struct NsSegment {
    /// The namespace name.
    pub name: String,
    /// The comment run directly above the `[<NS>]` header, newline-
    /// terminated and verbatim; empty when there is none. Held separately
    /// so a namespace's introduction is not counted as the *previous*
    /// namespace's content (BUG-069).
    pub lead: String,
    /// The verbatim block text, trailing-trimmed.
    pub block: String,
}

/// Segment `pack.toml` text into [`NsSegment`]s in declaration order. A
/// namespace block is the contiguous run of lines from its `[<NS>]` header
/// through the table headers nested under it
/// (`[<NS>."core.required-metadata"]` etc.), up to the comment run
/// introducing the next top-level namespace, or that header itself.
/// Preamble before the first namespace header (the `# summary:` line) is
/// dropped. Trailing blank lines are trimmed.
pub(crate) fn namespace_segments(toml: &str) -> Vec<NsSegment> {
    let mut out: Vec<NsSegment> = Vec::new();
    let mut current: Option<NsSegment> = None;
    // Blank and comment lines seen since the last content line. They are
    // this block's only while a content line follows; a comment run that
    // ends at the next header introduces *that* block instead.
    let mut pending: Vec<&str> = Vec::new();
    for line in toml.lines() {
        // A non-namespace table ends the block it follows, the same way a
        // different namespace header does — including the handover, which
        // `config_segments` also performs here. Without it the two sides
        // disagree about a pack shaped `[X]\nk=1\n\n# note\n[changelog]`:
        // the pack keeps `# note` in the block and the consumer does not, so
        // `block == canon` never holds and the block is permanently dirty.
        if header_namespace(line).is_none() && line.trim_start().starts_with('[') {
            if let Some(mut done) = current.take() {
                take_lead_comments(&mut pending, Held::AfterBlock);
                push_tail(&mut done.block, &mut pending);
                done.block = done.block.trim_end().to_string();
                out.push(done);
            }
            pending.clear();
            continue;
        }
        let starts_new = match header_namespace(line) {
            Some(ns) => current.as_ref().map(|c| c.name != ns).unwrap_or(true),
            None => false,
        };
        if starts_new {
            let name = header_namespace(line).expect("checked above");
            let lead = take_lead_comments(
                &mut pending,
                if current.is_some() {
                    Held::AfterBlock
                } else {
                    Held::BeforeFirstBlock
                },
            );
            if let Some(mut done) = current.take() {
                push_tail(&mut done.block, &mut pending);
                done.block = done.block.trim_end().to_string();
                out.push(done);
            }
            pending.clear();
            current = Some(NsSegment {
                name,
                lead,
                block: String::new(),
            });
        }
        let Some(cur) = current.as_mut() else {
            continue; // preamble before the first namespace header
        };
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            pending.push(line);
        } else {
            push_interior(&mut cur.block, &pending);
            pending.clear();
            cur.block.push_str(line);
            cur.block.push('\n');
        }
    }
    if let Some(mut done) = current.take() {
        // At end of input there is no next block to claim the tail.
        push_tail(&mut done.block, &mut pending);
        done.block = done.block.trim_end().to_string();
        out.push(done);
    }
    out
}

/// Segment `pack.toml` text into `(namespace, verbatim_block)` pairs —
/// [`namespace_segments`] for callers that do not need the lead comments.
pub(crate) fn namespace_blocks(toml: &str) -> Vec<(String, String)> {
    namespace_segments(toml)
        .into_iter()
        .map(|s| (s.name, s.block))
        .collect()
}

/// What the held blank/comment lines follow — the only thing that decides
/// whether a comment run touching the next header introduces it or trails
/// the block before it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    /// The lines follow a namespace block's content.
    AfterBlock,
    /// The lines open the file; there is no block before them.
    BeforeFirstBlock,
}

/// Split the comment run that introduces the *next* block off the tail of
/// `pending`, returning it newline-terminated and leaving the rest.
///
/// The run qualifies only when a blank line separates it from the previous
/// block's content — a comment that starts immediately after a key belongs
/// to the block it follows (a commented-out option, say), not to the block
/// after it. Before the first namespace there is no previous block, so any
/// trailing run qualifies.
fn take_lead_comments(pending: &mut Vec<&str>, held: Held) -> String {
    // Lines after the last blank are the run touching the coming header;
    // by construction every line in `pending` is blank or a comment.
    let start = pending
        .iter()
        .rposition(|l| l.trim().is_empty())
        .map_or(0, |i| i + 1);
    if start == pending.len() || (held == Held::AfterBlock && start == 0) {
        return String::new();
    }
    let lead = pending[start..]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect::<String>();
    pending.truncate(start);
    lead
}

/// Append held blank/comment lines to a block because a content line
/// follows them: they are interior to the block, blanks included (the
/// blank between two sub-tables is part of the block's text).
fn push_interior(block: &mut String, pending: &[&str]) {
    for line in pending {
        block.push_str(line);
        block.push('\n');
    }
}

/// Append held lines at the *end* of a block, up to and including its last
/// comment — trailing blanks are the separator to the next block, not
/// content. Drains what it consumed, so `pending` is left holding exactly
/// the lines a byte-preserving caller must re-queue.
fn push_tail(block: &mut String, pending: &mut Vec<&str>) {
    let Some(end) = pending.iter().rposition(|l| !l.trim().is_empty()) else {
        return;
    };
    push_interior(block, &pending[..=end]);
    pending.drain(..=end);
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
pub(crate) struct Provenance {
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
pub(crate) fn parse_provenance(line: &str) -> Option<Provenance> {
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
            // fingerprints of the pre-split [AGENTS] block (680b40c): as
            // extracted today, and as extracted before BUG-069 moved the
            // block seam ahead of the comment introducing [SKILLS] — a
            // config that ends at that comment still yields the old digest.
            clean_fingerprints: &["001e05c08acd19bb", "7800619f29dbf307"],
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
        /// The comment run between the provenance line and the header,
        /// newline-terminated and verbatim; empty when there is none.
        lead: String,
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
            // The unbroken comment run reaching this header is the block's
            // prelude: a `# pack:` stamp at its head, then the comments
            // introducing the namespace. Both `pack add` and `init` write
            // that shape, so requiring the stamp to be the immediately
            // preceding line left most fingerprints unbound (ADR-126).
            let (before, provenance, lead) = split_block_prelude(&other);
            other = before;
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
            let mut pending: Vec<&str> = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                if let Some(h) = header_namespace(l) {
                    if h != ns {
                        break;
                    }
                } else if l.trim_start().starts_with('[') {
                    // A non-namespace table ([changelog], [ignore],
                    // [sources.*]) is not this namespace's content — without
                    // this the block, and its fingerprint, swallow it.
                    break;
                } else if parse_provenance(l).is_some() {
                    break;
                }
                if l.trim().is_empty() || l.trim_start().starts_with('#') {
                    // Hold blanks and comments: flushed into the block only
                    // if a content line follows (interior), else trailing.
                    pending.push(l);
                } else {
                    push_interior(&mut block, &pending);
                    pending.clear();
                    block.push_str(l);
                    block.push('\n');
                }
                i += 1;
            }
            // A comment run reaching the next block introduces *that* block,
            // so it is handed over rather than counted as this one's content
            // — otherwise deleting a namespace dirties its predecessor
            // (BUG-069).
            // …only when a next block actually follows; at end of file the
            // tail is this block's own (a commented-out option, say).
            let handover = if i < lines.len() {
                take_lead_comments(&mut pending, Held::AfterBlock)
            } else {
                String::new()
            };
            push_tail(&mut block, &mut pending);
            segments.push(ConfigSegment::Namespace {
                ns,
                provenance,
                lead,
                block: block.trim_end().to_string(),
            });
            // Trailing blanks separate this block from the next — re-queue
            // them so the next segment (or its provenance) keeps its spacing.
            for l in &pending {
                other.push_str(l);
                other.push('\n');
            }
            other.push_str(&handover);
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

/// Split the prelude of the namespace block that follows off the tail of
/// `other`: returns `(other_without_prelude, provenance_line, lead)`.
///
/// The prelude is the unbroken comment run reaching the header — a blank
/// line ends it, so a comment separated from the header does not bind. A
/// `# pack:` line anywhere in that run is the block's provenance; the
/// comments after it are the block's introduction, held as `lead` so they
/// travel with the block instead of dangling in `Other`.
///
/// Requiring the stamp to be the *immediately* preceding line (the earlier
/// rule) left 3 of this repo's 4 fingerprints unbound, because both
/// `pack add` and `init` write the stamp above the comments that introduce
/// the namespace (ADR-126).
fn split_block_prelude(other: &str) -> (String, Option<String>, String) {
    let lines: Vec<&str> = other.lines().collect();
    let start = lines
        .iter()
        .rposition(|l| l.trim().is_empty())
        .map_or(0, |i| i + 1);
    let run = &lines[start..];
    // `other` also carries non-namespace tables (`[ignore]`, `[sources.*]`);
    // only an all-comment run is a prelude.
    if run.is_empty() || run.iter().any(|l| !l.trim_start().starts_with('#')) {
        return (other.to_string(), None, String::new());
    }
    let rejoin = |ls: &[&str]| ls.iter().map(|l| format!("{l}\n")).collect::<String>();
    match run.iter().position(|l| parse_provenance(l).is_some()) {
        Some(k) => (
            rejoin(&lines[..start + k]),
            Some(run[k].to_string()),
            rejoin(&run[k + 1..]),
        ),
        None => (rejoin(&lines[..start]), None, rejoin(run)),
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
    /// True when the block's own text is unchanged and only its `# pack:`
    /// line is rewritten — a v1 stamp gaining a baseline (ADR-126 §
    /// DRF-008). Housekeeping, not drift: it never sets `pack outdated`'s
    /// exit code, because the block is by definition already current.
    pub stamp_only: bool,
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

/// One block whose provenance carries no fingerprint, so whether its pack
/// has moved is unanswerable (ADR-126 § DRF-001). Not drift: reported in
/// its own category and never counted toward the exit code.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BlockUnknown {
    /// The on-disk namespace.
    pub namespace: String,
    /// The provenance pack.
    pub pack: String,
    /// The digest that, written into this block's stamp as `sha:<value>`,
    /// declares the installed pack to be its baseline (ADR-126 § DRF-008).
    /// Carried on the row that reports the problem so the remedy is one
    /// command, not a cross-reference into `pack show`.
    pub fingerprint: String,
}

/// The result of planning a `pack migrate` (ADR-053 § PKM-002/004).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct MigratePlan {
    /// Clean blocks that were (or would be) rewritten.
    pub rewrites: Vec<BlockRewrite>,
    /// Dirty blocks left intact, surfaced as diffs.
    pub diffs: Vec<BlockDiff>,
    /// Blocks with no stored baseline — the pack-moved question cannot be
    /// asked of them (ADR-126). Left intact, exit code unaffected.
    pub unknown: Vec<BlockUnknown>,
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
                lead,
                block,
            } => {
                let prov = provenance.as_deref().and_then(parse_provenance);
                let pack_name = prov
                    .as_ref()
                    .map(|p| p.pack.clone())
                    .unwrap_or_else(|| owning_pack_name(&packs, &ns));

                let recipe = migration_recipes()
                    .iter()
                    .find(|r| r.pack == pack_name && r.from_ns == ns);

                let outcome =
                    classify_block(&packs, &ns, &pack_name, &block, prov.as_ref(), recipe);
                apply_outcome(
                    &mut plan,
                    &mut new_config,
                    &ns,
                    &pack_name,
                    BlockText {
                        provenance: provenance.as_deref(),
                        lead: &lead,
                        block: &block,
                    },
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
        /// Only the provenance line changes; the block text is identical.
        stamp_only: bool,
    },
    /// Dirty — leave intact, surface a diff.
    Diff { kind: String, proposed: Vec<String> },
    /// Customized, with no stored baseline to compare the pack against —
    /// leave intact, report separately, never count as drift (ADR-126).
    /// Carries the pack's current digest: the one value that, asserted by
    /// the consumer, gives the block a baseline (DRF-008).
    UnknownBaseline { fingerprint: String },
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
    provenance: Option<&Provenance>,
    recipe: Option<&MigrationRecipe>,
) -> BlockOutcome {
    let stored_sha = provenance.and_then(|p| p.sha.as_deref());
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
                    stamp_only: false,
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
                    stamp_only: false,
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

            // Two questions, deliberately kept apart (ADR-126 § DRF-006).
            //
            // "Is this safe to overwrite?" is byte-equality with the pack's
            // current text, and nothing else — it is the only condition under
            // which re-rendering from the pack loses nothing (PKM-003).
            if block == canon {
                let stamp_current = match stored_sha {
                    // A stale stamp on already-canonical text: refresh the
                    // label, the text is a no-change rewrite.
                    Some(sha) => sha == fingerprint(&canon),
                    // Bare v1 provenance carries no baseline. This block is
                    // byte-identical to the pack, so adopting the pack as its
                    // baseline is *proved*, not assumed — restamp it. Without
                    // this the block reports clean today and falls into the
                    // unresolvable no-baseline category the moment its pack
                    // moves, losing a true drift report (ADR-126 § DRF-008).
                    //
                    // Only for a block that already claims a pack. A block
                    // with no `# pack:` line at all is the consumer's own; it
                    // may happen to match a pack's text, and stamping it would
                    // have ctxgrd claim authorship the user never granted.
                    None => provenance.is_none(),
                };
                return if provenance_current && stamp_current {
                    BlockOutcome::NoOp
                } else {
                    BlockOutcome::Rewrite {
                        to_ns: vec![ns.to_string()],
                        targets: vec![(owner, canon)],
                        // `block == canon` reached this arm, so the only
                        // bytes that move are the provenance line's.
                        stamp_only: true,
                    }
                };
            }

            // The block is customized, so it is never rewritten. The only
            // question left is the one `pack outdated` exists to answer:
            // "has the pack moved since this block was stamped?" The stamp
            // *is* the pack-side baseline — `pack add` writes canonical text
            // and hashes what it writes — so it is compared against the
            // pack's text today, never against the consumer's (DRF-001).
            match stored_sha {
                // The pack has not moved. Consumer customization is not
                // drift, however heavy (DRF-002).
                Some(sha) if sha == fingerprint(&canon) => BlockOutcome::NoOp,
                Some(_) => BlockOutcome::Diff {
                    kind: "internals".to_string(),
                    proposed: vec![canon],
                },
                // No baseline: the question cannot be asked of this block.
                // Saying "drift" would be a guess, so say so instead — and
                // hand over the digest that would answer it, so the consumer
                // can assert what the tool must not assume.
                None => BlockOutcome::UnknownBaseline {
                    fingerprint: fingerprint(&canon),
                },
            }
        }
    }
}

/// The verbatim text of one on-disk block: the `# pack:` line it carried,
/// the comment run introducing it, and the block itself.
struct BlockText<'a> {
    provenance: Option<&'a str>,
    lead: &'a str,
    block: &'a str,
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
    text: BlockText<'_>,
    outcome: BlockOutcome,
) {
    let BlockText {
        provenance,
        lead,
        block,
    } = text;
    // Re-emit a block (its provenance and introducing comment run, if any)
    // verbatim.
    let emit_verbatim = |out: &mut String| {
        if let Some(prov) = provenance {
            out.push_str(prov);
            out.push('\n');
        }
        out.push_str(lead);
        out.push_str(block);
        out.push('\n');
    };
    match outcome {
        BlockOutcome::NoOp => {
            plan.noop_count += 1;
            emit_verbatim(new_config);
        }
        BlockOutcome::Rewrite {
            to_ns,
            targets,
            stamp_only,
        } => {
            plan.rewrites.push(BlockRewrite {
                from_ns: ns.to_string(),
                to_ns,
                pack: pack_name.to_string(),
                stamp_only,
            });
            for (i, (owner, canon)) in targets.iter().enumerate() {
                if i > 0 {
                    new_config.push('\n');
                }
                new_config.push_str(&provenance_comment(owner, canon));
                new_config.push('\n');
                // The comment run introducing the block is the consumer's,
                // not the pack's — a clean swap keeps it.
                if i == 0 {
                    new_config.push_str(lead);
                }
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
        BlockOutcome::UnknownBaseline { fingerprint } => {
            plan.unknown.push(BlockUnknown {
                namespace: ns.to_string(),
                pack: pack_name.to_string(),
                fingerprint,
            });
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
    if plan.rewrites.is_empty() && plan.diffs.is_empty() && plan.unknown.is_empty() {
        out.push_str("Nothing to migrate — every provenance block is at its pack's current shape.\n");
        return out;
    }

    // A stamp-only rewrite changes no block text — it gives a v1 stamp the
    // baseline drift detection needs. Reported apart from real swaps, and
    // never counted as drift (ADR-126 § DRF-008).
    let (restamps, swaps): (Vec<_>, Vec<_>) = plan.rewrites.iter().partition(|r| r.stamp_only);

    if !swaps.is_empty() {
        let verb = if dry_run { "Would migrate" } else { "Migrated" };
        out.push_str(&format!("{verb} {} block(s):\n", swaps.len()));
        for r in &swaps {
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

    if !restamps.is_empty() {
        if !swaps.is_empty() {
            out.push('\n');
        }
        let verb = if dry_run { "Would give" } else { "Gave" };
        out.push_str(&format!(
            "{verb} {} block(s) a baseline (stamp only — the block text is \
             unchanged):\n",
            restamps.len()
        ));
        for r in &restamps {
            out.push_str(&format!("  • [{}] (pack {})\n", r.from_ns, r.pack));
        }
    }

    if !plan.diffs.is_empty() {
        if !plan.rewrites.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "{} block(s) to reconcile by hand (left untouched):\n",
            plan.diffs.len()
        ));
        for d in &plan.diffs {
            // Word each row by what actually made it a diff. Only the
            // `internals` arm consults the stamp; `rename`/`split` fire on a
            // shape change and never asked whether the pack moved.
            let why = match d.kind.as_str() {
                "internals" => "the pack moved since this block was stamped",
                "rename" => "the namespace was renamed",
                "split" => "the namespace was split",
                other => other,
            };
            out.push_str(&format!("\n  [{}] (pack {}) — {why}\n", d.namespace, d.pack));
            out.push_str("    on disk:\n");
            for line in d.on_disk.lines() {
                out.push_str(&format!("      {line}\n"));
            }
            out.push_str("    proposed (the pack's block — it does NOT carry your edits):\n");
            for block in &d.proposed {
                for line in block.lines() {
                    out.push_str(&format!("      {line}\n"));
                }
            }
        }
    }

    if !plan.unknown.is_empty() {
        if !plan.rewrites.is_empty() || !plan.diffs.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "{} block(s) carry no baseline — a pre-v2 `# pack:` stamp, or none:\n",
            plan.unknown.len()
        ));
        // One line per block, carrying its digest: the earlier grouping was
        // right while nothing here was actionable per block, but the remedy
        // is now per block and the digest differs for each.
        for u in &plan.unknown {
            out.push_str(&format!(
                "  [{}] (pack {}) — baseline it with sha:{}\n",
                u.namespace, u.pack, u.fingerprint
            ));
        }
        // No command clears these, and none should: a block reaches this
        // state only by being customized, and nothing can recover which pack
        // revision it was copied from. But the assertion the tool must not
        // make, the consumer can — so name what it costs rather than only
        // saying it is impossible (ADR-126 § DRF-008).
        out.push_str(
            "  No command can resolve these: no baseline was recorded when the block was\n\
             \x20 written, so \"has the pack moved?\" has no answer, and adopting today's\n\
             \x20 pack would be a guess ctxgrd has no standing to make. You do: writing the\n\
             \x20 digest above into the block's `# pack:` line asserts that the installed\n\
             \x20 pack is its baseline. Review the block against `ctxgrd pack show <pack>`\n\
             \x20 first — the assertion is yours. They never set the exit code.\n",
        );
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
        assert_eq!(
            names,
            vec!["ADR", "PRD", "ROADMAP", "RFC", "BUG", "TODO", "README"]
        );
    }

    #[test]
    fn builtin_project_docs_roadmap_is_nnl_shaped() {
        // ADR-088 § RDM-001/002/003/006: nine core.* rules, NNL headings,
        // required owner/date metadata, horizon-as-status vocabulary.
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "project-docs")
            .unwrap();
        let roadmap = namespace_views(&pack)
            .into_iter()
            .find(|v| v.name == "ROADMAP")
            .unwrap();
        assert_eq!(
            roadmap.rules,
            vec![
                "core.frontmatter",
                "core.id",
                "core.id-unique",
                "core.dep-resolved",
                "core.dep-cycle",
                "core.required-headings",
                "core.required-metadata",
                "core.allowed-values",
                "core.min-docs",
            ]
        );
        assert_eq!(
            roadmap.required_metadata,
            vec!["id", "title", "status", "date", "owner"]
        );
        assert_eq!(roadmap.path_patterns, vec!["docs/roadmap/**"]);
        let headings = builtin_pack_headings_for(&pack, "ROADMAP").unwrap();
        assert_eq!(headings, vec!["Problem", "Outcome", "Ideas", "Success Metrics"]);
        let statuses: Vec<String> = pack
            .toml_text
            .parse::<Value>()
            .unwrap()
            .get("ROADMAP")
            .and_then(|v| v.get("core.allowed-values"))
            .and_then(|v| v.get("status"))
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        assert_eq!(statuses, vec!["now", "next", "later", "done", "dropped"]);
    }

    #[test]
    fn builtin_project_docs_prd_binds_min_docs() {
        // ADR-089 § MND-001: PRD is now mandatory when enabled, same as
        // README and the new ROADMAP.
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "project-docs")
            .unwrap();
        let prd = namespace_views(&pack)
            .into_iter()
            .find(|v| v.name == "PRD")
            .unwrap();
        assert!(
            prd.rules.contains(&"core.min-docs".to_string()),
            "PRD binds core.min-docs:\n{:?}",
            prd.rules
        );
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
                "ROADMAP".to_string(),
                "RFC".to_string(),
                "BUG".to_string(),
                "TODO".to_string(),
                "README".to_string()
            ]
        );
        // Every added block carries a provenance comment.
        assert_eq!(plan.blocks_text.matches("# pack: project-docs").count(), 6);
        assert!(plan.blocks_text.contains("[PRD]"));
        assert!(plan.blocks_text.contains("[ROADMAP]"));
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
        assert_eq!(plan.added.len(), 7);
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
        // ADR-105: HANDOFF joined them — the session-continuity store.
        let names = pack_namespace_names("workflow");
        assert_eq!(names, vec!["SPEC", "TASK", "PROMPT", "HANDOFF"]);
        let pack = builtin_packs()
            .into_iter()
            .find(|p| p.name == "workflow")
            .unwrap();
        for v in namespace_views(&pack) {
            // HANDOFF is the one path-claimed namespace here (ADR-105 § HND-001):
            // id-claiming would silently skip a handoff written without an `id`,
            // which is precisely the shape defect the namespace exists to catch.
            if v.name == "HANDOFF" {
                assert_eq!(v.path_patterns, vec!["docs/handoffs/**".to_string()]);
                continue;
            }
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

    // -- ADR-080 § AVS-004 `--pack` provenance resolution --------------

    #[test]
    fn stamped_runs_group_each_comment_with_the_blocks_that_follow_it() {
        // The `pack add` form (one comment per block) and the
        // hand-consolidated form (one comment over a run) in one file,
        // plus hand-written blocks that carry no stamp at all.
        let toml = "\
[ADR]
paths = [\"docs/adrs/**\"]

# pack: marketing
#
# Path-claimed, id-less (ADR-100).
[CAMPAIGN]
paths = [\"docs/strategy/campaigns/**\"]

[PERSONA]
paths = [\"docs/strategy/personas/**\"]

# pack: guide
[GUIDE]
paths = [\"docs/guides/**\"]

[pipeline]
stages = [\"PRD\", \"ADR\"]

[BUG]
paths = [\"docs/bugs/**\"]
";
        let runs = stamped_runs(toml);
        assert_eq!(
            runs,
            vec![
                (
                    "marketing".to_string(),
                    vec!["CAMPAIGN".to_string(), "PERSONA".to_string()]
                ),
                ("guide".to_string(), vec!["GUIDE".to_string()]),
            ]
        );
        // `[ADR]` precedes every stamp and `[BUG]` follows the
        // `[pipeline]` table that ends the run — neither is stamped, so
        // `--pack` cannot reach them (AVS-004's accepted failure mode).
        let stamped: Vec<&str> = runs
            .iter()
            .flat_map(|(_, ns)| ns.iter().map(String::as_str))
            .collect();
        assert_eq!(stamped, vec!["CAMPAIGN", "PERSONA", "GUIDE"]);
    }

    #[test]
    fn stamped_namespaces_keeps_a_trailing_block_only_when_the_pack_declares_it() {
        // `[PRD]` trails the `guide` stamp with no stamp of its own. The
        // guide pack does not declare PRD, so it is not the guide pack's
        // — over-attributing it would lint a namespace nobody scoped.
        let toml = "\
# pack: guide
[GUIDE]
paths = [\"docs/guides/**\"]

[PRD]
paths = [\"docs/prds/**\"]
";
        let root = Path::new(".");
        assert_eq!(
            stamped_namespaces(root, "guide", toml),
            BTreeSet::from(["GUIDE".to_string()])
        );
        // An unstamped block is reachable by no pack at all.
        assert_eq!(
            stamped_namespaces(root, "project-docs", toml),
            BTreeSet::new()
        );
    }

    #[test]
    fn stamped_namespaces_reads_this_repo_own_config() {
        // Dogfood: the live `ctxgrd.toml` carries both the `# pack: name`
        // and the full `# pack: name@version sha:…` stamp forms, and
        // consolidates the marketing pack's four blocks under one comment.
        let toml = include_str!("../ctxgrd.toml");
        let root = Path::new(".");
        assert_eq!(
            stamped_namespaces(root, "marketing", toml),
            BTreeSet::from([
                "CAMPAIGN".to_string(),
                "ICP".to_string(),
                "PERSONA".to_string(),
                "POSITIONING".to_string(),
            ])
        );
        assert_eq!(
            stamped_namespaces(root, "guide", toml),
            BTreeSet::from(["GUIDE".to_string()])
        );
        // The `@version sha:` form resolves identically.
        assert_eq!(
            stamped_namespaces(root, "arc42", toml),
            BTreeSet::from(["ARC42".to_string()])
        );
        // This repo's `[ADR]` block is hand-written, so no pack reaches it.
        assert_eq!(
            stamped_namespaces(root, "project-docs", toml).contains("ADR"),
            false
        );
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

        // The [AGENTS] digest moved when BUG-069 put the block seam ahead of
        // the comment introducing [SKILLS]; both are kept clean-detectable.
        assert_eq!(fingerprint(agents_block), "001e05c08acd19bb");
        assert!(
            !agents_block.contains("SKILLS claims"),
            "[AGENTS] must not carry [SKILLS]'s introduction:\n{agents_block}"
        );
        assert_eq!(fingerprint(skills_block), "f66294963c421057");

        // The recipe constants must contain the re-derived fingerprints.
        let agents_recipe = migration_recipes()
            .iter()
            .find(|r| r.pack == "agents" && r.from_ns == "AGENTS")
            .unwrap();
        assert!(
            agents_recipe
                .clean_fingerprints
                .contains(&fingerprint(agents_block).as_str()),
            "{:?}",
            agents_recipe.clean_fingerprints
        );
        assert!(
            agents_recipe.clean_fingerprints.contains(&"7800619f29dbf307"),
            "the pre-BUG-069 digest stays clean-detectable"
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
        // The bare stamp is upgraded to one carrying a baseline, but the
        // block's own text is untouched.
        assert!(plan.new_config.contains(&adr), "{}", plan.new_config);
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
    fn migrate_gives_a_bare_stamped_current_block_a_baseline() {
        // A bare v1 provenance on a block that equals the current canonical
        // shape. Adopting the pack as this block's baseline is *proved* —
        // the bytes are identical — so migrate restamps rather than leaving
        // it without one. Left bare, the block reports clean today and lands
        // in the unresolvable no-baseline category the moment its pack moves,
        // losing a true drift report (ADR-126 § DRF-008).
        let tmp = tempfile::tempdir().unwrap();
        let claude = builtin_packs()
            .into_iter()
            .find(|p| p.name == "claude")
            .unwrap();
        let canon = canonical_block(&[claude], "CLAUDEAGENTS").unwrap().1;
        let config = format!("# pack: claude\n{canon}\n");
        let plan = plan_migrate(&config, tmp.path());
        assert_eq!(plan.diffs, Vec::new());
        assert_eq!(plan.unknown, Vec::new());
        assert_eq!(plan.rewrites.len(), 1, "the stamp is upgraded");
        assert!(
            plan.new_config
                .contains(&format!("sha:{}", fingerprint(&canon))),
            "the new stamp carries the pack's fingerprint:\n{}",
            plan.new_config
        );
        // The block's own text is byte-identical; only the stamp changed.
        assert!(plan.new_config.contains(&canon), "{}", plan.new_config);
        // And a second run has nothing left to do.
        let second = plan_migrate(&plan.new_config, tmp.path());
        assert_eq!(second.rewrites, Vec::new());
        assert_eq!(second.noop_count, 1);
    }

    // -- drift asks "has the pack moved?" (ADR-126) ---------------------

    /// A `[CLAUDEAGENTS]` block as `pack add claude` writes it, then
    /// customized the three ways this repo's own config customizes blocks:
    /// an `owner` the linter asked for, an added rule, an overridden path.
    fn customized_claudeagents() -> (String, String) {
        let claude = builtin_packs()
            .into_iter()
            .find(|p| p.name == "claude")
            .unwrap();
        let canon = canonical_block(&[claude], "CLAUDEAGENTS").unwrap().1;
        let customized = canon
            .replace("[CLAUDEAGENTS]\n", "[CLAUDEAGENTS]\nowner = \"developer\"\n")
            .replace(
                "paths = [\".claude/agents/**/*.md\"]",
                "paths = [\"agents/**/*.md\"]",
            )
            .replace(
                "rules = [\"agent.frontmatter\"]",
                "rules = [\"agent.frontmatter\", \"core.min-docs\"]",
            );
        assert_ne!(customized, canon, "the customization must bite");
        (canon, customized)
    }

    #[test]
    fn outdated_is_silent_when_the_consumer_customized_but_the_pack_is_current() {
        // ADR-126 § DRF-001/002: drift means "the pack moved", not "you
        // edited it". A block stamped from the current pack and then
        // customized has nothing to report, however heavily it was edited.
        let tmp = tempfile::tempdir().unwrap();
        let (canon, customized) = customized_claudeagents();
        let config = format!("{}\n{customized}\n", provenance_comment("claude", &canon));
        let plan = plan_migrate(&config, tmp.path());
        assert_eq!(plan.diffs, Vec::new(), "consumer customization is not drift");
        assert_eq!(plan.rewrites, Vec::new(), "and is never a clean swap");
        assert_eq!(plan.unknown, Vec::new(), "the baseline is present");
        assert_eq!(plan.noop_count, 1);
    }

    #[test]
    fn migrate_never_rewrites_a_customized_block_whose_pack_is_current() {
        // ADR-126 § DRF-006 / ADR-053 § PKM-003. `outdated` and `migrate`
        // share this classifier, so "clean" must not be read as "safe to
        // overwrite": the arm DRF-001 makes reachable is the one that
        // re-renders from the pack. Every customization must survive.
        let tmp = tempfile::tempdir().unwrap();
        let (canon, customized) = customized_claudeagents();
        let config = format!("{}\n{customized}\n", provenance_comment("claude", &canon));
        let plan = plan_migrate(&config, tmp.path());
        assert!(
            plan.new_config.contains("owner = \"developer\""),
            "owner survives migrate:\n{}",
            plan.new_config
        );
        assert!(
            plan.new_config.contains("core.min-docs"),
            "the added rule survives migrate:\n{}",
            plan.new_config
        );
        assert!(
            plan.new_config.contains("paths = [\"agents/**/*.md\"]"),
            "the paths override survives migrate:\n{}",
            plan.new_config
        );
    }

    #[test]
    fn the_stamp_alone_decides_drift_for_one_identical_customized_block() {
        // The whole of DRF-001 in one test: the same customized block, twice,
        // with the *stamp* as the only varying input. Judged against the
        // consumer's text (the old rule) both are dirty; judged against the
        // pack (the new rule) they differ, and only the second is drift.
        // Held together so neither half can pass for the wrong reason.
        let tmp = tempfile::tempdir().unwrap();
        let (canon, customized) = customized_claudeagents();
        let older_pack_shape = canon.replace(
            "rules = [\"agent.frontmatter\"]",
            "rules = [\"agent.frontmatter\", \"skills.frontmatter\"]",
        );
        assert_ne!(older_pack_shape, canon, "the pack must have moved");

        let plan_for = |baseline: &str| {
            let config = format!("{}\n{customized}\n", provenance_comment("claude", baseline));
            plan_migrate(&config, tmp.path())
        };

        // Stamped from the pack as it is today: the pack has not moved.
        let current = plan_for(&canon);
        assert_eq!(current.diffs, Vec::new());
        assert_eq!(current.noop_count, 1);

        // Stamped from an older pack shape: it has.
        let moved = plan_for(&older_pack_shape);
        assert_eq!(moved.diffs.len(), 1);
        assert_eq!(moved.diffs[0].namespace, "CLAUDEAGENTS");
        assert_eq!(moved.diffs[0].kind, "internals");
        assert_eq!(moved.noop_count, 0);

        // Neither is ever rewritten — the block is customized (DRF-007).
        assert_eq!(current.rewrites, Vec::new());
        assert_eq!(moved.rewrites, Vec::new());
    }

    #[test]
    fn outdated_reports_a_baseline_less_block_as_unknown_rather_than_drift() {
        // ADR-126 Open Question 1: a v1 bare stamp carries no fingerprint, so
        // "has the pack moved?" has no answer. Reporting it as drift is a
        // lie; it belongs in its own category and must not set exit 1.
        let tmp = tempfile::tempdir().unwrap();
        let (_, customized) = customized_claudeagents();
        let config = format!("# pack: claude\n{customized}\n");
        let plan = plan_migrate(&config, tmp.path());
        assert_eq!(plan.diffs, Vec::new(), "no baseline is not drift");
        assert_eq!(plan.rewrites, Vec::new());
        // The row carries the digest that would resolve it — the only thing
        // a consumer can act on, since no command may (ADR-126 § DRF-008).
        // Derived from the pack here rather than pinned as a literal: the
        // claim is "this is the classifier's operand", not "this is hex".
        let (_, canon) = canonical_block(&discover(tmp.path()), "CLAUDEAGENTS").unwrap();
        assert_eq!(
            plan.unknown,
            vec![BlockUnknown {
                namespace: "CLAUDEAGENTS".to_string(),
                pack: "claude".to_string(),
                fingerprint: fingerprint(&canon),
            }]
        );
    }

    #[test]
    fn provenance_binds_through_the_comment_run_above_its_block() {
        // Requiring the stamp to be the immediately preceding line unbound 3
        // of this repo's 4 fingerprints: `pack add` and `init` both write
        // explanatory comments between the stamp and the header, so those
        // blocks silently fell to the no-baseline arm.
        let toml = "\
# pack: research@2.0.2 sha:15fa8bf558f1e44c
#
# Adopted 2026-08-03. ADR-093 shipped this pack unadopted here.
[RESEARCH]
paths = [\"docs/research/**\"]
";
        let bound = config_segments(toml).into_iter().find_map(|s| match s {
            ConfigSegment::Namespace { ns, provenance, .. } if ns == "RESEARCH" => Some(provenance),
            _ => None,
        });
        assert_eq!(
            bound.unwrap().as_deref(),
            Some("# pack: research@2.0.2 sha:15fa8bf558f1e44c")
        );
    }

    #[test]
    fn a_canonical_block_stops_before_the_comment_introducing_the_next_one() {
        // BUG-069: a block ended only at the next header, so the intake
        // pack's [CR] block swallowed the ten-line comment introducing
        // [FEEDBACK] — and `pack add` stamped a fingerprint over it.
        let intake = builtin_packs()
            .into_iter()
            .find(|p| p.name == "intake")
            .unwrap();
        let cr = canonical_block(&[intake], "CR").unwrap().1;
        assert!(
            !cr.contains("FEEDBACK"),
            "[CR] must not carry [FEEDBACK]'s introduction:\n{cr}"
        );
        assert!(cr.contains("[CR.\"core.required-headings\"]"), "{cr}");
    }

    #[test]
    fn planning_a_migrate_of_this_repos_own_config_preserves_every_byte() {
        // The segmentation splits a config across three sinks (`Other` text,
        // a block's `lead`, and the block) and reassembles it. This repo's
        // own ctxgrd.toml is the widest real input available — 20-odd blocks,
        // stamped and bare, with comment runs, sub-tables, `[changelog]` and
        // `[ignore]` between them. Nothing it plans may lose a byte.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let toml = fs::read_to_string(root.join("ctxgrd.toml")).unwrap();
        let plan = plan_migrate(&toml, root);
        assert_eq!(plan.rewrites, Vec::new(), "no rewrite: bytes must round-trip");
        assert_eq!(plan.new_config, toml);
    }

    #[test]
    fn a_clean_swap_keeps_the_comment_run_introducing_the_block() {
        // The lead is the consumer's prose, not the pack's; a rewrite that
        // re-renders the block from the pack must not drop it.
        let tmp = tempfile::tempdir().unwrap();
        let config = format!(
            "# pack: claude\n# Why this repo claims Claude's agent files.\n{}\n",
            clean_claudecode_block()
        );
        let plan = plan_migrate(&config, tmp.path());
        assert_eq!(plan.rewrites.len(), 1, "the rename is a clean swap");
        assert!(
            plan.new_config
                .contains("# Why this repo claims Claude's agent files."),
            "the introducing comment survives the swap:\n{}",
            plan.new_config
        );
    }

    #[test]
    fn a_blank_line_between_sub_tables_stays_in_the_block() {
        // Regression in this change: holding blank lines back to find the
        // block seam dropped the blank *between* two sub-tables. Nothing
        // comparing on-disk text to canonical can see it — both sides lose
        // the line together — so it only surfaces against a stored stamp.
        let toml = "[ADR]\nrules = [\"core.id\"]\n\n[ADR.\"core.allowed-values\"]\nstatus = [\"draft\"]\n";
        assert_eq!(namespace_blocks(toml)[0].1, toml.trim_end());
    }

    #[test]
    fn block_extraction_is_stable_against_a_stamp_an_earlier_binary_wrote() {
        // A stored `sha:` is the one oracle that can catch an extraction
        // change: every other comparison in this file re-derives both sides,
        // so a change that shifts them together is invisible. This repo's
        // [ARC42] stamp was written by `pack add` at 0.36.0 (fc8726c) and the
        // arc42 pack has one commit in its history, so the digest below is
        // what an untouched pack must still produce. Editing
        // packs/arc42/pack.toml is a real reason for this to fail; changing
        // how a block is cut out of a file is not.
        //
        // [ARC42] is the sole namespace in its pack and the last thing in the
        // file, so it pins the *interior* of a block only — but its digest is
        // independent evidence, written by a binary that predates this code.
        let packs = discover(Path::new(env!("CARGO_MANIFEST_DIR")));
        let (_, arc42) = canonical_block(&packs, "ARC42").expect("local arc42 pack");
        assert_eq!(fingerprint(&arc42), "ccc0b498cc251747", "interior");

        // [CR] pins the seams: it is followed by the comment run introducing
        // [FEEDBACK]. Its digest is a forward pin, not evidence — ADR-126
        // moved this seam deliberately, so the value is what today's rule
        // produces. The shape assertion beside it is what carries the intent.
        let (_, cr) = canonical_block(&packs, "CR").expect("intake pack");
        assert!(
            !cr.contains("FEEDBACK") && cr.ends_with("headings = [\"Summary\", \"References\"]"),
            "[CR] ends at its own last key, not inside its neighbour:\n{cr}"
        );
        assert_eq!(fingerprint(&cr), "79ebf75f5c1a1492", "seam");
    }

    #[test]
    fn a_block_stops_at_a_non_namespace_table() {
        // Same seam, other direction: this repo's [FEEDBACK] block swallowed
        // the entire [changelog] table, so its fingerprint covered config
        // with nothing to do with the namespace — and editing `[changelog]`
        // reported [FEEDBACK] as damaged.
        let tmp = tempfile::tempdir().unwrap();
        let toml = "\
# pack: intake
[FEEDBACK]
paths = [\"docs/feedback/**\"]

[changelog]
namespaces = [\"BUG\"]
";
        let block = config_segments(toml)
            .into_iter()
            .find_map(|s| match s {
                ConfigSegment::Namespace { ns, block, .. } if ns == "FEEDBACK" => Some(block),
                _ => None,
            })
            .unwrap();
        assert!(!block.contains("changelog"), "{block}");
        // And the table itself still survives the round trip.
        let plan = plan_migrate(toml, tmp.path());
        assert_eq!(plan.new_config, toml, "every byte preserved");
    }

    #[test]
    fn deleting_a_namespace_and_its_intro_comment_leaves_the_predecessor_clean() {
        // BUG-069 end-to-end: take intake exactly as `pack add` writes it,
        // then delete [FEEDBACK] together with the comment that introduces
        // it. [CR] was not touched, so it must stay clean.
        let tmp = tempfile::tempdir().unwrap();
        let intake = builtin_packs()
            .into_iter()
            .find(|p| p.name == "intake")
            .unwrap();
        let added = plan_add(&intake, "", tmp.path()).blocks_text;
        let cut = added
            .find("# FEEDBACK")
            .unwrap()
            .min(added.rfind("# pack: intake").unwrap());
        let config = format!("{}\n", added[..cut].trim_end());
        assert!(config.contains("[CR]") && !config.contains("[FEEDBACK]"), "{config}");
        let plan = plan_migrate(&config, tmp.path());
        assert_eq!(plan.diffs, Vec::new(), "deleting a neighbour is not drift");
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
