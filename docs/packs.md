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

## Applying a pack

```
ctxgrd pack add project-docs           # append its blocks to ctxgrd.toml
ctxgrd pack add project-docs --dry-run # print what it would write, touch nothing
```

`pack add` creates `ctxgrd.toml` if absent and appends to it if present.
It never overwrites a namespace block you already have: if the pack and
your config both define `[ADR]`, the pack's `[ADR]` is skipped and the
skip is reported. Each block it does write carries a `# pack: <name>`
provenance comment so you can see where it came from.

`init --pack` is sugar for the common first-run case — it is equivalent to
`init` followed by `pack add` for each name:

```
ctxgrd init --pack project-docs,agents
```

## Built-in packs

- `project-docs` — `ADR`, `PRD`, `RFC`, and `BUG` doc types with status
  vocabularies and required headings, plus a path-claimed `TODO` namespace that
  lints the repo-root `TODO.md` (freshness line + checklist structure).
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
    via the compiled `spec.requires-prd` rule. `ears.clause-syntax` (ADR-031)
    parses `EARS-<NN>.<M>`-id'd `Requirements` clauses against the six EARS
    patterns; self-gating — a SPEC with no `EARS-` ids is untouched.
  - `TASK` (id-claim) — bounded executable slices; opt-in
    `tasks.files-allowed` warns on missing `Files allowed` paths.
  - `PROMPT` (id-claim) — reusable prompts; structural rules only.

  Path-claimed namespaces (AGENTS, SKILLS) fire immediately on adoption;
  id-claimed ones (SPEC, TASK, PROMPT) activate when you create a document.
  `pack add agents` prints a receipt that shows this split.

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
