# EndeavorMCP

A plugin that lets an AI coding agent edit and run live Pluto (Julia)
notebooks. You watch the same notebook in a browser. It runs on your computer,
or on a server or Slurm cluster that you reach over ssh.

## Install

**Claude Code:**

```
claude plugin marketplace add jowch/EndeavorMCP
claude plugin install endeavor@endeavor
```

Then start Claude Code in your project folder. New notebooks go there.

Claude Code doesn't update a plugin from this marketplace on its own. Turn
that on in `/plugin`, under Marketplaces, with "Enable auto-update", or update
by hand:

```
claude plugin marketplace update endeavor
claude plugin update endeavor@endeavor
```

Each plugin pins one build of the `endeavor` program, and old builds are
removed from the release after a few days. A plugin that is behind says so
when it can't get its build, with the update step for its agent.

**Codex:**

```
codex plugin marketplace add jowch/EndeavorMCP
codex plugin add endeavor@endeavor
```

Start Codex in your project folder. To update the plugin, run
`codex plugin marketplace upgrade endeavor` and start Codex again. It differs
from Claude Code in three ways:

- Codex does not tell the plugin's server the project folder, so notebook paths
  must be absolute. The agent gives its working folder plus the file name, or
  the server refuses a relative path and says so.
- `codex exec` fails every tool that changes something ("MCP tool call requires
  approval, but approval policy is never") unless you approve the plugin's server
  in `~/.codex/config.toml`. This leaves Codex's sandbox on:

  ```toml
  [plugins."endeavor@endeavor".mcp_servers.endeavor]
  default_tools_approval_mode = "approve"
  ```

- Codex has no start hook, so the first start downloads the program while the
  agent waits. That can take longer than Codex lets a server start. If the
  notebook tools don't appear, ask the agent to finish the download; it asks
  before running anything.

Without the plugin, Codex can run `endeavor mcp` as an MCP server instead
([serve.md](docs/serve.md#on-the-same-machine-the-stdio-form)); it then starts
the server in the folder Codex was started in. This was tried on Linux with
Codex 0.161.0 through `codex exec`, installing the plugin from a local copy of
this repository (the `owner/repo` form above was not run).

**Antigravity** (the `agy` command line; tried with agy 1.3.2 on Windows 10
only):

```
agy plugin install https://github.com/jowch/EndeavorMCP/tree/main/antigravity-plugin
```

Give the `antigravity-plugin` folder's URL as above, or the path to that folder
in a local copy. The repository's own URL installs the wrong plugin: agy
installs every plugin it finds there, and the Claude Code one overwrites this
one under the same name, leaving the skills without the notebook tools.
`agy plugin uninstall endeavor` removes it, and running the install line again
updates it. It differs from Claude Code in these ways:

- Notebook paths must be absolute, as with Codex: the plugin's server is not
  told the project folder.
- On Windows the plugin runs its launcher with `sh`, which Git for Windows
  installs but doesn't put on the PATH. Without it the notebook tools are
  silently missing. Add Git's `bin` folder to your PATH, then restart
  Antigravity. In PowerShell:

  ```powershell
  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','User') + ';C:\Program Files\Git\bin', 'User')
  ```

  Choosing "Git and optional Unix tools" when installing Git does the same.
- With no `julia.exe` on the PATH, Endeavor offers to install juliaup and
  Julia 1.12.6 for you (no admin needed). To do it yourself instead:
  `winget install --id 9NJNWW8PVKMN -e -s msstore`, then `juliaup add 1.12`
  and `juliaup default 1.12`.
- agy names the server `endeavor_endeavor`. `agy -p` (print mode) refuses
  every MCP call that isn't allowed in `~/.gemini/antigravity-cli/settings.json`,
  one tool at a time:

  ```json
  { "permissions": { "allow": ["mcp(endeavor_endeavor/new_notebook)", "mcp(endeavor_endeavor/edit_cell)"] } }
  ```

  `mcp(endeavor_endeavor/*)` would also allow `run_shell` without asking.
  An interactive agy session asks instead; that was not tried.

Without the plugin, agy can run `endeavor mcp` as an ordinary MCP server. It
needs no `sh`, and you get no skills:

```
agy mcp add endeavor "C:\path\to\endeavor.exe" -- mcp --no-folder
```

Once the plugin has run, `endeavor.exe` is in
`~/.local/share/endeavor/bin/<key>/`. agy then calls the server `endeavor`.
macOS, Linux, the Antigravity desktop app and installing from a marketplace
were not tried.

On Linux and macOS, a plugin needs nothing else. The first start:

- Downloads the `endeavor` program, about 5 MB, into
  `~/.local/share/endeavor/bin/`.
- Starts Julia only when the first Julia notebook is opened or made. Then it
  gets its own Julia 1.12.6 if it finds no `julia` (Julia 1.11 or newer works
  if you have it): with juliaup if you have it, else downloaded into
  `~/.cache/endeavor/`. Then it installs Julia's packages.
  That takes a few minutes the first time. Someone who only opens R notebooks
  never needs Julia.

If the notebook tools do not appear, the download may have outlasted the
agent's start limit. Reconnect the MCP server (`/mcp` in Claude Code, or the
agent's MCP or plugin menu). The plugin's set-up skill tells the agent to ask
you to do this.

## Your first notebook

Ask the agent for notebook work, for example:

> Make a notebook that loads `results.csv` and plots each column.
>
> Open `analysis.jl` and fix the cell that fails.

The notebook opens in your browser when the agent makes or opens it. It is
Pluto's own page, so you can read, edit and run cells there too. If it doesn't
open (on a computer you reach over ssh, say), the agent gives you the address,
and the page says how to get in. The agent can:

- Create a notebook, or open one you have. An opened notebook starts in safe
  preview: nothing runs until you click **Run notebook code** or ask the agent
  to run it.
- Add, edit, delete, move and fold cells, then run them.
- Read cell outputs, including tables, and look at plots.

Each agent session works in one notebook. If you edit a cell while the agent
works, it reads your change before it overwrites it.

## What keeps running

The notebook runtime keeps running after the agent session ends. A later
session can open the same notebook and join it as it is. A notebook stops after
48 hours without use, and the runtime the plugin started ends when no notebook
has been open that long.

`endeavor status` shows what is running and where Endeavor keeps its files. It
changes nothing. `endeavor stop` stops the runtime and every notebook in it.
The plugin's program is not on your `PATH`, so run it by its path, or install
`endeavor` by hand ([Without a plugin](#without-a-plugin)):

```
~/.local/share/endeavor/bin/BUILD/endeavor status
```

`BUILD` is the folder in `bin/` that the file `bin/.newest` names. Both
commands look at this computer only. To stop a runtime on a server, ask the
agent.

## On a server or cluster

Tell the agent which machine to use, for example "run the notebooks on
`lab-server`", and which folder on it the notebooks go in. Otherwise they go in
the machine's home folder.

- The agent signs in with your ssh keys and ssh agent, never a password. If
  sign-in fails it tells you what to run, usually `ssh-add` or `ssh HOST` once
  in a terminal. Servers that ask for a password or a code at every login
  cannot be used yet.
- The agent asks before it installs anything on the server: the Endeavor
  program, an update of it, or a Julia, if none is found.
- On a Slurm cluster the agent proposes resources, such as 8 CPUs, 32 GB and 8
  hours, and submits the job only after you agree. It tells you when the job
  ends.
- The browser address works while the agent session is connected. Afterwards
  the notebooks keep running and you can forward the runtime's port yourself,
  using the `remote_port` the agent reported:

  ```
  ssh -L PORT:127.0.0.1:PORT HOST
  ```

  A browser that opened the notebooks during the session still gets in at
  `http://localhost:PORT/`. Another browser gets a page that names the
  `endeavor open` command to run on the server, which prints the link with the
  runtime's token. On a cluster the
  runtime is on a compute node, and there is no supported way to open it
  between sessions.

## Limits to know before you start

- The Endeavor desktop app and the plugin cannot work on the same notebook
  yet. They use different state folders today, so each would start its own
  runtime on the file.
- Do not open one notebook file in two runtimes, for example over a shared
  disk. Each runtime saves over the other.
- Codex on Windows is not supported: a runtime would end with the session.
- The macOS build passes CI, but no person has used it. On Windows, people
  have run the Antigravity plugin and ssh to a Linux server by hand; the
  Claude Code plugin there was not tried.
- Sign-in is ssh keys only.
- The skills were tried in short Claude Code runs only.

Known gaps are tracked as [GitHub issues](https://github.com/jowch/EndeavorMCP/issues).

## Without a plugin

`endeavor serve` runs the notebooks in your terminal and prints how to connect
any MCP agent, and `endeavor mcp` is the stdio server the plugin runs. See
[docs/serve.md](docs/serve.md). To install `endeavor` by hand on Linux or
macOS:

```
curl -fsSL https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.sh | sh
```

It puts `endeavor` in `~/.local/bin` after checking its SHA-256. The Windows
script, other folders and building from source are in
[docs/serve.md](docs/serve.md#install-the-program-by-hand).

## Update and remove

The plugin keeps its program in `~/.local/share/endeavor/bin/BUILD/`. Each
copy of the plugin names one build, the one made from the same source as its
skills, and runs only that build, downloading it at the first start that needs
it. A new copy of the plugin therefore brings its own build. Until the first
release the plugin's version stays 0.1.0, so your agent may keep the copy it
first installed after the repository has changed. A runtime that is already
running keeps its build: run `endeavor stop`, then start a new session.

For a copy installed by hand, run `endeavor update`. It refuses to replace the
plugin's copy.

There is no uninstall command. Stop the runtime, remove the plugin with your
agent, then delete what you do not want to keep (`endeavor status` prints
each path):

- `~/.local/share/endeavor/`: the program. Old builds stay until you delete them.
- `~/.local/state/endeavor/`: the runtime's state and what each project remembers.
- `~/.cache/endeavor/`: Julia, the packages and the unpacked runtime. This is
  the largest, and the Endeavor desktop app also installs there.
- `~/.config/endeavor/machines.json`: the machines you added.
- `~/.local/bin/endeavor`: a copy installed by hand.

A server you used has `~/.cache/endeavor/` and `~/.local/state/endeavor/` too.

## In this repository

The notebook tools of [Endeavor](https://github.com/jowch/Endeavor), a macOS
app with a Claude Code agent beside a live Pluto notebook, which uses the same
`endeavor` binary.

- `crates/endeavor-mcp/`: the `endeavor` binary and its MCP server.
- `crates/wire/`: the protocol between Endeavor and its helper on servers.
- `runtime/`: the Julia side, built into the binary.
- `plugin/`: the skills. `claude-plugin/` (listed in
  `.claude-plugin/marketplace.json`), `codex-plugin/` (listed in
  `.agents/plugins/marketplace.json`) and `antigravity-plugin/` are the plugins; `scripts/plugins.sh` keeps their copies
  in step.
- `scripts/`: the install scripts, the plugins' launcher and the helper builds.
- `docs/`: [plugins-and-remote.md](docs/plugins-and-remote.md) (the design),
  [endeavor-mcp.md](docs/endeavor-mcp.md), [runtime-core.md](docs/runtime-core.md),
  [one-port.md](docs/one-port.md), [testing.md](docs/testing.md) and
  [status.md](docs/status.md). Known gaps are [GitHub issues](https://github.com/jowch/EndeavorMCP/issues).

Build and test with `cargo build` and `cargo test`.
