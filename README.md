# EndeavorMCP

Live Pluto (Julia) notebooks for AI agents. Run the notebook tools on a
workstation, a lab server or a cluster node, and point any MCP agent and a
web browser at them. You start them yourself, the way you start Pluto or
Jupyter. Your agent edits and runs a live Pluto notebook, and you watch it in
the browser.

These are the notebook tools of [Endeavor](https://github.com/jowch/Endeavor),
a macOS app with a Claude Code agent beside a live Pluto notebook. The app
depends on these crates and installs the same binary on servers.

Logging in, ssh, tunnels and Slurm allocations stay with you or your agent.
Endeavor doesn't do them for you here.

The command is `endeavor-remote`, the same binary Endeavor installs on
servers. It may be renamed later.

## Install it

You need Julia 1.11 or newer, or nothing: when no `julia` is on your login
shell's `PATH`, Endeavor downloads its own pinned Julia (1.12.6) into
`~/.cache/endeavor/` the first time.

To build from source, install Rust 1.89 or newer, then run:

```
cargo install --git https://github.com/jowch/EndeavorMCP endeavor-remote
```

The binary carries Endeavor's Julia code (`runtime/`) and the skills, so it is
all you install. On first use it unpacks the Julia code into
`~/.cache/endeavor/serve/<version>/`.

For Linux (x86_64 and aarch64) there are prebuilt binaries on the
[Helpers release](https://github.com/jowch/EndeavorMCP/releases/tag/helpers),
built by the Helpers workflow for each change to the helper's source. Each
file is named `endeavor-remote-<key>-<platform>`, and `endeavor-remote-<key>.sha256`
holds the checksums for that key. Take the newest key and check the download:

```
key=<newest key on the release page>
base=https://github.com/jowch/EndeavorMCP/releases/download/helpers
curl -fLO $base/endeavor-remote-$key-linux-x86_64
curl -fLO $base/endeavor-remote-$key.sha256
sha256sum -c --ignore-missing endeavor-remote-$key.sha256
chmod +x endeavor-remote-$key-linux-x86_64
mv endeavor-remote-$key-linux-x86_64 ~/.local/bin/endeavor-remote
```

Binaries from before `serve` was added don't have it. Run
`endeavor-remote serve --help` to check.

## Start it

In the folder where you want your notebooks, run:

```
endeavor-remote serve
```

The first start installs Pluto into Endeavor's package folder, which takes a
few minutes. Later starts take seconds. Once Julia is ready, `serve` prints
everything you need to connect:

```
Endeavor's notebooks are running on lab3, port 41873. New notebooks go in /home/ada/project.

Open them in a browser:
    http://localhost:41873/?token=9780…

From another computer, forward the port first:
    ssh -L 41873:localhost:41873 lab3

Connect an agent over MCP (Streamable HTTP):
    URL:    http://localhost:41873/mcp
    Header: Authorization: Bearer 9780…

Claude Code:
    claude mcp add --transport http endeavor http://localhost:41873/mcp --header "Authorization: Bearer 9780…"
…
Press Ctrl-C to stop Julia.
```

It also prints a Codex, a Gemini CLI and a generic JSON configuration with
the same URL and header.

To stop Julia, press Ctrl-C in that terminal. `serve` also stops Julia when
it gets SIGTERM or SIGHUP, such as when the terminal closes or a Slurm job
ends.

If Julia is already running from the same state folder, `serve` uses it
instead of starting another, prints the same details, and leaves it running
when you press Ctrl-C. Stop that runtime with `endeavor-remote stop`.

### Options

| Option | What it does | Default |
|---|---|---|
| `--folder DIR` | Where new notebooks go, and where relative paths start | The current folder |
| `--port PORT` | The port on `127.0.0.1`. Fix it so your `ssh -L` line stays the same | A free port |
| `--julia PATH` | The `julia` to use, or `auto` | `auto`: your login shell's `julia`, else Endeavor's own download |
| `--julia-shell LINE` | A shell line that puts `julia` on the `PATH`, such as `'module load julia/1.12'` | |
| `--depot DEPOT` | `JULIA_DEPOT_PATH` for the runtime | `~/.cache/endeavor/depot:`, or `$SCRATCH/endeavor/depot:` when `$SCRATCH` is set |
| `--idle-stop HOURS` | Stop a notebook nobody has used for this long. `0` never stops one | `48` |
| `--host-tools` | Give every agent session `list_folder`, `read_file` and `run_shell` on this machine (`serve` only) | Off |
| `--state-dir DIR` | Where the runtime keeps `runtime.json`, its token and `runtime.log` | `~/.local/state/endeavor/serve/<host name>` |

The default depot is the one the app uses on servers, so packages installed
for one are there for the other. The trailing `:` puts your own `~/.julia`
behind it, read-only.

Use `--host-tools` when your agent runs on another computer, such as your
laptop, and its own file and shell tools can't see this machine. Without
it, the agent can only edit and run notebooks.

The state folder is per machine by default, because a cluster's nodes
often share one home folder.

## Connect your agent

Run the agent where it can reach `localhost:PORT`: on the same machine, or on
your computer after you forward the port.

**Claude Code.** Paste the `claude mcp add` line that `serve` printed.

**Codex.** Add the printed block to `~/.codex/config.toml`:

```toml
[mcp_servers.endeavor]
url = "http://localhost:41873/mcp"
http_headers = { Authorization = "Bearer 9780…" }
```

**Gemini CLI.** Add the printed object to `~/.gemini/settings.json`:

```json
{ "mcpServers": { "endeavor": { "httpUrl": "http://localhost:41873/mcp", "headers": { "Authorization": "Bearer 9780…" } } } }
```

**Other agents.** Use the URL and the `Authorization` header. The server
speaks MCP's Streamable HTTP transport.

The token stays the same across restarts, because it's kept in the state
folder. The port stays the same only if you fix it with `--port`.

### On the same machine: the stdio form

When the agent runs on the same machine as Julia, it can start Endeavor
itself with `endeavor-remote mcp`, which speaks MCP over stdin and stdout.
`mcp` starts Julia in the background, or uses the one already running from
the state folder, and relays the agent's messages to it. Julia keeps running
after the agent exits, so the next session finds the notebook still open. It
stops itself once no notebook has been open for the `--idle-stop` time, and
`endeavor-remote stop` stops it at once.

`mcp` takes the options above except `--host-tools`, plus `--skills plugin`
for an agent that loads Endeavor's skills from a plugin. Each agent session
gets its own `--folder`, even when they share one Julia.

`mcp` answers the agent's handshake at once. A tool call while Julia is still
starting waits up to 45 seconds, then says Julia is still starting, so the
agent can try again. The first start installs packages and takes a few
minutes.

**Claude Code plugin.** This repository is a plugin marketplace. Its
`endeavor` plugin (`claude-plugin/`) carries the skills and runs
`endeavor-remote mcp --skills plugin --folder ${CLAUDE_PROJECT_DIR}`. Put
`endeavor-remote` on your `PATH`, then:

```
claude plugin marketplace add jowch/EndeavorMCP
claude plugin install endeavor@endeavor
```

**Codex**, in `~/.codex/config.toml`:

```toml
[mcp_servers.endeavor]
command = "endeavor-remote"
args = ["mcp"]
```

**Gemini CLI**, in `~/.gemini/settings.json`:

```json
{ "mcpServers": { "endeavor": { "command": "endeavor-remote", "args": ["mcp"] } } }
```

`mcp` puts new notebooks in the folder it starts in. If your agent starts MCP
servers in another folder, add `"--folder", "/path/to/project"` to `args`.
Without the plugin, the agent gets a `notebook_guide` tool with
the same skills, and the server's instructions tell it to read the guide
first.

`mcp` writes its progress and the browser link to stderr, which agents keep
in their MCP logs. The agent also gets the link: `new_notebook`,
`open_notebook` and `pluto_session_status` return `browser_url`, and the
server's instructions tell the agent to give it to you.

## Watch the notebook in a browser

Open the link `serve` printed. The link sets a cookie for this runtime and
reloads the page without the token. You get Pluto's own page, where you can
read, edit and run cells too.

Each notebook has its own link, `http://localhost:PORT/edit?id=<notebook id>&token=…`.
The agent's `browser_url` is that link.

If you forward a different local port than the runtime's, change the port in
the link.

## Reach it from another computer

The runtime listens only on `127.0.0.1` of the machine it runs on. To reach it
from your computer, forward its port with ssh:

```
ssh -L 41873:localhost:41873 lab3
```

Keep that ssh session open while you work. Then open the browser link and
point your agent at `http://localhost:41873/mcp` on your computer.

## Run it on a cluster

Clusters don't want long-running programs on their login nodes. Run `serve`
inside a job.

Interactively:

```
salloc -c 8 --mem 32G -t 8:00:00
srun --pty endeavor-remote serve --port 41873 --julia-shell 'module load julia/1.12'
```

As a batch job, with the details in the job's output file:

```
sbatch -c 8 --mem 32G -t 8:00:00 -o serve-%j.out --wrap "endeavor-remote serve --port 41873"
```

Inside a job, `serve` prints a tunnel line that jumps through the login node
the job came from (`SLURM_SUBMIT_HOST`):

```
ssh -J login2 -L 41873:localhost:41873 n2cn0216
```

Use the name you ssh to for the login node. The compute node must accept ssh
from the login node, as clusters with `pam_slurm_adopt` do.

When the job ends or you `scancel` it, Slurm sends SIGTERM and `serve` stops
Julia. Notebooks are saved as you go, so a new job picks up the files.

On clusters that set `$SCRATCH`, packages go to `$SCRATCH/endeavor/depot`,
because home quotas are small.

## Open a notebook you already have

Opening a notebook from disk runs nothing. Whether the agent opens it with
`open_notebook` or you open it from Pluto's start page in the browser, it
opens in safe preview: you see the code and the saved outputs, and no cell
runs. To run it, click **Run notebook code** at the top of Pluto's page. The
agent can also call `allow_execution`, but only when you ask it to.

Notebooks the agent creates with `new_notebook` run without safe preview,
because they start empty. On Pluto's start page, the play button next to a
recent notebook starts it and runs it.

## Keep it safe

- The token lets anyone who has it run code as you on that machine. Don't
  paste it into shared chats or commit agent configs that hold it.
- The runtime listens only on `127.0.0.1`. Other users on a shared node can
  still reach `127.0.0.1`, which is why every request needs the token, either
  as the bearer header or as the browser cookie.
- The browser cookie opens only Pluto's page. The MCP endpoint (`/mcp`) takes
  only the header, so code in a notebook's output can't call the agent's tools.
- The token file, `runtime.json` and `runtime.log` are readable only by you.
- To change the token, stop the runtime, delete `token` in the state folder,
  and start it again.

## What's different from the app

- Nothing asks you before a run. Your agent's own permission prompts are the
  approval. There is no Plan, Ask or Manual mode and no run card.
- There is no notebook pane, no annotation mode and no "Point to the chat".
  Pluto's page in the browser is the view.
- The skills still describe the app in places, such as the notebook pane and
  the approval card. The server's instructions tell the agent to skip those
  parts.
- An agent connected over HTTP has no session of its own: it isn't held to
  one notebook, and `list_notebooks` shows `this_session` false everywhere.
  The stdio form gives each agent session one notebook, as the app does.
- The idle stop is a flag, not a setting, and applies to the whole runtime.
- On Windows, `serve` is untried. Ctrl-C ends `serve` but leaves Julia
  running; use `endeavor-remote stop`.

## Planned: an update command

A command to update the binary, `endeavor-remote update` (or `endeavor update`
after a rename), is planned and not built. Open questions:

- Where it gets new versions: the Helpers release assets that
  `scripts/helpers.sh` downloads, or a tagged release.
- How it checks the download: a checksum, as `scripts/helpers.sh` does, or a
  signature.
- How it replaces its own binary while it runs.
- What happens to a runtime already running from the older build. The app's
  build check and its Restart Julia path handle this case today; `serve`
  would need its own message.
- How a `cargo install` user updates instead: probably `cargo install` again.

## What's in this repository

- `crates/endeavor-mcp`: the `endeavor-remote` binary. It is the runtime
  core, the MCP server, `serve`, `mcp` and `stop`, and the helper Endeavor
  runs on servers.
- `crates/wire`: the protocol between Endeavor and the helper.
- `runtime/`: the Julia side (`boot.jl` and the `EndeavorRuntime` package),
  built into the binary.
- `plugin/`: the Pluto skills. Endeavor loads this folder as its Claude Code
  plugin (unpacked from the crate, like `runtime/`), and the binary serves the
  same files through `notebook_guide`.
- `claude-plugin/` and `.claude-plugin/marketplace.json`: the standalone
  Claude Code plugin, which runs `endeavor-remote mcp` with the skills.
- `scripts/helpers.sh` and `scripts/build-helpers.sh`: get or build the Linux
  binaries. The Helpers workflow publishes them to the
  [Helpers release](https://github.com/jowch/EndeavorMCP/releases/tag/helpers).
- `docs/`: how the runtime is built ([runtime-core.md](docs/runtime-core.md),
  [one-port.md](docs/one-port.md), [endeavor-mcp.md](docs/endeavor-mcp.md))
  and how to test it ([testing.md](docs/testing.md)).

Build and test with `cargo build` and `cargo test`.
