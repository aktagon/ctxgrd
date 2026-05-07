# ctxgrd

Lint your context.

A command-line linter for the structured markdown your team writes — ADRs, PRDs, post-mortems, runbooks. Anything with `<NAMESPACE>-<number>` IDs and YAML frontmatter. Markdown only. On purpose.

ctxgrd checks the contracts between those documents. Cross-references that don't resolve. Dependency cycles. Frontmatter that doesn't match the schema. Broken IDs after a renumber. The kind of drift that nobody notices until an LLM reads it back.

![ctxgrd demo](assets/demo.svg)

## How it works

You write ADRs and PRDs. Files reference each other (`see ADR-007 § REF-005`). Source code references documents (`// per ADR-001`). Over time, those references rot.

ctxgrd's model is pointers and a DAG. Each document is a named node (`ADR-001`, `PRD-042`). Cross-references in body text, `depends_on` entries in frontmatter, scanner hits in source code — those are pointers. The whole set forms a directed acyclic graph. The rules check the graph holds: every pointer resolves to a real node, no node has a duplicate name, no dependency cycles.

Same shape as `cargo check`. Different graph: pointers between documents instead of types between modules. Three-valued exit code: `0` clean, `1` lint failures, `2` kernel error. Designed for CI.

A file becomes a node one of two ways. Frontmatter contains `id: ADR-001` (id-claim). Or the path matches a configured `[<NS>].paths` glob (path-claim). Files with neither are skipped without a diagnostic. You can run ctxgrd in a repo full of Hugo pages, design tokens, and prompt files. It won't fire on every README.

## Install

Pre-built binaries are attached to each release. Linux x86_64, macOS Intel and Apple Silicon, Windows x86_64.

```sh
# Linux x86_64
curl -fsSL https://github.com/aktagon/ctxgrd/releases/latest/download/ctxgrd-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz -C ~/.local/bin

# macOS Apple Silicon
curl -fsSL https://github.com/aktagon/ctxgrd/releases/latest/download/ctxgrd-aarch64-apple-darwin.tar.gz \
  | tar -xz -C ~/.local/bin

# macOS Intel
curl -fsSL https://github.com/aktagon/ctxgrd/releases/latest/download/ctxgrd-x86_64-apple-darwin.tar.gz \
  | tar -xz -C ~/.local/bin
```

Make sure `~/.local/bin` is on your `PATH`.

From source:

```sh
cargo install --git https://github.com/aktagon/ctxgrd --tag v0.4.0
```

Or clone and build:

```sh
git clone --branch v0.4.0 https://github.com/aktagon/ctxgrd.git
cd ctxgrd-public
make install
```

crates.io publish is not yet shipped.

## Five-minute tour

```sh
ctxgrd init                           # scaffold ctxgrd.toml with sensible defaults
ctxgrd --root docs/                   # lint a tree
ctxgrd new ADR "Use event sourcing"   # scaffold a new record
ctxgrd rules --namespace ADR          # show resolved rules for one namespace
ctxgrd lint --format json             # machine-readable diagnostics for CI
ctxgrd refs ADR-001                   # show every pointer to a document
ctxgrd docs <topic>                   # bundled user docs
```

`ctxgrd --help` for the full surface.

## Configuration

Drop a `ctxgrd.toml` at the root of your tree. Each top-level section is a namespace. Each namespace declares which files belong to it (`paths`) and which rules apply.

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
  "core.required-headings",
  "core.required-metadata",
  "core.allowed-values",
]

[ADR."core.required-headings"]
headings = ["Status", "Context", "Decision", "Consequences"]

[ADR."core.required-metadata"]
keys = ["id", "title", "status"]

[ADR."core.allowed-values"]
status = ["draft", "accepted", "rejected", "superseded"]
```

The default rule set covers the graph: `core.id` and `core.id-unique` check that nodes are well-named and unique; `core.dep-resolved` and `core.cross-ref` check that pointers resolve; `core.dep-cycle` keeps the graph acyclic; `core.frontmatter` checks the node payload parses.

External rules live at `rules/<ns>/<name>/run`. Make it executable, add the rule code to the namespace's `rules` list, done. Any language. Any check.

External sources live at `sources/<name>/run`. Activate them with `[sources.<name>]`. Lint documents from JIRA, Notion, or any system with an API. Same rules, different inputs.

## Subcommands

| Command                                  | Description                                        |
| ---------------------------------------- | -------------------------------------------------- |
| `ctxgrd init`                            | Bootstrap `ctxgrd.toml` with opinionated defaults. |
| `ctxgrd lint` (default)                  | Walk the tree, run rules, print diagnostics.       |
| `ctxgrd new <NS> "<title>"`              | Scaffold a new record.                             |
| `ctxgrd rules [--namespace NS] [<code>]` | Show the resolved rule set.                        |
| `ctxgrd refs <ID>`                       | Enumerate every pointer to a document.             |
| `ctxgrd docs <topic>`                    | Bundled user documentation.                        |

Global flag: `--root <path>`. Format flag on `lint` and `rules`: `--format text|json`.

## Exit codes

| Code | Meaning                                                              |
| ---- | -------------------------------------------------------------------- |
| `0`  | No error-severity diagnostics.                                       |
| `1`  | One or more error-severity diagnostics.                              |
| `2`  | Kernel error. Bad config, unknown rule, invalid params, I/O failure. |

Runtime errors from external rules and sources promote to exit `1`, not `2`.

## Examples

`examples/` is a runnable fixture. Four namespaces, two external rules, one source stub, one intentionally broken document. Run `ctxgrd --root examples` and you'll see the linter find every issue. Read `examples/README.md` for the guided tour.

## Contributing

This repo is a read-only mirror of an internal working repo. Issues are open. Pull requests are accepted but not merged here directly. A maintainer pulls the patch upstream and ships it in the next release.

If you've found a bug, an issue with a minimal reproduction is the fastest path. If you want to propose a change, an issue describing the change is more useful than a PR.

## License

MIT. See [LICENSE-MIT](LICENSE-MIT).
