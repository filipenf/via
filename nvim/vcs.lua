-- via.vcs — shared jj/git discovery and changed-path helpers.
-- Load with: require("via.vcs")
--
-- All process spawns go through pcall-wrapped vim.system so a missing binary
-- (common for `jj`) never raises into callers like open_file / context_bridge.

local M = {}

local executable_cache = {}

local function executable(name)
  local cached = executable_cache[name]
  if cached ~= nil then
    return cached
  end
  local ok = vim.fn.executable(name) == 1
  executable_cache[name] = ok
  return ok
end

--- Run `vim.system` safely. Returns wait() result or nil on spawn/wait failure.
local function safe_system(cmd, opts)
  local ok, handle_or_err = pcall(vim.system, cmd, opts)
  if not ok or handle_or_err == nil then
    return nil
  end
  local wait_ok, result = pcall(function()
    return handle_or_err:wait()
  end)
  if not wait_ok then
    return nil
  end
  return result
end

--- Capture non-empty stdout lines. Empty / failure → {}.
function M.systemlist(cmd, cwd)
  local result = safe_system(cmd, { cwd = cwd, text = true })
  if not result or result.code ~= 0 then
    return {}
  end
  local cleaned = {}
  for item in (result.stdout or ""):gmatch("[^\n]+") do
    if item ~= "" then
      table.insert(cleaned, item)
    end
  end
  return cleaned
end

function M.system_ok(cmd, cwd)
  local result = safe_system(cmd, { cwd = cwd })
  return result ~= nil and result.code == 0
end

local function first_line(cmd, cwd)
  return M.systemlist(cmd, cwd)[1]
end

local function git_base_ref(root)
  for _, ref in ipairs({ "main", "master", "@{upstream}" }) do
    if M.system_ok({ "git", "rev-parse", "--verify", ref }, root) then
      return ref
    end
  end
  return nil
end

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

--- Walk from `start` (default: cwd) toward `/` looking for a VCS root.
--- At each directory, `.jj` wins over `.git` / `.git` file (colocated → jj).
--- Returns `kind, root_dir` or `nil, nil`. Does not spawn VCS binaries.
local function find_repo_root(start)
  local dir = vim.fn.fnamemodify(start or vim.fn.getcwd(), ":p")
  while dir and dir ~= "" do
    -- Trim trailing slash except for filesystem root.
    if #dir > 1 and dir:sub(-1) == "/" then
      dir = dir:sub(1, -2)
    end
    local jj_dir = dir .. "/.jj"
    local git_path = dir .. "/.git"
    if vim.fn.isdirectory(jj_dir) == 1 then
      return "jj", dir
    end
    if vim.fn.isdirectory(git_path) == 1 or vim.fn.filereadable(git_path) == 1 then
      return "git", dir
    end
    local parent = vim.fn.fnamemodify(dir, ":h")
    if parent == dir then
      break
    end
    dir = parent
  end
  return nil, nil
end

--- Detect VCS kind and absolute workspace root.
--- Prefer jj only when a `.jj` directory exists in an ancestor of cwd; otherwise
--- git. Having the `jj` binary installed must not select jj for a pure git repo.
--- Returns `kind, root` or `nil, nil`.
function M.root()
  local kind, marker_root = find_repo_root()
  if not kind then
    return nil, nil
  end

  if kind == "jj" then
    if executable("jj") then
      local jj_root = first_line({ "jj", "root", "--no-pager" }, marker_root)
      if jj_root then
        return "jj", jj_root
      end
    end
    -- Colocated / jj metadata present but binary missing or `jj root` failed:
    -- fall through to git at the same tree when possible.
    local git_path = marker_root .. "/.git"
    if
      (vim.fn.isdirectory(git_path) == 1 or vim.fn.filereadable(git_path) == 1)
      and executable("git")
    then
      local git_root = first_line({ "git", "rev-parse", "--show-toplevel" }, marker_root)
      if git_root then
        return "git", git_root
      end
    end
    return "jj", marker_root
  end

  if executable("git") then
    local git_root = first_line({ "git", "rev-parse", "--show-toplevel" }, marker_root)
    if git_root then
      return "git", git_root
    end
  end
  return "git", marker_root
end

--- Working-tree changed paths, relative to `root` (VCS tool output as-is).
function M.working_tree_paths(kind, root)
  if kind == "jj" then
    if not executable("jj") then
      return {}
    end
    return M.systemlist({ "jj", "diff", "--name-only", "--no-pager" }, root)
  end
  if kind == "git" then
    if not executable("git") then
      return {}
    end
    return parse_git_porcelain(M.systemlist({ "git", "status", "--porcelain" }, root))
  end
  return {}
end

--- Branch-vs-base changed paths, relative to `root`.
function M.branch_changed_paths(kind, root)
  if kind == "jj" then
    if not executable("jj") then
      return {}
    end
    return M.systemlist({ "jj", "diff", "--from", "trunk()", "--name-only", "--no-pager" }, root)
  end
  if kind == "git" then
    if not executable("git") then
      return {}
    end
    local base = git_base_ref(root)
    if not base then
      return {}
    end
    return M.systemlist({ "git", "diff", "--name-only", base .. "...HEAD" }, root)
  end
  return {}
end

--- Resolve VCS root-relative paths to absolute paths under `root`.
--- Avoids `fnamemodify(..., ":p")` against Neovim's cwd when cwd ≠ VCS root.
function M.resolve_paths(root, paths)
  local out = {}
  for _, p in ipairs(paths or {}) do
    if p ~= "" then
      if p:sub(1, 1) == "/" or p:match("^%a:[/\\]") then
        table.insert(out, vim.fn.fnamemodify(p, ":p"))
      else
        table.insert(out, vim.fn.fnamemodify(root .. "/" .. p, ":p"))
      end
    end
  end
  return out
end

-- Test / debug hooks (not part of the stable public API).
M._internal = {
  executable = executable,
  safe_system = safe_system,
  parse_git_porcelain = parse_git_porcelain,
  find_repo_root = find_repo_root,
  reset_executable_cache = function()
    executable_cache = {}
  end,
  set_executable_cache = function(name, value)
    executable_cache[name] = value
  end,
}

return M
