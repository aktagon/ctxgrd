# Editor Integrations

`ctxgrd` provides a Language Server Protocol (LSP) server that enables rich editor support for navigating and linting your document graph.

## Features

The `ctxgrd lsp` server supports:

- **Diagnostics**: Missing references, duplicate IDs, non-canonical ID formats.
- **Navigation**: Go to definition for document IDs (e.g., `ADR-001`).
- **Discovery**: Find all references to a specific document.
- **Hover**: Preview document title and status by hovering over its ID.
- **Completion**: Smart completion of document IDs as you type.
- **Workspace Symbols**: Search for documents by ID across the whole project.

## Neovim

A minimal Lua plugin is provided in `editors/nvim`.

### Quick Start

```lua
-- Add to your init.lua
require('ctxgrd').setup()
```

See [editors/nvim/README.md](../editors/nvim/README.md) for details.

## Helix

Add the following to your `~/.config/helix/languages.toml`:

```toml
[[language]]
name = "markdown"
language-servers = [ "ctxgrd-lsp" ]

[language-server.ctxgrd-lsp]
command = "ctxgrd"
args = ["lsp"]
```

## VS Code

*(Coming soon)*

## Emacs (eglot)

Add the following to your `init.el`:

```elisp
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '(markdown-mode . ("ctxgrd" "lsp"))))
```

## Vim (coc.nvim)

Add to your `coc-settings.json`:

```json
{
  "languageserver": {
    "ctxgrd": {
      "command": "ctxgrd",
      "args": ["lsp"],
      "filetypes": ["markdown"]
    }
  }
}
```
