local path = __PATH__; local line = __LINE__;
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

