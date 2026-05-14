# ctxgrd Claude Code Plugin

This plugin extends Claude Code with support for `ctxgrd` — a command-line linter for structured markdown records.

## Features

- **Code Intelligence**: LSP server support for markdown files, providing diagnostics, go-to-definition, and more.
- **Skills**:
  - `/ctxgrd:lint`: Run the linter.
  - `/ctxgrd:new`: Scaffold a new document.
  - `/ctxgrd:refs`: Find all references to a document ID.

## Requirements

- `ctxgrd` binary installed and available in your `PATH`.
