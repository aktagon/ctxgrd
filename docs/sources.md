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

### `expect_min` — the floor on what a source must emit

Writing `[sources.<name>]` states that the source is expected to produce
something, so ctxgrd holds it to a floor of **one document by default**. A
source that runs to completion and emits fewer warns:

```
warning[src.too-few-documents]: source 'statute' emitted 0 of an expected minimum of 1
```

This is not pedantry. A source that exits 0 without writing to stdout
produces a run byte-identical to one where every document passed — the gate
disappears and nothing says so. A `set -e` that aborts an emit loop partway
is the usual way this happens.

Set the floor explicitly when you know how many documents to expect, which
also catches a partial emit:

```toml
[sources.statute]
expect_min = 8   # one per clause of the statute
```

Set it to `0` for a source that may legitimately emit nothing:

```toml
[sources.optional-feed]
expect_min = 0
```

`expect_min` is ctxgrd's own key and is stripped from the parameters before
they reach the script — a source never sees it in `CTXGRD_SOURCE_PARAMS`.

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
| stdout        | JSONL document envelopes, one per line (see below), and optional |
|               | warning lines.                                                   |
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

A line that is a JSON object with a string `warning` field and no `id` is a
warning, not a document. ctxgrd reports it as `src.warning`, naming the
source, and it never changes the exit code. A source that must fail exits
non-zero instead.

```json
{"warning": "the cache is older than 24 hours; refresh it"}
```

ctxgrd parses `body` as markdown, as it parses a local file, so heading, link and
cross-reference rules apply to it. A relative link resolves only when `location`
names a file under the lint root.

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

## GitHub issues

`examples/sources/github-issues/run` lets a document wait on a GitHub issue.
ctxgrd runs every source on every `lint`, `status` and turn-end hook, so this
script reads only a cache file. You write the cache with `--refresh`, which
calls `gh issue list`. ctxgrd never passes arguments to a source, so no lint
can call `gh` (ADR-153).

1. Copy the script below to `sources/github-issues/run` and make it
   executable. Add `.ctxgrd/cache/` to `.gitignore`.
2. Configure the source, a namespace for the issues, and what settles an
   edge into it:

   ```toml
   [sources.github-issues]
   # repo = "acme/app"     # default: the repository gh finds from the cwd
   # max_age_hours = 24    # warn when the cache is older
   # expect_min = 0        # if the repository may have no issues

   [GH]
   rules = ["core.id"]

   [ADR."core.dep-status"]
   terminal = ["accepted", "superseded"]
   satisfied-by = { GH = ["closed"] }
   ```

3. Run `sources/github-issues/run --refresh` from the project root whenever
   you want current issue state.

An ADR then lists `GH-7` in `depends_on`. `status` reports it blocked by
`GH-7` until a refresh records the issue as `closed`, and `core.dep-status`
reports an `accepted` ADR that rests on an open issue.

With no cache the source exits non-zero, so `lint` reports
`src.runtime-error` and is never clean. With a cache older than
`max_age_hours`, `lint` and `status` print a `src.warning` that names the
refresh command. The turn-end hook stays silent on warnings, as it does for
every warning.

```sh
#!/bin/sh
# GitHub-issue source for ctxgrd (ADR-153). Copy this folder to
# sources/github-issues/ in your project and activate it with a
# [sources.github-issues] table in ctxgrd.toml.
#
#   run            print the cached issues as document envelopes; no network
#   run --refresh  call `gh issue list` and rewrite the cache
#
# ctxgrd runs a source with no arguments, so lint, status and the turn-end
# hook only ever read the cache. Only you run --refresh.
#
# Parameters, from [sources.github-issues]:
#   namespace      id prefix, default "GH" (GH-123)
#   cache          cache file relative to the project root,
#                  default ".ctxgrd/cache/github-issues.jsonl"
#   max_age_hours  warn when the cache is older than this, default 24
#   repo           OWNER/NAME, default: the repository gh finds from the cwd
#   limit          most issues to fetch, default 1000
#
# --refresh reads the same parameters from CTXGRD_SOURCE_PARAMS when it is
# set, and uses the defaults when it is not.
set -eu

params=${CTXGRD_SOURCE_PARAMS:-'{}'}

# The value of the flat string or number key $1 in $params, or $2.
param() {
	v=$(printf '%s' "$params" | sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p")
	printf '%s' "${v:-$2}"
}

namespace=$(param namespace GH)
cache=$(param cache .ctxgrd/cache/github-issues.jsonl)
max_age_hours=$(param max_age_hours 24)
repo=$(param repo '')
limit=$(param limit 1000)
refresh='sources/github-issues/run --refresh'

if [ "${1:-}" = "--refresh" ]; then
	envelope='.[] | {id: ("'"$namespace"'-" + (.number|tostring)), body: ("# " + .title + "\n"), location: .url, depends_on: [], extra: {title: .title, status: (.state|ascii_downcase)}}'
	set -- issue list --state all --limit "$limit" --json number,title,state,url --jq "$envelope"
	if [ -n "$repo" ]; then
		set -- "$@" --repo "$repo"
	fi
	mkdir -p "$(dirname "$cache")"
	tmp="$cache.tmp.$$"
	if gh "$@" >"$tmp"; then
		mv "$tmp" "$cache"
		echo "github-issues: wrote $(wc -l <"$cache" | tr -d ' ') issues to $cache" >&2
	else
		rm -f "$tmp"
		echo "github-issues: gh failed; $cache was left as it was" >&2
		exit 1
	fi
	exit 0
fi

if [ ! -f "$cache" ]; then
	echo "github-issues: no cache at $cache; run \`$refresh\` first" >&2
	exit 1
fi
if [ -n "$(find "$cache" -mmin +"$((max_age_hours * 60))" 2>/dev/null)" ]; then
	printf '{"warning":"%s is older than %s hours; refresh it with `%s`"}\n' \
		"$cache" "$max_age_hours" "$refresh"
fi
cat "$cache"
```

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
