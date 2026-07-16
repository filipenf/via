#!/usr/bin/env bash
# Acceptance check for the task-board eval fixture.
# Exit 0 only when all unit tests pass (PASS_TO_PASS and FAIL_TO_PASS).
# Uses Neovim's Lua runtime (same dependency as via's nvim/ tests).
set -euo pipefail

cd "$(dirname "$0")"

if ! command -v nvim >/dev/null 2>&1; then
  echo "error: nvim not found on PATH (needed for the Lua fixture suite)" >&2
  exit 127
fi

exec nvim --headless -u NONE -i NONE -n -l ./test_algo.lua
