local file_arg = __FILE_PATH__

-- Pick up external edits before reading diagnostics. Agents often edit files
-- outside Neovim, and rust-analyzer reports diagnostics for Neovim buffers.
vim.cmd("checktime")

local function severity_name(severity)
  if severity == vim.diagnostic.severity.ERROR then
    return "error"
  end
  if severity == vim.diagnostic.severity.WARN then
    return "warn"
  end
  if severity == vim.diagnostic.severity.INFO then
    return "info"
  end
  if severity == vim.diagnostic.severity.HINT then
    return "hint"
  end
  return "unknown"
end

local bufnr = 0
local path = vim.api.nvim_buf_get_name(0)

if file_arg ~= "" then
  local resolved = file_arg
  if vim.fn.filereadable(resolved) == 0 and vim.fn.fnamemodify(resolved, ":p") ~= resolved then
    resolved = vim.fn.fnamemodify(resolved, ":p")
  end
  bufnr = vim.fn.bufnr(resolved, false)
  if bufnr < 0 then
    bufnr = vim.fn.bufadd(resolved)
  end
  vim.fn.bufload(bufnr)
  vim.api.nvim_buf_call(bufnr, function()
    vim.cmd("filetype detect")
  end)
  path = vim.api.nvim_buf_get_name(bufnr)
end

if path == "" then
  path = file_arg
end

local attached_lsp_clients = {}
for _, client in ipairs(vim.lsp.get_clients({ bufnr = bufnr })) do
  attached_lsp_clients[client.id] = true
end

local function diagnostic_lsp_client_id(diagnostic)
  if diagnostic.namespace == nil then
    return nil
  end
  local namespace = vim.diagnostic.get_namespace(diagnostic.namespace)
  local user_data = namespace and namespace.user_data
  local lsp_data = user_data and user_data.lsp
  return lsp_data and lsp_data.client_id or nil
end

local function diagnostic_matches_buffer(diagnostic)
  -- `vim.diagnostic.get(bufnr)` can include stale LSP diagnostics if a buffer was
  -- previously created without normal filetype/LSP attachment. Only keep LSP
  -- diagnostics from clients currently attached to this buffer; diagnostics from
  -- non-LSP producers have no client id and are kept.
  local client_id = diagnostic_lsp_client_id(diagnostic)
  if client_id ~= nil and not attached_lsp_clients[client_id] then
    return false
  end
  return true
end

local diagnostics = vim.diagnostic.get(bufnr)
local items = {}
local summary = { errors = 0, warnings = 0, infos = 0, hints = 0 }

for _, diagnostic in ipairs(diagnostics) do
  if not diagnostic_matches_buffer(diagnostic) then
    goto continue
  end

  local severity = severity_name(diagnostic.severity)
  if severity == "error" then
    summary.errors = summary.errors + 1
  elseif severity == "warn" then
    summary.warnings = summary.warnings + 1
  elseif severity == "info" then
    summary.infos = summary.infos + 1
  elseif severity == "hint" then
    summary.hints = summary.hints + 1
  end

  items[#items + 1] = {
    lnum = diagnostic.lnum + 1,
    col = diagnostic.col + 1,
    end_lnum = diagnostic.end_lnum + 1,
    end_col = diagnostic.end_col + 1,
    message = diagnostic.message,
    severity = severity,
    source = diagnostic.source,
    code = diagnostic.code,
  }

  ::continue::
end

return vim.json.encode({
  path = path,
  summary = summary,
  items = items,
})
