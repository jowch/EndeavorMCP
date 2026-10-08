# Running Endeavor by hand

Use this page when you want to start the notebook runtime yourself, such as on a
lab server or in a Slurm job, and point any MCP agent and a browser at it,
instead of installing the plugin ([README](../README.md)). Here ssh, tunnels
and Slurm allocations stay with you. The plugin's agent does them through tools.

The command is `endeavor`. It used to be called `endeavor-remote`; if you
installed that, delete it once `endeavor` is on your `PATH`.

## Install the program by hand

You need Julia 1.11 or newer, or nothing: when no `julia` is on your login
shell's `PATH`, Endeavor downloads its own pinned Julia (1.12.6) into
`~/.cache/endeavor/` the first time.

On Linux and macOS:

```
curl -fsSL https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.sh | sh
```

On Windows, in PowerShell (this script has never been run, see
[gaps.md](gaps.md)):

```
irm https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.ps1 | iex
```

The script finds the newest build on the
[Helpers release](https://github.com/jowch/EndeavorMCP/releases/tag/helpers),
checks its SHA-256, and installs it as `endeavor` in `~/.local/bin`
(`%LOCALAPPDATA%\Endeavor\bin` on Windows), replacing a copy that is there.
Pick another folder with `--dir` (`sh -s -- --dir FOLDER`) or by setting
`ENDEAVOR_INSTALL_DIR` first. If the folder isn't on your `PATH`, the Linux
and macOS script prints the line to add to your shell's startup file, and
changes none itself; the Windows script adds the folder to your user `PATH`
and says so. It needs no administrator rights and no `sudo`. It trusts the
release the way `curl | sh` always does: the checksum file comes from the
same release as the binary, so the check catches a damaged download and not a
tampered release.

To build from source instead, install Rust 1.89 or newer, then run:

```
cargo install --git https://github.com/jowch/EndeavorMCP endeavor-mcp
```

The binary carries Endeavor's Julia code (`runtime/`) and the skills, so it is
all you install. On first use it unpacks the Julia code into
`~/.cache/endeavor/serve/<version>/`. A new version removes the older
versions' folders once no running Julia uses them and none has been used for
a day.

To install without the script: the release has binaries for Linux (x86_64 and
aarch64), macOS (x86_64 and aarch64) and Windows (x86_64), built by the Helpers
workflow for each change to the helper's source. Each file is named
`endeavor-<key>-<platform>` (`.exe` on Windows), `endeavor-<key>.sha256` holds
the checksums for that key, and `LATEST` names the newest key. Download one
and check it (the platforms are `linux-x86_64`, `linux-aarch64`,
`darwin-x86_64`, `darwin-aarch64` and `windows-x86_64`):

```
base=https://github.com/jowch/EndeavorMCP/releases/download/helpers
key=$(curl -fsSL $base/LATEST)
curl -fLO $base/endeavor-$key-linux-x86_64
curl -fLO $base/endeavor-$key.sha256
sha256sum -c --ignore-missing endeavor-$key.sha256
chmod +x endeavor-$key-linux-x86_64
mv endeavor-$key-linux-x86_64 ~/.local/bin/endeavor
```

On a Mac, `shasum -a 256 -c` replaces `sha256sum -c`.

`endeavor --version` prints the version and the build, a hash of the source it
was built from; a release build adds a second line, `release <key>`, the
release's key for that source. A Mac or Windows computer reaches a Linux
server with the release build: when the agent adds the machine, `endeavor`
fetches the Linux helper from the release by that key, checks its SHA-256 and
keeps it in `~/.cache/endeavor/helpers/` (`%LOCALAPPDATA%\Endeavor\helpers` on
Windows). A build from source has no key and can only send its own binary, so
it reaches servers of its own platform.

## Update it

With a binary from the Helpers release (on Linux, macOS or Windows), run:

```
endeavor update
```

It downloads the newest build that `LATEST` names, checks it against the
release's SHA-256 file, and puts it in place of the binary you ran. If you
already have the newest build, it says so. It needs curl or wget, and write
access to the binary's folder. On Windows the running `endeavor.exe` can't be
overwritten, so the old one is renamed `endeavor.exe.old`, and the next
update removes it. It has not been run on macOS or Windows
([gaps.md](gaps.md)).

Other installs update the way they were installed:

- If you installed with `cargo install`, run the same `cargo install` line
  again. `endeavor update` prints it.
- A platform the release has no binary for (anything but the five above)
  reinstalls with `cargo install`. `endeavor update` says so.
- The copy the Endeavor app installs on a server (in
  `~/.cache/endeavor/<version>/`) updates with the app. `endeavor update`
  refuses to replace it.
- The copy a plugin fetched (in `~/.local/share/endeavor/bin/<key>/`) updates
  with the plugin. `endeavor update` refuses to replace it.

A Julia that's already running keeps running from the build that started
it. Another build uses it as it is when the two builds offer the same tools
and calls. `serve` and `mcp` tell you when the running Julia came from a build
that doesn't, and `endeavor update` when it came from a build other than the
new one. To switch, run `endeavor stop`, then start it again.

## Start it

In the folder where you want your notebooks, run:

```
endeavor serve
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
```

It also prints a Codex, a Gemini CLI and a generic JSON configuration with
the same URL and header.

To stop Julia, press Ctrl-C in that terminal. `serve` also stops Julia when
it gets SIGTERM or SIGHUP, such as when the terminal closes or a Slurm job
ends. On Windows, closing the console window stops Julia too.

If Julia is already running from the same state folder, `serve` uses it
instead of starting another, prints the same details, and leaves it running
when you press Ctrl-C. Stop that runtime with `endeavor stop`. `endeavor
status` shows what is running and where.

### Options

| Option | What it does | Default |
|---|---|---|
| `--folder DIR` | Where new notebooks go, and where relative paths start | The current folder |
| `--no-folder` | The agent is not told a project folder, so its notebook paths must be absolute (`mcp` only; not with `--folder`). Used by the Codex plugin | |
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
your computer after you forward the port. No real agent client has been tried
against `serve` over HTTP yet ([gaps.md](gaps.md)), and the Codex and Gemini
lines below come from those tools' documentation. (Codex was tried with the
stdio form, below.)

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
speaks MCP's Streamable HTTP transport. The agent's MCP client must send back
the `Mcp-Session-Id` it gets from `initialize`, as Streamable HTTP clients do;
that id is what gives the agent a notebook of its own.

The token stays the same across restarts, because it's kept in the state
folder. The port stays the same only if you fix it with `--port`.

### On the same machine: the stdio form

When the agent runs on the same machine as Julia, it can start Endeavor
itself with `endeavor mcp`, which speaks MCP over stdin and stdout.
`mcp` relays the agent's messages to Julia. It starts Julia at the first tool
call that needs it (or uses the one already running from the state folder), so
a session that never uses a notebook starts nothing. Julia keeps running
after the agent exits, so the next session finds the notebook still open. It
stops itself once no notebook has been open for the `--idle-stop` time, and
`endeavor stop` stops it at once.

`mcp` takes the options above except `--host-tools`, plus `--skills plugin`
for an agent that loads Endeavor's skills from a plugin. Each agent session
gets its own `--folder`, even when they share one Julia.

`mcp` answers the agent's handshake at once. The first tool call that needs
Julia starts it and, like any call while Julia is still starting, waits up to
45 seconds, then says Julia is still starting, so the agent can try again. The
first start installs packages and takes a few minutes.

**Codex**, in `~/.codex/config.toml` (tried on Linux with Codex 0.161.0 through
`codex exec`):

```toml
[mcp_servers.endeavor]
command = "endeavor"
args = ["mcp"]
default_tools_approval_mode = "approve"
```

`codex exec` fails every tool that changes something ("MCP tool call requires
approval, but approval policy is never") unless the server has
`default_tools_approval_mode = "approve"`; Codex's sandbox stays on. Codex
starts the server in the folder Codex was started in, so new notebooks go there.

**Gemini CLI**, in `~/.gemini/settings.json`:

```json
{ "mcpServers": { "endeavor": { "command": "endeavor", "args": ["mcp"] } } }
```

`mcp` puts new notebooks in the folder it starts in. If your agent starts MCP
servers in another folder, add `"--folder", "/path/to/project"` to `args`.
Without the plugin, the agent gets a `notebook_guide` tool that serves
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
srun --pty endeavor serve --port 41873 --julia-shell 'module load julia/1.12'
```

As a batch job, with the details in the job's output file:

```
sbatch -c 8 --mem 32G -t 8:00:00 -o serve-%j.out --wrap "endeavor serve --port 41873"
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
- The notebook skill keeps what holds only in the app in its own file
  (`reference/app.md`). The server's instructions tell the agent that it is
  working without the app, so it skips that file.
- Each agent connection is one session with one notebook, as in the app:
  the first notebook it creates or opens. Over HTTP the session is the
  `Mcp-Session-Id` that `initialize` gives the agent's MCP client; the stdio
  form makes one for each run. A notebook another session made, or one you
  opened in the browser, shows `this_session` false
  ([endeavor-mcp.md](endeavor-mcp.md#session-identity)).
- The idle stop is a flag, not a setting, and applies to the whole runtime.
- On Windows, `serve` is untried.
