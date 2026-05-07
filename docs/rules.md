# Writing rules

Rules come in two flavors: core rules (built into ctxgrd) and external rules
(executable scripts you write). Both are assigned to namespaces in
`ctxgrd.toml` and appear together in `ctxgrd rules` output.

## External rule layout

An external rule is a directory under `rules/` containing an executable
named `run`. The directory path determines the rule code:

```
rules/<namespace>/<name>/run  →  <namespace>.<name>
```

Examples:

```
rules/adr/consequences-non-empty/run   →  adr.consequences-non-empty
rules/adr/supersession-consistent/run  →  adr.supersession-consistent
rules/prd/links-resolve/run            →  prd.links-resolve
```

The `run` file must be executable (`chmod +x rules/adr/my-rule/run`).
Drop a `README.md` alongside `run` — `ctxgrd rules <code>` will print it
in the detail view.

## Activating a rule

Add the rule code to the namespace's `rules` list in `ctxgrd.toml`:

```toml
[ADR]
rules = [
  "core.frontmatter",
  ...
  "adr.consequences-non-empty",
]
```

That's the entire activation path. ctxgrd discovers the script from the
directory layout; the rule code is the bijection between config and disk.

## The script contract

ctxgrd invokes each rule **once per lint run** (ADR-002 batch mode). All
documents in the rule's namespace arrive on stdin as JSONL — one JSON
object per line — and the rule emits zero or more JSONL diagnostics on
stdout. After the last document is written, ctxgrd closes stdin so a
`while read` loop terminates cleanly.

```sh
CTXGRD_RULE_PARAMS='{"min_items":3}' rules/adr/my-rule/run < <(
  echo '{"path":"/abs/adrs/ADR-001.md","context":{...}}'
  echo '{"path":"/abs/adrs/ADR-002.md","context":{...}}'
)
```

| Input                | Description                                                                     |
| -------------------- | ------------------------------------------------------------------------------- |
| stdin                | JSONL records of `{"path": ..., "context": {...}}`, one per document. EOF after |
|                      | the last document.                                                              |
| `CTXGRD_RULE_PARAMS` | JSON object of rule parameters from `ctxgrd.toml`. `{}` if none.                |
| argv                 | None — rules receive no positional arguments.                                   |

| Output        | Description                                                       |
| ------------- | ----------------------------------------------------------------- |
| stdout        | Zero or more JSONL diagnostic lines (one JSON object per line).   |
| stderr        | Ignored by ctxgrd; use for debug output during development.       |
| exit 0        | Script ran cleanly. Diagnostics on stdout are recorded.           |
| exit non-zero | Runtime error. ctxgrd emits an `ext.runtime-error` diagnostic and |
|               | continues linting other rules.                                    |

## Stdin record format

Each line on stdin is a JSON object with two top-level fields:

| Field     | Type   | Description                                                                      |
| --------- | ------ | -------------------------------------------------------------------------------- |
| `path`    | string | Absolute path to the document body. For file-based docs this is the real file;   |
|           |        | for source-derived docs it is a temp file the kernel materialised with the body. |
| `context` | object | Everything the kernel knows about the document (see below).                      |

```json
{
  "path": "/abs/adrs/ADR-001-use-event-sourcing-for-audit.md",
  "context": {
    "id": "ADR-001",
    "namespace": "ADR",
    "number": 1,
    "location": "adrs/ADR-001-use-event-sourcing-for-audit.md",
    "depends_on": ["PRD-001"],
    "metadata": {
      "id": "ADR-001",
      "title": "Use event sourcing for the audit trail",
      "status": "accepted"
    }
  }
}
```

The `context.metadata` object is the **unified metadata map**: frontmatter
keys for local files, `extra` fields for source-derived documents, with
frontmatter winning on conflict. Rules that need metadata read from this
inline context, not from the raw document body, so they work identically
for both document types.

Read fields with jq inside the loop:

```sh
while IFS= read -r line; do
  path=$(printf '%s' "$line" | jq -r '.path')
  status=$(printf '%s' "$line" | jq -r '.context.metadata.status // ""')
  # ... your check ...
done
```

## JSONL diagnostic format

Each line emitted to stdout must be a JSON object with these fields:

| Field      | Type   | Required | Description                                                            |
| ---------- | ------ | -------- | ---------------------------------------------------------------------- |
| `path`     | string | Yes      | The `path` value from a stdin record — tells ctxgrd which document the |
|            |        |          | diagnostic belongs to.                                                 |
| `severity` | string | Yes      | `"error"` or `"warning"`.                                              |
| `message`  | string | Yes      | Human-readable description of the problem.                             |
| `line`     | number | Yes      | Line number in the document (0 if not line-specific).                  |
| `col`      | number | Yes      | Column offset (0-based; 0 if not column-specific).                     |

Do **not** emit a `code` field — ctxgrd attaches the rule code from the
directory path automatically.

Diagnostics may arrive in any order, and you may emit multiple
diagnostics per document.

```sh
printf '{"path":%s,"severity":"error","message":"Consequences section is empty","line":%s,"col":0}\n' \
  "$(printf '%s' "$path" | jq -Rs .)" "$heading_line"
```

## Passing parameters to external rules

If your rule needs configuration (thresholds, required strings, etc.),
declare a sub-table in `ctxgrd.toml`:

```toml
[ADR."adr.min-consequences"]
min_items = 3
```

The sub-table value is serialized to JSON and passed as
`CTXGRD_RULE_PARAMS`. Rule parameters are per-rule (not per-document), so
they live in env, not in the stdin records:

```sh
min=$(printf '%s' "${CTXGRD_RULE_PARAMS:-{\}}" | jq -r '.min_items // 1')
```

## Per-rule timeout

The default timeout for a rule's whole batch is 60 seconds. Override it
in the same sub-table:

```toml
[ADR."adr.min-consequences"]
min_items = 3
timeout_sec = 120
```

The timeout applies to the entire invocation (start of spawn → EOF on
stdout), not per-document. On timeout, ctxgrd kills the process and
emits a single `ext.runtime-error` diagnostic; other rules keep running.

## Complete example

A rule that checks whether every ADR has at least one external link in
its body:

```sh
#!/usr/bin/env bash
set -euo pipefail

while IFS= read -r line; do
  path=$(printf '%s' "$line" | jq -r '.path')
  if grep -qE 'https?://' "$path"; then
    continue
  fi
  printf '{"path":%s,"severity":"warning","message":"ADR body contains no external links","line":0,"col":0}\n' \
    "$(printf '%s' "$path" | jq -Rs .)"
done
```

Save it to `rules/adr/has-external-link/run`, make it executable, and
add `"adr.has-external-link"` to the `[ADR]` rules list.
