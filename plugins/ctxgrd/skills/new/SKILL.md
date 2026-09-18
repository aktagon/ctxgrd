---
name: new
description: Scaffold a new ctxgrd document (e.g., ADR, PRD) or a new external rule.
argument-hint: <NAMESPACE> "<TITLE>"
---
# ctxgrd new

Scaffold a new document with the correct frontmatter and ID.

## Usage

```bash
ctxgrd new ADR "My New Decision"
```

To scaffold a new external rule:

```bash
ctxgrd new rule my-namespace.rule-name "Description of rule"
```
