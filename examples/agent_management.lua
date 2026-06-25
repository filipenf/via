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

-- Discover the agents via spawned. Returns { id, role, command, primary } tables.
vim.defer_fn(function()
  for _, agent in ipairs(via.agent.list()) do
    print(string.format("agent %s (role=%s)", agent.id, agent.role or "-"))
  end
end, 3000)

-- Agents themselves talk to each other through the `via agent` CLI (see the
-- via-agents skill): `via agent list`, `via agent send --to <id> -m ...`,
-- `via agent inbox`, `via agent whoami`.
