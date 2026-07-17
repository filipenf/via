local socket = vim.g.via_editor_socket
local lsp_bridge_socket = vim.g.via_lsp_bridge_socket
local uv = vim.uv or vim.loop
local pending_selection_update = false
local pending_file_index_update = false
local pending_symbol_index_update = false
local lsp_pipe = nil
local clients = {}
local last_file_index_payload = nil
local last_symbol_index_payload = nil
local symbol_index_generation = 0
local show_symbols_after_publish = false
local pending_symbols_filter = nil
local maybe_show_symbols_after_publish
local handle_lsp_request

-- LSP SymbolKind: exclude Variable/Field (common-word false positives).
-- Note: SymbolKind has no Parameter; kind 6 is Method and must stay indexed.
local EXCLUDED_SYMBOL_KINDS = {
  [8] = true, -- Field
  [13] = true, -- Variable
}

local MAX_SYMBOLS_PER_BUFFER = 500
local MAX_SYMBOLS_TOTAL = 4000

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

local function open_buffer_paths()
  local paths = {}
  local seen = {}
  for _, bufnr in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(bufnr) and vim.bo[bufnr].buflisted then
      local name = vim.api.nvim_buf_get_name(bufnr)
      if name ~= "" and vim.bo[bufnr].buftype == "" then
        local abs = vim.fn.fnamemodify(name, ":p")
        if not seen[abs] then
          seen[abs] = true
          table.insert(paths, abs)
        end
      end
    end
  end
  return paths
end

local function payload_equal(a, b)
  if a == b then
    return true
  end
  if type(a) ~= "table" or type(b) ~= "table" then
    return false
  end
  return encode(a) == encode(b)
end

local function send_file_index()
  local vcs = require("via.vcs")
  local kind, root = vcs.root()
  local payload = {
    type = "file_index_changed",
    buffers = open_buffer_paths(),
    vcs_working_tree = kind and vcs.working_tree_paths(kind, root) or {},
    vcs_branch = kind and vcs.branch_changed_paths(kind, root) or {},
  }
  if payload_equal(payload, last_file_index_payload) then
    return
  end
  last_file_index_payload = payload
  notify(payload)
end

local function schedule_file_index()
  if pending_file_index_update then
    return
  end
  pending_file_index_update = true
  vim.defer_fn(function()
    pending_file_index_update = false
    send_file_index()
  end, 150)
end

local function symbol_name_ok(name)
  if type(name) ~= "string" or name == "" then
    return false
  end
  -- Strength gate: length >= 3, or contains `_`, or already qualified.
  if #name >= 3 then
    return true
  end
  if name:find("_", 1, true) then
    return true
  end
  if name:find("::", 1, true) or name:find(".", 1, true) or name:find("#", 1, true) then
    return true
  end
  return false
end

local function symbol_kind_ok(kind)
  return type(kind) == "number" and not EXCLUDED_SYMBOL_KINDS[kind]
end

local function symbol_line_1based(range)
  if not range or not range.start or range.start.line == nil then
    return nil
  end
  return range.start.line + 1
end

local function flatten_document_symbols(items, path, out, per_buf_count)
  if not items then
    return
  end
  for _, item in ipairs(items) do
    if #out >= MAX_SYMBOLS_TOTAL or per_buf_count[1] >= MAX_SYMBOLS_PER_BUFFER then
      return
    end
    local name = item.name
    local kind = item.kind
    local line = nil
    if item.selectionRange then
      line = symbol_line_1based(item.selectionRange)
    elseif item.range then
      line = symbol_line_1based(item.range)
    elseif item.location and item.location.range then
      line = symbol_line_1based(item.location.range)
    end
    if symbol_name_ok(name) and symbol_kind_ok(kind) and line then
      table.insert(out, {
        name = name,
        kind = kind,
        path = path,
        line = line,
      })
      per_buf_count[1] = per_buf_count[1] + 1
    end
    if item.children then
      flatten_document_symbols(item.children, path, out, per_buf_count)
    end
  end
end

local function listed_file_bufnr_info(bufnr)
  if not vim.api.nvim_buf_is_valid(bufnr) then
    return nil
  end
  local name = vim.api.nvim_buf_get_name(bufnr)
  if name == "" then
    return nil
  end
  local listed = vim.api.nvim_get_option_value("buflisted", { buf = bufnr })
  local buftype = vim.api.nvim_get_option_value("buftype", { buf = bufnr })
  if not listed or buftype ~= "" then
    return nil
  end
  return {
    bufnr = bufnr,
    path = vim.fn.fnamemodify(name, ":p"),
    loaded = vim.api.nvim_buf_is_loaded(bufnr),
  }
end

-- Session restore often creates listed buffers that stay unloaded (and without LSP)
-- until BufEnter. Load them and nudge FileType once so documentSymbol clients can
-- attach; LspAttach will schedule another symbol-index publish. Do not re-fire
-- FileType on every symbol collect (TextChanged debounce) — that re-enters
-- ftplugins and can thrash LSP on markdown/json/slow-attach buffers.
local function warm_listed_file_buffers()
  for _, bufnr in ipairs(vim.api.nvim_list_bufs()) do
    local info = listed_file_bufnr_info(bufnr)
    if info then
      if not info.loaded then
        pcall(vim.fn.bufload, bufnr)
      end
      if vim.api.nvim_buf_is_loaded(bufnr) and not vim.b[bufnr].via_symbol_warmed then
        local clients_for_buf = vim.lsp.get_clients({ bufnr = bufnr })
        if #clients_for_buf == 0 then
          pcall(vim.api.nvim_buf_call, bufnr, function()
            if vim.bo.filetype == "" then
              vim.cmd("silent! filetype detect")
            else
              vim.cmd("silent! doautocmd <nomodeline> FileType " .. vim.bo.filetype)
            end
          end)
        end
        vim.b[bufnr].via_symbol_warmed = true
      end
    end
  end
end

local function buffers_for_symbol_index()
  warm_listed_file_buffers()

  local bufs = {}
  for _, bufnr in ipairs(vim.api.nvim_list_bufs()) do
    local info = listed_file_bufnr_info(bufnr)
    if info and vim.api.nvim_buf_is_loaded(bufnr) then
      local clients_for_buf = vim.lsp.get_clients({ bufnr = bufnr })
      local has_doc_symbols = false
      for _, client in ipairs(clients_for_buf) do
        local caps = client.server_capabilities or {}
        if caps.documentSymbolProvider then
          has_doc_symbols = true
          break
        end
      end
      if has_doc_symbols then
        table.insert(bufs, {
          bufnr = bufnr,
          path = info.path,
        })
      end
    end
  end
  return bufs
end

local function publish_symbol_index(symbols)
  local payload = {
    type = "symbol_index_changed",
    symbols = symbols,
  }
  if not payload_equal(payload, last_symbol_index_payload) then
    last_symbol_index_payload = payload
    notify(payload)
  end
  maybe_show_symbols_after_publish()
end

local function send_symbol_index()
  symbol_index_generation = symbol_index_generation + 1
  local generation = symbol_index_generation
  local bufs = buffers_for_symbol_index()
  if #bufs == 0 then
    publish_symbol_index({})
    return
  end

  local pending = #bufs
  local collected = {}

  local function finish_one()
    pending = pending - 1
    if pending > 0 then
      return
    end
    if generation ~= symbol_index_generation then
      return
    end
    publish_symbol_index(collected)
  end

  for _, entry in ipairs(bufs) do
    local params = {
      textDocument = vim.lsp.util.make_text_document_params(entry.bufnr),
    }
    local clients_for_buf = vim.lsp.get_clients({ bufnr = entry.bufnr })
    local client = nil
    for _, c in ipairs(clients_for_buf) do
      local caps = c.server_capabilities or {}
      if caps.documentSymbolProvider then
        client = c
        break
      end
    end
    if not client then
      finish_one()
    else
      local ok, requested = pcall(function()
        return client.request("textDocument/documentSymbol", params, function(err, result)
          if generation == symbol_index_generation and not err and result then
            local per_buf = { 0 }
            flatten_document_symbols(result, entry.path, collected, per_buf)
          end
          finish_one()
        end, entry.bufnr)
      end)
      if not ok or not requested then
        finish_one()
      end
    end
  end
end

local function schedule_symbol_index()
  if pending_symbol_index_update then
    return
  end
  pending_symbol_index_update = true
  vim.defer_fn(function()
    pending_symbol_index_update = false
    send_symbol_index()
  end, 250)
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

-- LSP SymbolKind → short label for :ViaSymbols dumps.
local SYMBOL_KIND_NAMES = {
  [1] = "File",
  [2] = "Module",
  [3] = "Namespace",
  [4] = "Package",
  [5] = "Class",
  [6] = "Method",
  [7] = "Property",
  [8] = "Field",
  [9] = "Constructor",
  [10] = "Enum",
  [11] = "Interface",
  [12] = "Function",
  [13] = "Variable",
  [14] = "Constant",
  [15] = "String",
  [16] = "Number",
  [17] = "Boolean",
  [18] = "Array",
  [19] = "Object",
  [20] = "Key",
  [21] = "Null",
  [22] = "EnumMember",
  [23] = "Struct",
  [24] = "Event",
  [25] = "Operator",
  [26] = "TypeParameter",
}

local function symbol_kind_label(kind)
  return SYMBOL_KIND_NAMES[kind] or tostring(kind or "?")
end

local function open_symbols_dump(filter)
  local symbols = (last_symbol_index_payload and last_symbol_index_payload.symbols) or {}
  local lines = {
    "# via symbol index (Neovim → Rust last publish)",
    "# Unique names open as file+line on Ctrl-click; missing/ambiguous fall back to",
    "# workspace-symbol search (Telescope \"No symbols found\" when that search is empty).",
    "#",
    string.format("# count=%d  caps: per_buf=%d total=%d  excluded kinds: Field(8), Variable(13)",
      #symbols, MAX_SYMBOLS_PER_BUFFER, MAX_SYMBOLS_TOTAL),
  }

  local eligible = buffers_for_symbol_index()
  local eligible_set = {}
  for _, entry in ipairs(eligible) do
    eligible_set[entry.bufnr] = true
  end

  table.insert(lines, string.format("# buffers with documentSymbolProvider: %d", #eligible))
  for _, entry in ipairs(eligible) do
    table.insert(lines, string.format("#   %s", entry.path))
  end

  local pending_lsp = {}
  for _, bufnr in ipairs(vim.api.nvim_list_bufs()) do
    local info = listed_file_bufnr_info(bufnr)
    if info and not eligible_set[bufnr] then
      local reason = "no documentSymbol LSP"
      if not vim.api.nvim_buf_is_loaded(bufnr) then
        reason = "not loaded"
      end
      table.insert(pending_lsp, string.format("#   %s (%s)", info.path, reason))
    end
  end
  table.insert(lines, string.format("# listed file buffers not yet indexed: %d", #pending_lsp))
  for _, line in ipairs(pending_lsp) do
    table.insert(lines, line)
  end
  if filter and filter ~= "" then
    table.insert(lines, string.format("# filter: %s", filter))
  end
  table.insert(lines, "#")
  table.insert(lines, "# name\tkind\tpath:line")

  local shown = 0
  local filter_lower = filter and filter ~= "" and filter:lower() or nil
  for _, sym in ipairs(symbols) do
    local name = sym.name or ""
    if not filter_lower or name:lower():find(filter_lower, 1, true) then
      local rel = vim.fn.fnamemodify(sym.path or "", ":.")
      table.insert(lines, string.format(
        "%s\t%s\t%s:%s",
        name,
        symbol_kind_label(sym.kind),
        rel,
        tostring(sym.line or "?")
      ))
      shown = shown + 1
    end
  end

  if shown == 0 then
    table.insert(lines, "# (no symbols" .. (filter_lower and " matching filter" or "") .. ")")
  end

  local bufnr = vim.fn.bufnr("via://symbols", false)
  local buf
  if bufnr ~= -1 then
    buf = bufnr
    vim.bo[buf].modifiable = true
  else
    buf = vim.api.nvim_create_buf(false, true)
    vim.api.nvim_buf_set_name(buf, "via://symbols")
    vim.bo[buf].filetype = "via-symbols"
    vim.bo[buf].buftype = "nofile"
    vim.bo[buf].bufhidden = "wipe"
    vim.bo[buf].swapfile = false
  end
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].modifiable = false
  vim.bo[buf].modified = false

  local win = vim.fn.bufwinid(buf)
  if win == -1 then
    vim.cmd("botright split")
    vim.api.nvim_win_set_buf(0, buf)
    vim.cmd("resize " .. math.min(20, #lines + 2))
  else
    vim.api.nvim_set_current_win(win)
  end
end

-- Called from publish_symbol_index when :ViaSymbols! requested a refresh+dump.
maybe_show_symbols_after_publish = function()
  if not show_symbols_after_publish then
    return
  end
  show_symbols_after_publish = false
  local filter = pending_symbols_filter
  pending_symbols_filter = nil
  open_symbols_dump(filter)
end

vim.api.nvim_create_user_command("ViaSymbols", function(opts)
  local filter = opts.args
  if opts.bang then
    show_symbols_after_publish = true
    pending_symbols_filter = filter
    send_symbol_index()
    vim.notify("via: refreshing symbol index…", vim.log.levels.INFO)
    return
  end
  open_symbols_dump(filter)
end, {
  desc = "Dump last published via symbol index (:ViaSymbols! refreshes first)",
  bang = true,
  nargs = "?",
})

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
  for _, info in pairs(clients) do
    table.insert(list, info)
  end
  lsp_notify({ type = "lsp_clients", clients = list })
end

handle_lsp_request = function(msg)
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
  local handler = function(err, result)
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
    schedule_file_index()
    schedule_symbol_index()
  end,
})

vim.api.nvim_create_autocmd({ "BufAdd", "BufDelete", "BufFilePost", "BufWritePost", "DirChanged", "FileChangedShellPost" }, {
  group = group,
  callback = function()
    schedule_file_index()
    schedule_symbol_index()
  end,
})

vim.api.nvim_create_autocmd({ "TextChanged", "TextChangedI" }, {
  group = group,
  callback = function(event)
    local bufnr = event.buf
    if vim.bo[bufnr].buftype ~= "" then
      return
    end
    local clients_for_buf = vim.lsp.get_clients({ bufnr = bufnr })
    for _, client in ipairs(clients_for_buf) do
      local caps = client.server_capabilities or {}
      if caps.documentSymbolProvider then
        schedule_symbol_index()
        return
      end
    end
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

vim.api.nvim_create_autocmd("LspAttach", {
  group = group,
  callback = function(event)
    local client = vim.lsp.get_client_by_id(event.data.client_id)
    if client then
      clients[client.id] = get_client_info(client)
      send_clients()
      schedule_symbol_index()
    end
  end,
})

vim.api.nvim_create_autocmd("LspDetach", {
  group = group,
  callback = function(event)
    clients[event.data.client_id] = nil
    send_clients()
    schedule_symbol_index()
  end,
})

vim.keymap.set({ "n", "v" }, "<leader>ab", "<cmd>ViaBufferSend<cr>",
  { desc = "Send current buffer or selection to agent" })

vim.schedule(function()
  -- Only send diagnostics on startup; buffer/selection context is now explicit via :ViaBufferSend
  send_diagnostics()
  send_file_index()
  schedule_symbol_index()
  for _, client in ipairs(vim.lsp.get_clients()) do
    clients[client.id] = get_client_info(client)
  end
  send_clients()
end)

-- Agent helpers (`ViaAgentDel`, `require('via').agent.*`) live in via.lua.
pcall(require, "via")
