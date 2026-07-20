-- via.path_match — truncated / partial path suffix helpers for open_file.
-- Load with: require("via.path_match")

local M = {}

local function normalize_slashes(s)
  return (s:gsub("\\", "/"))
end

--- Query after the earliest `...` or Unicode ellipsis (`…`), if any.
--- “Leading” means earliest marker by byte index (cwd-joined `/repo/...z/foo`
--- counts); a literal component like `foo...bar` can false-positive (v1).
function M.truncated_query_from(s)
  if type(s) ~= "string" or s == "" then
    return nil
  end
  local ascii = s:find("...", 1, true)
  local uni_start, uni_end = s:find("\u{2026}", 1, true)
  local after
  if ascii and (not uni_start or ascii < uni_start) then
    after = s:sub(ascii + 3)
  elseif uni_start then
    after = s:sub(uni_end + 1)
  else
    return nil
  end
  after = after:gsub("^[\\/]+", "")
  if after == "" then
    return nil
  end
  return normalize_slashes(after)
end

--- Progressive suffixes, longest first (`a/b/c.rs` → `a/b/c.rs`, `b/c.rs`, `c.rs`).
function M.path_suffix_queries(query)
  local parts = {}
  for part in query:gmatch("[^/]+") do
    table.insert(parts, part)
  end
  local suffixes = {}
  for start = 1, #parts do
    table.insert(suffixes, table.concat(parts, "/", start))
  end
  return suffixes
end

function M.path_ends_with_suffix(path_str, suffix)
  path_str = normalize_slashes(path_str)
  if path_str == suffix then
    return true
  end
  local needle = "/" .. suffix
  return path_str:sub(-#needle) == needle
end

--- Filter candidates to the first non-empty longest-suffix match set.
function M.filter_by_longest_suffix(candidates, query)
  if not query or query == "" then
    return candidates
  end
  for _, suffix in ipairs(M.path_suffix_queries(query)) do
    local matched = {}
    for _, cand in ipairs(candidates) do
      if M.path_ends_with_suffix(cand, suffix) then
        table.insert(matched, cand)
      end
    end
    if #matched > 0 then
      return matched
    end
  end
  return {}
end

M._internal = {
  normalize_slashes = normalize_slashes,
}

return M
