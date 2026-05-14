# ctxgrd-nvim

Minimal Neovim integration for the [ctxgrd](https://github.com/aktagon/ctxgrd) Language Server.

## Features

- **Diagnostics**: Real-time linting of document IDs and cross-references.
- **Go to Definition**: Jump to the document defining an ID (e.g., `ADR-001`).
- **Find References**: Find all documents referencing a specific ID.
- **Hover**: Preview document title and status.
- **Completion**: Autocomplete document IDs from your workspace.
- **Workspace Symbols**: Search for documents by ID across the whole project.

## Requirements

- Neovim 0.8+
- `ctxgrd` binary installed and available in your `PATH`.

## Installation

### Manual

1. Copy `lua/ctxgrd.lua` to your Neovim configuration directory (usually `~/.config/nvim/lua/ctxgrd.lua`).
2. Add the following to your `init.lua`:

```lua
require('ctxgrd').setup()
```

### With Plugin Manager

If you use a plugin manager like `lazy.nvim`, you can point it to this repository (or a fork):

```lua
-- lazy.nvim
{
  'aktagon/ctxgrd',
  config = function()
    require('ctxgrd').setup()
  end,
}
```

## Configuration

You can pass options to the `setup` function:

```lua
require('ctxgrd').setup({
  -- Command to start the LSP server
  cmd = { "ctxgrd", "lsp" },
  -- Filetypes to attach to
  filetypes = { "markdown" },
  -- Files to identify the workspace root
  root_markers = { "ctxgrd.toml", ".git" },
})
```

## Troubleshooting

Run `:LspInfo` in a markdown file to check if `ctxgrd` is attached.
Check `:messages` for any error output from the server.

## Documentation Index

Fetch the complete documentation index at: [llms.txt](../../llms.txt)
Use this file to discover all available pages before exploring further.
