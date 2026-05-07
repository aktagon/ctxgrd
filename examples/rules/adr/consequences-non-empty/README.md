# adr.consequences-non-empty

An external rule: an ADR's `## Consequences` section, when present, must
contain at least one bullet (`- `, `* `) or numbered (`1. `) list item with
non-empty content.

A missing `## Consequences` heading is not this rule's concern — that is
reported by `core.required-headings`.

## Rule code

The rule's code is derived from the directory path relative to `rules/`:

```
rules/adr/consequences-non-empty/  →  adr.consequences-non-empty
```

## Activation

Add the code to the ADR namespace's `rules` list in `ctxgrd.toml`:

```toml
[ADR]
rules = [..., "adr.consequences-non-empty"]
```

## How ctxgrd invokes it

ADR-002 batch mode: ctxgrd spawns the script once per lint run and pipes
every ADR document on stdin as JSONL.

```sh
CTXGRD_RULE_PARAMS='{}' ./run < <(
  echo '{"path":"/abs/adrs/ADR-001-use-event-sourcing-for-audit.md","context":{...}}'
  echo '{"path":"/abs/adrs/ADR-099-broken-demo.md","context":{...}}'
)
```

Each stdin record carries the document body's path plus a `context` object
with everything the kernel knows about the document:

```json
{
  "path": "/abs/adrs/ADR-001-use-event-sourcing-for-audit.md",
  "context": {
    "id": "ADR-001",
    "namespace": "ADR",
    "number": 1,
    "location": "adrs/ADR-001-use-event-sourcing-for-audit.md",
    "depends_on": ["PRD-001"],
    "metadata": { "title": "...", "status": "accepted" }
  }
}
```

This rule reads only the path — it operates purely on the document body
on disk. Rules that need metadata extract it with
`jq -r .context.metadata.status` from each input line.

Each diagnostic on stdout MUST include a `path` field matching one of
the input paths so ctxgrd can attribute it to the right document. The
host prepends the rule code; the script never emits its own `code`
field.

## Example output

```json
{
  "path": "/abs/adrs/ADR-099-broken-demo.md",
  "severity": "error",
  "message": "## Consequences section is empty or contains no bullet items",
  "line": 25,
  "col": 0
}
```

After the host attaches the code:

```
adrs/ADR-099-broken-demo.md:25:0: error: [adr.consequences-non-empty] ## Consequences section is empty or contains no bullet items
```
