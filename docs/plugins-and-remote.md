# Plugins and remote machines

How EndeavorMCP ships as a plugin for Claude Code, Codex and Antigravity, and
how an agent on your computer works on notebooks that run on a server or a
cluster. A design: nothing here is built except where marked.

_Drafted 2026-10-05, rewritten the same day after reading the Endeavor app's
remote code ([remote-sessions.md](https://github.com/jowch/Endeavor/blob/main/docs/remote-sessions.md)).
Facts about Codex and Antigravity come from their documentation and are
untested._

## Summary

- One binary, `endeavor`, and one plugin per harness. The plugin holds the
  skills and one MCP entry, `endeavor mcp`, over stdio.
- You install the plugin and the binary. The agent does the rest through
  tools: adds a server, starts a runtime there or submits a Slurm job, and gives
  you the browser link. There is no configuration file to edit and no tunnel
  to open.
- Remote uses what the app uses: one `ssh` to `endeavor connect` on the
  server and the wire protocol over it. The server half is in this
  repository already. The client half moves here from the app.
- A runtime is shared: the app and the plugin, from any of your computers,
  attach to the same runtime on a server. A notebook has one agent session at
  a time, and another session takes it over on purpose.
- First version: login with ssh keys only, and the binary installed with
  `curl` from the GitHub release.

## Decided

| Question | Decision |
|---|---|
| Transport to a server | The wire protocol over one `ssh`, as in the app. Not `ssh -L` |
| Who sets a server up | The agent, through tools. No file the user edits |
| Sign-in | Your ssh configuration, keys and agent. No password prompt in the first version |
| Installing the binary | `curl` from the GitHub release. npm later, once there are version tags |
| Signing | Not needed for a `curl` install; wait |
| State folder | One on each machine, yours included, for `serve`, `mcp`, the plugin and the app: the one `serve` uses today. The app stops choosing its own |
| The list of machines | One file the binary owns, in its own folder. The app reads and writes it there |
| Jobs on a cluster | One at a time for each user. A second client attaches to the job as the first one asked for it, and is told its size |
| Several clients on one runtime | Allowed. No client makes another exit |
| Several agent sessions on one notebook | One owner at a time, with a deliberate takeover |
| Windows | A target soon, so nothing macOS-only in the design |

## Three roles

| Role | Command | Where | State |
|---|---|---|---|
| Front | `endeavor mcp` | your computer, one for each agent session | built |
| Runtime | the core (`endeavor core`) and the engines behind it | where the notebooks run | built for Pluto |
| Link | a background process, one for each server | your computer | not built |

**The runtime is the core, not Julia.** The core is one Rust process on a
machine. It owns the port, the sessions and who works in which notebook,
and drives each notebook engine through an adapter
([runtime-core.md](runtime-core.md)). Pluto on Julia is the one engine
built; Ember (R) and marimo come behind the same core. What clients share
is the core, so everything below about sharing and owning holds for every
engine. Today the core starts Julia with itself and the two stop together.

```text
Your computer                                        Server or login node        Compute node

harness ── stdio ── endeavor mcp ─┐
harness ── stdio ── endeavor mcp ─┼─ 127.0.0.1:PORT ── link ── ssh ── endeavor connect ── (relay) ── the runtime
browser ──────────────────────────┘
```

- **On this computer** the front starts a runtime or finds the one already
  recorded in the state folder, as it does today. No link is involved.
- **On a server** the front asks the link for it. The link runs
  `ssh <alias> endeavor connect`, and opens one loopback port on your
  computer. Every connection to that port becomes a stream to the runtime's
  port on the server. The agent's calls and Pluto's page both use it, so
  `browser_url` works without a tunnel of your own.
- **On a cluster** the helper on the login node submits the job and passes
  the streams to a relay on the job's node (`slurm.rs`, built).

**One link for each server.** Every agent session starts its own front. The
fronts on one computer share one link, so there is one `ssh`, one sign-in
and one browser address for a server. A front finds the link through a file
in a local state folder, as `mcp` finds `runtime.json`, and starts it under
a lock when there is none. The app keeps its own connection; the two don't
disturb each other (see [Sharing a runtime](#sharing-a-runtime)).

**How long things last.** These are separate:

- The link stays for 8 hours after the last agent session, so the browser
  page keeps working, then exits. It is the plugin's only; the app's
  connection lasts as long as the app runs.
- A link or an app that goes away detaches. It never stops the runtime.
- The runtime's idle stop is the core's, as today: 48 hours unless set
  otherwise, for the whole runtime, by whoever started it.

**The token.** The helper sends the runtime's token when the runtime is
ready. The link keeps it in its state folder (readable only by you), and the
front adds it to each request, as it does for a local runtime.

**The session.** The front makes one session key for its run and sends it on
every request, with the server's name once it uses a server. The front
outlives a dropped `ssh`, so the agent keeps its notebook when the link
reconnects.

## Sharing a runtime

Not built. Today a second `endeavor connect` makes the first one exit, and
the app keeps its runtime in a folder of its own.

**One runtime on a server, however it started.** `serve`, `mcp` and
`endeavor connect` find a runtime with the same code (`existing` in
`lib.rs`) and the same `runtime.json`. They all use one state folder,
`~/.local/state/endeavor/serve/<host name>`. So:

- If you started `serve` on the server earlier, `use_machine` attaches to
  it, and its result says a runtime was already running and in which folder.
- If nothing runs, `use_machine` starts one there. `endeavor stop` on the
  server stops it, and a later `serve` finds it.
- The app on your laptop and the plugin on your workstation reach the same
  runtime and the same open notebooks.
- Your own `ssh -L` and browser tab keep working. The link listens on a
  port of its own on your computer, and the runtime's port takes any number
  of connections that carry the token.

**The same on your own computer.** The app keeps its local runtime in its
own data folder today, and `endeavor mcp` uses `serve`'s folder. With one
folder, the app and the plugin on one computer share one runtime and the same
rule for who works in a notebook.

**A machine that is both.** A workstation you sit at and also reach over
`ssh` needs nothing more. The app at its desk and a helper started over
`ssh` from your laptop look in the same folder and find the same runtime.
They don't share a helper: each client has its own, and many helpers attach
to one runtime. The remote file tools go by session, so the laptop's
sessions get them and the desk's don't.

**How the folder is chosen.** `--state-dir` becomes optional for
`endeavor connect`, with the default `serve` and `mcp` use. The app stops
passing it. For a while the helper also looks in the app's old folder, so a
runtime already running from it is found and not started twice.

**Quitting the app.** By default the app stops its local runtime when it
quits (`--quit-with-client`). Shared, that would stop it under a plugin
session, or under your laptop. Quitting should stop the runtime only when
no other session was active lately, and otherwise leave it to the idle stop.

**The package folder locally.** On servers the app and `serve` already use
the same one. On your own computer they differ (the app's is in its data
folder). A shared runtime uses the folder of whichever client started it, so
the two should become one, or packages install twice.

**No client makes another exit.** The helper holds the state folder's lock
only while it starts a runtime, as `mcp` does with `start.lock`. Any number
of helpers then attach. The runtime's port already serves many clients.

**Stopping the runtime stops it for everyone.** It is not part of ordinary work:
the idle stop and a job's time limit end a runtime. `stop_machine` is for
when you ask, such as to give a cluster node back. It first says which
other sessions were active lately, and the clients still attached are told
the runtime was stopped from another connection.

**A runtime from another build** is used, as `serve` and `mcp` do today.
`use_machine` and `pluto_session_status` say that the runtime there is from
another version of endeavor and that restarting it gets the latest changes.
The link sends its own build's helper, so those two always match, and the
helper reaches the runtime over its port. The front answers the tool list
from its own build, so a tool or argument newer than the runtime fails with
the runtime's own error. Only a runtime from before one port per runtime is
refused, as built.

**`serve` in a job you submitted yourself** is recorded under the compute
node's name, and a helper on the login node refuses a runtime on another
node. In the first version, add the node as the machine, reached through
the login node in your ssh configuration, where nodes accept `ssh`. Later
the login helper could find a live runtime in the `serve` folders and reach
it with the relay.

**What the app changes.**

- It uses the shared state folder, on servers and on your computer, and one
  package folder locally.
- Quitting stops the local runtime only when nobody else uses it.
- It shows "taken over" for a notebook where it shows "In use from another
  connection" for a server.
- Its rule for a runtime from another build compares builds for equality,
  so it would hold back runs in Ask to run whenever the plugin's build
  started the runtime. It should ask what the runtime can do.
- Its own calls to the runtime (`/endeavor/…`) change together with the app
  today. An app from one build now meets a runtime from another, so those
  calls need a version ([endeavor-mcp.md](endeavor-mcp.md), "The control
  API").

Read from the code, not run.

## Who works in a notebook

Not built. Today the core binds each session to the first notebook it opens
and refuses a bound session on any other (`one_notebook`). Opening a
notebook that is already open is an error, a session with no notebook is
held to nothing, and a notebook whose session ended can't be picked up
([status.md](status.md), "Known gaps").

**The rule.** Opening a notebook makes the session its owner. If the
current owner made a call in the last 5 minutes, the open fails unless it
says to take over.

- The owner edits and runs. Any session can list and read, and a session
  must own a notebook before it changes one.
- The owner's record has a label ("Claude Code on jc-workstation", "the
  Endeavor app on jc-laptop") and when it was last active.
- A quiet owner is replaced without a question. That is how a new agent
  session picks up yesterday's notebook, and how you move to another
  computer.
- Taking over from an active owner needs your intent, not the other
  session's consent: it is one person's runtime. In the plugin it is an
  argument to `open_notebook`, shown in the harness's permission prompt. In
  the app it is a dialog.
- The notebook's state, variables and running cells stay as they are. It is
  the same notebook process.
- You are not locked. Pluto's page stays open to you in any browser.

**For the session that lost the notebook:**

- A call in progress finishes and returns as usual. A running cell isn't
  cancelled.
- Its next edit or run fails: "This notebook was taken over by the Endeavor
  app on jc-laptop at 14:02. Stop, and tell the user."
- It can still read.
- It learns at its next call. The front can't send the agent a message.

**Reconnecting never takes over.** The app reopens its notebooks when its
connection comes back. If another session owns one by then, the app shows
the live page with "Claude Code on jc-workstation is working in this
notebook. Take over here?".

Example: you close the laptop with a notebook open in the app, and at the
office an agent in Claude Code opens it. The app's session has been quiet,
so the agent owns it. You open the laptop while the agent is still working:
the app reconnects, shows the page and the question, and takes nothing. If
you take over, the agent's next edit fails with the message above.

| Case | What happens |
|---|---|
| Two agent sessions on one computer open the same notebook | The second is told it is in use and how lately; the agent asks you |
| A cell the old owner started still runs at takeover | The new owner sees it running and may interrupt it |
| The old owner had edits prepared and not applied | Dropped. The read-before-edit check makes it read again if it comes back |
| A run waited for your answer in the app | Cancelled, as the core does today for a call that goes away |
| You edit in the browser while an agent owns the notebook | Allowed, as today |
| The runtime restarts or the job ends | Ownership ends with it. The files are saved, and the next to open one owns it |

**Not covered: one file open in two runtimes**, such as a notebook on a
shared disk opened on two servers, or on your computer and on a server.
Each runtime knows only its own owners, and each saves over the other.
Don't do this. One state folder per server removes the common case.

## What is reused

Built, in this repository: `endeavor connect`, the process and Slurm
launchers, `endeavor relay`, the wire protocol and its stream multiplexer,
the file requests, and the remote file and shell tools.

Moved from the app (about 1,100 to 1,200 lines with no GUI in them; std
threads and blocking I/O, as here, and no new dependencies):

- Starting `ssh`, the install script and the tar it sends (`remote.rs`).
- The channel and the local listener (`runtime.rs`).
- The server and cluster records and reading `Host` names from
  `~/.ssh/config` (`hosts.rs`).
- Turning `ssh`'s errors into plain messages (`explain`).

Written again without the GUI: the connect and retry rules (about 300 lines
of `connection.rs`).

New: the link process, the machine tools, and getting the Linux helper to
send.

The app keeps its copy until it switches to the moved code. That is a change
in both repositories: it lands here, the Helpers release builds, then the
app moves its pin ([status.md](status.md)).

## What the user sees

**The first time on a server.** You say "use hoffman2 for this".

1. The agent calls `list_machines`: the machines you've added, and the `Host`
   names in `~/.ssh/config`.
2. It calls `add_machine("hoffman2")`. The link connects, sends the helper if
   the server lacks this build, and reports the machine's name, whether
   Slurm is there, the partitions with their limits, and whether Julia was
   found. That report becomes the saved record.
3. On a cluster it proposes resources ("8 CPUs, 32 GB, 8 hours on
   `shared`?"), then calls `use_machine`.

**Waiting for a job.** `use_machine` returns at once with the job's number.
`pluto_session_status` then gives the queue state, Slurm's reason in plain
words, the time waited, and once the runtime is up, when the job ends. No call
waits longer than 45 seconds (Codex's tool timeout defaults to 60).

**The notebook.** `browser_url` is on your computer's loopback and stays the
same while the link runs.

**The next session.** The project remembers its machine and folder. If the
runtime is still up, the first tool call attaches without asking. If it
needs a new job, the result says so and the agent asks you.

**Approval.** The harness's own permission prompt, for every tool call.

### Tools the front adds

| Tool | What it does |
|---|---|
| `list_machines` | Saved machines with their state, and ssh `Host` names not yet added |
| `add_machine` | Connect to an ssh alias, install the helper, report and save what was found |
| `use_machine` | Put this session on a machine (or back on this computer), with a folder and, on a cluster, resources. Attaches to the runtime there, starts it, or submits the job |
| `stop_machine` | Stop the runtime there for every client; on a cluster, cancel the job. Says first who else was active |

`open_notebook` gains the take-over argument. `list_notebooks` and
`pluto_session_status` gain each notebook's owner and when it was last
active, and the machine, the job and its end time.

`list_folder`, `read_file` and `run_shell` run on the server. The runtime
lists and allows them only for a session that names its server, so the
front lists them always and they refuse on this computer, as built.

### What the skill tells the agent

- Don't add a machine, submit a job, switch machines or stop the runtime unless
  the user asked.
- Take a notebook over from an active session only when the user asks in
  this conversation. When told yours was taken over, stop and say so; don't
  take it back.
- Never ask for a password or passphrase, and never run `ssh` with one.
- On a server, files are there: use `list_folder`, `read_file` and
  `run_shell`. A project checked out on both machines has the same paths in
  both places, so your own file tools would read the local copy without an
  error.
- Tell the user the browser link, the queue state and when the job ends.

## Sign-in

The link runs the system `ssh` with the alias, keepalives and batch mode.
`ssh` applies your configuration, keys and agent. Endeavor reads only the
`Host` names from `~/.ssh/config`, never a key, and stores no password or
key path. It leaves host-key checking as you have it.

In batch mode `ssh` fails instead of asking. Three cases fail, each with a
message that says what to do:

| Case | What the user does |
|---|---|
| A key with a passphrase that isn't in the ssh agent | `ssh-add`, once, in a terminal |
| A host never connected to before | `ssh <host>` once in a terminal, to accept its key |
| A server that asks for a password or a code on every login | Unsupported in the first version |

Later, for the third case: a sign-in page on the link's loopback port, the
same on macOS, Linux and Windows. The answer goes from the page to `ssh` and
never through the agent. Behind the page, macOS and Linux use askpass, which
the binary already is. Windows askpass has been unreliable, so Windows needs
a tested choice between askpass and running `ssh` under a pseudo-terminal.
The app has the same open question.

## Installing the binary

The plugin's entry is `endeavor mcp`, found on the `PATH`.

- Two install scripts, `sh` and PowerShell: pick the platform, download from
  the release, check the SHA-256, put `endeavor` on the `PATH`.
- When the binary is missing, the MCP server fails to start but the skills
  still load. A setup skill tells the agent to run the install line with the
  user's approval, then to ask them to reconnect.
- A file fetched with `curl` carries no quarantine mark, so Gatekeeper and
  SmartScreen don't check it, and the Rust linker signs Apple Silicon
  binaries ad hoc. Untested here.

Needed first:

- macOS and Windows builds on the release. The Helpers workflow has the
  macOS rows commented out and no Windows rows.
- `endeavor update` for macOS and Windows. It replaces Linux binaries only.
- A way for a binary to find its own Linux build to send to a server. The
  binary's build hash (`build.rs`) and the release's key
  (`scripts/helpers.sh --key`) are computed differently, so the build has to
  record the key. The link downloads that helper, checks its SHA-256 and
  keeps it. When the server is the same platform as your computer, it sends
  its own binary.

## Plugins

One plugin per harness with the same content: the skills and the stdio
entry. `claude-plugin/` exists and is checked live.

| | Claude Code | Codex | Antigravity |
|---|---|---|---|
| Manifest | `.claude-plugin/plugin.json` | `plugin.json` at the root | `plugin.json` at the root |
| MCP file | `.mcp.json` | `mcp.json` | `mcp_config.json` |
| Skills | `skills/` | `skills/` | `skills/` |
| Install | `claude plugin marketplace add`, `claude plugin install` | `codex plugin marketplace add` | `agy plugin install <folder>` |
| Project folder | `--folder ${CLAUDE_PROJECT_DIR}` | the folder `mcp` starts in | the folder `mcp` starts in |

`plugin/skills/` stays the one copy of the skills. Each harness's folder
points at it; where a harness doesn't follow the link, a script copies the
skills in and CI checks the copies match.

To check: that Codex and Antigravity start a plugin's server in the project
folder, and that each loads the skills.

## Windows

- The wire protocol needs one plain `ssh` and no connection sharing, which
  Windows OpenSSH lacks.
- With keys and batch mode, nothing in the first version is Unix-only except
  process groups and the kill used to cancel, which need Windows code. The
  app's Windows client can't connect today only because its password prompt
  doesn't start there.
- `serve` and `mcp` on Windows are built and untried on a real machine.

## Safety

- Both ends stay on loopback. The link's port needs the token, as a
  runtime's does.
- `add_machine` takes an ssh alias: letters, digits, `.`, `-`, `_` and an
  optional `user@`, never starting with `-`. `ssh` gets an argument list
  with the host after `--`.
- The job script quotes its values and `sbatch` gets an argument list. Two
  values the agent can supply still run on the server as given: extra
  `sbatch` flags, and the line that sets Julia up, which is shell code. Both
  show in the harness's permission prompt.
- The helper sent to a server is checked against the release's SHA-256
  before it is kept. The app sends its bundled helper unchecked.

## Open

- **The machines file's place and format.** The app's `hosts.json` records
  (`Server`, `Cluster`) move here with the client code. The folder is not
  chosen.
- **A cluster's state folder.** One job for each user means one folder for
  a cluster, shared by its login nodes. Its name is not chosen; the app's
  is `~/.cache/endeavor/cluster-<id>` with an id of its own.
- **Stopping one engine or the whole runtime**, once there is a second
  engine.

## Build order

1. Move the client code here as a library. Test it against a server with key
   login.
2. Helpers attach without making each other exit. Notebook owners and
   takeover in the core.
3. The link process and the machine tools in `mcp`.
4. The Slurm path through the tools.
5. macOS and Windows builds, the install scripts, the build's release key.
6. Codex and Antigravity plugin folders.
7. The app: the moved code, the shared state folder, takeover in its
   interface, a version on its calls to the runtime.

## Not in the first version

- Password, passphrase or two-factor prompts.
- npm packages.
- Signed or notarized binaries.
- A session on two machines at once.
- Guarding one notebook file open in two runtimes.
- Turning off the agent's own file tools on a server, which a plugin can't
  do.
