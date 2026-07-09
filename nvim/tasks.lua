-- tasks.lua
-- Project Management UI for via task boards.
-- Load with: require('via.tasks')
--
-- :ViaTasks (or <leader>at) toggles a structured buffer listing tasks on the active board.
-- One task per line: via:<id>  STATUS  ASSIGNEE  TITLE. Plain text, greppable.
-- "Your queue" section filters on assignee=human AND status=review|in_progress.
-- :w diffs against the loaded snapshot and calls `via task update` for changed
-- rows only (field-scoped, not whole-row). Buffer-local keys: gR refresh,
-- <CR> open body, gn new board, gb switch board.
-- A background autorefresh poll watches `via task list --json` and silently
-- redraws the board when the store changes and the buffer has no unsaved
-- edits. Set `vim.g.via_tasks_autorefresh_ms` (default 2000; 0 disables).
-- Prefer non-leader keys here so LazyVim/which-key global leader maps never
-- race the board UI (leader prefixes open which-key before buffer maps run).

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

--- Async command-execution seam for the autorefresh poll. Invokes
--- `on_done(stdout, exit_code)` exactly once, on the main loop. The default
--- impl uses `vim.fn.jobstart` so a slow `via task list` does not block the
--- editor; tests stub this with a synchronous call to drive the poll
--- deterministically without spawning a subprocess.
function M.run_async(cmd, on_done)
  if not vim.fn.jobstart then
    local output, code = M.run(cmd)
    vim.schedule(function()
      on_done(output, code)
    end)
    return
  end
  local stdout = {}
  local job = vim.fn.jobstart(cmd, {
    stdout_buffered = true,
    on_stdout = function(_, data)
      if type(data) == "table" then
        for _, line in ipairs(data) do
          table.insert(stdout, line)
        end
      end
    end,
    on_exit = function(_, code)
      -- stdout entries may carry trailing empty strings from the buffered
      -- job; join with "" (not "\n") because the original CLI output had no
      -- separator between chunks.
      local out = table.concat(stdout, "")
      vim.schedule(function()
        on_done(out, code)
      end)
    end,
  })
  if job <= 0 then
    vim.schedule(function()
      on_done("", 1)
    end)
  end
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

--- Build the top-of-buffer board identity line.
--- Example: `# Board: default  (others: phase2, sprint-a)`
local function board_header(data)
  local board = data.board or "?"
  local title = data.board_title
  local label = board
  if type(title) == "string" and title ~= "" then
    label = board .. " — " .. title
  end
  local others = {}
  for _, entry in ipairs(data.boards or {}) do
    local id = entry
    if type(entry) == "table" then
      id = entry.id
    end
    if type(id) == "string" and id ~= "" and id ~= board then
      table.insert(others, id)
    end
  end
  if #others > 0 then
    return string.format("# Board: %s  (others: %s)", label, table.concat(others, ", "))
  end
  return string.format("# Board: %s", label)
end

--- Run `via task list --json` and return the parsed tasks table plus the raw
--- stdout (used as a cheap change-signature by the autorefresh poll).
--- Returns `(data, raw_output)` or `(nil, nil)` on failure.
function M.load_tasks()
  local output, code = M.run({ "via", "task", "list", "--json" })
  if code ~= 0 then
    vim.notify("via: failed to run `via task list` (exit " .. code .. ")", vim.log.levels.ERROR)
    return nil, nil
  end
  local ok, data = pcall(vim.json.decode, output)
  if not ok or type(data) ~= "table" then
    vim.notify("via: failed to parse `via task list` output", vim.log.levels.ERROR)
    return nil, nil
  end
  return data, output
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

  table.insert(lines, board_header(data))

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

--- Display the board buffer in a bottom-right horizontal split of bounded
--- height. Uses `nvim_win_set_buf` rather than `:split {name}` so the buffer's
--- `buflisted=false` invariant is preserved (`:split {name}` would re-list it).
local function show_tasks_split(bufnr)
  vim.cmd("botright " .. split_height() .. "split")
  vim.api.nvim_win_set_buf(0, bufnr)
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

--- Configure a via-tasks board buffer: pin it as transient (unlisted, hidden
--- on window close) and bind the board keymaps. Idempotent — safe to re-run on
--- every display path (open / refresh / toggle / BufWinEnter / FileType).
local function setup_tasks_buffer(bufnr)
  if type(bufnr) ~= "number" or bufnr <= 0 or not vim.api.nvim_buf_is_valid(bufnr) then
    return
  end

  vim.bo[bufnr].buflisted = false
  vim.bo[bufnr].bufhidden = "hide"

  -- Ensure filetype stays set: some paths (e.g. refresh) can leave it blank,
  -- which drops FileType-based setup.
  if vim.bo[bufnr].filetype ~= "via-tasks" then
    vim.bo[bufnr].filetype = "via-tasks"
  end

  local opts = { buffer = bufnr, silent = true, noremap = true, nowait = true }
  vim.keymap.set("n", "gR", M.refresh, vim.tbl_extend("force", opts, { desc = "Refresh via task board" }))
  vim.keymap.set("n", "<CR>", function()
    M.open_task_body()
  end, opts)
  vim.keymap.set("n", "<C-LeftMouse>", function()
    M.open_task_at_mouse()
  end, vim.tbl_extend("force", opts, { desc = "Open task id under cursor" }))
  -- Non-leader keys avoid which-key's <Space> proxy (which raced <leader>s).
  vim.keymap.set(
    "n",
    "gn",
    M.new_board,
    vim.tbl_extend("force", opts, { desc = "Create and switch to a new via task board" })
  )
  vim.keymap.set(
    "n",
    "gb",
    M.switch_board,
    vim.tbl_extend("force", opts, { desc = "Switch via task board" })
  )
  vim.keymap.set(
    "n",
    "gc",
    M.create_task,
    vim.tbl_extend("force", opts, { desc = "Create a new via task" })
  )

  -- Kick the background autorefresh poll on (idempotent). The poll itself
  -- short-circuits when the buffer isn't visible, so starting the timer here
  -- is cheap and guarantees it's running whenever a board is on screen.
  M.start_autorefresh()
end

-- Re-apply buffer-local maps whenever a via-tasks buffer is shown. open() alone
-- is not enough: refresh/toggle paths and LazyVim/which-key can leave the
-- buffer without our maps while global <leader>n / <leader>s* still apply.
vim.api.nvim_create_autocmd({ "FileType", "BufWinEnter" }, {
  group = augroup,
  callback = function(ev)
    local bufnr = ev.buf
    if vim.bo[bufnr].filetype == "via-tasks" or vim.api.nvim_buf_get_name(bufnr) == TASKS_BUF then
      setup_tasks_buffer(bufnr)
    end
  end,
})

--- Open the :ViaTasks buffer in a horizontal split.
function M.open()
  local data, raw_output = M.load_tasks()
  if not data then
    return
  end

  local bufnr = vim.fn.bufnr(TASKS_BUF, false)
  if bufnr <= 0 then
    bufnr = vim.api.nvim_create_buf(false, true)
    vim.api.nvim_buf_set_name(bufnr, TASKS_BUF)
  end

  -- buftype before filetype so FileType autocmds see the final buffer options.
  vim.bo[bufnr].buftype = "acwrite"
  vim.bo[bufnr].swapfile = false
  vim.bo[bufnr].modifiable = true
  vim.bo[bufnr].filetype = "via-tasks"

  local content = build_content(data)
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, content.lines)

  -- Store the snapshot as a buffer-local variable for diffing on :w.
  vim.b[bufnr].via_tasks_snapshot = content.snapshot
  vim.b[bufnr].via_tasks_board = data.board
  -- Signature of the loaded board state; the autorefresh poll compares this
  -- against fresh `via task list --json` output to decide whether to redraw.
  vim.b[bufnr].via_tasks_signature = raw_output or ""

  vim.bo[bufnr].modifiable = false

  -- Open in a bounded horizontal split.
  show_tasks_split(bufnr)
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
    -- Buffer exists but is hidden: show it. Refresh when unmodified so a
    -- reopened board picks up tasks created since it was last loaded.
    show_tasks_split(bufnr)
    setup_tasks_buffer(bufnr)
    if not vim.bo[bufnr].modified then
      M.refresh()
    end
    return
  end
  M.open()
end

--- Refresh the buffer from `via task list --json` (gR).
function M.refresh()
  local data, raw_output = M.load_tasks()
  if not data then
    return
  end
  local bufnr = vim.api.nvim_get_current_buf()
  local content = build_content(data)
  vim.bo[bufnr].modifiable = true
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, content.lines)
  vim.b[bufnr].via_tasks_snapshot = content.snapshot
  vim.b[bufnr].via_tasks_board = data.board
  vim.b[bufnr].via_tasks_signature = raw_output or ""
  vim.bo[bufnr].modified = false
  -- Rebuilt from the store (always valid rows); clear any stale diagnostics.
  vim.diagnostic.reset(diagnostics_ns, bufnr)
  setup_tasks_buffer(bufnr)
  apply_task_id_highlight(vim.fn.win_getid())
  vim.notify("via: tasks refreshed (" .. #(data.tasks or {}) .. " tasks)", vim.log.levels.INFO)
end

--- Build `via task board new` argv for `id` / optional `title`.
local function board_new_cmd(id, title)
  local cmd = { "via", "task", "board", "new", "--id", id }
  if type(title) == "string" then
    local trimmed = title:match("^%s*(.-)%s*$")
    if trimmed ~= "" then
      table.insert(cmd, "--title")
      table.insert(cmd, trimmed)
    end
  end
  return cmd
end

--- Build `via task create` argv for a (required) title and optional body.
--- Whitespace-only body is treated as absent.
local function task_create_cmd(title, body)
  local cmd = { "via", "task", "create", title }
  if type(body) == "string" then
    local trimmed = body:match("^%s*(.-)%s*$")
    if trimmed ~= "" then
      table.insert(cmd, "-m")
      table.insert(cmd, trimmed)
    end
  end
  return cmd
end

--- Derive a board id from a display name: lowercase, non-alnum → `-`, collapse.
--- "Phase 2 Work" → "phase-2-work". Returns nil when nothing usable remains.
local function slugify_board_id(name)
  if type(name) ~= "string" then
    return nil
  end
  local slug = name:lower():gsub("[^a-z0-9]+", "-"):gsub("^%-+", ""):gsub("%-+$", "")
  if slug == "" then
    return nil
  end
  return slug
end

--- Prompt for a board name, derive the id, create+activate it, refresh.
--- Bound to buffer-local `gn` on the :ViaTasks buffer only.
function M.new_board()
  local bufnr = vim.api.nvim_get_current_buf()
  if vim.bo[bufnr].modified then
    vim.notify("via: save or discard board edits before creating a new board", vim.log.levels.WARN)
    return
  end

  vim.ui.input({ prompt = "New board name: " }, function(name)
    if type(name) ~= "string" then
      return
    end
    name = name:match("^%s*(.-)%s*$")
    if name == "" then
      return
    end

    local id = slugify_board_id(name)
    if not id then
      vim.notify("via: board name must contain letters or digits", vim.log.levels.WARN)
      return
    end

    local output, code = M.run(board_new_cmd(id, name))
    if code ~= 0 then
      vim.notify("via: failed to create board: " .. tostring(output), vim.log.levels.ERROR)
      return
    end
    vim.notify("via: created and activated board " .. id, vim.log.levels.INFO)
    M.refresh()
  end)
end

--- Prompt for a task title and optional context, create it via the CLI, refresh.
--- Bound to buffer-local `gc` on the :ViaTasks buffer only.
function M.create_task()
  local bufnr = vim.api.nvim_get_current_buf()
  if vim.bo[bufnr].modified then
    vim.notify("via: save or discard board edits before creating a task", vim.log.levels.WARN)
    return
  end

  vim.ui.input({ prompt = "New task title: " }, function(title)
    if type(title) ~= "string" then
      return
    end
    title = title:match("^%s*(.-)%s*$")
    if title == "" then
      return
    end

    vim.ui.input({ prompt = "Additional context (optional): " }, function(body)
      local output, code = M.run(task_create_cmd(title, body))
      if code ~= 0 then
        vim.notify("via: failed to create task: " .. tostring(output), vim.log.levels.WARN)
        return
      end
      vim.notify("via: created task", vim.log.levels.INFO)
      M.refresh()
    end)
  end)
end

--- Format epoch-millis as a UTC `YYYY-MM-DD HH:MM` stamp (or "-" when missing).
local function format_millis(ms)
  if type(ms) ~= "number" or ms <= 0 then
    return "-"
  end
  return os.date("!%Y-%m-%d %H:%M", math.floor(ms / 1000))
end

--- One-line label for the board switcher picker.
local function format_board_item(board, active_id)
  local name = board.id or "?"
  if type(board.title) == "string" and board.title ~= "" then
    name = name .. " — " .. board.title
  end
  local when = format_millis(board.last_used_at or board.created_at)
  local marker = (active_id and board.id == active_id) and " *" or ""
  return string.format("%s%s  (%s)", name, marker, when)
end

--- Recency key for sorting boards (newest first).
local function board_sort_key(board)
  return board.last_used_at or board.created_at or 0
end

--- Run `via task board list --json` and return the parsed table, or nil.
function M.load_boards()
  local output, code = M.run({ "via", "task", "board", "list", "--json" })
  if code ~= 0 then
    vim.notify("via: failed to run `via task board list` (exit " .. code .. ")", vim.log.levels.ERROR)
    return nil
  end
  local ok, data = pcall(vim.json.decode, output)
  if not ok or type(data) ~= "table" then
    vim.notify("via: failed to parse `via task board list` output", vim.log.levels.ERROR)
    return nil
  end
  return data
end

--- Pick a board and switch to it (`via task board use`), then refresh.
--- Bound to buffer-local `gb` on the :ViaTasks buffer only.
function M.switch_board()
  local bufnr = vim.api.nvim_get_current_buf()
  if vim.bo[bufnr].modified then
    vim.notify("via: save or discard board edits before switching boards", vim.log.levels.WARN)
    return
  end

  local data = M.load_boards()
  if not data then
    return
  end
  local boards = data.boards or {}
  if #boards == 0 then
    vim.notify("via: no boards in this workspace", vim.log.levels.WARN)
    return
  end

  table.sort(boards, function(a, b)
    local ar, br = board_sort_key(a), board_sort_key(b)
    if ar == br then
      return (a.id or "") < (b.id or "")
    end
    return ar > br
  end)

  local active = data.active_board
  vim.ui.select(boards, {
    prompt = "Switch task board",
    format_item = function(board)
      return format_board_item(board, active)
    end,
  }, function(choice)
    if not choice or type(choice.id) ~= "string" then
      return
    end
    if choice.id == active then
      vim.notify("via: already on board " .. choice.id, vim.log.levels.INFO)
      return
    end
    local output, code = M.run({ "via", "task", "board", "use", choice.id })
    if code ~= 0 then
      vim.notify("via: failed to switch board: " .. tostring(output), vim.log.levels.ERROR)
      return
    end
    vim.notify("via: switched to board " .. choice.id, vim.log.levels.INFO)
    M.refresh()
  end)
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
  -- Block the autorefresh poll from racing a save: `save` calls `via task
  -- update` + `refresh` synchronously, but a poll fired mid-save would
  -- operate on a stale snapshot. The flag is cleared on every exit path.
  M._saving = true
  local function save_impl()
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
  local ok, err = pcall(save_impl)
  M._saving = false
  if not ok then
    error(err, 0)
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

--- Open the task body in a transient split buffer (`<CR>` / Ctrl+click on
--- `via:<id>`). The buffer is unlisted and `bufhidden=wipe`, so closing the
--- window wipes it (no `:ls!` leak); `buftype=acwrite` keeps it writeable via
--- the buffer-local `BufWriteCmd`, which calls `via task update <id> --body`.
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
    vim.bo[bufnr].bufhidden = "wipe"
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

  vim.cmd("split")
  vim.api.nvim_win_set_buf(0, bufnr)
end

--- Autorefresh poll interval in milliseconds.
--- Read once per poll so `:let g:via_tasks_autorefresh_ms = 0` disables live.
--- A non-number (or nil) falls back to the default of 2000 ms.
local AUTOREFRESH_DEFAULT_MS = 2000

local function autorefresh_interval_ms()
  local v = vim.g.via_tasks_autorefresh_ms
  if type(v) == "number" then
    return v
  end
  return AUTOREFRESH_DEFAULT_MS
end

--- True when a via-tasks buffer exists, is loaded, and is shown in some window.
--- Used to short-circuit the poll when nobody is looking at the board (no
--- point spawning `via task list` if the buffer is hidden or gone).
local function tasks_buffer_visible()
  local bufnr = vim.fn.bufnr(TASKS_BUF, false)
  if bufnr <= 0 or not vim.api.nvim_buf_is_valid(bufnr) then
    return nil
  end
  for _, win in ipairs(vim.api.nvim_list_wins()) do
    if vim.api.nvim_win_is_valid(win) and vim.api.nvim_win_get_buf(win) == bufnr then
      return bufnr
    end
  end
  return nil
end

--- Silently redraw the board from `data` (no notify). Runs only when the
--- buffer is unmodified, and restores the cursor in every window showing it.
--- Differs from `M.refresh` (which is the explicit `gR` path and notifies):
--- autorefresh should stay quiet and never steal cursor focus.
function M.silent_refresh(bufnr, data)
  if type(bufnr) ~= "number" or not vim.api.nvim_buf_is_valid(bufnr) then
    return
  end
  -- The buffer may have switched to modified since the poll gated on it (the
  -- async fetch can race user typing); bail rather than clobbering edits.
  if vim.bo[bufnr].modified then
    return
  end
  if M._saving then
    return
  end

  -- Snapshot cursor per window so we can restore it after set_lines.
  local cursors = {}
  for _, win in ipairs(vim.api.nvim_list_wins()) do
    if vim.api.nvim_win_is_valid(win) and vim.api.nvim_win_get_buf(win) == bufnr then
      local ok, pos = pcall(vim.api.nvim_win_get_cursor, win)
      if ok and pos then
        cursors[win] = pos
      end
    end
  end

  local content = build_content(data)
  vim.bo[bufnr].modifiable = true
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, content.lines)
  vim.b[bufnr].via_tasks_snapshot = content.snapshot
  vim.b[bufnr].via_tasks_board = data.board
  vim.bo[bufnr].modified = false
  -- Rebuilt from the store (always valid rows); clear any stale diagnostics.
  vim.diagnostic.reset(diagnostics_ns, bufnr)
  -- Re-apply maps / id highlight in case a refresh path dropped them.
  setup_tasks_buffer(bufnr)

  local line_count = vim.api.nvim_buf_line_count(bufnr)
  for _, win in ipairs(vim.api.nvim_list_wins()) do
    if vim.api.nvim_win_is_valid(win) and vim.api.nvim_win_get_buf(win) == bufnr then
      apply_task_id_highlight(win)
      local pos = cursors[win]
      if pos then
        local row = math.max(1, math.min(pos[1], line_count))
        local col = math.max(0, pos[2])
        pcall(vim.api.nvim_win_set_cursor, win, { row, col })
      end
    end
  end
end

--- One autorefresh poll tick. Cheap fast-path checks (no visible buffer,
--- buffer modified, save in flight) run synchronously; only when something
--- may have changed does it dispatch `via task list --json` via `M.run_async`
--- and silently redraw on a signature mismatch.
---
--- The signature is the raw `via task list --json` stdout. We compare it
--- against the last value stored in `vim.b[bufnr].via_tasks_signature`
--- (updated by `open`/`refresh`/`silent_refresh` via `load_tasks` callers),
--- so we only redraw when the board actually changed — not on every tick.
function M.poll_autorefresh()
  if M._polling then
    return
  end
  local bufnr = tasks_buffer_visible()
  if not bufnr then
    return
  end
  if vim.bo[bufnr].modified then
    return
  end
  if M._saving then
    return
  end
  M._polling = true
  M.run_async({ "via", "task", "list", "--json" }, function(output, code)
    M._polling = false
    if code ~= 0 or type(output) ~= "string" then
      return
    end
    -- The buffer may have been modified or wiped while the fetch was in
    -- flight; re-validate before touching it.
    if not vim.api.nvim_buf_is_valid(bufnr) or vim.bo[bufnr].modified or M._saving then
      return
    end
    local last = vim.b[bufnr].via_tasks_signature or ""
    if output == last then
      return
    end
    local ok, data = pcall(vim.json.decode, output)
    if not ok or type(data) ~= "table" then
      return
    end
    vim.b[bufnr].via_tasks_signature = output
    M.silent_refresh(bufnr, data)
  end)
end

--- Start the autorefresh poll timer (idempotent). Interval is read each tick
--- from `vim.g.via_tasks_autorefresh_ms`, so changing it takes effect without
--- restarting Neovim. A value of 0 disables autorefresh entirely.
local autorefresh_timer
function M.start_autorefresh()
  if autorefresh_timer then
    return
  end
  autorefresh_timer = vim.loop.new_timer()
  if not autorefresh_timer then
    return
  end
  -- start(delay, repeat): the repeat value is a ceiling; if the user later
  -- sets the interval to 0 we re-check on each tick and stop the timer.
  autorefresh_timer:start(AUTOREFRESH_DEFAULT_MS, AUTOREFRESH_DEFAULT_MS, vim.schedule_wrap(function()
    if autorefresh_interval_ms() <= 0 then
      return
    end
    M.poll_autorefresh()
  end))
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
  board_header = board_header,
  board_new_cmd = board_new_cmd,
  task_create_cmd = task_create_cmd,
  slugify_board_id = slugify_board_id,
  format_millis = format_millis,
  format_board_item = format_board_item,
  board_sort_key = board_sort_key,
  validate_rows = validate_rows,
  split_height = split_height,
  diagnostics_ns = diagnostics_ns,
  TASK_ID_PREFIX = TASK_ID_PREFIX,
  TASKS_BUF = TASKS_BUF,
  autorefresh_interval_ms = autorefresh_interval_ms,
}

return M
