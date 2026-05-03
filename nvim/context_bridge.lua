local socket = vim.g.spectre_editor_socket
local uv = vim.uv or vim.loop
local pending_active_update = false

local function encode(payload)
  if vim.json and vim.json.encode then
    return vim.json.encode(payload)
  end

  return vim.fn.json_encode(payload)
end

local function notify(payload)
  if not socket or socket == "" or not uv then
    return
  end

  local pipe = uv.new_pipe(false)
  if not pipe then
    return
  end

  pipe:connect(socket, function(err)
    if err then
      pipe:close()
      return
    end

    pipe:write(encode(payload) .. "\n", function()
      pipe:close()
    end)
  end)
end

local function current_file_path()
  local buf = vim.api.nvim_get_current_buf()
  local path = vim.api.nvim_buf_get_name(buf)

  if path == "" or vim.bo[buf].buftype ~= "" then
    return nil
  end

  local stat = uv.fs_stat(path)
  if not stat or stat.type ~= "file" then
    return nil
  end

  return path
end

local function send_active_buffer()
  local path = current_file_path()
  if not path then
    return
  end

  local pos = vim.api.nvim_win_get_cursor(0)
  notify({
    type = "active_buffer_changed",
    path = path,
    line = pos[1],
    column = pos[2] + 1,
  })
end

local function schedule_active_buffer()
  if pending_active_update then
    return
  end

  pending_active_update = true
  vim.defer_fn(function()
    pending_active_update = false
    send_active_buffer()
  end, 75)
end

local function send_diagnostics()
  local path = current_file_path()
  if not path then
    return
  end

  local errors = 0
  local warnings = 0

  for _, diagnostic in ipairs(vim.diagnostic.get(0)) do
    if diagnostic.severity == vim.diagnostic.severity.ERROR then
      errors = errors + 1
    elseif diagnostic.severity == vim.diagnostic.severity.WARN then
      warnings = warnings + 1
    end
  end

  notify({
    type = "diagnostics_changed",
    path = path,
    error_count = errors,
    warning_count = warnings,
  })
end

local group = vim.api.nvim_create_augroup("SpectreContextSync", { clear = true })

vim.api.nvim_create_autocmd({ "BufEnter", "BufFilePost", "CursorMoved", "CursorMovedI" }, {
  group = group,
  callback = schedule_active_buffer,
})

vim.api.nvim_create_autocmd("DiagnosticChanged", {
  group = group,
  callback = send_diagnostics,
})

vim.schedule(function()
  send_active_buffer()
  send_diagnostics()
end)
