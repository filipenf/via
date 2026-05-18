local repo = "__WORKING_DIRECTORY__"

if vim.fn.exists(":ViaReview") == 2 then
  vim.cmd("ViaReview")
  return
end

local files = vim.fn.systemlist({ "git", "-C", repo, "diff", "--name-only", "--relative" })
if vim.v.shell_error ~= 0 then
  vim.notify("via: git diff failed", vim.log.levels.ERROR)
  return
end

if #files == 0 then
  vim.notify("via: no working tree changes to review", vim.log.levels.INFO)
  return
end

local qf_items = {}
for _, file in ipairs(files) do
  table.insert(qf_items, { filename = repo .. "/" .. file, lnum = 1, text = "modified" })
end
vim.fn.setqflist(qf_items, "r")

local file = files[1]
local path = repo .. "/" .. file
vim.cmd("tabedit " .. vim.fn.fnameescape(path))
local worktree_win = vim.api.nvim_get_current_win()

vim.cmd("vnew")
local base_buf = vim.api.nvim_get_current_buf()
vim.api.nvim_buf_set_name(base_buf, "HEAD:" .. file)
vim.bo[base_buf].buftype = "nofile"
vim.bo[base_buf].bufhidden = "wipe"
vim.bo[base_buf].swapfile = false
vim.bo[base_buf].modifiable = true

local base = vim.fn.systemlist({ "git", "-C", repo, "show", "HEAD:" .. file })
if vim.v.shell_error ~= 0 then
  base = {}
end
vim.api.nvim_buf_set_lines(base_buf, 0, -1, false, base)
vim.bo[base_buf].modifiable = false
vim.cmd("diffthis")

vim.api.nvim_set_current_win(worktree_win)
vim.cmd("diffthis")
vim.cmd("wincmd l")
