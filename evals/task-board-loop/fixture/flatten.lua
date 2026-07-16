-- Nested-list helpers for the via task-board eval fixture.
-- Intentionally almost-correct: most smoke cases pass; one classic defect remains.

local M = {}

--- Flatten a nested list of numbers into a single list (depth-first).
function M.flatten(nested)
  local out = {}
  for _, v in ipairs(nested) do
    if type(v) == "table" then
      -- BUG: only one level deep; nested tables are copied as elements.
      for _, x in ipairs(v) do
        out[#out + 1] = x
      end
    else
      out[#out + 1] = v
    end
  end
  return out
end

return M
