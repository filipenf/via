local file_arg = __FILE_PATH__

if file_arg == "" then
  vim.cmd("silent! checktime")
  return vim.json.encode({ refreshed = "all" })
end

local resolved = file_arg
if vim.fn.filereadable(resolved) == 0 and vim.fn.fnamemodify(resolved, ":p") ~= resolved then
  resolved = vim.fn.fnamemodify(resolved, ":p")
end

local bufnr = vim.fn.bufnr(resolved, false)
if bufnr < 0 then
  return vim.json.encode({ refreshed = "missing", path = resolved })
end

vim.cmd("silent! checktime " .. bufnr)
return vim.json.encode({ refreshed = "file", path = vim.api.nvim_buf_get_name(bufnr) })
