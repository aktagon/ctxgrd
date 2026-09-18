local M = {}

--- Setup ctxgrd LSP for Neovim.
--- @param opts table|nil Configuration options.
---   - cmd: table|nil The command to start the LSP server (default: { "ctxgrd", "lsp" })
---   - filetypes: table|nil List of filetypes to attach to (default: { "markdown" })
---   - root_markers: table|nil List of files to identify the workspace root (default: { "ctxgrd.toml", ".git" })
function M.setup(opts)
  opts = opts or {}
  local cmd = opts.cmd or { "ctxgrd", "lsp" }
  local filetypes = opts.filetypes or { "markdown" }
  local root_markers = opts.root_markers or { "ctxgrd.toml", ".git" }

  local group = vim.api.nvim_create_augroup("ctxgrd", { clear = true })

  vim.api.nvim_create_autocmd("FileType", {
    group = group,
    pattern = filetypes,
    callback = function(args)
      local root_file = vim.fs.find(root_markers, { upward = true, path = vim.api.nvim_buf_get_name(args.buf) })[1]
      local root_dir = root_file and vim.fs.dirname(root_file) or vim.fn.getcwd()

      vim.lsp.start({
        name = "ctxgrd",
        cmd = cmd,
        root_dir = root_dir,
        settings = {},
      })
    end,
  })
end

return M
