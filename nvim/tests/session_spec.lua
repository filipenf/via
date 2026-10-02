-- session_spec.lua — session file names match folke/persistence.

local t = require("helpers")
local session = t.load_session_module()

t.it("session_name encodes the working directory", function()
  t.eq(
    "%home%filipenf%sandbox%personal%via.vim",
    session.session_name("/home/filipenf/sandbox/personal/via", nil)
  )
end)

t.it("session_name appends a feature branch", function()
  t.eq(
    "%home%filipenf%sandbox%personal%via%%feat%session-restore.vim",
    session.session_name("/home/filipenf/sandbox/personal/via", "feat/session-restore")
  )
end)

t.it("session_name ignores main and master", function()
  local cwd = "/home/filipenf/sandbox/personal/via"
  local encoded = session.session_name(cwd, nil)
  t.eq(encoded, session.session_name(cwd, "main"))
  t.eq(encoded, session.session_name(cwd, "master"))
  t.eq(encoded, session.session_name(cwd, ""))
end)

t.it("session_path joins the state sessions directory", function()
  t.eq(
    session.session_dir() .. "%repo.vim",
    session.session_path("/repo", nil)
  )
end)

t.it("save skips only after persistence loaded the session", function()
  local previous = package.loaded.persistence
  package.loaded.persistence = {
    load = function() end,
  }
  local ok, err = pcall(function()
    t.truthy(session.restore())
    t.eq("persistence", session._owner)
    t.eq(false, session.save())
  end)
  package.loaded.persistence = previous
  session._owner = nil
  if not ok then
    error(err, 0)
  end
end)

t.it("failed persistence load leaves via able to save", function()
  local previous = package.loaded.persistence
  local previous_stdpath = vim.fn.stdpath
  local cwd = vim.fn.getcwd()
  local state = vim.fn.tempname()
  vim.fn.mkdir(state, "p")
  local empty = vim.fn.tempname()
  vim.fn.mkdir(empty, "p")
  package.loaded.persistence = {
    load = function()
      error("setup was not called")
    end,
  }
  vim.fn.stdpath = function(what)
    if what == "state" then
      return state
    end
    return previous_stdpath(what)
  end
  vim.fn.chdir(empty)

  local ok, err = pcall(function()
    t.eq(false, session.restore())
    t.is_nil(session._owner)
  end)

  package.loaded.persistence = previous
  vim.fn.stdpath = previous_stdpath
  vim.fn.chdir(cwd)
  session._owner = nil
  if not ok then
    error(err, 0)
  end
end)

t.it("write_session drops terminal and blank and restores sessionoptions", function()
  local previous_options = vim.o.sessionoptions
  local previous_cmd = vim.cmd
  vim.o.sessionoptions = "blank,buffers,curdir,folds,help,tabpages,winsize,terminal"
  local during
  vim.cmd = function()
    during = vim.o.sessionoptions
    error("mksession failed")
  end

  local ok, err = pcall(function()
    t.eq(false, session.write_session("/tmp/via-session-does-not-write.vim"))
    t.eq("blank,buffers,curdir,folds,help,tabpages,winsize,terminal", vim.o.sessionoptions)
    t.is_nil(during:find("terminal", 1, true))
    t.is_nil(during:find("blank", 1, true))
  end)

  vim.cmd = previous_cmd
  vim.o.sessionoptions = previous_options
  if not ok then
    error(err, 0)
  end
end)
