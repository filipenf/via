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
  bufnr = vim.fn.bufnr(resolved, true)
  if bufnr < 0 then
    bufnr = vim.fn.bufadd(resolved)
    vim.fn.bufload(bufnr)
  end
  path = vim.api.nvim_buf_get_name(bufnr)
end

if path == "" then
  path = file_arg
end

local diagnostics = vim.diagnostic.get(bufnr)
local items = {}
local summary = { errors = 0, warnings = 0, infos = 0, hints = 0 }

for _, diagnostic in ipairs(diagnostics) do
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
end

return vim.json.encode({
  path = path,
  summary = summary,
  items = items,
})
