-- tasks_spec.lua
-- Tests for nvim/tasks.lua (the :ViaTasks Project Management UI).

local t = require("helpers")
local tasks = t.load_tasks_module()
local I = tasks._internal

-- ---------------------------------------------------------------------------
-- parse_row
-- ---------------------------------------------------------------------------

t.it("parse_row: valid row with via: prefix", function()
  local parsed, err = I.parse_row("via:t1   queued   agent   Hello world")
  t.is_nil(err)
  t.eq({ id = "t1", status = "queued", assignee = "agent", title = "Hello world" }, parsed)
end)

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

t.it("format_row: prefixes task id with via:", function()
  local row = I.format_row({ id = "t1", status = "queued", assignee = "agent", title = "Title" })
  t.contains(row, "via:t1")
end)

t.it("task_id_on_line: returns id only when cursor is on the id column", function()
  local line = I.format_row({ id = "t1", status = "queued", assignee = "agent", title = "Title" })
  t.eq("t1", I.task_id_on_line(line, 3))
  t.is_nil(I.task_id_on_line(line, #line - 1))
end)

t.it("parse_task_id: strips via: prefix", function()
  t.eq("phase2-cli", I.parse_task_id("via:phase2-cli"))
  t.eq("t1", I.parse_task_id("t1"))
end)

t.it("format_row: round-trips through parse_row (legacy bare id)", function()
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
  t.contains(joined, "# Board: default")
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
  t.contains(joined, "# Board: default")
  t.truthy(not joined:find("# Your queue", 1, true), "should not have a Your queue section")
  t.contains(joined, "# All tasks")
end)

t.it("build_content: board header lists other boards", function()
  local data = {
    board = "default",
    board_title = "Main",
    boards = {
      { id = "default", title = "Main" },
      { id = "phase2", title = "Phase 2" },
      { id = "spike" },
    },
    tasks = {},
  }
  local content = I.build_content(data)
  t.eq("# Board: default — Main  (others: phase2, spike)", content.lines[1])
end)

t.it("board_new_cmd: id only", function()
  t.eq({ "via", "task", "board", "new", "--id", "phase2" }, I.board_new_cmd("phase2"))
end)

t.it("board_new_cmd: includes trimmed title when provided", function()
  t.eq(
    { "via", "task", "board", "new", "--id", "phase2", "--title", "Phase 2" },
    I.board_new_cmd("phase2", "  Phase 2  ")
  )
end)

t.it("board_new_cmd: omits empty title", function()
  t.eq({ "via", "task", "board", "new", "--id", "phase2" }, I.board_new_cmd("phase2", "   "))
end)

t.it("slugify_board_id: derives id from display name", function()
  t.eq("phase-2-work", I.slugify_board_id("Phase 2 Work"))
  t.eq("osc8-click-cues", I.slugify_board_id("  OSC8 click cues!  "))
  t.is_nil(I.slugify_board_id("!!!"))
  t.is_nil(I.slugify_board_id(""))
end)

t.it("format_millis: formats epoch millis as UTC", function()
  t.eq("2026-07-10 14:25", I.format_millis(1783693555327))
  t.eq("-", I.format_millis(nil))
  t.eq("-", I.format_millis(0))
end)

t.it("format_board_item: shows name, active marker, and last-used date", function()
  local item = I.format_board_item({
    id = "phase2",
    title = "Phase 2",
    last_used_at = 1783693555327,
  }, "phase2")
  t.eq("phase2 — Phase 2 *  (2026-07-10 14:25)", item)
end)

t.it("format_board_item: falls back to created_at when last_used_at missing", function()
  local item = I.format_board_item({
    id = "default",
    created_at = 1783693555327,
  }, "other")
  t.eq("default  (2026-07-10 14:25)", item)
end)

t.it("task_create_cmd: title only", function()
  t.eq({ "via", "task", "create", "Do the thing" }, I.task_create_cmd("Do the thing"))
end)

t.it("task_create_cmd: title + body", function()
  t.eq(
    { "via", "task", "create", "Do the thing", "-m", "Some context" },
    I.task_create_cmd("Do the thing", "  Some context  ")
  )
end)

t.it("task_create_cmd: omits --body when body is empty/whitespace", function()
  t.eq({ "via", "task", "create", "Do the thing" }, I.task_create_cmd("Do the thing", "   "))
end)

t.it("task_create_cmd: omits --body when body is nil", function()
  t.eq({ "via", "task", "create", "Do the thing" }, I.task_create_cmd("Do the thing", nil))
end)

-- ---------------------------------------------------------------------------
-- M.create_task (stubs vim.ui.input + mod.run)
-- ---------------------------------------------------------------------------

--- Drive `create_task` with a queue of pre-scripted `vim.ui.input` answers.
--- `answers` is consumed FIFO; each call pops one and invokes on_done sync.
--- Restores `vim.ui.input` on any error via pcall.
local function with_inputs(answers, fn)
  local ui = vim.ui.input
  vim.ui.input = function(_, on_done)
    on_done(table.remove(answers, 1))
  end
  local ok, err = pcall(fn)
  vim.ui.input = ui
  if not ok then
    error(err, 0)
  end
end

--- Build a fresh module + buffer for create_task tests. Returns (mod, bufnr, calls).
local function create_task_fixture()
  local mod = t.load_tasks_module()
  local calls = {}
  mod.run = function(cmd)
    table.insert(calls, cmd)
    if cmd[3] == "list" then
      return '{"board":"default","tasks":[]}', 0
    end
    return "", 0
  end
  local bufnr = vim.api.nvim_create_buf(true, false)
  vim.api.nvim_buf_set_name(bufnr, "via://tasks-create-" .. bufnr)
  vim.bo[bufnr].buftype = "acwrite"
  vim.bo[bufnr].filetype = "via-tasks"
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, { "# Board: default", "# All tasks" })
  vim.b[bufnr].via_tasks_snapshot = {}
  vim.b[bufnr].via_tasks_board = "default"
  vim.b[bufnr].via_tasks_signature = ""
  vim.bo[bufnr].modified = false
  vim.api.nvim_win_set_buf(0, bufnr)
  return mod, bufnr, calls
end

local function create_calls(calls)
  local result = {}
  for _, c in ipairs(calls) do
    if c[3] == "create" then
      table.insert(result, c)
    end
  end
  return result
end

local function list_call_count(calls)
  local n = 0
  for _, c in ipairs(calls) do
    if c[3] == "list" then
      n = n + 1
    end
  end
  return n
end

t.it("create_task: calls via task create and refreshes on success", function()
  local mod, _, calls = create_task_fixture()
  with_inputs({ "My title", nil }, function()
    mod.create_task()
  end)
  local creates = create_calls(calls)
  t.eq(1, #creates)
  t.eq({ "via", "task", "create", "My title" }, creates[1])
  t.eq(1, list_call_count(calls), "refresh (list) should run after a successful create")
end)

t.it("create_task: aborts when title is empty", function()
  local mod, _, calls = create_task_fixture()
  with_inputs({ "   ", "ignored body" }, function()
    mod.create_task()
  end)
  t.eq(0, #create_calls(calls), "no task create should be sent for an empty title")
  t.eq(0, list_call_count(calls), "refresh should not run when no task was created")
end)

t.it("create_task: aborts when title prompt is cancelled", function()
  local mod, _, calls = create_task_fixture()
  with_inputs({ nil }, function()
    mod.create_task()
  end)
  t.eq(0, #create_calls(calls))
end)

t.it("create_task: passes body through when provided", function()
  local mod, _, calls = create_task_fixture()
  with_inputs({ "My title", "Some context" }, function()
    mod.create_task()
  end)
  local creates = create_calls(calls)
  t.eq(1, #creates)
  t.eq({ "via", "task", "create", "My title", "-m", "Some context" }, creates[1])
end)

t.it("create_task: omits -m when body is empty", function()
  local mod, _, calls = create_task_fixture()
  with_inputs({ "My title", "   " }, function()
    mod.create_task()
  end)
  local creates = create_calls(calls)
  t.eq(1, #creates)
  t.eq({ "via", "task", "create", "My title" }, creates[1])
end)

t.it("create_task: notifies and does not refresh when via task create fails", function()
  local mod, _, calls = create_task_fixture()
  local notified = {}
  local orig_notify = vim.notify
  vim.notify = function(msg, level)
    table.insert(notified, { msg = msg, level = level })
  end
  mod.run = function(cmd)
    table.insert(calls, cmd)
    if cmd[3] == "create" then
      return "boom", 1
    end
    return "", 0
  end
  local ok = pcall(function()
    with_inputs({ "My title", nil }, function()
      mod.create_task()
    end)
  end)
  vim.notify = orig_notify
  if not ok then
    error("unexpected error", 0)
  end
  t.eq(0, list_call_count(calls), "refresh must not run after a failed create")
  local saw_warn = false
  for _, n in ipairs(notified) do
    if n.level == vim.log.levels.WARN and string.find(n.msg, "failed to create task", 1, true) then
      saw_warn = true
    end
  end
  t.truthy(saw_warn, "should notify at WARN level about the failure")
end)

t.it("create_task: refuses to run when the board buffer is modified", function()
  local mod, _, calls = create_task_fixture()
  local ui_invocations = 0
  local ui = vim.ui.input
  vim.ui.input = function(_, on_done)
    ui_invocations = ui_invocations + 1
    on_done(nil)
  end
  local orig_notify = vim.notify
  vim.notify = function() end
  vim.bo[vim.api.nvim_get_current_buf()].modified = true
  local ok, err = pcall(mod.create_task)
  vim.bo[vim.api.nvim_get_current_buf()].modified = false
  vim.ui.input = ui
  vim.notify = orig_notify
  if not ok then
    error(err, 0)
  end
  t.eq(0, ui_invocations, "vim.ui.input must not fire when the buffer is modified")
  t.eq(0, #create_calls(calls), "no task create should be sent when the buffer is modified")
  t.eq(0, list_call_count(calls), "refresh should not run when the buffer is modified")
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

t.it("open_task_body: opens the task Markdown file in a regular editor buffer", function()
  local mod = t.load_tasks_module()
  local path = vim.fn.tempname() .. ".md"
  vim.fn.writefile({
    "---",
    "title: Hello",
    "status: queued",
    "created_at: 1",
    "updated_at: 1",
    "---",
    "",
    "line one",
    "line two",
  }, path)
  local calls = {}
  mod.run = function(cmd)
    table.insert(calls, cmd)
    return path .. "\n", 0
  end
  mod.open_task_body("t1")
  local bufnr = vim.fn.bufnr(path, false)
  t.eq({ "via", "task", "path", "t1" }, calls[1])
  t.neq(-1, bufnr, "task file buffer should exist")
  t.eq(path, vim.api.nvim_buf_get_name(bufnr))
  t.eq("", vim.bo[bufnr].buftype, "task file should use a regular buffer")
  t.eq(true, vim.bo[bufnr].buflisted, "task file should be listed like a regular buffer")
  t.eq("markdown", vim.bo[bufnr].filetype, "task file should use Markdown filetype")
  t.eq({
    "---",
    "title: Hello",
    "status: queued",
    "created_at: 1",
    "updated_at: 1",
    "---",
    "",
    "line one",
    "line two",
  }, vim.api.nvim_buf_get_lines(bufnr, 0, -1, false))
  pcall(vim.api.nvim_buf_delete, bufnr, { force = true })
  vim.fn.delete(path)
end)

t.it("open_task_body: replaces the task board window instead of splitting", function()
  local mod = t.load_tasks_module()
  local board = vim.api.nvim_create_buf(false, false)
  vim.api.nvim_buf_set_name(board, "via://tasks-replace-test")
  vim.bo[board].buftype = "acwrite"
  vim.bo[board].filetype = "via-tasks"
  vim.api.nvim_win_set_buf(0, board)
  local wins_before = #vim.api.nvim_list_wins()
  local path = vim.fn.tempname() .. ".md"
  vim.fn.writefile({ "# Task body" }, path)
  mod.run = function()
    return path .. "\n", 0
  end
  mod.open_task_body("replace1")
  local bufnr = vim.fn.bufnr(path, false)
  t.eq(wins_before, #vim.api.nvim_list_wins(), "must not open a new split")
  t.eq(bufnr, vim.api.nvim_get_current_buf(), "current window should show the task file")
  t.eq(path, vim.api.nvim_buf_get_name(0))
  pcall(vim.api.nvim_buf_delete, bufnr, { force = true })
  pcall(vim.api.nvim_buf_delete, board, { force = true })
  vim.fn.delete(path)
end)

t.it("open_task_body: regular Markdown buffers remain editable and listed", function()
  local mod = t.load_tasks_module()
  local path = vim.fn.tempname() .. ".md"
  vim.fn.writefile({ "# Task", "hello" }, path)
  mod.run = function()
    return path .. "\n", 0
  end
  mod.open_task_body("body1")
  local bufnr = vim.fn.bufnr(path, false)
  t.eq(true, vim.bo[bufnr].modifiable, "task file should be editable")
  t.eq(false, vim.bo[bufnr].readonly, "task file should not be read-only")
  pcall(vim.api.nvim_buf_delete, bufnr, { force = true })
  vim.fn.delete(path)
end)

-- ---------------------------------------------------------------------------
-- autorefresh (poll_autorefresh / silent_refresh)
-- ---------------------------------------------------------------------------

--- Force-delete any buffer still holding the via://tasks name so each test
--- starts clean. Idempotent and safe when no such buffer exists.
local function tasks_buf_cleanup()
  local existing = vim.fn.bufnr(I.TASKS_BUF, false)
  if existing > 0 and vim.api.nvim_buf_is_valid(existing) then
    pcall(vim.api.nvim_buf_delete, existing, { force = true })
  end
end

--- Create a via-tasks buffer bound to the current window and seed it with the
--- snapshot/signature. Returns `(mod, bufnr)`. `opts.output` becomes the
--- `via task list --json` stdout returned by the stubbed `run_async`;
--- `opts.signature` is the previously-seen signature (defaults to the same
--- output, so the poll sees "no change" unless overridden).
local function autorefresh_fixture(opts)
  opts = opts or {}
  tasks_buf_cleanup()
  local mod = t.load_tasks_module()
  -- Avoid starting a real libuv timer in tests (FileType autocmd calls
  -- setup_tasks_buffer -> start_autorefresh); the poll under test is driven
  -- manually via mod.poll_autorefresh().
  mod.start_autorefresh = function() end
  local output = opts.output or '{"board":"default","tasks":[]}'
  mod.run_async = function(cmd, on_done)
    on_done(output, 0)
  end

  local bufnr = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_name(bufnr, I.TASKS_BUF)
  -- Use a normal-like buftype so `modified` actually tracks the way the real
  -- :ViaTasks buffer does (scratch buffers reset `modified` automatically).
  vim.bo[bufnr].buftype = ""
  vim.bo[bufnr].swapfile = false
  vim.bo[bufnr].filetype = "via-tasks"
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, opts.lines or {
    "# Board: default",
    "# All tasks",
    "via:t1  queued  agent  Old",
  })
  vim.b[bufnr].via_tasks_snapshot = opts.snapshot or {
    t1 = { status = "queued", assignee = "agent", title = "Old" },
  }
  vim.b[bufnr].via_tasks_board = "default"
  vim.b[bufnr].via_tasks_signature = opts.signature or output
  vim.bo[bufnr].modified = false

  -- Display the buffer in the current window so tasks_buffer_visible() finds it.
  vim.api.nvim_win_set_buf(0, bufnr)
  return mod, bufnr
end

t.it("autorefresh_interval_ms: defaults to 2000 and honors vim.g override", function()
  local mod = t.load_tasks_module()
  local saved = vim.g.via_tasks_autorefresh_ms
  vim.g.via_tasks_autorefresh_ms = nil
  t.eq(2000, mod._internal.autorefresh_interval_ms())
  vim.g.via_tasks_autorefresh_ms = 1500
  t.eq(1500, mod._internal.autorefresh_interval_ms())
  vim.g.via_tasks_autorefresh_ms = 0
  t.eq(0, mod._internal.autorefresh_interval_ms(), "0 should disable")
  vim.g.via_tasks_autorefresh_ms = "garbage"
  t.eq(2000, mod._internal.autorefresh_interval_ms(), "non-number falls back to default")
  vim.g.via_tasks_autorefresh_ms = saved
  tasks_buf_cleanup()
end)

t.it("poll_autorefresh: no-op when no via-tasks buffer is visible", function()
  tasks_buf_cleanup()
  local mod = t.load_tasks_module()
  local calls = 0
  mod.run_async = function(cmd, on_done)
    calls = calls + 1
    on_done("{}", 0)
  end
  mod.poll_autorefresh()
  t.eq(0, calls, "run_async must not fire when no visible via-tasks buffer")
end)

t.it("poll_autorefresh: silently refreshes when the signature changes", function()
  local old_output = '{"board":"default","tasks":[{"id":"t1","status":"queued","assignee":"agent","title":"Old"}]}'
  local new_output =
    '{"board":"default","tasks":[{"id":"t1","status":"queued","assignee":"agent","title":"Old"},{"id":"t2","status":"queued","assignee":"agent","title":"New"}]}'
  local mod, bufnr = autorefresh_fixture({ output = new_output, signature = old_output })
  -- Place cursor on the task row; silent refresh should keep the row stable.
  vim.api.nvim_win_set_cursor(0, { 3, 0 })

  mod.poll_autorefresh()

  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  local joined = table.concat(lines, "\n")
  t.contains(joined, "via:t2", "new task should appear after refresh")
  t.eq(new_output, vim.b[bufnr].via_tasks_signature, "signature should be updated")
  t.eq(false, vim.bo[bufnr].modified, "refresh should leave the buffer unmodified")
  t.eq(3, vim.api.nvim_win_get_cursor(0)[1], "cursor row should be preserved")
  tasks_buf_cleanup()
end)

t.it("poll_autorefresh: does nothing when the signature is unchanged", function()
  local output = '{"board":"default","tasks":[{"id":"t1","status":"queued","assignee":"agent","title":"Old"}]}'
  local mod, bufnr = autorefresh_fixture({ output = output, signature = output })
  local async_calls = 0
  mod.run_async = function(cmd, on_done)
    async_calls = async_calls + 1
    on_done(output, 0)
  end

  mod.poll_autorefresh()

  t.eq(1, async_calls, "fetch should run once")
  local joined = table.concat(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), "\n")
  t.truthy(not joined:find("via:t2", 1, true), "buffer content should not have changed")
  t.eq(output, vim.b[bufnr].via_tasks_signature, "signature unchanged")
  tasks_buf_cleanup()
end)

t.it("poll_autorefresh: skips when the buffer is modified", function()
  local new_output = '{"board":"default","tasks":[{"id":"t2","status":"queued","assignee":"agent","title":"New"}]}'
  local mod, bufnr = autorefresh_fixture({ output = new_output, signature = "OLD" })
  mod.run_async = function(_, _)
    -- Should never be called: the poll short-circuits on `modified` first.
    error("run_async must not fire when buffer is modified")
  end
  vim.bo[bufnr].modified = true

  mod.poll_autorefresh()

  local joined = table.concat(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), "\n")
  t.truthy(not joined:find("via:t2", 1, true), "buffer content should not have changed")
  t.eq(true, vim.bo[bufnr].modified, "buffer should remain modified")
  tasks_buf_cleanup()
end)

t.it("poll_autorefresh: skips while a save is in flight", function()
  local new_output = '{"board":"default","tasks":[{"id":"t2","status":"queued","assignee":"agent","title":"New"}]}'
  local mod, bufnr = autorefresh_fixture({ output = new_output, signature = "OLD" })
  mod.run_async = function(_, _)
    error("run_async must not fire during a save")
  end
  mod._saving = true

  mod.poll_autorefresh()

  local joined = table.concat(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), "\n")
  t.truthy(not joined:find("via:t2", 1, true), "buffer content should not have changed")
  tasks_buf_cleanup()
end)

t.it("poll_autorefresh: does not redraw if buffer was modified during async fetch", function()
  local new_output = '{"board":"default","tasks":[{"id":"t2","status":"queued","assignee":"agent","title":"New"}]}'
  local mod, bufnr = autorefresh_fixture({ output = new_output, signature = "OLD" })
  mod.run_async = function(_, on_done)
    -- Simulate the user starting to type while the fetch is in flight.
    vim.bo[bufnr].modified = true
    on_done(new_output, 0)
  end

  mod.poll_autorefresh()

  local joined = table.concat(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), "\n")
  t.truthy(not joined:find("via:t2", 1, true), "buffer content should not have changed")
  t.eq(true, vim.bo[bufnr].modified, "user's unsaved edit must survive")
  tasks_buf_cleanup()
end)

t.it("poll_autorefresh: ignores unparseable stdout without modifying the buffer", function()
  local mod, bufnr = autorefresh_fixture({ output = "not-json", signature = "OLD" })

  mod.poll_autorefresh()

  local joined = table.concat(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), "\n")
  t.truthy(not joined:find("via:t2", 1, true), "buffer content should not have changed")
  tasks_buf_cleanup()
end)

t.it("silent_refresh: rebuilds buffer from data preserving cursor and clearing modified", function()
  local mod, bufnr = autorefresh_fixture({
    output = '{"board":"default","tasks":[{"id":"t1","status":"queued","assignee":"agent","title":"Old"}]}',
    signature = "OLD",
  })
  vim.api.nvim_win_set_cursor(0, { 3, 5 })

  mod.silent_refresh(bufnr, {
    board = "default",
    tasks = {
      { id = "t1", status = "queued", assignee = "agent", title = "Old" },
      { id = "t2", status = "done", assignee = "human", title = "Done item" },
    },
  })

  local joined = table.concat(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), "\n")
  t.contains(joined, "via:t2")
  t.contains(joined, "via:t1")
  t.eq(false, vim.bo[bufnr].modified, "silent refresh should clear modified")
  t.eq(3, vim.api.nvim_win_get_cursor(0)[1], "cursor row should be preserved")
  tasks_buf_cleanup()
end)

t.it("silent_refresh: no-op when the buffer is modified (do not clobber edits)", function()
  local mod, bufnr = autorefresh_fixture({
    output = '{"board":"default","tasks":[{"id":"t1","status":"queued","assignee":"agent","title":"Old"}]}',
    signature = "OLD",
  })
  vim.bo[bufnr].modified = true
  local before = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)

  mod.silent_refresh(bufnr, {
    board = "default",
    tasks = { { id = "t2", status = "queued", assignee = "agent", title = "New" } },
  })

  t.eq(before, vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), "modified buffer must be left untouched")
  t.eq(true, vim.bo[bufnr].modified)
  tasks_buf_cleanup()
end)

-- ---------------------------------------------------------------------------
-- M.open: transient board buffer options (buflisted=false, bufhidden=hide)
-- ---------------------------------------------------------------------------

--- Open the board via `M.open()` with `mod.run` stubbed. Returns `(mod, bufnr)`.
--- `list_output` becomes the `via task list --json` stdout; defaults to empty.
local function open_board_fixture(list_output)
  tasks_buf_cleanup()
  local mod = t.load_tasks_module()
  mod.start_autorefresh = function() end
  mod.run = function(cmd)
    if cmd[3] == "list" then
      return list_output or '{"board":"default","tasks":[]}', 0
    end
    return "", 0
  end
  mod.open()
  return mod, vim.fn.bufnr(I.TASKS_BUF, false)
end

--- Make `bufnr` the current buffer by focusing a window that shows it.
local function focus_buf(bufnr)
  for _, win in ipairs(vim.api.nvim_list_wins()) do
    if vim.api.nvim_win_is_valid(win) and vim.api.nvim_win_get_buf(win) == bufnr then
      vim.api.nvim_set_current_win(win)
      return
    end
  end
end

t.it("open: board buffer is unlisted (buflisted=false, excluded from :ls)", function()
  local _, bufnr = open_board_fixture()
  t.truthy(bufnr > 0, "board buffer should exist")
  t.eq(false, vim.bo[bufnr].buflisted, "board buffer must be unlisted")
  local ls = vim.api.nvim_exec2("ls", { output = true }).output
  t.truthy(not ls:find("via://tasks", 1, true), "board buffer must not appear in :ls")
  tasks_buf_cleanup()
end)

t.it("open: board buffer has bufhidden=hide (long-lived, reused across opens)", function()
  local _, bufnr = open_board_fixture()
  t.eq("hide", vim.bo[bufnr].bufhidden, "board buffer must hide, not wipe, on window close")
  tasks_buf_cleanup()
end)

t.it("open: unlisted board buffer is still resolvable by name via bufnr(name, false)", function()
  local _, bufnr = open_board_fixture()
  t.eq(bufnr, vim.fn.bufnr(I.TASKS_BUF, false), "bufnr(name, false) must find the unlisted board buffer")
  tasks_buf_cleanup()
end)

t.it("open: board buffer keeps buftype=acwrite and is unlisted (skipped by :wa, :bn/:bp)", function()
  local _, bufnr = open_board_fixture()
  t.eq("acwrite", vim.bo[bufnr].buftype, "board buffer must stay acwrite")
  t.eq(false, vim.bo[bufnr].buflisted, "unlisted buffers are skipped by :wa and excluded from :bn/:bp")
  tasks_buf_cleanup()
end)

t.it("refresh: still updates the unlisted board buffer (bufnr lookup intact)", function()
  local mod, bufnr = open_board_fixture(
    '{"board":"default","tasks":[{"id":"t1","status":"queued","assignee":"agent","title":"Old"}]}'
  )
  t.eq(false, vim.bo[bufnr].buflisted)
  focus_buf(bufnr)
  mod.run = function(cmd)
    if cmd[3] == "list" then
      return '{"board":"default","tasks":[{"id":"t1","status":"queued","assignee":"agent","title":"Old"},{"id":"t2","status":"done","assignee":"human","title":"New"}]}', 0
    end
    return "", 0
  end
  mod.refresh()
  local joined = table.concat(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), "\n")
  t.contains(joined, "via:t2", "refresh should still update the unlisted board buffer")
  t.eq(false, vim.bo[bufnr].buflisted, "board buffer must remain unlisted after refresh")
  t.eq(bufnr, vim.fn.bufnr(I.TASKS_BUF, false), "board buffer must remain resolvable after refresh")
  tasks_buf_cleanup()
end)
