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

**Antigravity and Codex** are not supported yet. The `antigravity-plugin/` and
`codex-plugin/` folders were built to those agents' documentation and have
never been installed. Whether they start the server, find the project folder
and load the skills is not known ([gaps.md](docs/gaps.md)).

You need nothing else. The first start:

- Downloads the `endeavor` program, about 30 MB, into
  `~/.local/share/endeavor/bin/`.
- Downloads its own Julia into `~/.cache/endeavor/` if it finds no `julia`
  (Julia 1.11 or newer works if you have it).
- Installs Julia packages when the first notebook starts. That takes a few
  minutes.

If the notebook tools do not appear, the download may have outlasted the
agent's start limit. Reconnect the MCP server (`/mcp` in Claude Code, or the
agent's MCP or plugin menu). The plugin's set-up skill tells the agent to ask
you to do this.

## Your first notebook

Ask the agent for notebook work, for example:

> Make a notebook that loads `results.csv` and plots each column.
>
> Open `analysis.jl` and fix the cell that fails.

The agent gives you a browser address. Open it to watch the notebook. It is
Pluto's own page, so you can read, edit and run cells there too. The agent can:

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

  Then open `http://localhost:PORT/?token=TOKEN`. `TOKEN` is the content of the
  file `token` in the runtime's state folder on the server, by default
  `~/.local/state/endeavor/serve/<server's host name>/`. On a cluster the
  runtime is on a compute node, and there is no supported way to open it
  between sessions.

## Limits to know before you start

- The Endeavor desktop app and the plugin cannot work on the same notebook
  yet. They use different state folders today, so each would start its own
  runtime on the file.
- Do not open one notebook file in two runtimes, for example over a shared
  disk. Each runtime saves over the other.
- Codex on Windows is not supported: a runtime would end with the session.
- macOS and Windows builds pass CI, but no person has used them. The plugin's
  launcher is a `sh` script, and it has never run on Windows.
- Sign-in is ssh keys only.
- The skills were tried in short Claude Code runs only.

The full list is in [docs/gaps.md](docs/gaps.md).

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

The plugin keeps its program in `~/.local/share/endeavor/bin/BUILD/`. It pins
no build yet, so it runs the newest one it has, looks for a newer one at the
start of a Claude Code session and once a day, and uses a newer one from the
next start. Once a plugin version names a build, it runs exactly that build and
updating the plugin updates the program. A runtime that is already running
keeps its build: run `endeavor stop`, then start a new session.

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
  `.claude-plugin/marketplace.json`), `codex-plugin/` and
  `antigravity-plugin/` are the plugins; `scripts/plugins.sh` keeps their copies
  in step.
- `scripts/`: the install scripts, the plugins' launcher and the helper builds.
- `docs/`: [plugins-and-remote.md](docs/plugins-and-remote.md) (the design),
  [endeavor-mcp.md](docs/endeavor-mcp.md), [runtime-core.md](docs/runtime-core.md),
  [one-port.md](docs/one-port.md), [gaps.md](docs/gaps.md),
  [testing.md](docs/testing.md) and [status.md](docs/status.md).

Build and test with `cargo build` and `cargo test`.
