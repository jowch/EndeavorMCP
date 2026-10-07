# Plugins and remote machines

How EndeavorMCP ships as a plugin for Claude Code, Codex and Antigravity, and
how an agent on your computer works on notebooks that run on a server or a
cluster. A design; what is built is marked.

_Drafted 2026-10-05, rewritten the same day after reading the Endeavor app's
remote code ([remote-sessions.md](https://github.com/jowch/Endeavor/blob/main/docs/remote-sessions.md)).
Facts about Codex and Antigravity come from their documentation and are
untested._

_Revised 2026-10-07: see [The revision](#the-revision-decided-2026-10-07-planned).
It replaces the link process and the front's own local launcher. Until the
code follows, the sections below describe what is built, and say where the
revision changes them. Where they say the link connects, runs `ssh`, sends
the helper or fetches one, the front does that after the revision._

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

## The revision (decided 2026-10-07, planned)

Nothing in this section is built. It was decided after three review rounds
found most of their faults in two places: between the front and the link,
and where the front starts this computer's runtime with code of its own
beside the helper's.

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

- **No link.** Each `endeavor mcp` holds its own `ssh` and helper for the
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
  only in how the runtime's address is obtained. There is one status, one
  way to use a machine and one way to stop its runtime.
- **Sessions come and go without ceremony.** A client attaches, works and
  goes quiet. Nothing says "I'm done": `endeavor/end_session` and the
  record of ended sessions go. The runtime keeps, for each session, the
  notebook it works in, what it has read, and the time of its last call,
  which is used only to forget a session after 7 days.
- **No list of other sessions.** `other_sessions` and `active_seconds_ago`
  go from `list_notebooks` and `pluto_session_status`, and the skill no
  longer tells the agent to mention them. Reading before writing
  (`stale_read`) and the check before a run (`run_conflict`) are what keep
  two sessions from undoing each other, and they stay. The note that names
  cells another session changed lately (`other_session`) goes as well.
- **One rule ends a notebook: the idle limit.** It is the same on this
  computer and on a server, whoever started the runtime, and it is recorded
  with the runtime. The app's "local notebooks quit with the app" is not
  needed; the app can attach and detach as the plugin does.

**What goes.**

- The link process: `endeavor link`, its control port and token,
  `link.json`, `link.lock`, `link.log`, `server.json`, `link::PROTOCOL`,
  reading a link of another protocol, the rules for replacing a link, the
  four-minute status call and the 8-hour lifetime.
- In the front: polling the link and settling on an outcome, the record
  comparison, ending the link of a machine that was never saved, and the
  second target type for this computer (`Target::Local`, `use_local`,
  `stop_local`, the front's own start and attach).
- In the runtime: `end_session`, the ended list, the other-sessions list.

Rough size, an estimate from reading: about 1,000 to 1,400 lines of source
go, with `tests/link.rs` and `e2e_link`. About 600 lines of the link (the
connect, retry and re-attach rules) are not removed but move into the
library. The larger gain is fewer places that decide.

**What stays.** The helper, the wire protocol with request ids, the process
and Slurm launchers and `job.json`, the client library's `ssh` and channel,
the machine tools, `machines.json` with its schema number and unknown
fields (the app is a second writer; a machine is still saved only after a
connect succeeded, which needs no hand-over now), `projects.json` and
attach-only for a remembered project, the question before installing on a
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
   starting; a start outlives the client that asked for it (below).
2. Connect, retry and re-attach as one library type that answers with an
   outcome.
3. The front holds its connections in process and has one kind of target.
   The link and the front's code for it are deleted. `Ready` gains the
   runtime's port (it has none today), so that results can name it.
4. The session records: no `end_session`, no other-sessions list; the
   skills and tool descriptions follow.
5. One idle rule, recorded in `runtime.json`.
6. One local state folder for the app and the plugin, with the paths module.

**Decided on the open points (2026-10-07).**

- The note that names cells another session changed in the last two minutes
  (`other_session`) goes too. It is added for any cell of the notebook, not
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
  can say (step 5).

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
| Who holds the connection to a server | Planned (2026-10-07): each `endeavor mcp`, in process, as the app does. Built today: a link process for each server |
| This computer | Planned (2026-10-07): the front calls the one find-or-start function the helper also calls. Built today: the front has its own |
| When a session ends | Planned (2026-10-07): nothing is said and nothing is listed. The idle limit is the only thing that ends a notebook, the same everywhere |
| Windows | A target soon, so nothing macOS-only in the design |

## Three roles

_Built as described here. The revision removes the link: the front holds
the connection itself. The runtime and the helper are unchanged._

| Role | Command | Where | State |
|---|---|---|---|
| Front | `endeavor mcp` | your computer, one for each agent session | built |
| Runtime | the core (`endeavor core`) and the engines behind it | where the notebooks run | built for Pluto |
| Link | a background process, one for each server | your computer | built; the front calls it |

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

- **On this computer** the front starts a runtime, or finds the one already
  recorded in the state folder, when a call first needs it, not when the
  front starts. No link is involved.
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

**The link, as built.** `endeavor link --machine <id>` is a hidden command; a
front starts it with `link::ensure(<server record>)`, which returns the link's control
port and token. The front writes the record to `links/<id>/server.json` (owner-only) before
it starts the link, which reads it once and keeps it, also when it reconnects: it doesn't read
the machines file, and the machine need not be in it. A tool that asks for a link
whose record has other connection settings, or none, quits it and starts a new one
(or refuses, when Julia is in use through it). The link connects as
the library does (batch sign-in), sends its own binary as the helper when the
server's platform is this computer's (and else the release's helper for that
platform, see [Installing the binary](#installing-the-binary)), and starts the one loopback port that
relays to the runtime. It keeps `links/<id>/link.json` (its pid, control
port, token, build and control protocol number), `server.json`, `link.lock` and `link.log` in the state folder
(`~/.local/state/endeavor`, `%LOCALAPPDATA%\Endeavor` on Windows). The control
interface is HTTP on a loopback port of its own, with the token from
`link.json` as a bearer token. It refuses a Host that isn't loopback and any
request with an Origin.

| Call | What it does |
|---|---|
| `GET /link/status` | `state` (connecting, connected, starting, queued, ready, failed), the last `step`, an `error`, what the helper said (`hello`), the `runtime` once ready (the listener's port, the runtime's token, the page URL), and for a job its `job` and `queue` |
| `POST /link/start` | `{"job": …, "only_running": bool, "install": bool}`: start the runtime or attach to the one running, in the background. Returns the status at once. A start under way, or a runtime attached, is not an error. With `only_running` it attaches only if a runtime runs or a job waits, and else starts nothing: the state is `connected` and `nothing_running` is true. `install` is the user's agreement to what this start needs (the helper if missing, then whatever the start finds it needs). A front uses a link of its own control protocol number (`link::PROTOCOL`, in `link.json`; a missing number is 0) fully, whatever its build. To a link of another protocol it sends no start: it replaces one that nothing hangs on, and uses one with a runtime as it is |
| `POST /link/stop` | Stop the runtime for every client, and say why it didn't. The link stays connected |
| `POST /link/quit` | Remove the record, detach and exit. The record goes first, so a front that asks for a link right after gets a new one |

When the connection drops, the link connects again (waiting 1 s, then more,
up to 30 s apart, for 10 minutes; a name that doesn't resolve or a network
that is down is retried too) and attaches to the runtime it had, if the
helper says it is still running or its job still waits, on the same listener
port. A start that the drop cut short is taken up again the same way. A
runtime that is gone is not started again: the state is `failed` and says
so. A failed sign-in or host key is not retried, nor is giving up after 10
minutes: the state is `failed` with the message, the listener tells the agent
to call `use_machine`, and the next start tries again. A first connect that
fails is reported once and not retried. The link starts the runtime with
`--exit-idle`.

A front that finds a link whose process lives but doesn't answer waits a few
seconds, then reports that the link isn't answering. It doesn't start a second
one. A machine's id is lower-case letters, digits, `-`, `_` and `.`, since it
is the link's folder name on every system.

**How long things last.** These are separate:

- The link stays for 8 hours after the last agent session, so the browser
  page keeps working, then exits. It is the plugin's only; the app's
  connection lasts as long as the app runs.
- A link or an app that goes away detaches. It never stops the runtime.
- The runtime's idle stop is the core's, as today: 48 hours unless set
  otherwise, for the whole runtime, by whoever started it.
- A runtime the link starts also ends once no notebook has been open for that
  long (`endeavor connect --exit-idle`, like one `mcp` starts), so a runtime
  nobody uses doesn't stay up for good. Attaching to a running runtime
  changes nothing about it. On a cluster the flag goes to the core in the job,
  and the job's time limit ends it too.
- The link counts every control request as activity, and ends 8 hours after
  the last one. Pluto's page and the agent's calls through its port aren't
  control requests, so a front calls `status` while its session lasts.

**The token.** The helper sends the runtime's token when the runtime is
ready. The link keeps it in its state folder (readable only by you), and the
front adds it to each request, as it does for a local runtime.

**The session.** The front makes a session key for its run and sends it on
every request, with the server's name once it uses a server. The front
outlives a dropped `ssh`, so the agent keeps its notebook when the link
reconnects. When the session moves to another runtime, the front ends its key
on the old one and makes a new one (`<first key>-N`): a runtime ignores a key
it has ended, and a notebook binding means nothing on another runtime.

**What the front does with the link (built).** The front asks its link for its
status every four minutes while its target is a machine, so the 8 hours count
from the end of the last session. A call that finds the link gone starts it
again and attaches to what runs. A link of another control protocol than the front's is
used as it is, except that `use_machine` quits it and starts a new one when no
runtime is attached through it (the new link has another port, and an open
browser page would break); with a runtime attached it keeps the old link and the
result says so. When the front's input ends it ends its key on the runtime it is
on and leaves the link running.

**What a project remembers (built).** `projects.json` in the local state folder
(`<state home>/endeavor/`, next to `links/`; `%LOCALAPPDATA%\Endeavor` on
Windows) maps a project folder, the front's `--folder` as a canonical path, to
`{machine, folder}`: the machine's id and the folder there. It is written whole
and renamed, owner-only, under a lock, as `machines.json` is. `use_machine`
writes it, and `"local"` removes the entry. A front that starts in such a project
targets the machine and starts nothing; its first runtime call asks the link to
attach only to a runtime that is there (`only_running`). With none: a plain
server starts one, and a cluster submits nothing, so the call fails with a
message that names the machine, the defaults and `use_machine`. A machine that
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
- Your own `ssh -L` and browser tab keep working. The link (with the
  revision: the front) listens on a
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
own Stop or Detach. Not built: the app should detach instead of stopping
when `other_sessions` shows another session was active lately. (Changes
with the revision: there is no such list, and the app detaches when it
quits; the idle limit ends its notebooks.)

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
when you ask, such as to give a cluster node back. It first says which
other sessions were active lately (not with the revision, which has no such
list), and the clients still attached are told
the runtime was stopped from another connection. Every `Stop` and `StartRuntime` carries an id the client chooses, and the
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
do the same, and the first marks the stop as made from a connection.

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
- Quitting detaches, and stops the local runtime only when `other_sessions`
  shows no other session was active lately. (Changes with the revision:
  quitting only detaches.)
- It no longer hears "In use from another connection": nothing makes it
  exit. It can show a notebook's other sessions instead. (Not with the
  revision.)
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
    `link::Link`'s `status`, `start`, `install` and `attach` take the wait as
    their last argument (`link::CALL_WAIT` is the usual one).
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
- **Who else is there** (goes with the revision; only the time of a
  session's last call is kept). The core records each session's last call and a
  label its client sends ("Claude Code on jc-workstation"). `list_notebooks`
  and `pluto_session_status` show a notebook's other sessions and how lately
  each was active, so an agent can say that someone else is working there
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

New: the link process (built), the machine tools, and getting the helper to
send (built). The link sends its own binary when the server is the same
platform as your computer, and else fetches the release's helper for the
server's platform.

The app keeps its copy until it switches to the moved code. That is a change
in both repositories: it lands here, the Helpers release builds, then the
app moves its pin ([status.md](status.md)).

## What the user sees

**The first time on a server (built).** You say "use hoffman2 for this".

1. The agent calls `list_machines`: the machines you've added, and the `Host`
   names in `~/.ssh/config`.
2. It calls `add_machine("hoffman2")`. The link connects and looks. If the
   server lacks this build's helper it installs nothing: the result is
   `needs_install` (what would be copied, where, about how big, whether a
   runtime already runs there), nothing is saved yet, and the agent
   asks you. If you agree it calls again with `install: true`; the link
   sends the helper, and reports the machine's node and home folder,
   whether Slurm is there, and the partitions with their limits. That report
   becomes the saved record. Julia isn't looked for until the first runtime
   starts there (the helper has no call for it), so the report has it as null
   until then. A machine that turns out to have Slurm is saved as a cluster
   and its link is quit, so the next connection starts the helper for Slurm.
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
is not waited for. The time waited is not given: the link doesn't know when a
job was submitted by an earlier connection. (With the revision the front
holds the connection, and the same holds for it.)

**The notebook.** `browser_url` is on your computer's loopback and stays the
same while the link runs. (With the revision: while the session is
connected.)

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
| `stop_machine` | Stop the runtime there for every client; on a cluster, cancel the job. Says first who else was active (not with the revision). Needs this build's helper there, so it can ask to install it too |

`open_notebook` joins a notebook that is already open. `list_notebooks` and
`pluto_session_status` gain a notebook's other sessions and when each was
last active (these go with the revision), and the machine, the job and its
end time.

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
- When another session was active in your notebook lately, say so before
  you change it. (Goes with the revision.)
- Never ask for a password or passphrase, and never run `ssh` with one.
- On a server, files are there: use `list_folder`, `read_file` and
  `run_shell`. A project checked out on both machines has the same paths in
  both places, so your own file tools would read the local copy without an
  error.
- Tell the user the browser link, the queue state and when the job ends.

## Sign-in

The link (with the revision: the front) runs the system `ssh` with the alias, keepalives and batch mode.
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

Later, for the third case: a sign-in page on the link's loopback port (with
the revision there is no link; where the page lives is open), the
same on macOS, Linux and Windows. The answer goes from the page to `ssh` and
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
- `endeavor update` works on all five platforms (`release.rs`, which the link
  shares for downloads and checksums). On Windows the running exe is renamed
  to `endeavor.exe.old` and the new one put in its place; the next update
  deletes the old one.
- When the server's platform isn't this computer's, the link fetches
  `endeavor-<key>-<platform>` from the release, checks it against
  `endeavor-<key>.sha256` from the same release, and keeps it as
  `<cache>/endeavor/helpers/<key>/<platform>/endeavor` (`~/.cache` by
  default, `%LOCALAPPDATA%\Endeavor\helpers` on Windows; the folders 0700
  and the file 0600), with the checksum beside it. The next connect uses the
  kept file once its SHA-256 matches the one recorded, with no download. A
  build without a release key says that a helper for that platform needs a
  release build. A download that fails or doesn't match is deleted, the
  link's state is `failed` with the message, and nothing retries it. The
  release's names for platforms are in one place (`release::platform_name`);
  a server that reports another platform (or Windows) is refused with "no
  runtime helper for <os> <arch> servers". Servers are reached by `uname`'s
  words, so macOS servers are `darwin-*`.
- When the server is the same platform as this computer, the link sends its
  own binary, as before.

**Not run:** the PowerShell script (no PowerShell here), the workflow's new
rows (they run on the next push to `main`), `endeavor update` on macOS and
Windows, and a Mac or Windows computer reaching a Linux server. A file
fetched with `curl` carries no quarantine mark, so Gatekeeper and SmartScreen
shouldn't check it, and the Rust linker signs Apple Silicon binaries ad hoc.
Untested.

**Built, not run on a real agent:** the plugin's launcher, below.

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
replaced by `ENDEAVOR_BIN`. `codex-plugin/` and `antigravity-plugin/` are built
to what their documentation says and tried on no real install.

| | Claude Code | Codex | Antigravity |
|---|---|---|---|
| Manifest | `.claude-plugin/plugin.json` | `plugin.json` at the root | `plugin.json` at the root |
| MCP file | `.mcp.json` | `mcp.json` | `mcp_config.json` |
| Command | `sh ${CLAUDE_PLUGIN_ROOT}/launch/endeavor-mcp.sh …` | `sh ${PLUGIN_ROOT}/launch/endeavor-mcp.sh …` | `sh -c` that finds the plugin in `~/.gemini/antigravity-cli/plugins/endeavor` and runs the launcher |
| Skills | `skills/` (a link) | `skills/` (a copy) | `skills/` (a copy) |
| Download early | a `SessionStart` hook runs `--fetch-only` | none | none |
| Install | `claude plugin marketplace add`, `claude plugin install` | `codex plugin marketplace add` | `agy plugin install <folder>` |
| Project folder | `--folder ${CLAUDE_PROJECT_DIR}` | the folder `mcp` starts in | the folder `mcp` starts in |

Each plugin folder holds `launch/endeavor-mcp.sh`, `launch/install.sh` and
`launch/release-key`. The command is `sh` with the script as its argument, so
it doesn't depend on the file's execute bit.

One source for each part: `plugin/skills/` for the skills, `scripts/` for the
launcher, `install.sh` and `release-key`. `scripts/plugins.sh sync` writes the
copies and `check` fails if any differs; CI runs `check`. `claude-plugin/skills`
stays a link to `../plugin/skills`, which works there; the other two folders
hold copies, since those agents may not follow a link out of the plugin.
`release-key` is not in `plugin/`, because that folder is hashed into the
build's key.

**What the documentation says.** Codex
([developers.openai.com/plugins/build/plugins](https://developers.openai.com/plugins/build/plugins)):
`plugin.json` at the root, `skills/<name>/SKILL.md`, MCP servers in `mcp.json`
with `mcpServers`, `PLUGIN_ROOT` and `PLUGIN_DATA` for lifecycle hooks. It
shows no stdio entry, so the entry is by analogy with the HTTP one. Antigravity
([antigravity.google/docs/plugins](https://antigravity.google/docs/plugins)):
`plugin.json`, `mcp_config.json`, `hooks.json`, `skills/`, installed to
`~/.gemini/antigravity-cli/plugins/<name>/`. It gives no shape for
`mcp_config.json` or `hooks.json`, so the file uses the `mcpServers` shape the
others use, and no variable is assumed.

To check: that Codex expands `${PLUGIN_ROOT}` in an MCP entry, that Codex and
Antigravity accept the entries, start a plugin's server in the project folder,
and load the skills, and that Claude Code's `SessionStart` hook runs before the
server starts (if not, the first start downloads, as in the other agents).

## Windows

- The wire protocol needs one plain `ssh` and no connection sharing, which
  Windows OpenSSH lacks.
- With keys and batch mode, nothing in the first version is Unix-only except
  process groups and the kill used to cancel, which need Windows code. The
  app's Windows client can't connect today only because its password prompt
  doesn't start there.
- `serve` and `mcp` on Windows are built and untried on a real machine.

## Safety

- Nothing is installed on a server without the user's agreement. The link
  connects with installs not allowed (the bootstrap script only reports the
  platform, whether this build's helper is there, and, with `sh`, `cat`, `kill`,
  `ps` and `squeue`, whether a runtime or a job is recorded in the state folder
  the helper would use), and starts with `install` false. The
  agreement to the helper is for one link process: it lasts for its reconnects,
  and a new link starts without it, which is harmless since the helper is
  installed by then. (With the revision: for one session's connection.) The agreement to what a start needs (Julia's download) is
  for one start and is not kept. The app, which asks its user itself, passes
  `allow_install` true and starts with `install` true.
- Both ends stay on loopback. The link's port needs the token, as a
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
   notebook joins it, and the core shows a notebook's other sessions.
3. The link process (built) and the machine tools in `mcp` (built), with the
   `endeavor-machines` skill.
4. The Slurm path through the tools (built against fake Slurm, and checked on
   real Slurm: the helper's side in `e2e_slurm`, the whole path through
   `endeavor mcp` in `e2e_machines_slurm`).
5. macOS and Windows builds, the install scripts, the build's release key
   (built; the workflow's new rows and the PowerShell script are unrun, see
   [gaps.md](gaps.md)).
6. The plugin gets the binary itself: the launcher, the Claude Code plugin
   using it, Codex and Antigravity plugin folders (built to their documentation
   and unverified), the `endeavor-setup` skill, and `endeavor update` leaving a
   plugin's binary alone. The pinned key is set after the first release from
   `main`.
7. The revision of 2026-10-07, in the order given in
   [The revision](#the-revision-decided-2026-10-07-planned). It comes before
   the app, which then takes the library as revised.
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
