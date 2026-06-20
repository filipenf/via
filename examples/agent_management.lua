local via = require('via')

via.agent.spawn("reviewer", "reviewer", "opencode")
via.agent.spawn("security-review", "security-review", "crush")

vim.defer_fn(function()
  -- Wait for the panes to be created before sending data
  via.agent.send("security-review",
    "Review the changes in this branch. Check for any potential security " ..
    "issues based on security best practices and known vulnerabilities",
    false)
  via.agent.send("reviewer",
    "Review the changes on this branch. Look for correctness, adherence to " ..
    "language best practices and idiomatic code. Let me know if there are " ..
    "any opportunities for simplifying the code, reducing duplication",
    true)
  via.agent.send(nil, "Hello orchestrator!", false)
end, 2000)

-- print(via.agent.list())
