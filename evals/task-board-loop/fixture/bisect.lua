-- Binary search helpers for the via task-board eval fixture.
-- Intentionally almost-correct: most smoke cases pass; one classic defect remains.

local M = {}

--- Return the 1-based index of the first occurrence of `x` in sorted `arr`,
--- or -1 if missing.
function M.find_first_in_sorted(arr, x)
  local lo, hi = 1, #arr
  while lo <= hi do
    local mid = math.floor((lo + hi) / 2)
    if x == arr[mid] then
      -- BUG: returns the first hit, which is not necessarily the leftmost.
      return mid
    elseif x < arr[mid] then
      hi = mid - 1
    else
      lo = mid + 1
    end
  end
  return -1
end

return M
