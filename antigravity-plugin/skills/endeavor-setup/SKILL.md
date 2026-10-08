---
name: endeavor-setup
description: >-
  Use when the user asks for notebook work (Pluto or Julia notebooks) and
  Endeavor's notebook tools (`new_notebook`, `open_notebook`, ...) aren't in
  your tool list, or when the endeavor MCP server failed to start or isn't
  connected.
---

# Endeavor's server didn't start

The plugin starts Endeavor's server itself. The first time on a computer it first downloads the `endeavor` program, about 30 MB, and that can take longer than the agent's start limit (30 seconds in Claude Code). The download goes on, or is redone cleanly, and the next start finds it.

In the Endeavor app, the app runs the server, not a plugin. There, tell the user the notebook tools aren't connected and ask them to restart the session. Don't suggest downloading or installing anything, and skip the steps below.

## What to do

1. Tell the user the notebook tools aren't connected, and ask them to reconnect the `endeavor` server: `/mcp` in Claude Code, or the agent's own MCP or plugin menu elsewhere. If Endeavor may never have run on this computer, ask them to wait a minute before reconnecting, so the download can finish.
2. If it fails again, ask the user what that menu shows for `endeavor`. The launcher's last line starts with `endeavor:` and says what failed, such as no network, a checksum that didn't match, or no build for this platform. In Claude Code, `claude --debug` also logs it.
3. If the cause is the download, the plugin's launcher can download again in a terminal, where no start limit applies. The plugin's folder is the one that holds `skills/endeavor-setup/SKILL.md` (this file) and `launch/endeavor-mcp.sh`. Tell the user what you would run, and run it only after they agree:

   ```sh
   sh "<plugin folder>/launch/endeavor-mcp.sh" --fetch-only
   ```

   It downloads the build the plugin asks for into the folder the plugin's server starts from. Then ask the user to reconnect again.

Don't edit the plugin's files or the agent's settings to get past this, and don't work around it with your own `julia` or Pluto: without the server there are no notebook tools.
