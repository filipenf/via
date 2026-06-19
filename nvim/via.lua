-- via.lua
-- Lua module for Neovim plugins to interact with via agents.
-- Load with: require('via')

local M = {}

local socket = vim.g.via_editor_socket
local uv = vim.uv or vim.loop

local function encode(payload)
  if vim.json and vim.json.encode then
    return vim.json.encode(payload)
  end
  return vim.fn.json_encode(payload)
end

local function notify(payload)
  if not socket or socket == "" or not uv then
    vim.notify("via: editor socket not available", vim.log.levels.WARN)
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

-- List known agent panes (placeholder for future richer discovery).
function M.agent.list()
  -- Currently returns a placeholder; real listing can be added via RPC response.
  return { "orchestrator" }
end

return M
