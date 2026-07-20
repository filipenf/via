local path = __PATH__; local line = __LINE__; local index_candidates = __CANDIDATES__;
local vcs = require("via.vcs")
local path_match = require("via.path_match")

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

-- Open buffers whose path ends with the longest matching truncated suffix.
local function open_buffer_suffix_matches(query)
  if not query or query == "" then
    return {}
  end
  local names = {}
  for _, bufnr in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(bufnr) and vim.bo[bufnr].buflisted then
      local name = vim.api.nvim_buf_get_name(bufnr)
      if name ~= "" then
        table.insert(names, abspath(name))
      end
    end
  end
  return path_match.filter_by_longest_suffix(names, query)
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

-- Fixed-list picker for trusted index candidates (never widen to full-repo search).
local function select_candidate(fname, candidates)
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

-- Cold-index fallback: Telescope find_files when available, else vim.ui.select.
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
  select_candidate(fname, candidates)
end

local function resolve_and_open()
  if vim.fn.filereadable(path) == 1 then
    drop(path)
    return
  end

  local fname = basename(path)

  -- Trusted index candidates from Rust: skip independent glob/VCS rediscovery.
  if type(index_candidates) == "table" and #index_candidates > 0 then
    if #index_candidates == 1 then
      drop(index_candidates[1])
      return
    end
    select_candidate(fname, index_candidates)
    return
  end

  local trunc_query = path_match.truncated_query_from(path)
  if trunc_query then
    local buf_suffix = open_buffer_suffix_matches(trunc_query)
    if #buf_suffix == 1 then
      drop(buf_suffix[1])
      return
    end
    if #buf_suffix > 1 then
      select_candidate(fname, buf_suffix)
      return
    end
  end

  local buf_matches = open_buffer_matches(fname)
  if #buf_matches == 1 then
    drop(buf_matches[1])
    return
  end

  local candidates = filesystem_matches(fname)
  if trunc_query and #candidates > 0 then
    candidates = path_match.filter_by_longest_suffix(candidates, trunc_query)
  end

  if #candidates == 0 then
    drop(path)
    return
  end
  if #candidates == 1 then
    drop(candidates[1])
    return
  end

  local kind, root = vcs.root()
  if kind then
    local wt = filter_by_set(
      candidates,
      path_set(vcs.resolve_paths(root, vcs.working_tree_paths(kind, root)))
    )
    if #wt == 1 then
      drop(wt[1])
      return
    end
    if #wt > 1 then
      candidates = wt
    else
      local branch = filter_by_set(
        candidates,
        path_set(vcs.resolve_paths(root, vcs.branch_changed_paths(kind, root)))
      )
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
