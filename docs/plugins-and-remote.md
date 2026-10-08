# Plugins and remote machines

How EndeavorMCP ships as a plugin for Claude Code, Codex and Antigravity, and
how an agent on your computer works on notebooks that run on a server or a
cluster. A design; what is built is marked.

_Drafted 2026-10-05, rewritten the same day after reading the Endeavor app's
remote code ([remote-sessions.md](https://github.com/jowch/Endeavor/blob/main/docs/remote-sessions.md)).
Facts about Antigravity come from its documentation and are untested; Codex
was tried on 2026-10-08 (see "What was seen with Codex")._

_Revised 2026-10-07: see [The revision](#the-revision-decided-2026-10-07).
It removed the link process, a background process for each server, and the
front's own local launcher and the end-of-session and other-sessions
records. Steps 1 to 4 of it are built and the sections
below describe them; the later steps are marked where they change a
section._

## Summary

- One binary, `endeavor`, and one plugin per harness. The plugin holds the
  skills and one MCP entry, `endeavor mcp`, over stdio.
- You install the plugin; the plugin fetches the binary the first time it
  starts. The agent does the rest through
  tools: adds a server, starts a runtime there or submits a Slurm job, and gives
  you the browser link. There is no configuration file to edit and no tunnel
  to open while a session is connected.
- Remote uses what the app uses: one `ssh` to `endeavor connect` on the
  server and the wire protocol over it. The server half is in this
  repository already. The client half moves here from the app.
- A runtime is shared: the app and the plugin, from any of your computers,
  attach to the same runtime on a server. Several sessions can work in one
  notebook, as in the app today.
- First version: login with ssh keys only, and the binary fetched from the
  GitHub release by the plugin's launcher (or by hand with the install
  script).

## The revision (decided 2026-10-07)

Steps 1 to 6 of the order of work below are built. It was
decided after three review rounds found most of their faults in two places:
between the front and the link process it used, and where the front started
this computer's runtime with code of its own beside the helper's.

**What matters, and what does not.** A runtime and its notebooks keep
running when a session ends, crashes or goes quiet, on this computer and on
a server. That is built and stays. A server's browser page staying
reachable on your computer after the session has ended is not needed, and
that was the only thing the link was for.

**The design.**

```text
Your computer                              Server or login node        Compute node

harness ── stdio ── endeavor mcp ── ssh ── endeavor connect ── (relay) ── the runtime
                         │
                         └── this computer: the runtime, found or started directly
```

- **No link process.** Each `endeavor mcp` holds its own `ssh` and helper for the
  machine its session is on, through the client library, as the app does.
  Several sessions on one server each have their own connection; many
  helpers attach to one runtime, as built.
- **One way to find or start a runtime.** The steps (find the one running,
  take `start.lock`, find Julia, start the core, wait until it answers, give
  its port and token) are one function in the library. The helper is a thin
  wrapper that turns `StartRuntime` into that call. On this computer the
  front calls it directly: no child process and no messages. `serve` calls
  it too.
- **The connection looks after itself.** Connect, retry and re-attach move
  from the link into one library type that both the front and the app use.
  A caller says what it wants (attach only, or start, with the settings and
  whether the user agreed to an install) and gets an outcome: ready, queued,
  nothing running, needs install (what), failed (why) or still working
  (which step). The front no longer works an outcome out from state words.
- **One kind of target in the front.** This computer and a server differ
  only in how the runtime's address is obtained and how it is stopped
  (`Provider`, `standalone/target.rs`). There is one status, one way to use a
  machine and one way to stop its runtime.
- **Sessions come and go without ceremony** (built). A client attaches,
  works and goes quiet. Nothing says "I'm done": `endeavor/end_session` and
  the record of ended sessions are gone. The runtime keeps, for each
  session, the notebook it works in, what it has read, and the time of its
  last call, which is used to forget a session after 7 days and by the check
  `stop_machine` makes before it stops a runtime.
- **No list of other sessions** (built). `other_sessions` and
  `active_seconds_ago` are gone from `list_notebooks` and
  `pluto_session_status`, with the label a client sent, and the skill no
  longer tells the agent to mention other sessions. Reading before writing
  (`stale_read`) and the check before a run (`run_conflict`) are what keep
  two sessions from undoing each other, and they stay. The note that named
  cells another session changed lately (`other_session`) is gone as well.
- **One rule ends a notebook: the idle limit** (built). It is the same on this
  computer and on a server, whoever started the runtime, and the runtime says
  which kind it is. The app's "local notebooks quit with the app" is not
  needed; the app can attach and detach as the plugin does.

**What went (step 3).**

- The link process: `endeavor link`, its control port and token,
  `link.json`, `link.lock`, `link.log`, `server.json`, `link::PROTOCOL`,
  reading a link of another protocol, the rules for replacing a link, the
  four-minute status call and the 8-hour lifetime.
- In the front: polling the link and settling on an outcome, the record
  comparison, ending the link of a machine that was never saved, and the
  second target type for this computer (`Target::Local`, `use_local`,
  `stop_local`, the front's own start and attach).

**What went (step 4).** In the runtime: `end_session`, the ended list, the
other-sessions list and the label it needed, and the note about cells another
session changed. In the front: ending its key when its input ends, when the
session moves and when `stop_machine` looks at another runtime, the new key
for each runtime, and the agent's name that went with the label.

Step 4 took 233 lines of source out net (330 deleted, 97 added), and 143 lines
of tests with them (222 deleted, 79 added). The runtime and the front now share
no sign-out and no label, and `stop_machine`'s question to the runtime is
a count and a time instead of the whole list of notebooks.

**What stays.** The helper, the wire protocol with request ids, the process
and Slurm launchers and `job.json`, the client library's `ssh` and channel,
the machine tools, `machines.json` with its schema number and unknown
fields (the app is a second writer; a machine is still saved only after a
connect succeeded, which needs no hand-over now), `projects.json` and
a remembered project that attaches first (a cluster never starts without a job
the user agreed to), the question before installing on a
server and none on this computer, the local runtime starting at the first
call that needs it, `keep_notebook_alive`, the launcher and the plugins.

**What is given up.**

- A server's browser address works while a session or the app is
  connected. When the session ends the tab disconnects; the notebook keeps
  running. A new session gives a new address.
- Between sessions you can forward the port yourself: results will name the
  runtime's port on the server, so `ssh -L <port>:127.0.0.1:<port> <host>`
  and the browser address with its token reach it. On a cluster the runtime is on a
  compute node behind the login node, and there is no supported way to open
  the page between sessions.
- Several sessions on one server mean several `ssh` connections, and each
  subagent's server its own.
- A later interactive sign-in (password or code) would be asked once for
  each session, not once for each server.

**Order of work.** Each step can be stopped after.

1. One find-or-start function, used by the helper, `serve` and the front.
   Stopping the local runtime takes `start.lock`, which it does not today.
   _Built 2026-10-07:_ `runtime::find_or_start`, `runtime::look` and
   `runtime::end`; the local stop, `stop_machine` and `endeavor stop`, take
   `start.lock` (held for a look and a spawn) and say so when Julia is still
   starting; a start outlives the client that asked for it (below). With
   `force` they cancel a start that is under way (`endeavor stop --force`,
   `stop_machine` with `force`): the core names itself in `starting.lock`.
2. Connect, retry and re-attach as one library type that answers with an
   outcome.
   _Built 2026-10-07:_ `client::Session` (`src/client/session.rs`) holds one
   machine's connection, the listener's port, the retries and the re-attach
   after a drop, in a thread of its own that ends when the session is closed
   or dropped (the runtime keeps running). `ensure(want, wait)` answers
   `Ready` (the listener's port, the runtime's token, the page address, pid,
   node, job), `Queued`, `NothingRunning`, `NeedsInstall`, `Failed` or
   `StillWorking(step)`; it returns as soon as that is known, and a later call
   with the same want goes on from there. `status()`, `stop()`,
   `allow_install()` and `close()` are the rest. The link process was this
   type plus the record, the control port, the idle limit and the stop signals.
3. The front holds its connections in process and has one kind of target.
   The link and the front's code for it are deleted. `Ready` gains the
   runtime's port (it has none today), so that results can name it.
   _Built 2026-10-07._ `endeavor mcp` holds one `client::Session` for each
   machine it uses, made by `standalone::open_session` (the one place a
   session is set up), and closes them when its input ends, leaving every
   runtime running. `endeavor link`, `src/link.rs` and their tests are gone.
   A target is one type (`Target`, `standalone/target.rs`) whose runtime comes
   from a `Provider`: `Local` calls `runtime::find_or_start`, `look` and `end`
   directly, and a machine's is its `Session`. Both answer with the same
   `client::Outcome` (this computer never with `NeedsInstall` or `Queued`).
   The machine tools and the notebook calls ask the provider and read its
   `Outcome`; there is one use, one stop, one route and one status. A runtime
   that has gone (died, exited when idle, stopped from elsewhere) is treated
   alike: it is not running, and the next notebook call that needs one starts
   one on this computer and on a plain server (a cluster asks for a job),
   while `list_notebooks` and `pluto_session_status` never start one. Nothing
   running is never remembered: each call that attaches looks again. A
   failure is kept for every call, on this computer and on a machine, until a
   call asks to try again: `use_machine` always does, a notebook call that
   needs a runtime does after the one that reported the failure. A runtime
   that ended while the connection was down is a kept failure, since nobody saw
   it end. `list_machines` shows the state only of the machines this front is
   connected to and connects to nothing. `ToApp::Ready` has `port`
   (`#[serde(default)]`, protocol still 1), and `use_machine` and
   `pluto_session_status` give it as `remote_port`, with `ssh -L` for a
   plain server. The test variables that were the link's (`ENDEAVOR_TEST_SHELL`,
   `_ROOT`, `_STATE`, `_DEPOT`, `_ASK`) are read where the session is made.
4. The session records: no `end_session`, no other-sessions list; the
   skills and tool descriptions follow.
   _Built 2026-10-07._ The runtime no longer has `endeavor/end_session`, the
   ended list, `other_sessions`, the client label (`X-Endeavor-Client`, the
   name from `initialize`), the `other_session` warning or the time on a
   cell's change record. The front no longer ends its key anywhere, and has
   one key for its whole run: a key ended on the old runtime was what made a
   new one necessary, and a binding on a runtime a session returns to is the
   notebook it made there if that is still open (a binding to a notebook that
   is closed counts as none). `stop_machine` asks the runtime for how many
   other sessions working in an open notebook made a tool call in the last 15
   minutes and how long ago the
   latest did (`endeavor/recent_sessions`, which records no call), and
   refuses without `force` when there is one.
5. One idle rule, recorded in `runtime.json`.
   _Built 2026-10-07._ A notebook with no activity for the idle limit stops,
   48 hours unless a caller sets another (`notebooks::IDLE_HOURS`, the only
   place the default is written), on every kind of start. A runtime a client
   starts in the background (`mcp` on this computer, a `Session` on a server
   or cluster) is started to exit once no notebook has been open for that
   long; `serve` is not. The core writes `"exits_when_idle": true|false` to
   `runtime.json` (absent in a record from before: not known). The live limit
   is not in the file, since `endeavor/set_idle_limit` changes it:
   `pluto_session_status` carries `idle_stop_hours` (0 never ends it, even with
   `exits_when_idle` true) and `exits_when_idle`.
6. One local state folder for the app and the plugin, with the paths module.
   _Built 2026-10-07._ `paths.rs` defines every folder Endeavor uses (`Env`
   and one method per folder, plus `server_root`); no other file builds one.
   It is public, so the app takes its folders from it. Two shell scripts
   still repeat a rule because they run before the binary exists: the
   bootstrap script's state folder (`paths::PICK_STATE_DIR_SH`, which `ssh.rs`
   puts in the script) and `scripts/endeavor-mcp.sh`'s binary store. Tests run the
   shell text and compare it with the module; there is no `endeavor paths`
   command, since neither script could call it. The bootstrap script now
   ignores a relative `XDG_STATE_HOME`, as Rust does. No default folder changed.

**Decided on the open points (2026-10-07).**

- The note that names cells another session changed in the last two minutes
  (`other_session`) went too. It is added for any cell of the notebook, not
  only the ones a session works on, and reading before writing already
  covers those.
- Codex on Windows is not supported for now: it puts an MCP server in a job
  object that its children cannot leave (read from its source, not run), so
  a runtime started from it would end with the session. In
  [gaps.md](gaps.md).
- Sign-in stays with keys only. A page for servers that ask for a password
  or a code is not planned; it stays under "Not in the first version".
- A start, once begun, finishes without the client that asked for it, so
  the next session finds the runtime. Built and tested in step 1, for this
  computer and for a server that runs it as a process: the core is its own
  session and holds `starting.lock` until it records itself, so a client
  that comes meanwhile waits for it and never stops it. A Slurm job was
  already left queued. A helper started with `--quit-with-client` still
  stops a runtime it started itself when its input ends.

- `endeavor serve` runs in your terminal until you stop it and has no idle
  exit: you started it on purpose where you can see it. A runtime that
  `mcp` or a server connection starts exits once no notebook has been open
  for the idle limit. `runtime.json` records which it is, so every client
  can say (built).

## Decided

| Question | Decision |
|---|---|
| Transport to a server | The wire protocol over one `ssh`, as in the app. Not `ssh -L` |
| Who sets a server up | The agent, through tools. No file the user edits |
| Sign-in | Your ssh configuration, keys and agent. No password prompt in the first version |
| Installing on a server | Only after the user agreed. Installing the plugin is the agreement to the binary on your own computer; the helper on a server, an update of it, and Endeavor's own Julia there each need a question first, the helper's first and Julia's only if none is found (built). Looking at what is there needs none beyond the harness's prompt for the tool call. The tools take `install: true` only after the user said yes |
| Installing the binary | The plugin does it: its launcher is the MCP command, and on first start it downloads the build the plugin pins from the GitHub release. Installing the plugin is the user's agreement to that. `scripts/install.sh` or `scripts/install.ps1`, run from the web, is the fallback (built). npm later, once there are version tags |
| Where the plugin's binary lives | `${XDG_DATA_HOME:-~/.local/share}/endeavor/bin/<key>/endeavor`, the same for every agent and every platform (`endeavor.exe` under Git Bash on Windows), so Claude Code, Codex and Antigravity share one download. Not the helpers cache, which prunes other keys |
| Which build a plugin runs | The key in `release-key` beside the launcher. Empty or missing: the newest build |
| Signing | Not needed for a `curl` install; wait |
| State folder | One on each machine, yours included, for `serve`, `mcp`, the plugin and the app: the one `serve` uses today. The app stops choosing its own |
| The list of machines | One file the binary owns: `machines.json`, `{"schema": 1, "machines": [...]}` with the app's server records, in `$XDG_CONFIG_HOME/endeavor/` (default `~/.config/endeavor/`, on macOS too), and in `%APPDATA%\Endeavor\` on Windows (built). The app will read and write it there. A bare list of records, the first shape, is read and rewritten as the object. Fields a build doesn't know, in the file, in a record, in its cluster, job defaults and partitions, are kept when it rewrites the file. A file with a higher `schema` than a build knows is read and never rewritten: its writing tools answer that a newer Endeavor wrote it. A machine is written only after a connect has succeeded |
| State folder on a cluster | `~/.local/state/endeavor/cluster`, the same from every login node (built). The app's own is `~/.cache/endeavor/cluster-<id>` until it moves |
| Jobs on a cluster | One at a time for each user. A second client attaches to the job as the first one asked for it, and is told its size |
| Several clients on one runtime | Allowed. No client makes another exit |
| Several agent sessions on one notebook | Allowed, as in the app today. No owner and no takeover |
| Who holds the connection to a server | Each `endeavor mcp`, in process, as the app does (built 2026-10-07). There is no background process |
| This computer | The front calls the one find-or-start function the helper also calls (built 2026-10-07) |
| When a session ends | Nothing is said and nothing is listed. The idle limit is the only thing that ends a notebook, the same everywhere (built 2026-10-07) |
| Windows | A target soon, so nothing macOS-only in the design |

## Two roles on your computer

_Built as described here._

| Role | Command | Where | State |
|---|---|---|---|
| Front (the stdio server) | `endeavor mcp` | your computer, one for each agent session | built |
| Runtime | the core (`endeavor core`) and the engines behind it | where the notebooks run | built for Pluto |

On a server a third process, the helper (`endeavor connect`), runs over the
front's `ssh` and ends with it. Nothing runs in the background for the
plugin: a runtime outlives the session, and nothing else does.

**The runtime is the core, not Julia.** The core is one Rust process on a
machine. It owns the port, the sessions and who works in which notebook,
and drives each notebook engine through an adapter
([runtime-core.md](runtime-core.md)). Pluto on Julia is the one engine
built; Ember (R) and marimo come behind the same core. What clients share
is the core, so everything below about sharing and owning holds for every
engine. Today the core starts Julia with itself and the two stop together.

```text
Your computer                                        Server or login node        Compute node

harness ── stdio ── endeavor mcp ─┬─ 127.0.0.1:PORT ── ssh ── endeavor connect ── (relay) ── the runtime
browser ──────────────────────────┘
```

- **On this computer** the front starts a runtime, or finds the one already
  recorded in the state folder, when a call first needs it, not when the
  front starts. It calls the one find-or-start function in `runtime.rs` that
  the helper and `serve` call; no child process stands between.
- **On a server** the front holds a `client::Session` for the machine. The
  session runs `ssh <alias> endeavor connect` and opens one loopback port on
  your computer. Every connection to that port becomes a stream to the
  runtime's port on the server. The agent's calls and Pluto's page both use
  it, so `browser_url` works without a tunnel of your own.
- **On a cluster** the helper on the login node submits the job and passes
  the streams to a relay on the job's node (`slurm.rs`, built).

**One connection for each session and server.** Every agent session starts
its own front, and each holds its own `ssh`, sign-in and loopback port for a
server. Many helpers attach to one runtime, so a second session on a server
works in the same runtime and the same notebooks as the first; only the
browser address differs. The app keeps its own connection; the two don't
disturb each other (see [Sharing a runtime](#sharing-a-runtime)).

**The connection, as built.** `client::Session` (`src/client/session.rs`)
holds one machine's connection and what hangs on it: sign-in, starting the
helper, starting the runtime or attaching to it, getting the connection back
when it drops, and the one loopback port. It does its work in a thread of its
own, which ends when the session is closed or dropped. The front makes it with
`standalone::open_session`: batch sign-in, its own binary as the helper when the
server's platform is this computer's (and else the release's helper for that
platform, see [Installing the binary](#installing-the-binary)). The front keeps
the settings a session was made with. A tool that asks for a machine whose
saved settings have other connection settings replaces the session, or
refuses when Julia is in use through it.

| Call | What it does |
|---|---|
| `ensure(Want::Attach { install }, wait)` | Attach to a runtime that runs, or a job that waits or runs, and start nothing otherwise: the outcome is `NothingRunning` |
| `ensure(Want::Start { job, install }, wait)` | Start the runtime, or attach to the one running. Returns as soon as the outcome is known: `Ready` (the listener's port, the runtime's token, the page address, pid, node, job), `Queued`, `NeedsInstall` (what), `Failed` (why) or `StillWorking` (which step, when `wait` ran out). A later call with the same want goes on from there. `install` is the user's agreement to what the start needs (the helper if missing, then whatever the start finds it needs) |
| `status()` | The state (connecting, connected, starting, queued, ready, failed, needs_install), the last step, an error, what the helper said, the runtime once ready, and for a job its job and queue |
| `stop()` | Stop the runtime for every client, and say why it didn't. The connection stays |
| `allow_install()` | The user agreed to the helper alone, without a start |
| `close()` | Detach and end the session's thread and port. The runtime goes on |

When the connection drops, the session connects again (waiting 1 s, then more,
up to 30 s apart, for 10 minutes; a name that doesn't resolve or a network
that is down is retried too) and attaches to the runtime it had, if the
helper says it is still running, still starting (`starting.lock` held, no record
yet: `RuntimeState::Starting`) or its job still waits, on the same listener
port. A start that the drop cut short is taken up again the same way: the
session asks for the runtime, and the helper waits for the core that is starting. A
runtime that is gone is not started again by the reconnect: the state is
`failed` and says so. A failed sign-in or host key is not retried, nor is
giving up after 10 minutes: the state is `failed` with the message, the
listener tells the agent to call `use_machine`, and the next ask tries again.
A first connect that fails is reported once and not retried. The session
starts the runtime with `--exit-idle`.

A machine's id is lower-case letters, digits, `-`, `_` and `.`, since it may
be a folder's name on every system.

**How long things last.** These are separate:

- A session's connection lasts as long as its front: when the front's input
  ends it closes the session, and the page address stops working. The app's
  connection lasts as long as the app runs.
- A front or an app that goes away detaches. It never stops the runtime.
- One rule ends a notebook: no activity for the idle limit, 48 hours unless
  set otherwise (`endeavor/set_idle_limit`, `--idle-stop`), for the whole
  runtime, on this computer and on a server, whoever started it.
- A runtime a client starts in the background (`mcp` here, a session on a
  server or cluster, with `endeavor connect --exit-idle`) also ends once no
  notebook has been open for that long, so a runtime nobody uses doesn't stay
  up for good. `serve` runs in your terminal until you stop it and does not.
  Attaching to a running runtime changes nothing about it, and
  `pluto_session_status` says which kind it is. On a cluster the flag goes to
  the core in the job, and the job's time limit ends it too.

**The token.** The helper sends the runtime's token when the runtime is
ready. The session keeps it in memory, and the front adds it to each
request, as it does for a runtime on this computer.

**The session key.** The front makes a key for its run and sends it on
every request, with the server's name once it uses a server. The front
outlives a dropped `ssh`, so the agent keeps its notebook when the
connection is made again. The key is the same on every runtime the session
uses, and nothing ends it. Each runtime binds a key to a notebook of its own,
so a binding means nothing on another runtime. Coming back to a runtime the
session worked on, the key finds the notebook it made there if that is still
open: it works on that notebook, and the one-notebook rule holds as it did
before it left. If the notebook was closed meanwhile, the binding counts as
none and the session may make or open another. The
front said "a new key (`<first key>-N`)" before: it was needed only because a
runtime ignored a key it had ended, and the old key also made the front's own
earlier session look like another one to the check before a stop.

**What the front does with a target (built).** The front holds one session
for each machine it uses (`Connections`, `standalone/machines.rs`), made by
the first call that needs it. A call goes to the session's target, which is one
type for this computer and for a machine (`Target`, `standalone/target.rs`):
its `Provider` gives the runtime's port and token (`ensure`,
`status`, `stop`). This computer's provider finds or starts the runtime in the
state folder; a machine's is its session. A call that needs a runtime asks
whether one runs, and starts one when none does, on this computer and on a
plain server alike. A cluster is not started without a job the user agreed to:
the call fails with a message that says what to ask. `list_notebooks` and
`pluto_session_status` use a runtime that runs and start none. A runtime that
`stop_machine` ended is not started again by a call until `use_machine`. When
the front's input ends it ends its key on the runtime it is on and closes its
sessions, leaving every runtime running.

**What a project remembers (built).** `projects.json` in the local state folder
(`<state home>/endeavor/`; `%LOCALAPPDATA%\Endeavor` on
Windows) maps a project folder, the front's `--folder` as a canonical path, to
`{machine, folder}`: the machine's id and the folder there. It is written whole
and renamed, owner-only, under a lock, as `machines.json` is. `use_machine`
writes it, and `"local"` removes the entry. A front that starts in such a project
targets the machine and starts nothing; its first call attaches to a runtime
that is there (`Want::Attach`). With none, a call that needs one starts it on a remembered plain
server and on this computer, and a cluster submits nothing, so the call fails with a
message that names the machine, the defaults and `use_machine`. Whether it is a
cluster is the record the connection was made with, so a server that was added again as
a cluster meanwhile submits nothing either. A machine that
is no longer in the machines file is dropped from the session, and the first
result says so.

## Sharing a runtime

Built here: helpers attach together, and `endeavor connect` without
`--state-dir` uses `serve`'s folder. Not built: the app's side. The app
still passes a folder of its own, and a helper from an older build still
makes another older one exit.

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
- Your own `ssh -L` and browser tab keep working. The front listens on a
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

**Quitting the app.** `--quit-with-client` stops the runtime when the app's
end of the helper's input closes without a Stop or Detach, which happens
when the app crashes. It stops the runtime for every client, as Stop does, so
it is only for an app on the same computer. An ordinary quit is the app's
own Stop or Detach. Not built: the app only detaches when it quits (there
is no list of other sessions to decide by); the idle limit ends its
notebooks.

**The package folder locally.** On servers the app and `serve` already use
the same one. On your own computer they differ (the app's is in its data
folder). A shared runtime uses the folder of whichever client started it, so
the two should become one, or packages install twice.

**No client makes another exit.** `serve`, `mcp` and the helper hold
`start.lock` in the state folder only while they find or start a runtime.
Any number of helpers then attach. The runtime's port already serves many
clients. On a cluster the lock covers submitting the job and not the wait
in the queue, so a second helper waits for the same job.

**Stopping the runtime stops it for everyone.** It is not part of ordinary work:
the idle stop and a job's time limit end a runtime. `stop_machine` is for
when you ask, such as to give a cluster node back. Stopping ends other
clients' work, which reading before writing does not guard, so it first
refuses, without `force`, when another session working in an open notebook
made a tool call in the last 15 minutes, and says how many and how long ago the latest did; and the
clients still attached are told the runtime was stopped from another
connection. Every `Stop` and `StartRuntime` carries an id the client chooses, and the
helper answers each once, naming it: `Stopped` or `NotStopped` and why, and for
a start `Ready`, `StartFailed`, `NeedsInstall`, `StartDied` or `StartCancelled`. A
stop that ends a start under way is answered with its own `Stopped`, and the
start with `StartCancelled`. The client waits 60 s for a stop's answer, and an
answer that comes after it gave up is dropped by its id. Stops said while
another stop is under way (before any start asked for after it) share its
outcome: each is answered by its own id, and nothing is stopped twice. A second
start on one channel while one waits fails at once on the client side ("Julia
is already starting."), without disturbing the first. What the client said
during a stop is heard by a start that follows it: a `Detach` or the end of
input ends the helper without starting, and a `Stop` ends that start with
`StartCancelled`. The helper's `Hello` carries `wire::PROTOCOL`, which is
raised when a client and a helper of the previous number can no longer work
together, and the client refuses a helper of another number when it connects
(a failed connection that is not retried, saying the server side is of
another version of Endeavor). A stop waits 20 s for
`start.lock`, so it does not stop a runtime that another helper is still
starting; it says so instead. `stop_machine` on this computer and `endeavor stop`
do the same, and the first marks the stop as made from a connection. Those two, with `force`,
cancel a start under way; the helper's Stop has no `force`.

**A runtime from another build** is used, as `serve` and `mcp` do today.
`use_machine` and `pluto_session_status` say that the runtime there is from
another version of endeavor and that restarting it gets the latest changes.
The front sends its own build's helper, so those two always match, and the
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
  package folder locally. It takes its folders from `endeavor_mcp::paths`
  (`Env::here()`: `state_dir`, `cluster_state_dir`, `machines_file`,
  `server_root`, `depot`) and builds none itself.
- Quitting only detaches.
- It no longer hears "In use from another connection": nothing makes it
  exit. The runtime has no list of other sessions to show instead.
- It does not call `endeavor/end_session` (gone: an unknown method) or read
  `other_sessions` (gone from `list_notebooks` and `pluto_session_status`). A
  session it drops is left to go quiet; the runtime forgets it after 7 days.
- Its rule for a runtime from another build compares builds for equality,
  so it would hold back runs in Ask to run whenever the plugin's build
  started the runtime. It should ask what the runtime can do.
- Its own calls to the runtime (`/endeavor/…`) change together with the app
  today. An app from one build now meets a runtime from another, so those
  calls need a version ([endeavor-mcp.md](endeavor-mcp.md), "The control
  API").

- The library API changed in the last two commits:
  - `client::Options.allow_install` is required: whether a connection may
    install the helper. The app passes `true`, since it asks its user itself.
  - `bootstrap_script(version, exit_idle)` takes no install flag. The script
    never installs on its own: when this build's helper is missing it writes
    nothing on the server and reports, and an install happens only when the
    connection then sends the helper.
  - A connection that may not install ends with `NeedsInstall` (platform, the
    folder, whether it is an update, what is running, and `bytes: Option<u64>`,
    known only for this computer's platform) instead of installing.
    `Running` is `Process { pid, checked }` or `Job { id, listed }`, where
    `checked` and `listed` say whether the script verified it or only read it
    from the record.
  - `ToHelper::StartRuntime { id, job, engine, install }` names the notebook
    system (`wire::ENGINE_PLUTO`) and says whether the helper may install what
    this start needs. A start that needs something it may not install is
    answered `ToApp::NeedsInstall { id, items }`; an item is `wire::Item
    { kind, name, size_mb, place }` (`kind` is `helper` or `runtime` so far, a
    plain string so that a new kind needs no decoder change). `FoundJulia` is
    now `ToApp::Found { name, version, path }`. `connect --no-julia-download`
    is gone.
  - `client::start(channel, listener, &StartOptions { job, engine, install },
    on, notice)` returns `Result<Runtime, StartError>`; `StartError::NeedsInstall`
    carries the items, `StartError::message()` the words. `StartOptions::default()`
    is no job, `ENGINE_PLUTO` and no install. `client::connect` returns
    `ConnectError` (`message`, `retry`, `needs`); there is no string-only twin.
  - `ToApp::NotStopped { id, .. }` answers a `Stop` that didn't end the runtime, and
    `Channel::stop()` returns a `Result`. Requests carry ids and every answer
    names one (`ToApp::answers`); `client::Hello` has the helper's `protocol`.

Read from the code, not run.

## Several sessions in one notebook

The app already lets several of its sessions work in one notebook, and the
core guards it cell by cell: a session must read a cell before it edits it,
and a cell another session changed since needs a fresh read (`stale_read`).
The same holds between the app and the plugin, and between computers. An
earlier draft gave each notebook one owner with a takeover; it was dropped
as more than the problem needs, since an agent acts only when you ask it to.

So moving between computers needs nothing special. You close the laptop
with a notebook open in the app, and at the office an agent in Claude Code
opens the same notebook and works in it. You open the laptop again: the app
reconnects and shows the live page, and both sessions can go on.

Built:

- **Opening a notebook that is already open joins it.** `open_notebook` on an
  open path binds the session to that notebook and returns it, with
  `already_open` true. Nothing runs and safe preview is unchanged. That is
  how a new agent session picks up yesterday's notebook. The app's own calls
  get the same result: it lists first and only opens what isn't open, so it
  never relied on the error.
- **Who else is there** is not shown: the core keeps only the time of each
  session's last call, to forget a session after 7 days and for the check
  before `stop_machine` stops a runtime
  ([endeavor-mcp.md](endeavor-mcp.md#session-identity)).

Each session still works in one notebook (`one_notebook`), as today.

**Not covered: one file open in two runtimes**, such as a notebook on a
shared disk opened on two servers, or on your computer and on a server.
Each runtime saves over the other. Don't do this. One state folder per
machine removes the common case.

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

New: `client::Session` and the target's providers (built), the machine tools,
and getting the helper to send (built). The front sends its own binary when
the server is the same platform as your computer, and else fetches the
release's helper for the server's platform.

The app keeps its copy until it switches to the moved code. That is a change
in both repositories: it lands here, the Helpers release builds, then the
app moves its pin ([status.md](status.md)).

## What the user sees

**The first time on a server (built).** You say "use hoffman2 for this".

1. The agent calls `list_machines`: the machines you've added, and the `Host`
   names in `~/.ssh/config`.
2. It calls `add_machine("hoffman2")`. The front connects and looks. If the
   server lacks this build's helper it installs nothing: the result is
   `needs_install` (what would be copied, where, about how big, whether a
   runtime already runs there), nothing is saved yet, and the agent
   asks you. If you agree it calls again with `install: true`; the front
   sends the helper, and reports the machine's node and home folder,
   whether Slurm is there, and the partitions with their limits. That report
   becomes the saved record. Julia isn't looked for until the first runtime
   starts there (the helper has no call for it), so the report has it as null
   until then. A machine that turns out to have Slurm is saved as a cluster
   and its connection is ended, so the next one starts the helper for Slurm.
   The same question comes with `use_machine` and `stop_machine` when a plugin
   update left the server with an older helper (an update, which sits beside the
   old one). Julia is part of the same question when it is known: if no Julia
   is found on the machine, `use_machine` returns `needs_install` with Julia as
   an item, which Endeavor would download itself (a few hundred MB, into
   `~/.cache/endeavor` there). With the helper missing too, nothing is known
   of Julia yet, so one `use_machine` with `install: true` covers the helper
   and then Julia in one call; a yes to `add_machine` covers the helper only.
   You can instead tell the
   agent where Julia is (`add_machine` with `julia`). A project's remembered
   machine never installs or downloads: the first notebook call says what is
   missing and the agent asks you.
3. On a cluster it proposes resources ("8 CPUs, 32 GB, 8 hours on
   `shared`?"), then calls `use_machine`.

**Waiting for a job.** `use_machine` returns with the job's number once the job is
queued. `pluto_session_status` then gives the queue state, Slurm's reason in
plain words, and once the runtime is up, when the job ends. No call waits
longer than 45 seconds (Codex's tool timeout defaults to 60), and a queued job
is not waited for. The time waited is not given: the front doesn't know when a
job was submitted by an earlier connection.

**The notebook.** `browser_url` is on your computer's loopback and stays the
same while the session is connected.

**The next session.** The project remembers its machine and folder. If the
runtime is still up, the first tool call attaches without asking. If it
needs a new job, the result says so and the agent asks you.

**Approval.** The harness's own permission prompt, for every tool call.

### Tools the front adds (built)

| Tool | What it does |
|---|---|
| `list_machines` | Saved machines with their state, and ssh `Host` names not yet added |
| `add_machine` | Connect to an ssh alias, report and save what was found; installs the helper only with `install: true`, which the user agreed to |
| `use_machine` | Put this session on a machine (or back on this computer), with a folder and, on a cluster, resources. Attaches to the runtime there, starts it, or submits the job |
| `stop_machine` | Stop the runtime there for every client; on a cluster, cancel the job. Refuses first, without `force`, when another session working in an open notebook made a tool call in the last 15 minutes, and says how many and how long ago. Needs this build's helper there, so it can ask to install it too |

`open_notebook` joins a notebook that is already open. `list_notebooks` and
`pluto_session_status` gain the machine, the job and its end time.

`list_folder`, `read_file` and `run_shell` run on the server. The runtime
lists and allows them only for a session that names its server, so the
front lists them always and they refuse on this computer, as built. Results
are in [endeavor-mcp.md](endeavor-mcp.md#the-machine-tools).

On a cluster `use_machine` with no job running and no resources given submits
nothing: it returns `needs_job` with the saved defaults, and the agent asks
you and calls again with them. The resources it submits become the machine's
defaults.

### What the skill tells the agent (built: `endeavor-machines`)

- Don't add a machine, submit a job, switch machines or stop the runtime unless
  the user asked.
- Never install on a machine (its helper, an update of it, or Endeavor's own
  Julia) without asking: a `needs_install` result says what it would do; set
  `install: true` only after the user agreed to that. Julia's download is a
  separate question from the helper's, asked again for each start that needs it.
- Several agents can work in one notebook; when a write or a run is refused
  with `stale_read` or `run_conflict`, read the cells again and retry.
- Never ask for a password or passphrase, and never run `ssh` with one.
- On a server, files are there: use `list_folder`, `read_file` and
  `run_shell`. A project checked out on both machines has the same paths in
  both places, so your own file tools would read the local copy without an
  error.
- Tell the user the browser link, the queue state and when the job ends.

## Sign-in

The front runs the system `ssh` with the alias, keepalives and batch mode.
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

Later, for the third case: a sign-in page, the same on macOS, Linux and
Windows. Where it lives is open, since no background process serves it. The answer goes from the page to `ssh` and
never through the agent. Behind the page, macOS and Linux use askpass, which
the binary already is. Windows askpass has been unreliable, so Windows needs
a tested choice between askpass and running `ssh` under a pseudo-terminal.
The app has the same open question.

## Installing the binary

The plugin's entry is `endeavor mcp`, found on the `PATH`.

**Built:**

- Two install scripts, `scripts/install.sh` (`sh`, macOS and Linux) and
  `scripts/install.ps1` (PowerShell 5.1, Windows): pick the platform, read
  `LATEST` from the release, download `endeavor-<key>-<platform>` (`.exe` on
  Windows) and `endeavor-<key>.sha256`, check the SHA-256, and put
  `endeavor` in `~/.local/bin` (`%LOCALAPPDATA%\Endeavor\bin`), or in the
  folder given by `--dir` / `-Dir` or `ENDEAVOR_INSTALL_DIR`. An existing
  binary is replaced; a Windows one that is running is renamed aside.
  `install.sh` prints the `PATH` line to add and edits no file;
  `install.ps1` adds the folder to the user `PATH` and says so. No `sudo`
  and no administrator rights. `ENDEAVOR_RELEASE_URL` replaces the release's
  address, for tests.
- The Helpers workflow builds `linux-x86_64`, `linux-aarch64`,
  `darwin-x86_64`, `darwin-aarch64` and `windows-x86_64`, and every build gets
  the release's key (`scripts/helpers.sh --key`) in `ENDEAVOR_RELEASE_KEY`.
  `build.rs` records it as `embedded::RELEASE_KEY`, and `endeavor --version`
  prints it as a second line, `release <key>`, after the first line it
  always had. A build without the variable has no key.
- `endeavor update` works on all five platforms (`release.rs`, which `endeavor mcp`
  shares for downloads and checksums). On Windows the running exe is renamed
  to `endeavor.exe.old` and the new one put in its place; the next update
  deletes the old one.
- When the server's platform isn't this computer's, the front fetches
  `endeavor-<key>-<platform>` from the release, checks it against
  `endeavor-<key>.sha256` from the same release, and keeps it as
  `<cache>/endeavor/helpers/<key>/<platform>/endeavor` (`~/.cache` by
  default, `%LOCALAPPDATA%\Endeavor\helpers` on Windows; the folders 0700
  and the file 0600), with the checksum beside it. The next connect uses the
  kept file once its SHA-256 matches the one recorded, with no download. A
  build without a release key says that a helper for that platform needs a
  release build. A download that fails or doesn't match is deleted, the
  session's state is `failed` with the message, and nothing retries it. The
  release's names for platforms are in one place (`release::platform_name`);
  a server that reports another platform (or Windows) is refused with "no
  runtime helper for <os> <arch> servers". Servers are reached by `uname`'s
  words, so macOS servers are `darwin-*`.
- When the server is the same platform as this computer, the front sends its
  own binary.

**Not run:** the PowerShell script (no PowerShell here), the workflow's new
rows (they run on the next push to `main`), `endeavor update` on macOS and
Windows, and a Mac or Windows computer reaching a Linux server. A file
fetched with `curl` carries no quarantine mark, so Gatekeeper and SmartScreen
shouldn't check it, and the Rust linker signs Apple Silicon binaries ad hoc.
Untested.

**Built, run on a real agent only once:** the plugin's launcher, below. Its
download ran once on Linux, by accident, under Codex ([gaps.md](gaps.md)).

When the launcher can't get the binary, it exits with one line saying what
failed and the manual install line. The server then fails to start, but the
skills still load, and the `endeavor-setup` skill tells the agent to ask the
user to reconnect and, with their go-ahead, to run the install line.

### The launcher

`scripts/endeavor-mcp.sh` is the MCP command in every plugin, run as
`sh <plugin>/launch/endeavor-mcp.sh <arguments>`. It finds the binary for the
plugin's build, or gets it, then runs `endeavor mcp` with its arguments.

- **Where.** `${XDG_DATA_HOME:-~/.local/share}/endeavor/bin/`, with
  `XDG_DATA_HOME` counting only if it is an absolute path and `HOME` required
  to be one otherwise (the launcher fails plainly without). With
  `ENDEAVOR_RELEASE_URL` set (tests and development) the folder is
  `bin-from/<checksum of the address>/` beside `bin/`, so such a release never
  puts a binary where a normal start would run it, and `bin/.newest` is
  never written from it.
- **Build.** The first line of `launch/release-key`. With a key, that build is
  fetched if missing and never replaced. With none, the newest one already
  there (`bin/.newest`, which must be a hex key like the others, else it is
  ignored) is run with no network use; the network is asked when none is
  there, by `--fetch-only`, and once a day after a start: the launcher
  starts a `--fetch-only` of its own in the background (stdin, stdout and
  stderr away from it, so it neither delays the server nor writes to its
  output), recorded by `bin/.checked`. The result applies from the next
  start. Claude Code's hook and the other harnesses thus converge.
- **Fetching** is the plugin's own copy of `install.sh` with `--quiet --into
  <bin> [--key <key>]`: the download and its checksum check, then a rename
  into `<bin>/<key>/`, so a binary there is always complete. `--quiet` sends
  everything to stderr. Stdout belongs to the MCP server, so nothing is
  printed there before the exec.
- **Two starts at once** take a lock (`mkdir <bin>/.lock`, holding the owner's
  pid). The second waits up to 25 s for the first, then uses its binary. A
  lock whose owner is gone, or that is over 20 minutes old (a pid can name an
  unrelated process), is taken over by renaming it aside, which only one
  start's rename does. A lock folder that can't be made at all (the data
  folder can't be written) fails at once and names the folder. A start
  removes only a lock holding its own pid.
- **Standard output.** The launcher moves stdout to stderr as its first act
  and gives it back to the server at the exec, so no command it or
  `install.sh` runs can write there.
- **A download cut short** (the agent kills a server that takes over 30 s)
  leaves at most a temporary folder in `<bin>`, never a partial binary. The
  next start downloads again and removes temporary folders older than an hour.
- **Failure** is non-zero with a last line starting `endeavor:`.
- `--fetch-only` only gets the binary. Claude Code's `SessionStart` hook
  (matcher `startup`, so not on resume, clear or compact) runs it in the
  background so the download is usually done before the server starts.
- Without `curl`, `install.sh` uses `wget` under `timeout`, or under a
  watchdog of its own, so a download over 15 minutes is given up on.
- `ENDEAVOR_BIN` names a binary to run instead (`docs/testing.md`).
- `endeavor update` leaves a binary in `<bin>` (or `bin-from/…`) alone and
  says the plugin manages it, and how to fetch the newest now.

`install.sh` also knows Git Bash on Windows (`uname -s` `MINGW*`, `MSYS*` or
`CYGWIN*`: `windows-x86_64` and the `.exe` name).

## Plugins

One plugin per harness with the same content: the skills, the launcher and one
stdio entry that runs it. `claude-plugin/` is checked live with the launcher
replaced by `ENDEAVOR_BIN`. `antigravity-plugin/` is built to what its
documentation says and tried on no real install. `codex-plugin/` was installed
from a local copy of the repository and used through `codex exec` (below).

| | Claude Code | Codex | Antigravity |
|---|---|---|---|
| Manifest | `.claude-plugin/plugin.json` | `plugin.json` at the root | `plugin.json` at the root |
| MCP file | `.mcp.json` | `mcp.json` | `mcp_config.json` |
| Command | `sh ${CLAUDE_PLUGIN_ROOT}/launch/endeavor-mcp.sh …` | `sh ${PLUGIN_ROOT}/launch/endeavor-mcp.sh …` (expanded in `args`) | `sh -c` that finds the plugin in `~/.gemini/antigravity-cli/plugins/endeavor` and runs the launcher |
| Skills | `skills/` (a copy) | `skills/` (a copy) | `skills/` (a copy) |
| Download early | a `SessionStart` hook runs `--fetch-only` | none | none |
| Install | `claude plugin marketplace add`, `claude plugin install` | `codex plugin marketplace add`, `codex plugin add endeavor@endeavor`, from `.agents/plugins/marketplace.json` | `agy plugin install <folder>` |
| Project folder | `--folder ${CLAUDE_PROJECT_DIR}` | none: `--no-folder`. Codex starts `mcp` in the plugin's cache folder and gives no project variable, so notebook paths must be absolute | the folder `mcp` starts in (from documentation) |

Each plugin folder holds `launch/endeavor-mcp.sh`, `launch/install.sh` and
`launch/release-key`. The command is `sh` with the script as its argument, so
it doesn't depend on the file's execute bit.

One source for each part: `plugin/skills/` for the skills, `scripts/` for the
launcher, `install.sh` and `release-key`. `scripts/plugins.sh sync` writes the
copies and `check` fails if any differs or if a `skills` folder is a link; CI
runs `check`. All three folders hold copies: some agents may not follow a link
out of the plugin, and a Windows checkout without `core.symlinks` turns a link
into a text file.
`release-key` is not in `plugin/`, because that folder is hashed into the
build's key.

**What the documentation says, and what was seen.** Antigravity
([antigravity.google/docs/plugins](https://antigravity.google/docs/plugins)):
`plugin.json`, `mcp_config.json`, `hooks.json`, `skills/`, installed to
`~/.gemini/antigravity-cli/plugins/<name>/`. It gives no shape for
`mcp_config.json` or `hooks.json`, so the file uses the `mcpServers` shape the
others use, and no variable is assumed. Not tried.

**What was seen with Codex** (0.161.0, Linux, through `codex exec`;
[developers.openai.com/plugins/build/plugins](https://developers.openai.com/plugins/build/plugins)
was the documentation used to build the folder):

- `codex plugin marketplace add <repo>` reads `.claude-plugin/marketplace.json`
  if that is the only marketplace file, and so offers the Claude plugin. With
  both it and `.agents/plugins/marketplace.json` in the folder, Codex used the
  `.agents` one: `codex plugin list` showed `endeavor@endeavor` with the source
  `./codex-plugin`. `codex plugin add endeavor@endeavor` installs the folder into
  Codex's `plugins/cache/` and lists its three skills to the model. The repo
  has the `.agents` file. The `owner/repo` form of `marketplace add` is in
  `--help`; only a local folder was run.
- Codex expands `${PLUGIN_ROOT}` in `args` and sets `PLUGIN_ROOT` and
  `PLUGIN_DATA` in the environment. It starts the server with the plugin's
  cache folder as its working folder, gives it no variable naming the project,
  and allows a plugin entry's `cwd` only inside the plugin. A relative path
  such as `trial.jl` was looked for in the cache folder; absolute paths worked.
  So `mcp` cannot find the project folder, and the entry passes `--no-folder`
  (see [`--no-folder`](endeavor-mcp.md)). Each `tools/call` carries
  `_meta["x-codex-turn-metadata"].workspaces`, an object whose keys are workspace
  paths; it is undocumented and not used.
- Codex does not pass its own environment to a plugin's server (`ENDEAVOR_BIN`
  and `XDG_*` from the shell were absent); an `env` object in the entry is
  passed.
- At the end of a turn Codex sends SIGTERM to the server's process group. The
  runtime is in its own session and, with Julia, survived each session's end;
  a second session reattached. One server runs per `exec` run.
- The server given as an ordinary entry (`mcp_servers.endeavor`, `command`
  `endeavor`, `args` `["mcp"]`) works and starts in the folder Codex was
  started in. There are no skills on that route; the agent read the guide
  through `notebook_guide`.
- Tool approval is per server. In `codex exec` the tools that change something
  fail ("MCP tool call requires approval, but approval policy is never") until
  the server has `default_tools_approval_mode = "approve"`; read-only tools ran
  without it.
- `codex-plugin/` has no hooks, so nothing fetches the binary early.
- With the plugin installed under a separate `CODEX_HOME` (its entry given an
  `env`), two `codex exec` runs in a folder outside any git repository created
  `hello.jl` there and reopened it. The first run's agent read the skills
  and gave an absolute path at once: `new_notebook`, `edit_cell`,
  `submit_changes` and `read_cell`, four calls. The second used `open_notebook`
  (`already_open`: the runtime had survived), `edit_cell`, `submit_changes` and
  `read_cell`. A third run told to open `hello.jl` as a relative path got
  `invalid_path` ("Give an absolute path: this server was not told the project
  folder.") and retried with the absolute path. Nothing was created in the
  plugin's cache folder.

Still from documentation or not tried: Codex's job object on Windows,
subagents, the tool timeout (default 60 s), interactive `codex` (where the
user is asked to approve), and Antigravity.

To check: that Antigravity accepts its entry, starts the server in the project
folder and loads the skills, and that Claude Code's `SessionStart` hook runs
before the server starts (if not, the first start downloads, as in the other
agents).

## Windows

- The wire protocol needs one plain `ssh` and no connection sharing, which
  Windows OpenSSH lacks.
- With keys and batch mode, nothing in the first version is Unix-only except
  process groups and the kill used to cancel, which need Windows code. The
  app's Windows client can't connect today only because its password prompt
  doesn't start there.
- `serve` and `mcp` on Windows are built and untried on a real machine.

## Safety

- Nothing is installed on a server without the user's agreement. The front
  connects with installs not allowed (the bootstrap script only reports the
  platform, whether this build's helper is there, and, with `sh`, `cat`, `kill`,
  `ps` and `squeue`, whether a runtime or a job is recorded in the state folder
  the helper would use), and starts with `install` false. The
  agreement to the helper is for one session's connection: it lasts for its
  reconnects, and a new session starts without it, which is harmless since the
  helper is installed by then. The agreement to what a start needs (Julia's download) is
  for one start and is not kept. The app, which asks its user itself, passes
  `allow_install` true and starts with `install` true.
- Both ends stay on loopback. The front's port for a machine needs the token, as a
  runtime's does.
- `add_machine` takes an ssh alias: letters, digits, `.`, `-`, `_` and an
  optional `user@`, never starting with `-`. `ssh` gets an argument list
  with the host after `--`.
- The job script quotes its values and `sbatch` gets an argument list. Two
  values the agent can supply still run on the server as given: extra
  `sbatch` flags, and the line that sets Julia up, which is shell code. Both
  show in the harness's permission prompt. Each extra flag must start with
  `-` (a flag and its value are one entry, `--qos=normal`, so that a bare
  word is never taken as the script), and `--wrap` (also as `--wr` or
  `--wra`) and line breaks are refused, by the front and again by the helper
  before it calls `sbatch`.
- The helper fetched for a server of another platform is checked against the
  release's SHA-256 before it is kept, and again each time it is used. The
  checksum file comes from the same release as the binary, so this guards
  against a damaged download and not a tampered release. The same holds for
  the install scripts and `endeavor update`. The app sends its bundled helper
  unchecked.

## Open

- **Stopping one engine or the whole runtime**, once there is a second
  engine.

## Build order

1. Move the client code here as a library. Test it against a server with key
   login.
2. Helpers attach without making each other exit (built). Opening an open
   notebook joins it. (The core also showed a notebook's other sessions; the
   revision removed that.)
3. The machine tools in `mcp` (built), with the `endeavor-machines` skill.
   They first reached a server through a link process, which the revision
   (item 7) removed.
4. The Slurm path through the tools (built against fake Slurm, and checked on
   real Slurm: the helper's side in `e2e_slurm`, the whole path through
   `endeavor mcp` in `e2e_machines_slurm`).
5. macOS and Windows builds, the install scripts, the build's release key
   (built; the workflow's new rows and the PowerShell script are unrun, see
   [gaps.md](gaps.md)).
6. The plugin gets the binary itself: the launcher, the Claude Code plugin
   using it, the Codex plugin folder (used in a trial through
   `codex exec`, with absolute paths only) and the Antigravity one (built to its documentation and
   unverified), the `endeavor-setup` skill, and `endeavor update` leaving a
   plugin's binary alone. The pinned key is set after the first release from
   `main`.
7. The revision of 2026-10-07, in the order given in
   [The revision](#the-revision-decided-2026-10-07); steps 1 to 6 are built.
   It comes before the app, which then takes the library as revised.
8. The app: the moved code, the shared state folder, a version on its calls
   to the runtime, and attaching and detaching as the plugin does.

## Not in the first version

- Password, passphrase or two-factor prompts.
- npm packages.
- Signed or notarized binaries.
- A session on two machines at once.
- Guarding one notebook file open in two runtimes.
- Turning off the agent's own file tools on a server, which a plugin can't
  do.
