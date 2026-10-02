-- session.lua
-- Restore and save the Neovim session for the directory via was started in.
-- When folke/persistence successfully loads the session, it also saves on exit.
-- Otherwise via reads and writes the same session files, under
-- stdpath("state")/sessions.

local M = {}

local SKIP_FILETYPES = {
  gitcommit = true,
  gitrebase = true,
  jj = true,
}

function M.session_dir()
  return vim.fn.stdpath("state") .. "/sessions/"
end

--- File name persistence.nvim uses for a working directory and optional branch.
--- `branch` is omitted for nil, empty, "main", and "master".
function M.session_name(cwd, branch)
  local name = (cwd or ""):gsub("[\\/:]+", "%%")
  if branch and branch ~= "" and branch ~= "main" and branch ~= "master" then
    name = name .. "%%" .. branch:gsub("[\\/:]+", "%%")
  end
  return name .. ".vim"
end

function M.session_path(cwd, branch)
  return M.session_dir() .. M.session_name(cwd, branch)
end

function M.branch_name()
  if vim.fn.isdirectory(".git") == 0 and vim.fn.filereadable(".git") == 0 then
    return nil
  end
  local lines = vim.fn.systemlist({ "git", "branch", "--show-current" })
  if vim.v.shell_error ~= 0 then
    return nil
  end
  local branch = lines[1]
  if not branch or branch == "" or branch == "main" or branch == "master" then
    return nil
  end
  return branch
end

local function file_buffer_count()
  local count = 0
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].buftype == "" and vim.api.nvim_buf_get_name(buf) ~= "" then
      if not SKIP_FILETYPES[vim.bo[buf].filetype] then
        count = count + 1
      end
    end
  end
  return count
end

local function source_session(path)
  if vim.fn.filereadable(path) == 0 then
    return false
  end
  vim.cmd("silent! source " .. vim.fn.fnameescape(path))
  return true
end

--- Load the session for the current directory. No-op when Neovim was given files.
function M.restore()
  if vim.fn.argc(-1) ~= 0 then
    return false
  end

  -- Nil unless this call hands the session to persistence. save() must still
  -- write when persistence is installed but could not load (setup never ran).
  M._owner = nil

  local ok, persistence = pcall(require, "persistence")
  if ok and type(persistence.load) == "function" and pcall(persistence.load) then
    M._owner = "persistence"
    return true
  end

  local cwd = vim.fn.getcwd()
  local path = M.session_path(cwd, M.branch_name())
  if source_session(path) or source_session(M.session_path(cwd, nil)) then
    M._owner = "via"
    return true
  end
  return false
end

--- Write a session file. `mksession!` is wrapped so a failure cannot stop exit.
function M.write_session(path)
  local saved = vim.o.sessionoptions
  vim.opt.sessionoptions:remove({ "terminal", "blank" })
  local ok = pcall(vim.cmd, "mksession! " .. vim.fn.fnameescape(path))
  vim.o.sessionoptions = saved
  return ok
end

--- Write a session file when persistence did not take ownership.
function M.save()
  if M._owner == "persistence" then
    return false
  end
  if file_buffer_count() < 1 then
    return false
  end

  local dir = M.session_dir()
  vim.fn.mkdir(dir, "p")
  local path = M.session_path(vim.fn.getcwd(), M.branch_name())
  return M.write_session(path)
end

function M.install()
  local group = vim.api.nvim_create_augroup("viaSession", { clear = true })
  vim.api.nvim_create_autocmd("VimEnter", {
    group = group,
    nested = true,
    callback = function()
      M.restore()
    end,
  })
  vim.api.nvim_create_autocmd("VimLeavePre", {
    group = group,
    callback = function()
      M.save()
    end,
  })
end

return M
