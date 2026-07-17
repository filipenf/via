-- Tests for nvim/vcs.lua (require("via.vcs")).

local t = require("helpers")

local function make_temp_repo(opts)
  local root = vim.fn.tempname()
  vim.fn.mkdir(root, "p")
  if opts.jj then
    vim.fn.mkdir(root .. "/.jj", "p")
  end
  if opts.git then
    if opts.git == "file" then
      vim.fn.writefile({ "gitdir: /tmp/fake" }, root .. "/.git")
    else
      vim.fn.mkdir(root .. "/.git", "p")
    end
  end
  if opts.subdir then
    vim.fn.mkdir(root .. "/" .. opts.subdir, "p")
  end
  return root
end

t.it("find_repo_root prefers .jj over .git at the same level", function()
  local vcs = t.load_vcs_module()
  local root = make_temp_repo({ jj = true, git = true })
  local kind, found = vcs._internal.find_repo_root(root)
  t.eq("jj", kind)
  t.contains(found, root)
end)

t.it("find_repo_root selects git when only .git exists", function()
  local vcs = t.load_vcs_module()
  local root = make_temp_repo({ git = true })
  local kind, found = vcs._internal.find_repo_root(root)
  t.eq("git", kind)
  t.contains(found, root)
end)

t.it("find_repo_root returns nil when neither .jj nor .git exists", function()
  local vcs = t.load_vcs_module()
  local root = make_temp_repo({})
  local kind, found = vcs._internal.find_repo_root(root)
  t.is_nil(kind)
  t.is_nil(found)
end)

t.it("root returns nil when cwd has no VCS markers", function()
  local vcs = t.load_vcs_module()
  local root = make_temp_repo({})
  vcs._internal.reset_executable_cache()
  -- Binaries available must not invent a repo without markers.
  vcs._internal.set_executable_cache("jj", true)
  vcs._internal.set_executable_cache("git", true)
  local saved = vim.fn.getcwd()
  vim.cmd("cd " .. vim.fn.fnameescape(root))
  local kind, found = vcs.root()
  vim.cmd("cd " .. vim.fn.fnameescape(saved))
  t.is_nil(kind)
  t.is_nil(found)
end)

t.it("find_repo_root walks up from a subdirectory", function()
  local vcs = t.load_vcs_module()
  local root = make_temp_repo({ git = true, subdir = "src/nested" })
  local kind, found = vcs._internal.find_repo_root(root .. "/src/nested")
  t.eq("git", kind)
  t.contains(found, root)
end)

t.it("find_repo_root treats .git file as a git worktree marker", function()
  local vcs = t.load_vcs_module()
  local root = make_temp_repo({ git = "file" })
  local kind = vcs._internal.find_repo_root(root)
  t.eq("git", kind)
end)

t.it("root uses git for a pure git tree even when jj binary is available", function()
  local vcs = t.load_vcs_module()
  local root = make_temp_repo({ git = true })
  vcs._internal.reset_executable_cache()
  -- Pretend jj is installed; marker-based detection must still choose git.
  vcs._internal.set_executable_cache("jj", true)
  vcs._internal.set_executable_cache("git", false) -- avoid spawning git in the temp dir
  local saved = vim.fn.getcwd()
  vim.cmd("cd " .. vim.fn.fnameescape(root))
  local kind, found = vcs.root()
  vim.cmd("cd " .. vim.fn.fnameescape(saved))
  t.eq("git", kind)
  t.contains(found, root)
end)

t.it("root prefers jj when .jj marker is present", function()
  local vcs = t.load_vcs_module()
  local root = make_temp_repo({ jj = true })
  vcs._internal.reset_executable_cache()
  vcs._internal.set_executable_cache("jj", false) -- no binary; still report jj from marker
  local saved = vim.fn.getcwd()
  vim.cmd("cd " .. vim.fn.fnameescape(root))
  local kind, found = vcs.root()
  vim.cmd("cd " .. vim.fn.fnameescape(saved))
  t.eq("jj", kind)
  t.contains(found, root)
end)

t.it("safe_system returns nil instead of raising on missing binary", function()
  local vcs = t.load_vcs_module()
  local result = vcs._internal.safe_system({ "via-definitely-missing-binary-xyz", "--help" }, { text = true })
  t.is_nil(result)
end)

t.it("systemlist returns empty table when binary is missing", function()
  local vcs = t.load_vcs_module()
  local lines = vcs.systemlist({ "via-definitely-missing-binary-xyz", "root" })
  t.eq({}, lines)
end)

t.it("resolve_paths joins relative entries under the VCS root", function()
  local vcs = t.load_vcs_module()
  local resolved = vcs.resolve_paths("/repo", { "src/foo.lua", "tests/foo.lua" })
  t.eq(2, #resolved)
  t.contains(resolved[1], "/repo/src/foo.lua")
  t.contains(resolved[2], "/repo/tests/foo.lua")
end)

t.it("resolve_paths keeps absolute entries", function()
  local vcs = t.load_vcs_module()
  local resolved = vcs.resolve_paths("/repo", { "/other/abs.lua" })
  t.contains(resolved[1], "/other/abs.lua")
end)

t.it("parse_git_porcelain handles renames and plain entries", function()
  local vcs = t.load_vcs_module()
  local paths = vcs._internal.parse_git_porcelain({
    " M src/a.lua",
    "R  old.lua -> new.lua",
    "",
  })
  t.eq({ "src/a.lua", "new.lua" }, paths)
end)

t.it("working_tree_paths returns empty when kind binary missing", function()
  local vcs = t.load_vcs_module()
  vcs._internal.reset_executable_cache()
  vcs._internal.set_executable_cache("jj", false)
  t.eq({}, vcs.working_tree_paths("jj", "/repo"))
  vcs._internal.set_executable_cache("git", false)
  t.eq({}, vcs.working_tree_paths("git", "/repo"))
end)
