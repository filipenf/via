local path = __PATH__; local line = __LINE__;

local function basename(p)
  return vim.fn.fnamemodify(p, ":t")
end

local function abspath(p)
  return vim.fn.fnamemodify(p, ":p")
end

local function drop(p)
  local prev = vim.fn.win_getid(vim.fn.winnr("#"))
  local cur = vim.api.nvim_get_current_win()
  local buf = vim.api.nvim_win_get_buf(cur)
  local bt = vim.api.nvim_get_option_value("buftype", { buf = buf })
  if bt ~= "" and prev ~= 0 and vim.api.nvim_win_is_valid(prev) then
    vim.api.nvim_set_current_win(prev)
  end
  local escaped = vim.fn.fnameescape(p)
  if line then
    vim.cmd("drop +" .. line .. " " .. escaped)
  else
    vim.cmd("drop " .. escaped)
  end
end

local function systemlist(cmd)
  local out = vim.fn.systemlist(cmd)
  if vim.v.shell_error ~= 0 then
    return {}
  end
  local cleaned = {}
  for _, item in ipairs(out) do
    if item ~= "" then
      table.insert(cleaned, item)
    end
  end
  return cleaned
end

local function path_set(paths)
  local set = {}
  for _, p in ipairs(paths) do
    set[p] = true
    set[abspath(p)] = true
    set[vim.fn.fnamemodify(p, ":.")] = true
  end
  return set
end

local function filter_by_set(candidates, set)
  local matched = {}
  for _, cand in ipairs(candidates) do
    local rel = vim.fn.fnamemodify(cand, ":.")
    if set[cand] or set[abspath(cand)] or set[rel] then
      table.insert(matched, cand)
    end
  end
  return matched
end

local function vcs_kind()
  if vim.fn.isdirectory(".jj") == 1 then
    return "jj"
  end
  if vim.fn.isdirectory(".git") == 1 or vim.fn.filereadable(".git") == 1 then
    return "git"
  end
  return nil
end

local function git_base_ref()
  for _, ref in ipairs({ "main", "master", "@{upstream}" }) do
    vim.fn.system({ "git", "rev-parse", "--verify", ref })
    if vim.v.shell_error == 0 then
      return ref
    end
  end
  return nil
end

-- Parse `git status --porcelain` lines into path strings (final path for renames).
local function parse_git_porcelain(lines)
  local paths = {}
  for _, entry in ipairs(lines) do
    local renamed = entry:match("^.. .* %-> (.+)$")
    if renamed then
      table.insert(paths, renamed)
    else
      local plain = entry:match("^.. (.+)$")
      if plain then
        table.insert(paths, plain)
      end
    end
  end
  return paths
end

local function working_tree_paths(kind)
  if kind == "jj" then
    return systemlist({ "jj", "diff", "--name-only", "--no-pager" })
  end
  return parse_git_porcelain(systemlist({ "git", "status", "--porcelain" }))
end

local function branch_changed_paths(kind)
  if kind == "jj" then
    return systemlist({ "jj", "diff", "--from", "trunk()", "--name-only", "--no-pager" })
  end
  local base = git_base_ref()
  if not base then
    return {}
  end
  return systemlist({ "git", "diff", "--name-only", base .. "...HEAD" })
end

local function open_buffer_matches(fname)
  local matches = {}
  for _, bufnr in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(bufnr) and vim.bo[bufnr].buflisted then
      local name = vim.api.nvim_buf_get_name(bufnr)
      if name ~= "" and basename(name) == fname then
        table.insert(matches, abspath(name))
      end
    end
  end
  return matches
end

local function filesystem_matches(fname)
  local found = vim.fn.globpath(".", "**/" .. fname, false, true)
  local matches = {}
  local seen = {}
  for _, item in ipairs(found) do
    local abs = abspath(item)
    if not seen[abs] and vim.fn.filereadable(abs) == 1 then
      seen[abs] = true
      table.insert(matches, abs)
    end
  end
  return matches
end

local function pick_candidate(fname, candidates)
  local ok, builtin = pcall(require, "telescope.builtin")
  if ok and builtin.find_files then
    local actions = require("telescope.actions")
    local action_state = require("telescope.actions.state")
    builtin.find_files({
      default_text = fname,
      attach_mappings = function(prompt_bufnr, _)
        actions.select_default:replace(function()
          local selection = action_state.get_selected_entry()
          actions.close(prompt_bufnr)
          if selection and selection.path then
            drop(selection.path)
          elseif selection and selection.value then
            drop(selection.value)
          end
        end)
        return true
      end,
    })
    return
  end
  vim.ui.select(candidates, {
    prompt = "Open file matching " .. fname,
    format_item = function(item)
      return vim.fn.fnamemodify(item, ":.")
    end,
  }, function(choice)
    if choice then
      drop(choice)
    end
  end)
end

local function resolve_and_open()
  if vim.fn.filereadable(path) == 1 then
    drop(path)
    return
  end

  local fname = basename(path)
  local buf_matches = open_buffer_matches(fname)
  if #buf_matches == 1 then
    drop(buf_matches[1])
    return
  end

  local candidates = filesystem_matches(fname)
  if #candidates == 0 then
    drop(path)
    return
  end
  if #candidates == 1 then
    drop(candidates[1])
    return
  end

  local kind = vcs_kind()
  if kind then
    local wt = filter_by_set(candidates, path_set(working_tree_paths(kind)))
    if #wt == 1 then
      drop(wt[1])
      return
    end
    if #wt > 1 then
      candidates = wt
    else
      local branch = filter_by_set(candidates, path_set(branch_changed_paths(kind)))
      if #branch == 1 then
        drop(branch[1])
        return
      end
      if #branch > 1 then
        candidates = branch
      end
    end
  end

  if #candidates == 1 then
    drop(candidates[1])
    return
  end

  pick_candidate(fname, candidates)
end

resolve_and_open()
