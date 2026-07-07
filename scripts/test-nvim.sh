#!/usr/bin/env bash
# Run the Neovim Lua test suite for via's editor integration (:ViaTasks, etc.).
#
# Requires `nvim` >= 0.9 (for `-l` Lua script mode) on PATH. No luarocks /
# busted / plenary needed — the suite uses a tiny built-in harness under
# nvim/tests/. Exits non-zero if any test fails.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v nvim >/dev/null 2>&1; then
  echo "error: nvim not found on PATH (needed for the Lua test suite)" >&2
  exit 127
fi

exec nvim --headless -u NONE -i NONE -n -l "$root/nvim/tests/init.lua"
