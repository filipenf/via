-- tasks_spec.lua
-- Tests for nvim/tasks.lua (the :ViaTasks Project Management UI).

local t = require("helpers")
local tasks = t.load_tasks_module()
local I = tasks._internal

-- ---------------------------------------------------------------------------
-- parse_row
-- ---------------------------------------------------------------------------

t.it("parse_row: valid row", function()
  local parsed, err = I.parse_row("t1   queued   agent   Hello world")
  t.is_nil(err)
  t.eq({ id = "t1", status = "queued", assignee = "agent", title = "Hello world" }, parsed)
end)

t.it("parse_row: comment line is skipped (not an error)", function()
  local parsed, err = I.parse_row("# All tasks")
  t.is_nil(parsed)
  t.is_nil(err)
end)

t.it("parse_row: blank line is skipped (not an error)", function()
  local parsed, err = I.parse_row("   ")
  t.is_nil(parsed)
  t.is_nil(err)
end)

t.it("parse_row: leading indent (Your queue row) is stripped", function()
  local parsed, err = I.parse_row("  t1  review  human  A title")
  t.is_nil(err)
  t.eq({ id = "t1", status = "review", assignee = "human", title = "A title" }, parsed)
end)

t.it("parse_row: assignee '-' becomes nil", function()
  local parsed = I.parse_row("t1  queued  -  A title")
  t.is_nil(parsed.assignee)
end)

t.it("parse_row: missing title (3 columns) is an error", function()
  local parsed, err = I.parse_row("t1  queued  agent")
  t.is_nil(parsed)
  t.contains(err, "missing task title")
end)

t.it("parse_row: incomplete row (< 3 columns) is an error", function()
  local parsed, err = I.parse_row("t1  queued")
  t.is_nil(parsed)
  t.contains(err, "incomplete task row")
end)

t.it("parse_row: title with multiple words is joined", function()
  local parsed = I.parse_row("t1  queued  agent  one two three")
  t.eq("one two three", parsed.title)
end)

-- ---------------------------------------------------------------------------
-- format_row + round-trip
-- ---------------------------------------------------------------------------

t.it("format_row: round-trips through parse_row", function()
  local task = { id = "t1", status = "in_progress", assignee = "coder", title = "Do the thing" }
  local parsed = I.parse_row(I.format_row(task))
  t.eq(task, parsed)
end)

t.it("format_row: nil assignee renders '-' and round-trips to nil", function()
  local row = I.format_row({ id = "t1", status = "queued", assignee = nil, title = "Title" })
  t.contains(row, "-")
  local parsed = I.parse_row(row)
  t.is_nil(parsed.assignee)
end)

-- ---------------------------------------------------------------------------
-- build_content
-- ---------------------------------------------------------------------------

t.it("build_content: human review task appears in Your queue and All tasks", function()
  local data = {
    board = "default",
    tasks = {
      { id = "t1", status = "review", assignee = "human", title = "Review me" },
      { id = "t2", status = "queued", assignee = "coder", title = "Later" },
    },
  }
  local content = I.build_content(data)
  local joined = table.concat(content.lines, "\n")
  t.contains(joined, "# Your queue")
  t.contains(joined, "# All tasks")
  -- t1 appears twice (queue + all), t2 once.
  local t1_count = select(2, joined:gsub("t1%s", ""))
  t.eq(2, t1_count, "t1 should appear in both Your queue and All tasks")
  -- snapshot is keyed by id.
  t.truthy(content.snapshot.t1)
  t.truthy(content.snapshot.t2)
  t.eq("review", content.snapshot.t1.status)
end)

t.it("build_content: no Your queue header when nothing is human/review", function()
  local data = {
    board = "default",
    tasks = {
      { id = "t1", status = "queued", assignee = "coder", title = "Work" },
    },
  }
  local content = I.build_content(data)
  local joined = table.concat(content.lines, "\n")
  t.truthy(not joined:find("# Your queue", 1, true), "should not have a Your queue section")
  t.contains(joined, "# All tasks")
end)

-- ---------------------------------------------------------------------------
-- validate_rows (uses a real scratch buffer)
-- ---------------------------------------------------------------------------

t.it("validate_rows: clean buffer produces no diagnostics", function()
  local bufnr = vim.api.nvim_create_buf(false, true)
  local lines = {
    "# All tasks",
    "t1  queued  agent  Title one",
    "t2  done    -      Title two",
  }
  local parsed, errors = I.validate_rows(bufnr, lines)
  t.eq(0, errors)
  t.eq(2, #parsed)
end)

t.it("validate_rows: malformed row sets a diagnostic", function()
  local bufnr = vim.api.nvim_create_buf(false, true)
  local lines = {
    "# All tasks",
    "t1  queued  agent", -- missing title
  }
  local _, errors = I.validate_rows(bufnr, lines)
  t.eq(1, errors)
  local diags = vim.diagnostic.get(bufnr, { namespace = I.diagnostics_ns })
  t.eq(1, #diags)
  t.contains(diags[1].message, "missing task title")
  t.eq(1, diags[1].lnum, "diagnostic should be on the second line (0-based 1)")
end)

-- ---------------------------------------------------------------------------
-- M.save (stubs the command runner; uses a real buffer + snapshot)
-- ---------------------------------------------------------------------------

--- Set up a fresh module + buffer for a save test. Returns (mod, bufnr, calls).
local function save_fixture(lines, snapshot)
  local mod = t.load_tasks_module()
  local calls = {}
  mod.run = function(cmd)
    table.insert(calls, cmd)
    if cmd[3] == "list" then
      return '{"board":"default","tasks":[]}', 0
    end
    return "", 0
  end
  local bufnr = vim.api.nvim_create_buf(false, true)
  -- Use a normal-like buftype so `modified` tracks user edits the way the real
  -- :ViaTasks buffer does (scratch buffers reset `modified` automatically).
  vim.api.nvim_buf_set_name(bufnr, "via://tasks-test-" .. bufnr)
  vim.bo[bufnr].buftype = ""
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, lines)
  vim.b[bufnr].via_tasks_snapshot = snapshot
  vim.api.nvim_set_current_buf(bufnr)
  return mod, bufnr, calls
end

local function update_calls(calls)
  local updates = {}
  for _, c in ipairs(calls) do
    if c[3] == "update" then
      table.insert(updates, c)
    end
  end
  return updates
end

t.it("save: sends a field-scoped update for the changed field only", function()
  local mod, _, calls = save_fixture({
    "# All tasks",
    "t1  queued  agent  Hello",
  }, {
    t1 = { status = "done", assignee = "agent", title = "Hello" },
  })
  mod.save()
  local updates = update_calls(calls)
  t.eq(1, #updates)
  t.eq({ "via", "task", "update", "t1", "--status", "queued" }, updates[1])
end)

t.it("save: dedups a task shown in both Your queue and All tasks", function()
  local mod, _, calls = save_fixture({
    "# Your queue (assignee=human, status=review|in_progress)",
    "  t1  done  human  Task one",
    "",
    "# All tasks",
    "t1  done  human  Task one",
  }, {
    t1 = { status = "review", assignee = "human", title = "Task one" },
  })
  mod.save()
  local updates = update_calls(calls)
  t.eq(1, #updates, "duplicate rows for one id must produce a single update")
end)

t.it("save: aborts with a diagnostic when a row is malformed", function()
  local mod, bufnr, calls = save_fixture({
    "# All tasks",
    "t1  queued  agent", -- missing title
  }, {
    t1 = { status = "done", assignee = "agent", title = "Hello" },
  })
  mod.save()
  t.eq(0, #update_calls(calls), "no update should be sent when any row is invalid")
  local diags = vim.diagnostic.get(bufnr, { namespace = mod._internal.diagnostics_ns })
  t.eq(1, #diags)
  t.contains(diags[1].message, "missing task title")
end)

t.it("save: clears assignee when the column is set to '-'", function()
  local mod, _, calls = save_fixture({
    "# All tasks",
    "t1  queued  -  Hello",
  }, {
    t1 = { status = "queued", assignee = "coder", title = "Hello" },
  })
  mod.save()
  local updates = update_calls(calls)
  t.eq(1, #updates)
  t.eq({ "via", "task", "update", "t1", "--clear-assignee" }, updates[1])
end)

t.it("save: aborts with a diagnostic when a task id is unknown", function()
  local mod, bufnr, calls = save_fixture({
    "# All tasks",
    "t1  queued  agent  Hello",
  }, {
    t2 = { status = "done", assignee = "agent", title = "Other" },
  })
  mod.save()
  t.eq(0, #update_calls(calls), "no update should be sent when any id is unknown")
  local diags = vim.diagnostic.get(bufnr, { namespace = mod._internal.diagnostics_ns })
  t.eq(1, #diags)
  t.contains(diags[1].message, "unknown task id")
end)

t.it("save: keeps buffer modified and does not refresh when an update fails", function()
  local mod, bufnr, calls = save_fixture({
    "# All tasks",
    "t1  done  agent  Hello",
    "t2  done  agent  World",
  }, {
    t1 = { status = "queued", assignee = "agent", title = "Hello" },
    t2 = { status = "queued", assignee = "agent", title = "World" },
  })
  -- Simulate an active edit so `modified` is meaningful.
  vim.bo[bufnr].modified = true
  -- First update succeeds, second fails.
  local call_idx = 0
  mod.run = function(cmd)
    table.insert(calls, cmd)
    if cmd[3] == "list" then
      return '{"board":"default","tasks":[]}', 0
    end
    call_idx = call_idx + 1
    if call_idx == 1 then
      return "", 0
    end
    return "server error", 1
  end
  mod.save()
  local updates = update_calls(calls)
  t.eq(2, #updates, "both updates should have been attempted")
  t.eq(true, vim.bo[bufnr].modified, "buffer must stay modified after a failed update")
  local list_calls = 0
  for _, c in ipairs(calls) do
    if c[3] == "list" then
      list_calls = list_calls + 1
    end
  end
  t.eq(0, list_calls, "refresh (list) must not run after a failed update")
end)

t.it("open_task_body: round-trips body without adding a trailing empty line", function()
  local mod = t.load_tasks_module()
  mod.run = function(cmd)
    if cmd[3] == "show" then
      return '{"id":"t1","status":"queued","assignee":"agent","title":"Hello","body":"line one\\nline two"}', 0
    end
    return "", 0
  end
  -- Set up a parent buffer with a task line so open_task_body can read it.
  local parent = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(parent, 0, -1, false, { "# All tasks", "t1  queued  agent  Hello" })
  vim.api.nvim_win_set_buf(0, parent)
  vim.api.nvim_win_set_cursor(0, { 2, 0 })
  mod.open_task_body()
  local body_buf = vim.fn.bufnr("via://task/t1")
  t.neq(-1, body_buf, "body buffer should exist")
  local lines = vim.api.nvim_buf_get_lines(body_buf, 0, -1, false)
  t.eq({ "line one", "line two" }, lines)
end)
