local s = __SYMBOL__; local ok, builtin = pcall(require, 'telescope.builtin'); if ok and builtin.lsp_workspace_symbols then
  builtin.lsp_workspace_symbols({ query = s })
else
  vim.lsp.buf.workspace_symbol(s)
end

