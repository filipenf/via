-- path_match_spec.lua — unit tests for nvim/path_match.lua

local t = require("helpers")
local pm = t.load_path_match_module()

t.it("truncated_query_from strips ascii ellipsis", function()
  t.eq("z/some/long/path/main.rs", pm.truncated_query_from("...z/some/long/path/main.rs"))
  t.eq("src/lib.rs", pm.truncated_query_from("/repo/.../src/lib.rs"))
end)

t.it("truncated_query_from strips unicode ellipsis", function()
  t.eq("src/lib.rs", pm.truncated_query_from("\u{2026}/src/lib.rs"))
end)

t.it("truncated_query_from returns nil without marker or empty query", function()
  t.is_nil(pm.truncated_query_from("vendor/main.rs"))
  t.is_nil(pm.truncated_query_from("..."))
  t.is_nil(pm.truncated_query_from(".../"))
  t.is_nil(pm.truncated_query_from(nil))
end)

t.it("truncated_query_from uses earliest marker by byte index", function()
  t.eq("foo...bar", pm.truncated_query_from("\u{2026}foo...bar"))
  t.eq("foo\u{2026}bar", pm.truncated_query_from("...foo\u{2026}bar"))
end)

t.it("path_suffix_queries are longest first", function()
  t.eq({
    "some/long/path/main.rs",
    "long/path/main.rs",
    "path/main.rs",
    "main.rs",
  }, pm.path_suffix_queries("some/long/path/main.rs"))
end)

t.it("path_ends_with_suffix requires component boundary", function()
  t.truthy(pm.path_ends_with_suffix("/repo/some/long/path/main.rs", "long/path/main.rs"))
  t.truthy(pm.path_ends_with_suffix("main.rs", "main.rs"))
  -- Do not match mid-component: "ath/main.rs" inside "path/main.rs"
  t.eq(false, pm.path_ends_with_suffix("/repo/path/main.rs", "ath/main.rs"))
end)

t.it("filter_by_longest_suffix prefers longest non-empty set", function()
  local candidates = {
    "/repo/other/path/main.rs",
    "/repo/some/long/path/main.rs",
  }
  local matched = pm.filter_by_longest_suffix(candidates, "z/some/long/path/main.rs")
  t.eq({ "/repo/some/long/path/main.rs" }, matched)
end)

t.it("filter_by_longest_suffix falls back to shorter suffix", function()
  local candidates = {
    "/repo/src/main.rs",
    "/repo/tests/main.rs",
  }
  local matched = pm.filter_by_longest_suffix(candidates, "z/main.rs")
  t.eq(2, #matched)
end)

t.it("filter_by_longest_suffix returns empty when nothing matches", function()
  local matched = pm.filter_by_longest_suffix({ "/repo/src/lib.rs" }, "missing/main.rs")
  t.eq({}, matched)
end)
