# Reference solution (humans / grader authors only)

**Do not put finished implementations in the agent-visible `fixture/` tree.**
Agents should discover defects from `./verify.sh` output.

Defect shapes are QuixBugs-class (classic one-line algorithm bugs).

## Planted bugs

| File | Function | Defect |
| --- | --- | --- |
| `bisect.lua` | `find_first_in_sorted` | Returns the first binary-search hit instead of continuing left for the earliest index among duplicates |
| `flatten.lua` | `flatten` | Only unwraps one nesting level |
| `gcd.lua` | `gcd` | Returns the first Euclidean remainder instead of recursing `gcd(b, r)` |

## Intended implementations

### `bisect.lua`

```lua
local M = {}

function M.find_first_in_sorted(arr, x)
  local lo, hi = 1, #arr
  local result = -1
  while lo <= hi do
    local mid = math.floor((lo + hi) / 2)
    if x == arr[mid] then
      result = mid
      hi = mid - 1
    elseif x < arr[mid] then
      hi = mid - 1
    else
      lo = mid + 1
    end
  end
  return result
end

return M
```

### `flatten.lua`

```lua
local M = {}

function M.flatten(nested)
  local out = {}
  local function walk(node)
    for _, v in ipairs(node) do
      if type(v) == "table" then
        walk(v)
      else
        out[#out + 1] = v
      end
    end
  end
  walk(nested)
  return out
end

return M
```

### `gcd.lua`

```lua
local M = {}

function M.gcd(a, b)
  a, b = math.abs(a), math.abs(b)
  if b == 0 then
    return a
  end
  return M.gcd(b, a % b)
end

return M
```
