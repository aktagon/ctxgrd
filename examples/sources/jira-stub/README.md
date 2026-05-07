# jira-stub source

A stub source that emits two hardcoded JIRA-shaped documents without hitting
the network. It demonstrates the source wire contract the kernel expects from
any external source subprocess.

## Activation

Add a `[sources.jira-stub]` table to `ctxgrd.toml`. Discovery alone does not
invoke the source — it must be activated by config:

```toml
[sources.jira-stub]
project = "AUDIT"
```

## What ctxgrd hands to `run`

- `argv`: just the script path, no positional arguments.
- `env`:
  - `CTXGRD_SOURCE_NAME=jira-stub`
  - `CTXGRD_SOURCE_PARAMS='{"project":"AUDIT"}'`
  - plus the platform baseline (`PATH`, `HOME`, `LANG`, `LC_*`).
- `cwd`: the lint root.

## What `run` emits

JSONL on stdout, one document envelope per line:

```json
{"id":"JIRA-100","depends_on":["PRD-001"],"body":"# …","location":"https://jira.example.com/browse/JIRA-100","extra":{"project":"AUDIT","status":"Open","assignee":"alice"}}
{"id":"JIRA-101","depends_on":[],"body":"# …","location":"https://jira.example.com/browse/JIRA-101","extra":{"project":"AUDIT","status":"In Progress","assignee":"bob"}}
```

Required envelope fields: `id`, `body`, `location`. Optional: `depends_on`
(defaults to `[]`), `extra` (defaults to `{}`).

The `extra` map flows into the unified metadata map alongside frontmatter, so
`core.required-metadata` and `core.allowed-values` validate JIRA tickets the
same way they validate local markdown files.

## Writing a real JIRA source

Replace the `cat <<EOF … EOF` block with an actual JIRA API call:

```sh
curl -s --user "$JIRA_USER:$JIRA_TOKEN" \
  "https://jira.example.com/rest/api/3/search?jql=$(jq -r '.jql | @uri' <<< "$CTXGRD_SOURCE_PARAMS")" \
| jq -c '.issues[] | {
    id: .key,
    depends_on: [.fields.issuelinks[]?.outwardIssue?.key // empty],
    body: .fields.description,
    location: ("https://jira.example.com/browse/" + .key),
    extra: { status: .fields.status.name, assignee: .fields.assignee.name }
  }'
```

The wire contract is identical — only the data origin changes.
