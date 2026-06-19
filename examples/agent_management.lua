local via = require('via')

via.agent.spawn("reviewer", "reviewer", nil)
via.agent.spawn("security-expert", "security-expert", "crush")

vim.defer_fn(function()
  -- Wait for the panes to be created before sending data
  via.agent.send("security-expert", "hello mr security-expert", true)
  via.agent.send("reviewer", "hello from nvim", false)
  via.agent.send(nil, "hello mr orchestrator", false)
end, 2000)

-- print(via.agent.list())
