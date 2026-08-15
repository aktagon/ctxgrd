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

Download a pre-built binary from the [releases page](https://github.com/aktagon/ctxgrd/releases/latest). Linux x86_64, macOS Intel and Apple Silicon, Windows x86_64.

Asset names embed the version, so set it once and the rest of each block is copy-paste.

### macOS and Linux

```sh
VERSION=v2.3.0
TARGET=aarch64-apple-darwin         # macOS Apple Silicon
# TARGET=x86_64-apple-darwin        # macOS Intel
# TARGET=x86_64-unknown-linux-gnu   # Linux x86_64

BASE="https://github.com/aktagon/ctxgrd/releases/download/$VERSION"
curl -fsSLO "$BASE/ctxgrd-$VERSION-$TARGET.tar.gz"
curl -fsSLO "$BASE/checksums.txt"
```

Verify the download before you extract it:

```sh
shasum -a 256 -c checksums.txt --ignore-missing    # macOS
sha256sum   -c checksums.txt --ignore-missing      # Linux
```

`--ignore-missing` is not optional. `checksums.txt` covers all four platforms and you downloaded one; without the flag the check reports the three you don't have as failures and exits non-zero. With it, you should see exactly one `OK` line and exit `0`.

Then install:

```sh
tar -xzf "ctxgrd-$VERSION-$TARGET.tar.gz"
mkdir -p ~/.local/bin
mv "ctxgrd-$VERSION-$TARGET/ctxgrd" ~/.local/bin/ctxgrd
chmod +x ~/.local/bin/ctxgrd
ctxgrd --version
```

The archive is a directory, not a bare binary — it also carries `LICENSE`, `LICENSE-MIT` and `CHANGELOG.md`. Make sure `~/.local/bin` is on your `PATH`.

### Windows

```powershell
$Version = "v2.3.0"
$Target  = "x86_64-pc-windows-msvc"
$Base    = "https://github.com/aktagon/ctxgrd/releases/download/$Version"

Invoke-WebRequest "$Base/ctxgrd-$Version-$Target.zip" -OutFile "ctxgrd-$Version-$Target.zip"
Invoke-WebRequest "$Base/checksums.txt" -OutFile "checksums.txt"

# Compare this hash against the matching line in checksums.txt
(Get-FileHash "ctxgrd-$Version-$Target.zip" -Algorithm SHA256).Hash.ToLower()
Select-String -Path checksums.txt -Pattern $Target

Expand-Archive "ctxgrd-$Version-$Target.zip" -DestinationPath .
```

Move `ctxgrd-$Version-$Target\ctxgrd.exe` to a directory on your `PATH`.

### With the GitHub CLI

```sh
gh release download v2.3.0 --repo aktagon/ctxgrd \
  --pattern 'ctxgrd-*-aarch64-apple-darwin.tar.gz' --pattern 'checksums.txt'
```

### There is no build-from-source path

ctxgrd ships as a binary. This repository carries documentation and release
artifacts, not source, so `cargo install`, `git clone && make install` and
crates.io are not install routes — use the downloads above.

## Five-minute tour

```sh
ctxgrd init                           # scaffold ctxgrd.toml with sensible defaults
ctxgrd --root docs/                   # lint a tree
ctxgrd new ADR "Use event sourcing"   # scaffold a new record
ctxgrd rules --namespace ADR          # show resolved rules for one namespace
ctxgrd lint --format json             # machine-readable diagnostics for CI
ctxgrd refs ADR-001                   # show every pointer to a document
ctxgrd status                         # what's ready to work on, what's blocked
ctxgrd docs packs                     # bundled user docs, by topic
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

| Command                                  | Description                                                          |
| ---------------------------------------- | -------------------------------------------------------------------- |
| `ctxgrd lint` (default)                  | Walk the tree, run rules, print diagnostics.                         |
| `ctxgrd init`                            | Write a starter `ctxgrd.toml`.                                       |
| `ctxgrd new <NS> "<title>"`              | Scaffold a new document, or an external rule when `<NS>` is `rule`.  |
| `ctxgrd list`                            | List ingested documents grouped by namespace.                        |
| `ctxgrd rules [--namespace NS] [<code>]` | Introspect the resolved rule set.                                    |
| `ctxgrd refs <ID>`                       | List every location pointing at a document ID.                       |
| `ctxgrd status`                          | Report the work queue over the `depends_on` graph.                   |
| `ctxgrd pack`                            | Inspect and apply rule packs — reusable namespace bundles.           |
| `ctxgrd changelog`                       | Generate `CHANGELOG.md` from the document graph.                     |
| `ctxgrd pin`                             | Manage commit pins on documents.                                     |
| `ctxgrd hooks`                           | Manage git hooks that gate commits on ctxgrd.                        |
| `ctxgrd serve`                           | Serve a read-only, graph-aware web view of the governed docs.        |
| `ctxgrd lsp`                             | Start the Language Server Protocol server over stdio.                |
| `ctxgrd docs <topic>`                    | Print a bundled end-user guide.                                      |

Global flag: `--root <path>`. Every command that emits output offers `--format json` alongside its human-readable default, keeps `stdout` a clean parseable stream, and uses the exit codes below — so an agent can drive it without screen-scraping.

## Exit codes

| Code | Meaning                                                              |
| ---- | -------------------------------------------------------------------- |
| `0`  | No error-severity diagnostics.                                       |
| `1`  | One or more error-severity diagnostics.                              |
| `2`  | Kernel error. Bad config, unknown rule, invalid params, I/O failure. |

Runtime errors from external rules and sources promote to exit `1`, not `2`.

## Documentation

The binary carries its own reference docs. No network, no separate install:

```sh
ctxgrd docs namespaces    # configure namespaces and core rules in ctxgrd.toml
ctxgrd docs rules         # write external rule scripts
ctxgrd docs sources       # write external source scripts
ctxgrd docs references    # scan non-markdown files for pointer mentions
ctxgrd docs packs         # apply reusable namespace bundles
```

`ctxgrd init` writes a starter `ctxgrd.toml` you can lint against immediately, and `ctxgrd pack list` shows the bundled namespace sets. Guides live at [ctxgrd.aktagon.com](https://ctxgrd.aktagon.com).

## Issues

This repository is the release channel. It carries documentation and binaries; ctxgrd is developed in a private repository, so there is no source here to patch and pull requests cannot be merged.

Issues are open and are the right place for everything: a bug with a minimal reproduction, a rule that fires when it shouldn't, a missing check, or a change you'd like to see. Bug reports with a reproduction are the fastest path to a fix in the next release.

## License

[Elastic License 2.0](LICENSE). Free to use, including commercially. You may not
provide ctxgrd to third parties as a hosted service, and you may not circumvent
its license key functionality.

Releases up to and including v2.2.0 were published under the MIT License, which
still applies to them; see [LICENSE-MIT](LICENSE-MIT).
