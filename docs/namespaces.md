# Setting up namespaces

A namespace is the uppercase prefix in a document ID — `ADR` in `ADR-001`,
`PRD` in `PRD-042`. The namespace determines which rules run against that
document. Each namespace is configured by a top-level section in
`ctxgrd.toml`.

## How a file becomes a document

ctxgrd is a markdown linter, not a markdown crawler. A `.md` file under
the lint root is treated as a document candidate **only when it claims
intent**. Two mechanisms claim intent:

1. **id-claim** — the file's frontmatter contains an `id` field shaped
   `<NAMESPACE>-<number>` for a configured namespace (`id: ADR-001`).
2. **path-claim** — the file's location matches one of a configured
   namespace's `[<NS>].paths` globs.

Files satisfying neither are silently skipped — no `core.frontmatter`,
no `core.id`, no diagnostic of any kind. A repo-root README with
`---\ntitle: My Site\n---` Hugo-style frontmatter does not need an
`[ignore]` entry; under intent-based classification it simply isn't a
ctxgrd document.

The id-claim path is enough on its own — a file with `id: ADR-001`
anywhere in the tree is classified as an ADR even without
`[ADR].paths`. Most teams configure both: paths describe the canonical
home of each namespace's documents, ids stay authoritative for
identity.

## Minimal setup

Drop a `ctxgrd.toml` at the root of your document tree. The minimum
useful config assigns a path glob and a rule list:

```toml
[ADR]
paths = ["docs/adrs/**"]
rules = [
  "core.frontmatter",
  "core.id",
  "core.id-unique",
  "core.dep-resolved",
  "core.dep-cycle",
  "core.cross-ref",
]
```

With this, every `.md` file under `docs/adrs/` is an ADR document
candidate (path-claim). Any file outside that tree without an `id` is
silently skipped.

Run `ctxgrd init` to generate a `ctxgrd.toml` with sensible defaults
rather than writing one by hand:

```sh
ctxgrd init                        # ADR + PRD active; DDR/RFC/RUN/PMR commented
ctxgrd init --namespaces ADR,PRD   # only those two, nothing commented
ctxgrd init --stdout               # print without writing (preview)
```

`init` sniffs the tree for conventional ADR/PRD-shaped directories
(`docs/adrs/`, `adrs/`, `docs/decisions/`, …) and pre-fills
`[<NS>].paths` for any matches it finds. The namespaces it pre-fills
are announced on stderr.

## Configuring `[<NS>].paths`

`paths` is a list of globs, matched relative to the lint root.

```toml
[ADR]
paths = ["docs/adrs/**", "docs/decisions/**"]
```

**Set semantics.** Multiple entries form a set; their order does not
matter. A file is path-claimed by the namespace if it matches **any**
entry in the list.

**Glob syntax.** Globset syntax, not gitignore: patterns are anchored at
the lint root (a slash-free pattern like `TODO.md` matches only a
root-level entry — prefix `**/` to match at any depth), and `*` matches
across `/` separators (prefer `**` to make any-depth intent explicit).
`?` matches a single character.

**Negation is not supported.** Patterns starting with `!` (e.g.
`!docs/adrs/superseded/**`) are not allowed in `[<NS>].paths`. Each
config primitive owns one job: `[<NS>].paths` claims files for a
namespace; `[ignore].patterns` excludes files from linting entirely.
To exclude superseded ADRs from linting, add them to
`[ignore].patterns` instead.

**Absolute paths are rejected.** `paths = ["/abs/path/**"]` produces a
`cfg.paths-invalid` configuration error. ctxgrd is a per-repo tool; all
patterns must be root-relative.

**`[ignore]` wins.** When a file matches both `[<NS>].paths` and an
`[ignore].patterns` entry, `[ignore]` takes precedence and the file is
not walked at all. To silence a path-claimed file without tightening
the path glob, add it to `[ignore].patterns`.

**Source-emitted documents do not use `[<NS>].paths`.** External sources
(see `docs/sources.md`) emit documents with synthetic locations;
`[<NS>].paths` does not apply to them. Source-emitted documents are
classified by their `id` only.

## Adding parameterized rules

Three core rules accept parameters. Add them to the `rules` list, then
configure them in a sub-table named `[<NS>."<rule-code>"]`:

```toml
[ADR]
paths = ["docs/adrs/**"]
rules = [
  "core.frontmatter",
  "core.id",
  "core.id-unique",
  "core.dep-resolved",
  "core.dep-cycle",
  "core.cross-ref",
  "core.required-headings",   # requires sub-table below
  "core.required-metadata",   # requires sub-table below
  "core.allowed-values",      # requires sub-table below
]

[ADR."core.required-headings"]
headings = ["Status", "Context", "Decision", "Consequences"]

[ADR."core.required-metadata"]
keys = ["id", "title", "status"]

[ADR."core.allowed-values"]
status = ["draft", "accepted", "rejected", "superseded"]
```

**`core.required-headings`** — every listed string must appear as an
H2 heading (`## Heading`) in the document body. Exact-match,
case-sensitive.

**`core.required-metadata`** — every listed key must be present in the
unified metadata map (frontmatter keys for local files, `extra` fields
for source-derived documents). You can require domain-specific keys:

```toml
[PMR."core.required-metadata"]
keys = ["id", "title", "status", "incident_date"]
```

**`core.allowed-values`** — each listed key's value must be in the
given set. Works identically for frontmatter and source `extra` fields:

```toml
[JIRA."core.allowed-values"]
status = ["Open", "In Progress", "Closed"]
```

## Multiple namespaces

Each namespace is its own top-level section. They are independent — paths,
rules, parameters, and headings differ per namespace:

> Standing up several namespaces at once? Rather than hand-write a block
> per doc type, a **rule pack** can generate them — `ctxgrd pack add
project-docs` writes ADR, PRD, RFC, BUG, and TODO blocks in one step
> (`ctxgrd pack add ops` adds RUN runbooks and PMR postmortems).
> See `ctxgrd docs packs` (or [`packs.md`](packs.md)).

```toml
[ADR]
paths = ["docs/adrs/**"]
rules = ["core.frontmatter", "core.id", ..., "core.required-headings"]

[ADR."core.required-headings"]
headings = ["Status", "Context", "Requirements", "Consequences", "Open Questions", "References", "Change log"]

[PRD]
paths = ["docs/prds/**"]
rules = ["core.frontmatter", "core.id", ..., "core.required-headings"]

[PRD."core.required-headings"]
headings = ["Context", "Goals", "Non-goals", "User stories", "Requirements", "Definition of Done", "Open Questions", "References", "Change log"]
```

If two namespaces' `paths` overlap, ctxgrd resolves the conflict by
the file's id-claim (a file with `id: ADR-001` matched by both
`[ADR].paths` and `[PRD].paths` is classified as ADR). Files without
an id under overlapping paths produce a `cfg.path-conflict`
configuration error — they cannot be linted under two rule sets at
once.

## Core rules reference

| Rule code                | Parameterized | What it checks                                                |
| ------------------------ | ------------- | ------------------------------------------------------------- |
| `core.frontmatter`       | No            | YAML front-matter parses without error                        |
| `core.id`                | No            | `id` key is present and matches `<NAMESPACE>-<number>` format |
| `core.id-unique`         | No            | No two documents in the run share the same ID                 |
| `core.dep-resolved`      | No            | Every `depends_on` entry resolves to a known document         |
| `core.dep-cycle`         | No            | The `depends_on` graph contains no cycles                     |
| `core.cross-ref`         | No            | Every `NS-NNN` token in the body resolves to a known document |
| `core.required-headings` | Yes           | Required H2 headings are present                              |
| `core.required-metadata` | Yes           | Required metadata keys are present                            |
| `core.allowed-values`    | Yes           | Metadata values are in their configured allow-list            |

The first six rules need no parameters and can be added to any
namespace without a sub-table. If you include a parameterized rule but
omit its sub-table, ctxgrd exits with code 2 (kernel error) at startup.

`core.frontmatter` and `core.id` (IdMissing) only fire for files that
claim intent. A file with frontmatter but no `id` and no `[<NS>].paths`
match is silently skipped — those rules never see it.

## Global overrides

User-level config lives at `~/.ctxgrd/namespaces/<NS>.toml`. If that
file exists for a given namespace, it replaces the local `ctxgrd.toml`
section for that namespace entirely (whole-table replacement — no
per-rule merging).

This is useful for personal preferences (stricter heading requirements,
extra rules) that you don't want to impose on the team via the repo
config.

```sh
# ~/.ctxgrd/namespaces/ADR.toml
[ADR]
paths = ["docs/adrs/**"]
rules = ["core.frontmatter", "core.id", ..., "core.required-headings", "adr.my-personal-rule"]

[ADR."core.required-headings"]
headings = ["Status", "Context", "Decision", "Consequences", "References"]
```

## Scaffolding documents

Once a namespace is configured, `ctxgrd new` scaffolds a new document:

```sh
ctxgrd new ADR "Use append-only object storage"
# → docs/adrs/ADR-002-use-append-only-object-storage.md
```

The scaffolder picks `max(existing IDs in namespace) + 1`, slugifies
the title, stubs every key in `core.required-metadata.keys`, and lays
out an empty section per heading in `core.required-headings.headings`.
The file passes `ctxgrd` immediately after creation — the writer fills
in the body.

Useful flags:

```sh
ctxgrd new ADR "Title" --stdout    # don't write, just print
ctxgrd new ADR "Title" --id 7      # force a specific number
ctxgrd new ADR "Title" --out docs/ # write to a custom directory
```

## Checking the resolved rule set

Before drafting, writers can see exactly what ctxgrd will check:

```sh
ctxgrd rules --namespace ADR
```

This shows every rule resolved for the namespace, its source (core or
external), and its effective parameters. Use
`ctxgrd rules --format json` to pipe the output to tooling.
