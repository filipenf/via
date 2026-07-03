local socket = vim.g.via_editor_socket
local lsp_bridge_socket = vim.g.via_lsp_bridge_socket
local uv = vim.uv or vim.loop
local pending_active_update = false
local pending_selection_update = false
local lsp_pipe = nil
local clients = {}
local next_request_id = 0
local pending_requests = {}

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

local function ensure_lsp_pipe()
  if lsp_pipe then
    return lsp_pipe
  end
  if not lsp_bridge_socket or lsp_bridge_socket == "" or not uv then
    return nil
  end
  local pipe = uv.new_pipe(false)
  if not pipe then
    return nil
  end
  pipe:connect(lsp_bridge_socket, function(err)
    if err then
      pipe:close()
      lsp_pipe = nil
      return
    end
    lsp_pipe = pipe
    pipe:read_start(function(read_err, chunk)
      if read_err or not chunk then
        if pipe then pipe:close() end
        lsp_pipe = nil
        return
      end
      for line in chunk:gmatch("[^\n]+") do
        local decode = vim.json and vim.json.decode or vim.fn.json_decode
        local ok, msg = pcall(decode, line)
        if ok and msg and msg.type == "lsp_request" then
          handle_lsp_request(msg)
        end
      end
    end)
  end)
  return pipe
end

local function lsp_notify(payload)
  local pipe = ensure_lsp_pipe()
  if not pipe then
    return
  end
  pipe:write(encode(payload) .. "\n")
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

local function visual_mode()
  local mode = vim.api.nvim_get_mode().mode

  return mode == "v" or mode == "V" or mode == "\022"
end

local function selected_line_text(start_line, end_line)
  local lines = vim.api.nvim_buf_get_lines(0, start_line - 1, end_line, false)
  return table.concat(lines, "\n")
end

local function send_visual_selection()
  if not visual_mode() then
    return
  end

  local path = current_file_path()
  if not path then
    return
  end

  local start_line = vim.fn.getpos("v")[2]
  local cursor_line = vim.api.nvim_win_get_cursor(0)[1]

  if start_line > cursor_line then
    start_line, cursor_line = cursor_line, start_line
  end

  notify({
    type = "visual_selection_changed",
    path = path,
    start_line = start_line,
    end_line = cursor_line,
    text = selected_line_text(start_line, cursor_line),
  })
end

local function schedule_visual_selection()
  if pending_selection_update then
    return
  end

  pending_selection_update = true
  vim.defer_fn(function()
    pending_selection_update = false
    send_visual_selection()
  end, 25)
end

local function send_buffer_to_agent()
  local path = current_file_path()
  if not path then
    return
  end

  if visual_mode() then
    local start_line = vim.fn.getpos("v")[2]
    local cursor_line = vim.api.nvim_win_get_cursor(0)[1]
    if start_line > cursor_line then
      start_line, cursor_line = cursor_line, start_line
    end
    notify({
      type = "buffer_send_requested",
      path = path,
      start_line = start_line,
      end_line = cursor_line,
    })
  else
    notify({
      type = "buffer_send_requested",
      path = path,
    })
  end
end

vim.api.nvim_create_user_command("ViaBufferSend", send_buffer_to_agent, {})

local function get_client_info(client)
  if not client then return nil end
  local caps = client.server_capabilities or {}
  return {
    id = client.id,
    name = client.name,
    root = client.config and client.config.root_dir or "",
    languages = client.config and client.config.filetypes or {},
    capabilities_summary = {
      definition = caps.definitionProvider or false,
      references = caps.referencesProvider or false,
      hover = caps.hoverProvider or false,
      documentSymbol = caps.documentSymbolProvider or false,
    },
  }
end

local function send_clients()
  local list = {}
  for id, info in pairs(clients) do
    table.insert(list, info)
  end
  lsp_notify({ type = "lsp_clients", clients = list })
end

local function handle_lsp_request(msg)
  local req_id = msg.request_id
  local method = msg.method
  local params = msg.params or {}
  local client_id = msg.client_id
  local client = client_id and vim.lsp.get_client_by_id(client_id) or nil
  if not client then
    for _, c in ipairs(vim.lsp.get_clients({ bufnr = 0 })) do
      client = c
      break
    end
  end
  if not client then
    lsp_notify({ type = "lsp_response", request_id = req_id, error = "no lsp client" })
    return
  end
  local handler = function(err, result, _ctx, _config)
    if err then
      lsp_notify({ type = "lsp_response", request_id = req_id, error = tostring(err) })
    else
      lsp_notify({ type = "lsp_response", request_id = req_id, result = result })
    end
  end
  local ok, req_err = pcall(client.request, client, method, params, handler, 0)
  if not ok then
    lsp_notify({ type = "lsp_response", request_id = req_id, error = tostring(req_err) })
  end
end

local group = vim.api.nvim_create_augroup("viaContextSync", { clear = true })

vim.api.nvim_create_autocmd({ "FocusGained", "BufEnter" }, {
  group = group,
  callback = function()
    vim.cmd("silent! checktime")
  end,
})

-- Diagnostics are still pushed automatically (useful for the agent to see errors/warnings)
vim.api.nvim_create_autocmd("DiagnosticChanged", {
  group = group,
  callback = send_diagnostics,
})

-- Visual selection is tracked as editor state so ACP prompts can embed it, but it
-- is not pushed to the agent unless the user submits a prompt or uses :ViaBufferSend.
vim.api.nvim_create_autocmd({ "CursorMoved", "CursorMovedI" }, {
  group = group,
  callback = function()
    if visual_mode() then
      schedule_visual_selection()
    end
  end,
})

vim.api.nvim_create_autocmd("ModeChanged", {
  group = group,
  callback = function()
    if visual_mode() then
      schedule_visual_selection()
    end
  end,
})

-- NOTE: Active buffer context is no longer pushed automatically.
-- Use :ViaBufferSend (or <leader>ab) to explicitly send the current buffer/selection to the agent.

vim.api.nvim_create_autocmd("LspAttach", {
  group = group,
  callback = function(event)
    local client = vim.lsp.get_client_by_id(event.data.client_id)
    if client then
      clients[client.id] = get_client_info(client)
      send_clients()
    end
  end,
})

vim.api.nvim_create_autocmd("LspDetach", {
  group = group,
  callback = function(event)
    clients[event.data.client_id] = nil
    send_clients()
  end,
})

vim.keymap.set({ "n", "v" }, "<leader>ab", "<cmd>ViaBufferSend<cr>", { desc = "Send current buffer or selection to agent" })

vim.schedule(function()
  -- Only send diagnostics on startup; buffer/selection context is now explicit via :ViaBufferSend
  send_diagnostics()
  for _, client in ipairs(vim.lsp.get_clients()) do
    clients[client.id] = get_client_info(client)
  end
  send_clients()
end)

-- Agent helpers (`ViaAgentDel`, `require('via').agent.*`) live in via.lua.
pcall(require, "via")
