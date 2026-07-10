-- tasks.lua
-- Project Management UI for via task boards.
-- Load with: require('via.tasks')
--
-- :ViaTasks (or <leader>at) toggles a structured buffer listing tasks on the active board.
-- One task per line: via:<id>  STATUS  ASSIGNEE  TITLE. Plain text, greppable.
-- "Your queue" section filters on assignee=human AND status=review|in_progress.
-- :w diffs against the loaded snapshot and calls `via task update` for changed
-- rows only (field-scoped, not whole-row). Action keys: ga/gr/gd/gR/o.

local M = {}

local augroup = vim.api.nvim_create_augroup("ViaTasks", { clear = false })

-- Namespace for validation diagnostics on the task board buffer.
local diagnostics_ns = vim.api.nvim_create_namespace("via-tasks")

--- Command-execution seam. Runs `cmd` (a list) and returns `(stdout, exit_code)`.
--- Overridable in tests so the Lua logic can be exercised without the real
--- `via` binary or a live session.
function M.run(cmd)
  local output = vim.fn.system(cmd)
  return output, vim.v.shell_error
end

-- Column widths for the task table.
local TASK_ID_PREFIX = "via:"
local TASK_ID_MATCH = [[\vvia:[\w._-]+]]
local COL = {
  ID = 20,
  STATUS = 12,
  ASSIGNEE = 12,
}

-- Header prefix for "Your queue" section.
local YOUR_QUEUE_HEADER = "# Your queue (assignee=human, status=review|in_progress)"
local ALL_TASKS_HEADER = "# All tasks"
local SEPARATOR = "#"

--- Run `via task list --json` and return the parsed tasks table.
--- Returns { board = "...", tasks = { ... } } or nil on failure.
function M.load_tasks()
  local output, code = M.run({ "via", "task", "list", "--json" })
  if code ~= 0 then
    vim.notify("via: failed to run `via task list` (exit " .. code .. ")", vim.log.levels.ERROR)
    return nil
  end
  local ok, data = pcall(vim.json.decode, output)
  if not ok or type(data) ~= "table" then
    vim.notify("via: failed to parse `via task list` output", vim.log.levels.ERROR)
    return nil
  end
  return data
end

--- Pad a string to `width` columns (truncate if longer, pad with spaces if shorter).
local function pad(s, width)
  s = tostring(s)
  if #s > width then
    return s:sub(1, width)
  end
  return s .. string.rep(" ", width - #s)
end

local function format_task_id(raw_id)
  return pad(TASK_ID_PREFIX .. (raw_id or "?"), COL.ID)
end

--- Strip the display `via:` prefix; bare ids are accepted for hand-edited rows.
local function parse_task_id(token)
  if token:sub(1, #TASK_ID_PREFIX) == TASK_ID_PREFIX then
    return token:sub(#TASK_ID_PREFIX + 1)
  end
  return token
end

--- Task id under the cursor on a board row, or nil when `col` is outside the id.
local function task_id_on_line(line, col)
  local indent, rest = line:match("^(%s*)(.*)$")
  indent = indent or ""
  local display = rest:match("^(via:[%w._-]+)")
  if not display then
    display = rest:match("^([%w._-]+)")
    if not display then
      return nil
    end
  end
  if col ~= nil then
    local id_start = #indent
    local id_end = id_start + #display - 1
    if col < id_start or col > id_end then
      return nil
    end
  end
  return parse_task_id(display)
end

--- Format a single task as a table row.
--- `indent` is prepended for "Your queue" section rows.
local function format_row(task, indent)
  indent = indent or ""
  local id = format_task_id(task.id)
  local status = pad(task.status or "-", COL.STATUS)
  local assignee = pad(task.assignee or "-", COL.ASSIGNEE)
  local title = task.title or ""
  return indent .. id .. "  " .. status .. "  " .. assignee .. "  " .. title
end

-- Error message shown for a task row that is missing columns / a title.
local ROW_FORMAT_HINT = "task rows must be: via:<id>  STATUS  ASSIGNEE  TITLE"

--- Parse a table row back into { id, status, assignee, title }.
--- Returns `(parsed, nil)` for a valid task row, `(nil, nil)` for a comment or
--- blank line (not a task row), or `(nil, err)` for a malformed task row (e.g.
--- missing the title column). Titles are required — the store rejects empty
--- titles, so the buffer treats them as invalid rather than silently dropping
--- the row's other edits.
local function parse_row(line)
  line = line:gsub("^%s*", "")
  if line == "" or line:sub(1, 1) == "#" then
    return nil, nil
  end
  -- Split on whitespace to find column tokens.
  -- The format is: ID  STATUS  ASSIGNEE  TITLE
  local parts = {}
  for part in line:gmatch("(%S+)") do
    table.insert(parts, part)
  end
  if #parts < 3 then
    return nil, "incomplete task row (" .. ROW_FORMAT_HINT .. ")"
  end
  if #parts < 4 then
    return nil, "missing task title (" .. ROW_FORMAT_HINT .. ")"
  end
  -- ID, STATUS, ASSIGNEE are the first three; TITLE is the rest joined.
  local id = parse_task_id(parts[1])
  local status = parts[2]
  local assignee = parts[3]
  -- A literal "-" in the assignee column means "no assignee". (Do NOT use the
  -- `cond and nil or x` idiom here — `and nil` is falsy, so it would fall
  -- through to `x` and never clear the assignee.)
  if assignee == "-" then
    assignee = nil
  end
  local title = table.concat(parts, " ", 4)
  return {
    id = id,
    status = status,
    assignee = assignee,
    title = title,
  }, nil
end

--- Build the buffer content from a tasks data table.
--- Returns { lines = {...}, snapshot = { [id] = {status, assignee, title, dirty=false} } }.
--- The snapshot is the pre-edit state used to diff on `:w`; it is keyed by id,
--- so a task shown in both "Your queue" and "All tasks" has one entry.
local function build_content(data)
  local lines = {}
  local snapshot = {}

  local tasks = data.tasks or {}

  -- "Your queue" section: assignee=human, status=review or in_progress.
  local your_queue = {}
  local all_tasks = {}
  for _, task in ipairs(tasks) do
    if task.assignee == "human" and (task.status == "review" or task.status == "in_progress") then
      table.insert(your_queue, task)
    end
    table.insert(all_tasks, task)
  end

  if #your_queue > 0 then
    table.insert(lines, YOUR_QUEUE_HEADER)
    for _, task in ipairs(your_queue) do
      table.insert(lines, format_row(task, "  "))
      snapshot[task.id] = {
        status = task.status,
        assignee = task.assignee,
        title = task.title,
        dirty = false,
      }
    end
    table.insert(lines, "")
  end

  table.insert(lines, ALL_TASKS_HEADER)
  for _, task in ipairs(all_tasks) do
    table.insert(lines, format_row(task))
    snapshot[task.id] = {
      status = task.status,
      assignee = task.assignee,
      title = task.title,
      dirty = false,
    }
  end

  return { lines = lines, snapshot = snapshot }
end

--- Height for the task board split: 30% of the current editor height, capped at
--- 10 lines and never less than 3 lines.
local function split_height()
  return math.max(3, math.min(10, math.floor(vim.o.lines * 0.30)))
end

local TASKS_BUF = "via://tasks"

local function show_tasks_split()
  vim.cmd("botright " .. split_height() .. "split " .. vim.fn.fnameescape(TASKS_BUF))
end

vim.api.nvim_set_hl(0, "ViaTaskId", { link = "Special", default = true })

local function apply_task_id_highlight(winid)
  if type(winid) ~= "number" or winid <= 0 or not vim.api.nvim_win_is_valid(winid) then
    return
  end
  if vim.bo[vim.api.nvim_win_get_buf(winid)].filetype ~= "via-tasks" then
    return
  end
  if vim.w[winid].via_tasks_id_match then
    pcall(vim.fn.matchdelete, vim.w[winid].via_tasks_id_match)
  end
  vim.w[winid].via_tasks_id_match = vim.fn.matchadd("ViaTaskId", TASK_ID_MATCH, 10)
end

vim.api.nvim_create_autocmd("BufWinEnter", {
  group = augroup,
  callback = function(ev)
    if vim.bo[ev.buf].filetype ~= "via-tasks" then
      return
    end
    local winid = vim.fn.bufwinid(ev.buf)
    if winid < 0 then
      return
    end
    apply_task_id_highlight(winid)
  end,
})

local function setup_tasks_buffer(bufnr)
  local opts = { buffer = bufnr, silent = true, noremap = true }
  vim.keymap.set("n", "gR", M.refresh, opts)
  vim.keymap.set("n", "<CR>", function()
    M.open_task_body()
  end, opts)
  vim.keymap.set("n", "<C-LeftMouse>", function()
    M.open_task_at_mouse()
  end, vim.tbl_extend("force", opts, { desc = "Open task id under cursor" }))
end

--- Open the :ViaTasks buffer in a horizontal split.
function M.open()
  local data = M.load_tasks()
  if not data then
    return
  end

  -- Create or reuse the via-tasks buffer.
  local bufnr = vim.fn.bufnr(TASKS_BUF, false)
  if bufnr <= 0 then
    bufnr = vim.api.nvim_create_buf(false, true)
    vim.api.nvim_buf_set_name(bufnr, TASKS_BUF)
  end

  vim.bo[bufnr].filetype = "via-tasks"
  vim.bo[bufnr].buftype = "acwrite"
  vim.bo[bufnr].swapfile = false
  vim.bo[bufnr].modifiable = true

  local content = build_content(data)
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, content.lines)

  -- Store the snapshot as a buffer-local variable for diffing on :w.
  vim.b[bufnr].via_tasks_snapshot = content.snapshot
  vim.b[bufnr].via_tasks_board = data.board

  vim.bo[bufnr].modifiable = false

  -- Open in a bounded horizontal split.
  show_tasks_split()
  vim.bo[bufnr].modifiable = true

  setup_tasks_buffer(bufnr)

  -- BufWriteCmd: parse, diff, update.
  vim.api.nvim_clear_autocmds({ group = augroup, buffer = bufnr, event = "BufWriteCmd" })
  vim.api.nvim_create_autocmd("BufWriteCmd", {
    group = augroup,
    buffer = bufnr,
    callback = function()
      M.save()
    end,
  })
end

--- Toggle the :ViaTasks panel (open, focus, or close).
function M.toggle()
  local bufnr = vim.fn.bufnr(TASKS_BUF, false)
  if bufnr > 0 then
    local closed = false
    for _, win in ipairs(vim.api.nvim_list_wins()) do
      if vim.api.nvim_win_get_buf(win) == bufnr then
        vim.api.nvim_win_close(win, true)
        closed = true
      end
    end
    if closed then
      return
    end
    -- Buffer exists but is hidden: show without reloading (preserve edits).
    show_tasks_split()
    return
  end
  M.open()
end

--- Refresh the buffer from `via task list --json` (gR).
function M.refresh()
  local data = M.load_tasks()
  if not data then
    return
  end
  local bufnr = vim.api.nvim_get_current_buf()
  local content = build_content(data)
  vim.bo[bufnr].modifiable = true
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, content.lines)
  vim.b[bufnr].via_tasks_snapshot = content.snapshot
  vim.b[bufnr].via_tasks_board = data.board
  vim.bo[bufnr].modified = false
  -- Rebuilt from the store (always valid rows); clear any stale diagnostics.
  vim.diagnostic.reset(diagnostics_ns, bufnr)
  apply_task_id_highlight(vim.fn.win_getid())
  vim.notify("via: tasks refreshed (" .. #(data.tasks or {}) .. " tasks)", vim.log.levels.INFO)
end

--- Validate every task row in the buffer, publishing diagnostics on malformed
--- rows. A row is invalid when it is missing columns/a title, or (when
--- `snapshot` is given) when its ID is not a task on the board — the buffer
--- can only edit existing tasks, so an unknown ID is a typo and its edits
--- would otherwise be silently dropped. Returns `(parsed_rows, error_count)`.
local function validate_rows(bufnr, lines, snapshot)
  local diagnostics = {}
  local parsed_rows = {}
  for idx, line in ipairs(lines) do
    local parsed, err = parse_row(line)
    if err then
      table.insert(diagnostics, {
        lnum = idx - 1,
        col = 0,
        severity = vim.diagnostic.severity.ERROR,
        source = "via-tasks",
        message = err,
      })
    elseif parsed then
      if snapshot and not snapshot[parsed.id] then
        table.insert(diagnostics, {
          lnum = idx - 1,
          col = 0,
          severity = vim.diagnostic.severity.ERROR,
          source = "via-tasks",
          message = "unknown task id '" .. parsed.id .. "' (not on this board)",
        })
      else
        table.insert(parsed_rows, parsed)
      end
    end
  end
  vim.diagnostic.set(diagnostics_ns, bufnr, diagnostics)
  return parsed_rows, #diagnostics
end

--- Parse the buffer, diff against the snapshot, and call `via task update`
--- for changed rows only (:w).
---
--- Validation is all-or-nothing: if any task row is malformed (missing
--- title/columns) the save aborts before ANY `via task update`, diagnostics
--- mark the offending lines, and the buffer stays modified.
---
--- Command execution is per-row and NOT transactional: `via task update` runs
--- once per changed row, so an earlier update can persist even if a later one
--- fails. On any failed update we do NOT refresh (which would clobber the
--- user's still-unsaved edits) and leave the buffer modified so the user can
--- inspect and retry. Only a fully successful save refreshes + clears modified.
function M.save()
  local bufnr = vim.api.nvim_get_current_buf()
  local snapshot = vim.b[bufnr].via_tasks_snapshot or {}
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)

  local parsed_rows, error_count = validate_rows(bufnr, lines, snapshot)
  if error_count > 0 then
    -- Leave the buffer modified (no partial writes). Diagnostics mark the rows.
    vim.notify(
      "via: cannot save — " .. error_count .. " invalid task row(s); see diagnostics",
      vim.log.levels.WARN
    )
    return
  end

  local updates = {}
  local seen = {}
  for _, parsed in ipairs(parsed_rows) do
    if snapshot[parsed.id] and not seen[parsed.id] then
      seen[parsed.id] = true
      local snap = snapshot[parsed.id]
      local changed = {}
      if snap.status ~= parsed.status then
        table.insert(changed, "--status")
        table.insert(changed, parsed.status)
      end
      if snap.assignee ~= parsed.assignee then
        if parsed.assignee then
          table.insert(changed, "--assignee")
          table.insert(changed, parsed.assignee)
        else
          table.insert(changed, "--clear-assignee")
        end
      end
      if snap.title ~= parsed.title then
        table.insert(changed, "--title")
        table.insert(changed, parsed.title)
      end
      if #changed > 0 then
        table.insert(updates, { id = parsed.id, args = changed })
      end
    end
  end

  if #updates == 0 then
    vim.bo[bufnr].modified = false
    vim.notify("via: no changes to save", vim.log.levels.INFO)
    return
  end

  local errors = 0
  for _, update in ipairs(updates) do
    local cmd = vim.list_extend({ "via", "task", "update", update.id }, update.args)
    local output, code = M.run(cmd)
    if code ~= 0 then
      errors = errors + 1
      vim.notify("via: failed to update " .. update.id .. ": " .. output, vim.log.levels.WARN)
    end
  end

  if errors == 0 then
    vim.notify("via: saved " .. #updates .. " task update(s)", vim.log.levels.INFO)
    -- Refresh to reflect the new store state (also clears `modified`).
    M.refresh()
  else
    -- Some updates failed. Keep the buffer modified and do NOT refresh, so the
    -- user's unsaved edits survive for inspection/retry. Successful updates in
    -- this batch are already persisted (no rollback — the store is not
    -- transactional); `gR` will reconcile once the user is ready.
    vim.notify(
      "via: " .. errors .. " of " .. #updates .. " update(s) failed; buffer kept for retry",
      vim.log.levels.WARN
    )
  end
end

--- Open the task body in a split scratch buffer (<CR> / Ctrl+click on via:<id>).
--- The body is loaded via `via task show <id> --json`; :w in the scratch
--- buffer calls `via task update <id> --body "..."`.
function M.open_task_at_mouse()
  local pos = vim.fn.getmousepos()
  if not pos or pos.winid == 0 then
    return
  end
  local bufnr = vim.api.nvim_win_get_buf(pos.winid)
  local line = vim.api.nvim_buf_get_lines(bufnr, pos.line - 1, pos.line, false)[1]
  if not line then
    return
  end
  local id = task_id_on_line(line, math.max(0, pos.column - 1))
  if id then
    M.open_task_body(id)
  end
end

function M.open_task_body(task_id)
  local id = task_id
  if not id then
    local line = vim.api.nvim_get_current_line()
    id = task_id_on_line(line, vim.api.nvim_win_get_cursor(0)[2])
    if not id then
      local parsed = parse_row(line)
      if not parsed then
        return
      end
      id = parsed.id
    end
  end

  local output, code = M.run({ "via", "task", "show", id, "--json" })
  if code ~= 0 then
    vim.notify("via: failed to show task " .. id, vim.log.levels.ERROR)
    return
  end
  local ok, task = pcall(vim.json.decode, output)
  if not ok or not task then
    return
  end

  local buf_name = "via://task/" .. id
  local bufnr = vim.fn.bufnr(buf_name, false)
  if bufnr <= 0 then
    bufnr = vim.api.nvim_create_buf(false, true)
    vim.api.nvim_buf_set_name(bufnr, buf_name)
    vim.bo[bufnr].filetype = "via-task-body"
    vim.bo[bufnr].buftype = "acwrite"
    vim.bo[bufnr].swapfile = false
    vim.b[bufnr].via_task_id = id

    vim.api.nvim_create_autocmd("BufWriteCmd", {
      buffer = bufnr,
      callback = function()
        local id = vim.b[bufnr].via_task_id
        local body_text = table.concat(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), "\n")
        local result, code = M.run({ "via", "task", "update", id, "--body", body_text })
        if code ~= 0 then
          vim.notify("via: failed to save body: " .. result, vim.log.levels.WARN)
        else
          vim.bo[bufnr].modified = false
          vim.notify("via: saved body for " .. id, vim.log.levels.INFO)
        end
      end,
    })
  end

  -- Split on newlines without the trailing-empty-line artifact that
  -- `gmatch("[^\n]*")` produces (which would make an untouched body round-trip
  -- to a body with an extra trailing newline on save).
  local body_lines = vim.split(task.body or "", "\n", { plain = true })
  vim.bo[bufnr].modifiable = true
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, body_lines)
  vim.bo[bufnr].modified = false

  vim.cmd("split " .. vim.fn.fnameescape(buf_name))
end

-- Register the :ViaTasks user command and toggle mapping.
vim.api.nvim_create_user_command("ViaTasks", function()
  M.toggle()
end, {
  desc = "Toggle the via task board (Project Management UI)",
})

vim.keymap.set("n", "<leader>at", M.toggle, { desc = "Toggle via task board" })

-- Internal helpers exposed for the test suite (see nvim/tests/). Not part of the
-- public API — do not use these from user config.
M._internal = {
  pad = pad,
  format_task_id = format_task_id,
  parse_task_id = parse_task_id,
  task_id_on_line = task_id_on_line,
  format_row = format_row,
  parse_row = parse_row,
  build_content = build_content,
  validate_rows = validate_rows,
  split_height = split_height,
  diagnostics_ns = diagnostics_ns,
  TASK_ID_PREFIX = TASK_ID_PREFIX,
}

return M
