-- Acceptance tests for bisect / flatten / gcd. Run via ./verify.sh (nvim -l).
--
-- Buckets mirror SWE-bench-style grading:
--   PASS_TO_PASS  — already green on the buggy fixture; must stay green
--   FAIL_TO_PASS  — red today; must become green after repair

local this = debug.getinfo(1, "S").source:sub(2)
local dir = this:match("(.*/)") or "./"
package.path = dir .. "?.lua;" .. package.path

local bisect = require("bisect")
local flatten = require("flatten")
local gcd = require("gcd")

local failures = 0
local ran = 0

local function deep_eq(a, b)
  if type(a) ~= type(b) then
    return false
  end
  if type(a) ~= "table" then
    return a == b
  end
  if #a ~= #b then
    return false
  end
  for i = 1, #a do
    if not deep_eq(a[i], b[i]) then
      return false
    end
  end
  return true
end

local function fmt(v)
  if type(v) ~= "table" then
    return string.format("%q", tostring(v))
  end
  local parts = {}
  for i = 1, #v do
    parts[#parts + 1] = fmt(v[i])
  end
  return "{" .. table.concat(parts, ", ") .. "}"
end

local function assert_eq(got, want, bucket, name)
  ran = ran + 1
  local ok
  if type(want) == "table" then
    ok = deep_eq(got, want)
  else
    ok = got == want
  end
  if ok then
    io.write(string.format("ok  [%s] %s\n", bucket, name))
    return
  end
  failures = failures + 1
  io.write(
    string.format(
      "FAIL  [%s] %s\n  got:  %s\n  want: %s\n",
      bucket,
      name,
      fmt(got),
      fmt(want)
    )
  )
end

-- ---- bisect.find_first_in_sorted ------------------------------------------

assert_eq(bisect.find_first_in_sorted({ 1, 2, 3, 4, 5 }, 3), 3, "PASS_TO_PASS", "bisect unique mid")
assert_eq(bisect.find_first_in_sorted({ 1, 2, 3 }, 1), 1, "PASS_TO_PASS", "bisect first element")
assert_eq(bisect.find_first_in_sorted({ 1, 2, 3 }, 9), -1, "PASS_TO_PASS", "bisect missing")
assert_eq(bisect.find_first_in_sorted({}, 1), -1, "PASS_TO_PASS", "bisect empty")

assert_eq(
  bisect.find_first_in_sorted({ 1, 2, 2, 2, 3 }, 2),
  2,
  "FAIL_TO_PASS",
  "bisect first of duplicates"
)
assert_eq(
  bisect.find_first_in_sorted({ 0, 0, 0, 0 }, 0),
  1,
  "FAIL_TO_PASS",
  "bisect all equal"
)

-- ---- flatten.flatten ------------------------------------------------------

assert_eq(flatten.flatten({ 1, { 2, 3 }, 4 }), { 1, 2, 3, 4 }, "PASS_TO_PASS", "flatten one level")
assert_eq(flatten.flatten({ 1, 2, 3 }), { 1, 2, 3 }, "PASS_TO_PASS", "flatten already flat")
assert_eq(flatten.flatten({}), {}, "PASS_TO_PASS", "flatten empty")

assert_eq(
  flatten.flatten({ 1, { 2, { 3, 4 } }, 5 }),
  { 1, 2, 3, 4, 5 },
  "FAIL_TO_PASS",
  "flatten deep nest"
)
assert_eq(
  flatten.flatten({ { { 1 } }, 2 }),
  { 1, 2 },
  "FAIL_TO_PASS",
  "flatten deeply nested head"
)

-- ---- gcd.gcd --------------------------------------------------------------

assert_eq(gcd.gcd(10, 5), 5, "PASS_TO_PASS", "gcd divides evenly")
assert_eq(gcd.gcd(7, 0), 7, "PASS_TO_PASS", "gcd b zero")
assert_eq(gcd.gcd(0, 9), 9, "PASS_TO_PASS", "gcd a zero")
assert_eq(gcd.gcd(14, 21), 7, "PASS_TO_PASS", "gcd unordered single step")

assert_eq(gcd.gcd(48, 18), 6, "FAIL_TO_PASS", "gcd multi-step 48,18")
assert_eq(gcd.gcd(100, 35), 5, "FAIL_TO_PASS", "gcd multi-step 100,35")
assert_eq(gcd.gcd(-48, 18), 6, "FAIL_TO_PASS", "gcd negative input")

io.write(string.format("\n%d ran, %d failed\n", ran, failures))
os.exit(failures == 0 and 0 or 1)
