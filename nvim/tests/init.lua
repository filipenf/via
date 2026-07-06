-- init.lua
-- Test runner for via's Neovim Lua suite. Run with:
--   nvim --headless -u NONE -i NONE -n -l nvim/tests/init.lua
-- or via scripts/test-nvim.sh. Exits non-zero if any test fails.

local this = debug.getinfo(1, "S").source:sub(2)
local tests_dir = vim.fn.fnamemodify(this, ":h")

-- Make `require("helpers")` and specs resolvable regardless of cwd.
package.path = package.path .. ";" .. tests_dir .. "/?.lua"

local t = require("helpers")

-- Load and register every *_spec.lua in this directory.
local specs = vim.fn.glob(tests_dir .. "/*_spec.lua", true, true)
table.sort(specs)
if #specs == 0 then
  io.write("no *_spec.lua files found in " .. tests_dir .. "\n")
  os.exit(1)
end
for _, spec in ipairs(specs) do
  dofile(spec)
end

local ok = t.run()
os.exit(ok and 0 or 1)
