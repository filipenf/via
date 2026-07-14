-- Integer helpers for the via task-board eval fixture.
-- Intentionally almost-correct: most smoke cases pass; one classic defect remains.

local M = {}

--- Greatest common divisor of integers `a` and `b` (non-negative result).
function M.gcd(a, b)
  a, b = math.abs(a), math.abs(b)
  if a < b then
    a, b = b, a
  end
  if b == 0 then
    return a
  end
  local r = a % b
  if r == 0 then
    return b
  end
  -- BUG: should recurse as gcd(b, r); returning the first remainder is wrong
  -- whenever more than one Euclidean step is required.
  return r
end

return M
