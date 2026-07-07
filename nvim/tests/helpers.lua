-- helpers.lua
-- Minimal test framework for via's Neovim Lua suite. No external dependencies
-- (no busted/plenary) — runs under `nvim -l`. See nvim/tests/init.lua.

local M = {}

M._tests = {}
M.passed = 0
M.failed = 0

--- Register a test case.
function M.it(name, fn)
  table.insert(M._tests, { name = name, fn = fn })
end

local function fmt(v)
  return vim.inspect(v)
end

--- Assert deep equality (uses vim.deep_equal for tables).
function M.eq(expected, actual, msg)
  if not vim.deep_equal(expected, actual) then
    error(
      (msg or "values not equal")
        .. "\n  expected: "
        .. fmt(expected)
        .. "\n  actual:   "
        .. fmt(actual),
      2
    )
  end
end

--- Assert inequality.
function M.neq(expected, actual, msg)
  if vim.deep_equal(expected, actual) then
    error((msg or "values unexpectedly equal") .. ": " .. fmt(actual), 2)
  end
end

--- Assert a value is nil.
function M.is_nil(v, msg)
  if v ~= nil then
    error((msg or "expected nil") .. ", got " .. fmt(v), 2)
  end
end

--- Assert a value is truthy.
function M.truthy(v, msg)
  if not v then
    error(msg or "expected a truthy value", 2)
  end
end

--- Assert a string contains a substring (plain match, no patterns).
function M.contains(haystack, needle, msg)
  if type(haystack) ~= "string" or not haystack:find(needle, 1, true) then
    error(
      (msg or "expected string to contain")
        .. " '"
        .. needle
        .. "', got "
        .. fmt(haystack),
      2
    )
  end
end

--- Load a fresh copy of nvim/tasks.lua (the source of truth, not the installed
--- copy). Returns the module table `M` with `_internal` helpers exposed. Each
--- call returns a new instance so tests can stub `.run` in isolation.
function M.load_tasks_module()
  local helpers_src = debug.getinfo(1, "S").source:sub(2)
  local nvim_dir = vim.fn.fnamemodify(helpers_src, ":h:h") -- nvim/tests -> nvim
  local chunk = assert(loadfile(nvim_dir .. "/tasks.lua"))
  return chunk()
end

--- Run all registered tests, printing TAP-ish output. Returns true if all passed.
function M.run()
  for _, t in ipairs(M._tests) do
    local ok, err = pcall(t.fn)
    if ok then
      M.passed = M.passed + 1
      io.write("ok - " .. t.name .. "\n")
    else
      M.failed = M.failed + 1
      io.write("not ok - " .. t.name .. "\n")
      io.write("  " .. tostring(err):gsub("\n", "\n  ") .. "\n")
    end
  end
  io.write(string.format("\n%d passed, %d failed\n", M.passed, M.failed))
  return M.failed == 0
end

return M
