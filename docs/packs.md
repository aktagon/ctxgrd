# Rule packs

A pack is a named, reusable bundle of namespace config plus the external
rule scripts it needs. Applying a pack writes its `[<NAMESPACE>]` blocks
into your `ctxgrd.toml` and copies any rule scripts it ships — then walks
away. There is no runtime tie: ctxgrd never reads packs while linting, so
behaviour is identical whether or not the pack is still on disk afterward.

Packs solve two problems: standing up a multi-namespace documentation
suite without hand-copying the same config five times, and distributing an
opinionated convention across repositories by committing it to git.

## Inspect before you adopt

The two read-only commands never modify a file:

```
ctxgrd pack list                 # every discoverable pack + its source
ctxgrd pack show project-docs    # the namespaces, rules, and scripts it defines
```

### The machine-readable contract

`pack show <name> --format json` emits the **complete** namespace contract — not a
summary of it. Every `[NS."rule.code"]` param table the pack declares appears under
`namespaces[].params`, keyed by rule code: closed vocabularies in full, freshness
windows, conditional-rule params. Nothing is withheld (ADR-113 § PKJ-001).

This is the surface a tool or an agent should read when it needs to know a document's
shape, instead of restating that shape somewhere it can drift (ADR-076 § OWN-001/002).

```
ctxgrd pack show soc2 --format json | jq '.namespaces[0].params."core.allowed-values".criterion'
```

Two properties are contractual:

- **The output is canonical.** Object keys are sorted, arrays keep their declared order,
  and two invocations are byte-identical. So you can pin a digest you compute yourself —
  `shasum -a 256 <(ctxgrd pack show soc2 --format json)` — and fail when the pack moves.
  ctxgrd emits no digest field of its own (ADR-113 § PKJ-002).
- **`scope` says which artifact you are reading.** It is always `pack-definition`: the
  pack as shipped, *not* your project's `ctxgrd.toml`. `pack add` copies blocks into your
  config, so the two diverge as a pack evolves. `depends` names the base pack, whose
  namespaces are part of the real contract (`soc2` layers over `security`).

`pack outdated` reports the gap between your `ctxgrd.toml` and the pack definition. **It
never sees a downstream tool or skill** — do not cite it as evidence that anything outside
your config is in sync (ADR-113 § PKJ-004).

## Applying a pack

```
ctxgrd pack add project-docs           # append its blocks to ctxgrd.toml
ctxgrd pack add project-docs --dry-run # print what it would write, touch nothing
```

`pack add` creates `ctxgrd.toml` if absent and appends to it if present.
It never overwrites a namespace block you already have: if the pack and
your config both define `[ADR]`, the pack's `[ADR]` is skipped and the
skip is reported. Each block it does write carries a versioned, fingerprinted
provenance comment — `# pack: <name>@<version> sha:<hash>` (ADR-053) — so you
can see where it came from. The `sha:` is the fingerprint of the *pack's* block
at the moment it was copied, which is what lets a later `pack outdated` tell
"the pack has moved since you copied it" from "you edited your copy"
(ADR-126). The older bare `# pack: <name>` form is still valid input and an
older binary treats the `@version sha:` suffix as an inert comment — but a
block carrying no `sha:` has no baseline, so the pack-moved question cannot be
asked of it at all.

`init --pack` is sugar for the common first-run case — it is equivalent to
`init` followed by `pack add` for each name:

```
ctxgrd init --pack project-docs,agents
```

## Keeping adopted packs current

A pack is a generator, not a live dependency (ADR-013): `pack add` inlines a
snapshot. When a pack's definition later evolves — a namespace renamed or split
(e.g. ADR-061's `CLAUDECODE`→`CLAUDEAGENTS`, ADR-051's `agents`-pack split) —
two commands propagate the change into an already-adopted config without
clobbering your edits (ADR-053):

```
ctxgrd pack outdated                      # report blocks whose pack shape moved
ctxgrd pack migrate --dry-run             # show what migrate would do, write nothing
ctxgrd pack migrate --dry-run --format json   # the same, machine-readable
ctxgrd pack migrate                       # rewrite clean blocks in place
```

`pack migrate` compares each provenance-stamped block against its pack's current
shape. A block that is byte-for-byte the generated shape (a "clean" block) is
rewritten to the new shape and re-stamped. A block you edited is **left
untouched** — always, whatever its drift verdict — and migrate never merges or
overwrites your edits. Running it twice is a no-op.

`pack outdated` asks one question of each block: **has the pack moved since this
block was stamped?** It compares the stored `sha:` against the pack's block
today, so adding a rule, overriding `paths`, customizing a rule's params or
setting `owner` is not drift, however much of the block you have rewritten
(ADR-126). Blocks fall into three groups:

- **drift** — the pack moved; the diff (your block and the pack's current shape)
  is printed for you or an agent to reconcile by hand. Sets exit `1`.
- **no baseline** — the block carries a bare `# pack: <name>` stamp, or none, so
  there is nothing to compare the pack against and the question has no answer.
  Reported by name, never as drift. A block gains a baseline the next time
  `pack add` or `pack migrate` writes it.
- **current** — silent.

Exit codes follow the contract: `pack outdated` exits `0` when no pack has moved
and `1` when one has; `pack migrate` exits `0` when it finishes with no
unresolved dirty blocks and `1` when dirty blocks remain to reconcile; both exit
`2` on a config error. So CI or an agent can branch on the outcome without
parsing text.

## Built-in packs

- `project-docs` — `ADR`, `PRD`, `RFC`, and `BUG` doc types with status
  vocabularies and required headings, plus a path-claimed `TODO` namespace that
  lints the repo-root `TODO.md` (freshness line + checklist structure) and a
  `README` namespace for the repo's front door (`core.min-docs` so one exists,
  `core.requires-link` so it points at the entry guide `docs/guides/getting-started.md`
  when present — skip-if-missing, warning; ADR-055).
  `ears.clause-syntax` (ADR-031) parses `EARS-<NN>`-id'd clauses under a
  PRD's `Requirements` heading against the six EARS patterns; self-gating —
  a PRD with no `EARS-` ids is untouched.
- `ops` — the incident-management doc lifecycle, grounded in Google's SRE
  Book: `RUN` runbooks (`docs/runbooks/**`; "playbooks" in SRE ch. 11 terms —
  Trigger / Prerequisites / Steps / Rollback / Verification) and `PMR`
  postmortems (`docs/pmrs/**`; headings follow the blameless-postmortem
  template from SRE Book Appendix D, `incident_date` required). Split from
  `project-docs` because adopting an incident process is a separate decision
  from adopting ADRs.
- `agents` — everything a coding agent reads, is driven by, and is documented
  through. Five namespaces in one pack (ADR-023):
  - `AGENTS` (path-claim) — `CLAUDE.md`, `AGENTS.md`, `GEMINI.md`; linted by
    the compiled `agents.*` rules: headings, budget, and commit-context cache.
  - `SKILLS` (path-claim) — `.claude/skills/**/SKILL.md` and
    `.codex/skills/**/SKILL.md`; `skills.frontmatter` errors when the file
    lacks a non-empty `name` or `description`.
  - `SPEC` (id-claim) — design artifacts; requires a `PRD-<n>` in `depends_on`
    via `core.dep-shape` with `requires = ["PRD"]`. `ears.clause-syntax` (ADR-031)
    parses `EARS-<NN>.<M>`-id'd `Requirements` clauses against the six EARS
    patterns; self-gating — a SPEC with no `EARS-` ids is untouched.
  - `TASK` (id-claim) — bounded executable slices; opt-in
    `tasks.files-allowed` warns on missing `Files allowed` paths.
  - `PROMPT` (id-claim) — reusable prompts; structural rules only.

  Path-claimed namespaces (AGENTS, SKILLS) fire immediately on adoption;
  id-claimed ones (SPEC, TASK, PROMPT) activate when you create a document.
  `pack add agents` prints a receipt that shows this split.
- `guide` — end-user documentation (ADR-055). One path-claimed namespace,
  `GUIDE` (`docs/guides/**`), for the docs a *user* reads. Guides are id-less —
  the filename is the slug — and typed by the Diátaxis taxonomy via the
  `guide.frontmatter` rule, which requires a non-empty `title` and a `diataxis`
  object whose `type` is from a config-supplied allowlist (the pack ships
  `tutorial` / `how-to` / `reference` / `explanation`; the binary hardcodes no
  taxonomy). The class lives under `diataxis` rather than a top-level `type:`,
  which Hugo/Jekyll/Eleventy reserve as a layout selector (BUG-015). `core.min-docs` nudges a
  project that has no guides to write one. Prose quality — a how-to titled by its
  goal, a reference written dry — is the writer's job (see the
  `writing-end-user-guide` skill), not a structural rule.
- `c4` — architecture diagrams (ADR-075). One path-claimed namespace, `C4`
  (`docs/diagrams/**`), for the box-and-line views of a system typed by Simon
  Brown's C4 model. Diagram docs are id-less — the filename is the slug — and the
  `c4.frontmatter` rule requires a non-empty `title` and a `c4` object whose
  `level` is from a config-supplied allowlist (the pack ships the four model
  levels `context` / `container` / `component` / `code` plus the supplementary
  `deployment` / `dynamic` / `landscape` views; the binary hardcodes no
  taxonomy). The level lives under `c4` rather than a top-level `type:`, which
  SSGs reserve (BUG-015). `core.min-docs` nudges a project that has opted in but
  drawn no diagram. The diagram itself stays diagrams-as-code — a ` ```mermaid ` (or
  ` ```dot `) block inside the same `.md` file — which the markdown walker already
  lints; ctxgrd checks the markdown envelope, never the embedded graph. There is
  no `.mmd` source support: raw Mermaid is a non-markdown format, out of scope for
  the core walker (use an external source script if you need it).
- `checklist` — auditable, commit-pinned checklists (ADR-078). One path-claimed
  namespace, `CHECKLIST` (`docs/checklists/**`), for a checklist whose "done"
  state is a signed-off record, not an aspirational list. Checklists are id-less —
  the filename is the slug — with a two-state `status` (`living` | `sealed`). A
  `living` checklist (the template or an in-flight instance) is never gated; a
  `sealed` one is hard-gated by `checklist.complete` (zero unchecked boxes) and
  `checklist.pinned` (its `pinned_commit` must be a 40-hex SHA that resolves and is
  an ancestor of `HEAD`), so a green sealed checklist is tied to a named
  integration commit that landed. `checklist.pinned` shells out to git, so CI must
  fetch full history (`fetch-depth: 0`); it degrades to a warning in a shallow
  clone rather than a false error. The pack also binds the generic, config-driven
  `core.required-headings` — shipped with no section set (a bare checklist requires
  no particular sections); supply your own to require a shape, e.g. the seven-phase
  Stripe web-integration checklist shown commented in the pack's `pack.toml`.
  Matching strips a leading enumerator and is case-insensitive, so a heading may be
  numbered (`## 1. Plan`) while the config is not.
- `ddd` — strategic Domain-Driven Design docs on the dependency graph (ADR-082).
  Two id-claimed namespaces. `BOUNDEDCONTEXT` (`docs/ddd/bounded-contexts/**`,
  ids `BOUNDEDCONTEXT-<n>`) is the anchor artifact — an Evans model/language
  boundary with its Ubiquitous Language, aggregates, and domain events folded in
  as required headings (Purpose, Ubiquitous Language, Aggregates, Domain Events,
  Boundaries, Team / Ownership, Open Questions, References) alongside `status`
  (`draft`/`active`/`deprecated`) and `subdomain_type` (`core`/`supporting`/`generic`)
  required metadata, both config-overridable allowlists. `CONTEXTMAP`
  (`docs/ddd/context-maps/**`, ids `CONTEXTMAP-<n>`) models one relationship edge
  per file, carrying `depends_on: [BOUNDEDCONTEXT-x, BOUNDEDCONTEXT-y]` and a
  `pattern` drawn from Evans' eight strategic patterns (Partnership, Shared
  Kernel, Customer-Supplier, Conformist, Anticorruption Layer, Open Host Service,
  Published Language, Separate Ways). Both namespaces are id-claimed rather than
  path-claimed like `guide`/`c4` — DDD's value is the graph of relationships
  between contexts, so cross-references ride the existing `depends_on` dependency
  graph (`core.dep-resolved`, `core.dep-cycle`) instead of a namespace reading a
  sibling file off disk. The new `ddd.context-map-shape` rule catches what
  `core.dep-shape` cannot express on its own: a context map must resolve to
  exactly two `BOUNDEDCONTEXT` ids, and direction fields (`upstream`/`downstream`)
  are required for asymmetric patterns (Customer-Supplier, Conformist,
  Anticorruption Layer, Open Host Service, Published Language) and forbidden for
  symmetric ones (Partnership, Shared Kernel, Separate Ways). Ids use the long
  namespace form (`BOUNDEDCONTEXT-<n>`, not `BC-<n>`) because a document's
  namespace is derived from its id prefix, so the `[BOUNDEDCONTEXT]` section name
  and the id prefix are the same string. Tactical patterns (Value Objects,
  Entities, Repositories, Domain Services) are cut outright — they are
  code-shaped and belong in code, not docs.
- `governance` — a program/governance decision register (ADR-092). One id-claimed
  namespace, `DEC` (`docs/decisions/[0-9]*.md`, ids `DEC-<n>`), for the
  program-management **decision register** standard — one record per material
  program decision, the directory listing *is* the register. Pure `core.*`: required
  headings `Decision` / `Rationale` / `Impact` / `Approval`, required metadata `id` /
  `title` / `status` / `decision-maker` / `date`, and a governance `status`
  vocabulary (`proposed` / `pending` / `approved` / `rejected` / `deferred` /
  `superseded`), all config-overridable. Deliberately *not* the `ADR` namespace: an
  ADR is a technical decision owned by engineering; a `DEC` is a program/strategic
  decision emphasizing **authority** (the required `decision-maker`) and cross-cutting
  **impact** (scope / cost / schedule / benefits / risk / stakeholders) over technical
  consequences. `DEC`→`DEC` supersession rides the `depends_on` graph
  (`core.dep-cycle`, `core.successor-link`); external evidence (a board paper, change
  request, minutes) goes in a free-form `evidence:` key so it never trips
  `core.dep-resolved`. Ships no `core.min-docs`, so it is safe to adopt ahead of
  recording the first decision. The first member of the RAID+ register family, kept
  in its own opt-in pack the way the change register (`CR`) lives in `intake`.

## Pack layout

A pack is a directory containing a `pack.toml` and an optional `rules/`
subtree. No new file format: `pack.toml` uses the same grammar as the
`[<NS>]` sections of `ctxgrd.toml`, and bundled rule scripts follow the
external-rule contract (see `ctxgrd docs rules`). The built-in packs ship
no scripts — their rules are compiled in — but your own local or global
packs may bundle them.

```
packs/team-docs/pack.toml                 →  pack name: team-docs
packs/team-docs/rules/team/freshness/run  →  bundled rule code: team.freshness
```

A `# summary:` comment on the first line of `pack.toml` is shown in
`pack list`.

## Discovery

`pack list` and `pack show` discover packs from three sources, in order:

1. packs compiled into the binary (built-in),
2. `~/.ctxgrd/packs/*` (per-user global),
3. `./packs/*` relative to `--root` (per-repo local).

When two sources define a pack of the same name, the more local source
wins, and the resolved source is reported. Committing `./packs/<name>/`
to a repository is how a team shares a pack — git is the distribution
channel, so no registry is needed.

## First-touch stays silent

Adopting a pack defines namespaces; it does not re-classify your existing
markdown. ctxgrd stays quiet until a file claims intent — either an
`id: <NS>-<N>` frontmatter field or a match against a namespace's `paths`
glob (see `ctxgrd docs namespaces`). Trying a pack on a brownfield repo
produces no wall of diagnostics.

## Removing a pack

There is no `pack remove` verb. Because a pack leaves no runtime tie,
removal is an ordinary config edit: delete the `# pack: <name>` block(s)
from `ctxgrd.toml` and any rule scripts under `rules/` you no longer use.
