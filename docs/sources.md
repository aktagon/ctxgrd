# Writing sources

A source is an executable script that emits documents ctxgrd cannot read
from the filesystem — JIRA tickets, Notion pages, GitHub issues, anything
with an API. ctxgrd runs the script, reads its JSONL output, and lints
those documents alongside local markdown files using the same rules.

## Source layout

A source is a directory under `sources/` containing an executable named
`run`. The directory name is the source name:

```
sources/jira-stub/run   →  source name: jira-stub
sources/notion/run      →  source name: notion
```

The `run` file must be executable (`chmod +x sources/jira-stub/run`).

## Activation

Discovery is automatic, but invocation requires an entry in `ctxgrd.toml`.
A source that exists on disk but has no config table is never run:

```toml
[sources.jira-stub]
project = "AUDIT"
```

The table contents become the source's parameters (see below). An empty
table `[sources.jira-stub]` is valid — the source receives `CTXGRD_SOURCE_PARAMS='{}'`.

## The script contract

ctxgrd invokes the source once per lint run:

```sh
CTXGRD_SOURCE_NAME=jira-stub \
CTXGRD_SOURCE_PARAMS='{"project":"AUDIT"}' \
sources/jira-stub/run
```

| Input                  | Description                                                     |
| ---------------------- | --------------------------------------------------------------- |
| `CTXGRD_SOURCE_NAME`   | The source's name (directory name under `sources/`).            |
| `CTXGRD_SOURCE_PARAMS` | JSON object of the `[sources.<name>]` table from `ctxgrd.toml`. |
| `cwd`                  | The lint root.                                                  |
| argv                   | None — sources receive no positional arguments.                 |

| Output        | Description                                                      |
| ------------- | ---------------------------------------------------------------- |
| stdout        | JSONL document envelopes, one per line (see below).              |
| stderr        | Ignored by ctxgrd; use for debug output during development.      |
| exit 0        | Ran cleanly. Envelopes on stdout are processed.                  |
| exit non-zero | Runtime error. ctxgrd emits a `src.runtime-error` diagnostic but |
|               | continues linting all other sources and local files.             |

## Document envelope format

Each line of stdout must be a JSON object:

| Field        | Type            | Required | Description                                           |
| ------------ | --------------- | -------- | ----------------------------------------------------- |
| `id`         | string          | Yes      | Document ID in `<NAMESPACE>-<number>` format          |
| `body`       | string          | Yes      | Document body (markdown, no frontmatter fence needed) |
| `location`   | string          | Yes      | Canonical URL or path for diagnostics and display     |
| `depends_on` | array of string | No       | IDs this document depends on (defaults to `[]`)       |
| `extra`      | object          | No       | Arbitrary key/value metadata (defaults to `{}`)       |

```json
{
  "id": "JIRA-100",
  "depends_on": ["PRD-001"],
  "body": "# Fix retention cutoff\n\n…",
  "location": "https://jira.example.com/browse/JIRA-100",
  "extra": { "project": "AUDIT", "status": "Open", "assignee": "alice" }
}
```

## The `extra` map and unified metadata

The `extra` object flows into the same unified metadata map as frontmatter.
This means `core.required-metadata` and `core.allowed-values` work
identically for source-derived documents:

```toml
[JIRA]
rules = ["core.id", "core.id-unique", "core.dep-resolved", "core.dep-cycle",
         "core.required-metadata", "core.allowed-values"]

[JIRA."core.required-metadata"]
keys = ["status", "assignee"]

[JIRA."core.allowed-values"]
status = ["Open", "In Progress", "Closed"]
```

Frontmatter wins on conflict: if a source sets `extra.status = "Open"` and
the body also has a frontmatter `status:` key, frontmatter takes precedence.

## Namespacing source documents

The namespace is derived from the `id` field in the envelope, the same as
for local files. `JIRA-100` belongs to the `JIRA` namespace; `PRD-042`
would belong to `PRD`. You configure rules per namespace in `ctxgrd.toml`
regardless of whether documents come from files or sources.

Source documents participate in the cross-namespace dependency graph. A JIRA
ticket can `depends_on` a local PRD, and `core.dep-resolved` validates that
the PRD exists.

**Source-emitted documents are namespace-classified by their `id` only;
`[<NS>].paths` does not apply.** Path-classification is a property of the
markdown walker, not of the document model — sources emit synthetic
locations the user doesn't control. If you want a source to participate
in a namespace whose local documents are path-claimed, the source still
needs an `id` matching `<NAMESPACE>-<NUMBER>`; the path glob is irrelevant.

## Writing a real source

The stub in `examples/sources/jira-stub/run` emits hardcoded envelopes.
Replace the `cat <<EOF` block with an API call. Here is the pattern for
JIRA:

```sh
#!/usr/bin/env bash
set -euo pipefail

jql="$(printf '%s' "${CTXGRD_SOURCE_PARAMS:-{\}}" | jq -r '.jql // "project = AUDIT"')"

curl -s --user "$JIRA_USER:$JIRA_TOKEN" \
  "https://jira.example.com/rest/api/3/search?jql=$(printf '%s' "$jql" | jq -Rr @uri)" \
| jq -c '.issues[] | {
    id: .key,
    depends_on: [.fields.issuelinks[]?.outwardIssue?.key // empty],
    body: (.fields.summary + "\n\n" + (.fields.description // "")),
    location: ("https://jira.example.com/browse/" + .key),
    extra: {
      status:   .fields.status.name,
      assignee: .fields.assignee.displayName
    }
  }'
```

The wire contract is identical — only the data origin changes. Store
credentials in the environment and load them from the shell (never hardcode
them in the script or `ctxgrd.toml`).

## Development tips

Test a source directly before wiring it into ctxgrd:

```sh
CTXGRD_SOURCE_NAME=jira-stub \
CTXGRD_SOURCE_PARAMS='{"project":"AUDIT"}' \
sources/jira-stub/run | jq .
```

Each line should parse as valid JSON. `jq .` will error loudly on malformed
output. Once the script runs clean, activate it in `ctxgrd.toml` and run
`ctxgrd --root .` to see how ctxgrd validates the documents it emits.
