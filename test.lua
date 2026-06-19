local via = require('via')

-- print(via.agent.list())

via.agent.send("reviewer", "hello from nvim")
via.agent.send(nil, "hello from nvim")
via.agent.send("security-expert", "hello mr security-expert")


-- via.agent.spawn("reviewer", "reviewer", nil)
-- via.agent.spawn("security-expert", "security-expert", "crush")
