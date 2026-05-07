# Reference scanning

ctxgrd's documents — ADRs, PRDs, post-mortems, runbooks — live in
markdown files. But pointers into the document graph appear
everywhere else:

- `// see ADR-042` in source code,
- `# tracks PRD-001` in `Cargo.toml`,
- "implementing per ADR-007" in JIRA descriptions,
- `RUN-001` mentions in commit messages.

The reference scanner walks non-markdown files for tokens of the
form `<NAMESPACE>-<number>` and feeds them into `core.cross-ref`. A
pointer that doesn't resolve to a known document is flagged with the
exact `<file>:<line>:<col>` of the dangling mention — same diagnostic
shape as a body cross-ref, anchored at the offending token.

## Activating the scanner

Add a top-level `[references]` block to `ctxgrd.toml` with a list of
glob patterns to scan, relative to the lint root:

```toml
[references]
scan = [
  "**/*.toml",
  "src/**/*.rs",
  "src/**/*.go",
  "Cargo.toml",
]
```

Without `[references]`, the scanner is dormant — ctxgrd's behaviour
matches earlier versions exactly.

### Do NOT include markdown documents

The walker already tokenises every document body, with code-span and
strikethrough suppression intact. Re-scanning markdown via this block
would double-emit references and lose those suppressions. The `scan`
list should only mention non-markdown formats.

## Inheritance from ripgrep / `ignore`

The scanner uses the `ignore` crate's parallel walker. By default it
honours `.gitignore`, `.ignore`, and `.rgignore` in any ancestor
directory of the lint root, with the same semantics ripgrep uses
(hierarchical composition; later rules override earlier).

Files ctxgrd shouldn't read — `target/`, `node_modules/`, build
artefacts, generated code — are skipped automatically when they're
already listed in your existing ignore files. Nothing extra to wire
up.

## Inline suppression

A scanned line may contain a token you deliberately don't want
flagged — historical mentions, examples in prose, retired pointers.
Two markers suppress matches without disabling the rule entirely:

| Marker                | Effect                                        |
| --------------------- | --------------------------------------------- |
| `ctxgrd: ignore-line` | suppresses references on the same source line |
| `ctxgrd: ignore-next` | suppresses references on the next source line |

The marker is recognised as a literal substring anywhere on the
relevant line — ctxgrd does not parse comment delimiters per
language. Both of these suppress `ADR-9999`:

```rust
const FOO: &str = "ADR-9999"; // ctxgrd: ignore-line
```

```go
// ctxgrd: ignore-next
const supersededTopic = "ADR-9999"
```

## Namespace-prefix filter

ctxgrd only checks tokens whose namespace prefix is _known_: either
declared via a `[<NS>]` table in `ctxgrd.toml`, or used by a
discovered document. Tokens for unknown namespaces are silently
ignored.

This prevents `HTTP-2`, `ISO-8601`, `RFC-2119`, and other shape-matching
identifiers from polluting the diagnostic stream. If your project
genuinely needs to reference RFCs, add `[RFC]` to your `ctxgrd.toml`
and ctxgrd will start checking `RFC-NNN` mentions.

## `ctxgrd refs <ID>`

Detection (the rule above) tells you "did I break a pointer right
now?" Discovery — `ctxgrd refs <ID>` — tells you "what would I break
if I rename this?". The subcommand prints every location pointing at
a given document ID, sorted deterministically:

```
$ ctxgrd refs ADR-001
adrs/ADR-001-use-event-sourcing-for-audit.md:0:0: (self)
adrs/ADR-001-use-event-sourcing-for-audit.md:9:3: (body ref from ADR-001)
pmrs/PMR-001-audit-log-dropped-events-2026-03.md:6:0: (depends_on from PMR-001)
refs/main.go:12:32: (scanner)
refs/lib.rs:7:46: (scanner)
```

The output covers four kinds of pointers:

- `(self)` — the document file itself, when file-backed.
- `(depends_on from <ID>)` — `depends_on:` entry in another
  document's frontmatter.
- `(body ref from <ID>)` — body cross-reference token in another
  document.
- `(scanner)` — reference-scanner hit in a non-markdown file.

Use `--format json` for structured consumption:

```sh
ctxgrd refs ADR-001 --format json
```

The JSON form emits a single array, one object per hit, with
`{ file, line, col, kind, from? }` fields. Suitable for piping into
`jq`, `fzf`, or an editor's quickfix integration.

## Performance

The walker is parallel and uses ripgrep's literal pre-filter + lazy
DFA, so cold scans of a typical mid-sized repository (~5,000 files)
complete in well under 300 milliseconds. The scanner adds negligible
overhead to a `ctxgrd lint` run, so leaving it active in pre-commit
hooks is the intended workflow.
