---
name: endeavor-setup
description: >-
  Use when Endeavor's notebook tools (`new_notebook`, `open_notebook`, ...) are
  missing, or the endeavor MCP server failed to start or isn't connected.
---

# Endeavor's server didn't start

The plugin starts Endeavor's server itself. The first time on a computer it first downloads the `endeavor` program, about 30 MB, and that can take longer than the agent's start limit (30 seconds in Claude Code). The download goes on, or is redone cleanly, and the next start finds it.

## What to do

1. Tell the user the server isn't up and ask them to reconnect it: `/mcp` in Claude Code, and the agent's own MCP or plugin menu elsewhere. If this is the first start, wait a minute first.
2. If it fails again, read why. In Claude Code `/mcp` shows the server's error and `claude --debug` logs its output. The launcher ends with one line starting `endeavor:` that says what failed, such as no network, a checksum that didn't match, or no build for this platform.
3. If the cause is the download and the user wants to install by hand, say what you'd run and run it only after they agree:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.sh | sh
   ```

   On Windows use `scripts/install.ps1` from the repository, in PowerShell. Then ask them to reconnect again.

Don't edit the plugin's files or the agent's settings to get past this, and don't work around it with your own `julia` or Pluto: without the server there are no notebook tools.
