local path = __PATH__; local line = __LINE__;

-- If the path doesn't exist as given, try to find the file by name under cwd.
-- This handles bare filenames (e.g. "lsp_bridge.rs") that agents sometimes emit
-- without a directory prefix.
if vim.fn.filereadable(path) == 0 then
  local fname = vim.fn.fnamemodify(path, ':t');
  local found = vim.fn.findfile(fname, '**');
  if found ~= '' then
    path = vim.fn.fnamemodify(found, ':p');
  end
end

local prev = vim.fn.win_getid(vim.fn.winnr('#'));
local cur = vim.api.nvim_get_current_win();
local buf = vim.api.nvim_win_get_buf(cur);
local bt = vim.api
    .nvim_get_option_value(
      'buftype',
      { buf = buf });
if bt ~= '' and prev ~= 0 and vim.api.nvim_win_is_valid(prev) then
  vim.api.nvim_set_current_win(prev)
end;
local escaped = vim.fn.fnameescape(path);

if line then
  vim.cmd('drop +' .. line .. ' ' .. escaped)
else
  vim.cmd('drop ' .. escaped)
end

