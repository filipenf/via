-- via.lua
-- Lua module for Neovim plugins to interact with via agents.
-- Load with: require('via')

local M = {}

local uv = vim.uv or vim.loop

local function encode(payload)
  if vim.json and vim.json.encode then
    return vim.json.encode(payload)
  end
  return vim.fn.json_encode(payload)
end

local function notify(payload)
  local socket = vim.g.via_editor_socket
  if not socket or socket == "" or not uv then
    vim.notify("via: editor socket not available", vim.log.levels.WARN)
    return
  end

  local message = encode(payload) .. "\n"
  local pipe = uv.new_pipe(false)
  if not pipe then
    return
  end

  pipe:connect(socket, function(connect_failure)
    if connect_failure then
      pipe:close()
      return
    end
    pipe:write(message, function(write_failure)
      if write_failure then
        vim.schedule(function()
          vim.notify("via: failed to write editor notification: " .. tostring(write_failure), vim.log.levels.WARN)
        end)
      end
      pipe:close()
    end)
  end)
end

-- Agent namespace
M.agent = {}

-- Send content to an agent pane.
-- agent_id: optional string identifier (nil uses the primary/orchestrator)
-- content: string to send (treated as user input)
-- focus: optional boolean, whether to switch focus to the target pane (default true)
function M.agent.send(agent_id, content, focus)
  if focus == nil then focus = true end
  notify({
    type = "agent_send",
    agent_id = agent_id,
    content = content,
    focus = focus,
  })
end

-- Request spawning a new agent pane.
-- id: unique identifier for the new agent
-- role: optional role label (e.g. "architect", "implementer")
-- command: optional shell command to run (falls back to configured agent)
function M.agent.spawn(id, role, command)
  notify({
    type = "spawn_agent",
    id = id,
    role = role,
    command = command,
  })
end

-- Close a sub-agent pane and tear down its session.
-- id: agent id to terminate (cannot be the primary orchestrator).
function M.agent.del(id)
  notify({
    type = "terminate_agent",
    id = id,
  })
end

local function decode(text)
  if vim.json and vim.json.decode then
    return vim.json.decode(text)
  end
  return vim.fn.json_decode(text)
end

local function read_file(path)
  local fd = io.open(path, "r")
  if not fd then
    return nil
  end
  local contents = fd:read("*a")
  fd:close()
  return contents
end

-- List agents currently registered in this via session.
-- Reads the registry that via writes (resolved via the VIA_SESSION manifest).
-- Returns a list of { id, role, command, primary } tables (empty on failure).
function M.agent.list()
  local session_path = vim.env.VIA_SESSION
  if not session_path or session_path == "" then
    return {}
  end

  local ok, agents = pcall(function()
    local manifest = decode(read_file(session_path) or "")
    if type(manifest) ~= "table" or not manifest.agents_dir then
      return {}
    end
    local registry = read_file(manifest.agents_dir .. "/registry.json")
    if not registry then
      return {}
    end
    return decode(registry)
  end)

  if ok and type(agents) == "table" then
    return agents
  end
  return {}
end

local function complete_agent_ids()
  local ids = {}
  for _, agent in ipairs(M.agent.list()) do
    if agent.id and agent.id ~= "orchestrator" then
      table.insert(ids, agent.id)
    end
  end
  return ids
end

vim.api.nvim_create_user_command("ViaAgentDel", function(opts)
  local id = vim.trim(opts.args)
  if id == "" then
    vim.notify("via: usage :ViaAgentDel <agent-id>", vim.log.levels.WARN)
    return
  end
  M.agent.del(id)
end, {
  nargs = 1,
  desc = "Terminate a sub-agent pane in this via session",
  complete = function(arglead)
    local matches = {}
    for _, id in ipairs(complete_agent_ids()) do
      if id:find("^" .. vim.pesc(arglead)) then
        table.insert(matches, id)
      end
    end
    return matches
  end,
})

-- Load the task board UI (:ViaTasks) if available.
-- Wrapped in pcall so a load error doesn't break the core via module.
pcall(require, 'via.tasks')

return M
