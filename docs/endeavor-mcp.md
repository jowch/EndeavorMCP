# EndeavorMCP

Notes on separating the runtime core into its own product, EndeavorMCP, that
the app depends on and that other agents can use through a plugin. Nothing
here is decided or built except where marked. It builds on the core in
[runtime-core.md](runtime-core.md).

_Drafted 2026-09-28_

_2026-10-03: the core, the helper, the runtime and the skills now live in this
repository. Endeavor depends on its crates as a Cargo git dependency, pinned
to a commit by Endeavor's Cargo.lock, and takes `runtime/` and the skills
from the crate (`endeavor_mcp::embedded`). The Helpers release (Linux
binaries) comes from here; signed macOS binaries and version tags don't exist
yet._

## Summary

The core refactor did the hard part: the core is a self-contained Rust
process with its own tests, and Julia sits behind a narrow adapter interface.
What's left depends on how far the product goes:

| Scope | What it takes | Rough effort |
| --- | --- | --- |
| Separate crate, same repo | Move the core's modules (`core`, `http`, `mcp`, `host_tools`, `notebooks`) into an `endeavor-mcp` crate and binary; the helper and app depend on it | ~1 week |
| Usable by other MCP clients | Setup from the server itself, a standard transport, session identity without the app, approval, a public control API, the skills (below) | +3–5 weeks |
| Own repository and releases | CI, signed macOS and Linux binaries, versioning, the app pinning a version | +1–2 weeks |
| Windows | The core's process control has Windows code, untried on a real machine, and a few stubs; see [windows.md](https://github.com/jowch/Endeavor/blob/main/docs/windows.md) | see [windows.md](https://github.com/jowch/Endeavor/blob/main/docs/windows.md) |

Estimates, not measurements.

## What stays in the app

The notebook pane and the page script (annotations, change highlights), run
cards, ssh, Slurm and the relay, sign-in and session management. The
notebook page is Pluto's own, served by Julia, so standalone users can open it
in a browser; the product needs a tool or command that gives its address.

The Pluto adapter, the marimo adapter and Ember's adapter live with the
product, since each implements its interface. Ember itself stays its own
package.

## Deployment: a plugin

EndeavorMCP ships most likely as a Claude Code plugin: the MCP server entry,
the skills (today's `plugin/skills/`), and nothing for approval: the core
asks before runs itself (see [Approval](#approval)).

- **The binary.** Plugins are git repositories, so a per-platform Rust
  binary needs a launcher that downloads the right prebuilt `endeavor-mcp` on
  first run, as `npx` and `uvx` servers do. Publishing it as an npm package
  run with `npx` is the alternative.
- **Startup.** MCP clients time out a server that's slow to start. The server
  answers the handshake at once and sets up in the background; tools report
  progress ("setting up Julia, step 2 of 3") until it's ready.

## Julia: the user's own

The product uses the user's Julia and packages instead of managing its own.
Most of this exists: the helper looks for a path the user gave, a setup line
of theirs (`module load julia`), then `julia` on the login shell's PATH
(juliaup installs), and only then downloads its pinned Julia
(`crates/endeavor-mcp/src/julia.rs`). The product drops the download: with
no Julia found, it says how to install one (juliaup).

- **Packages.** The runtime already uses its own package store first, with the
  user's `~/.julia` behind it read-only (`src/runtime.rs`), so registries and
  packages the user has are reused and nothing is written into `~/.julia`.
  Keep that: it stops our pins clashing with the user's environments.
- **First run** still installs and precompiles the pinned Pluto the adapter
  needs (about a minute), hence the background setup above.
- **Many Julia versions.** The runtime's `Manifest.toml` is resolved for one
  Julia. Julia 1.11 and later read a version-named manifest when one exists
  (`Manifest-v1.11.toml`, `Manifest-v1.12.toml`), so ship one per supported
  minor version and test each in CI. The helper already requires 1.11 or
  later.

## Transport: Streamable HTTP

The bridge speaks MCP's Streamable HTTP transport, not SSE:

- The MCP spec deprecated SSE in 2025; clients may drop it.
- Endeavor's side supports it: the ACP crate has an HTTP server type
  (`McpServerHttp`), and the pinned adapter advertises
  `mcpCapabilities: { http: true }`.
- It's simpler: the tools are request and reply, so each POST returns its
  JSON directly, with no per-session event queue or keepalive thread.

The core serves one endpoint, `POST /mcp` on the runtime's one port
([one-port.md](one-port.md)): a request's reply comes back in
the same response (`200`, `application/json`); a notification or a response
from the client gets `202` with no body. A call the runtime holds for the
user's answer is the exception: Claude Code gives up on a POST whose
response hasn't begun within 60 seconds, so once a call waits, its response
begins at once as an event stream (`text/event-stream`, which the spec
allows for a client that accepts it). Every 15 seconds the stream says the
call is still waiting: a `notifications/progress` when the request carried
`_meta.progressToken`, else an SSE comment. The reply is the stream's last
event. `GET /mcp` is `405` (no
server-initiated stream); so is `DELETE`, since a session lasts as long as the
runtime. The reply to `initialize` carries an `Mcp-Session-Id` only for a
client without `X-Endeavor-Session` (see
[Session identity](#session-identity)); the app's sessions get none, as
before. `MCP-Protocol-Version` is
honoured: an unsupported value is `400`; a missing header (before
`initialize`, or from a client that never sends it) falls back to what the
server understands. The app registers the bridge as `McpServer::Http`.

A runtime from before the switch to Streamable HTTP also predates one port
per runtime, which the helper doesn't attach to (it asks for a restart of
Julia instead), so the app no longer registers `McpServer::Sse` for any
runtime.

For the plugin, a stdio mode is a thin shim that starts or attaches to the
long-running core (the helper's `runtime.json` and lock already do this), so
notebooks keep running between client sessions.

The stdio form starts the local runtime at the first call that needs it, not
at launch. This computer's runtime is a `Provider` (`Local`, `src/standalone/target.rs`)
beside a machine's `Session`, behind one target type; both answer with a
`client::Outcome`. Only a `tools/call` of a tool this build has starts a runtime. A
session needs one for: every notebook tool and `keep_notebook_alive`, and
`use_machine` with `"local"`. It does not for `initialize`, `ping`, `tools/list`,
`notebook_guide` (the front answers it from the guide it embeds, with the
runtime's check that `arguments` is an object; on a machine the call goes to that
machine's runtime), the host tools (the front refuses them for a session on this
computer, with the runtime's text), `list_machines`, `add_machine`,
`stop_machine`, or a session whose project is on a machine. While the target's
runtime isn't up, a notification is dropped, a request that is not a
`tools/call` gets JSON-RPC -32601, and a `tools/call` of an unknown tool gets
the runtime's `unknown_tool` result; with a runtime up all of these are
forwarded as before. A notebook call that finds none running starts one, on this
computer and on a plain server alike; on a cluster it asks for a job instead.
`list_notebooks` and `pluto_session_status` use a runtime that is running (the
provider's `Want::Attach`: this computer's looks in the state folder, the
session's asks the helper; the session's folder is told to the runtime before
the call goes through, and the notebooks line is printed) and otherwise answer
without starting one: `[]`, and `{pluto: "not running", notebooks: [], message}`
(on a machine with `machine` added). A record that can't be used (another node,
no port, a process that doesn't answer) is a failure, not "not running": the
status tool answers with `state` `failed` and the reason, and `list_notebooks`
fails with it. A start another process has under way is a start under way, not
"nothing runs". Nothing running is never remembered: each such call looks again
(this computer reads the state folder, a machine's connection asks the helper), so
a runtime that another session or the app started since is found. The first call
that starts a runtime waits up to `start_wait()` (45 s), then fails with "Julia is
starting on this computer ... call `pluto_session_status`", and the start goes on in
a thread of the provider.

A failure is kept, the same on this computer and on a machine, and every call that
asks is told it (`Provider::ensure` takes `retry`; a start that failed is not
cleared by being told). Who asks to try again is decided in one place, `route`: a
notebook call that needs a runtime reports a failure it has not reported yet and
asks to try again on the call after that (`Target::failed`); `use_machine` and
`stop_machine` always ask; `list_notebooks` and `pluto_session_status` never do and
never use up the report, and the status tool's result for a failure carries
`error`. A runtime that has gone (its process ended, it exited when idle, another
connection stopped it or took it over) is just not running: the next notebook call
that needs one starts one on this computer and on a plain server, and a cluster
asks for a job. A runtime that ended while the connection to its machine was down
is different: that is a failure, kept with its reason, since nobody saw it end.
Whether to start is decided from the connection's own record of the machine (is it
a cluster), not from a copy made when the session was pointed at it.

The helper, `serve` and the front find or start the runtime with one function
(`runtime::find_or_start`), look at what is running with one (`runtime::look`)
and stop it with one (`runtime::end`). A start, once begun, finishes without the
process that asked for it. The core is its own session. As the first thing it
does it holds `starting.lock` in the state folder, and lets go only after it has
written `runtime.json`; the OS lets go if it dies. The process that spawned it
holds `start.lock` for the look and the spawn, until the core holds its own lock,
and waits for the runtime without it. A client that takes `start.lock` and finds
no usable record but `starting.lock` held waits for that runtime, and looks again
when the lock comes free (the record is written first, so "free and no record"
means that start died; it then starts its own). A client that waits never stops
the start of another process: a `Stop` to the helper, Ctrl-C in `serve`, the end
of input and `--quit-with-client` end only a start this process spawned. A stop
(`endeavor stop`, `stop_machine` for this computer, the helper's Stop for a
runtime it is not attached to) finds no record and `starting.lock` held, and
stops nothing: it says Julia is still starting and to try again once it is up.

## Session identity

_Built 2026-10-03._

The core tells agent sessions apart by a key. Each session works on one
notebook (the first it opens or creates, or the one the app gives it), and
`list_notebooks` marks that notebook `this_session`. Where the key comes from:

- **The app's sessions** send `X-Endeavor-Session` (and, on a server,
  `X-Endeavor-Host`) from their MCP config. Nothing else is involved.
- **The stdio form** (`endeavor mcp`) makes a key for each run
  (`stdio-<pid>-<millis>`) and sends it as `X-Endeavor-Session`, so each
  agent that starts it is one session.
- **A client over plain HTTP** (`endeavor serve`, with only the bearer token)
  gets an `Mcp-Session-Id` in the reply to its `initialize`
  (`mcp-` and 32 random hex digits). Streamable HTTP clients send it back on
  every request after, and the core uses it as the key. The core doesn't keep
  a list of the ids it issued: an id it doesn't know (after a restart of the
  runtime, say) starts a new session under that id rather than a `404`.
- **A request with neither header** is treated as the app's own calls are:
  no notebook of its own, so it is held to none and `this_session` is false
  everywhere. Only a client that skips the handshake (curl, a script) lands
  here.

So in every form, one agent connection is one session, and the skills'
rules hold as written: a notebook the agent created or opened is
`this_session` true; one another session created, or the user opened from
Pluto's page in the browser, is false.

Why the MCP session id and not something else:

- It is the transport's own way to name a session: the spec requires a
  client to send it back once issued, so the user configures nothing beyond
  the URL and the token. (Not yet checked live with Claude Code, Codex or
  Gemini CLI; `e2e_serve` sends it back as the spec says.)
- Issuing it only when `X-Endeavor-Session` is missing leaves the app's
  sessions exactly as they were.
- One key for every header-less client (all of them one session) was
  rejected: two agents on one runtime would share a notebook, and an agent
  would stay bound to an old notebook after it restarts.
- The TCP connection was rejected: clients open new connections freely
  (`serve`'s own tests use one per request).
- Marking every notebook `this_session` for a header-less client was
  rejected: it would tell the agent that the user's and other sessions'
  notebooks are its own.

A notebook the user already opened in the browser, or another session
opened, can become an agent's notebook: `open_notebook` on an open path binds
the session to it and returns it as it is, with `already_open` true. The core
catches the adapter's `notebook_already_open` error and builds the result from
a fresh snapshot, so nothing runs and safe preview is unchanged. The app's own
calls, which carry no session, get the same result. A session already bound
to another notebook is still refused (`one_notebook`).

Each session also leaves a record: when it last made a tool call (a held
`/endeavor/events` stream doesn't count). A session has a record from when it
is bound or makes a tool call. The record goes a week after the session's last
call (after it was bound, if it made none), with the binding; clearing a binding
leaves the record. Nothing else is kept about a session but the notebook it
works in and what it has read (for `stale_read` and `run_conflict`): there is
no sign-out, no label for the client, and no list of other sessions in
`list_notebooks` or `pluto_session_status`. A session whose agent has gone
just stops calling. A call still under way when it goes may bind it to a
notebook; that is forgotten a week after its last call like any other.

One thing reads the records besides forgetting: `stop_machine` asks the runtime
(`endeavor/recent_sessions`, with `owner` and `within_seconds`) how many
sessions other than `owner` that work in a notebook that is open made a tool
call within that time, and how long ago the latest did: `{count, active_seconds_ago}` (null when none). The question
records no call. `endeavor/end_session` is gone, as are `other_sessions` and
`active_seconds_ago` in the results of `list_notebooks` and
`pluto_session_status`, the `X-Endeavor-Client` header and the `other_session`
warning.

**Where the browser reaches the runtime.** The results that name a notebook or
the session (`new_notebook`, `open_notebook`, `pluto_session_status`) carry a
`browser_url`. A request to `/mcp` may send `X-Endeavor-Browser-Port: <port>`,
a port from 1 to 65535, and then the link is on `http://localhost:<port>`, on
any runtime. That is how a runtime reached through the front's connection (its
loopback port on the user's computer) gives a link that works there. Without the header, or
with one that isn't a port, only a runtime started by `serve` or `mcp` adds a
`browser_url`, on its own port, as before.

**How a runtime ends when idle.** A notebook with no activity for the idle
limit stops: 48 hours unless the caller sets another (`--idle-stop`,
`ENDEAVOR_IDLE_HOURS`, or `endeavor/set_idle_limit` while it runs; 0 never).
It is the same everywhere. A runtime a client starts in the background
(`mcp` on this computer, a session on a server or cluster) is also started to
end once no notebook has been open for that long (`ENDEAVOR_EXIT_IDLE`, which
`endeavor connect --exit-idle` sets); `serve` is not. `runtime.json` records
which it is as `"exits_when_idle": true|false`, written when the runtime is
ready; a record without it is from before and says nothing. The limit itself
is not in the file, since it can change: `pluto_session_status` carries
`idle_stop_hours` (the limit now in force; 0 for never, also for a negative
or non-finite one) and `exits_when_idle`. A runtime with `exits_when_idle` true
and `idle_stop_hours` 0 never ends. The app reads `runtime.json` and calls the runtime in the same way.

## The machine tools

_Built 2026-10-06, in `endeavor mcp` only._

`endeavor mcp` (the front) lists four tools that it answers itself and a
runtime doesn't have: `list_machines`, `add_machine`, `use_machine` and
`stop_machine` ([plugins-and-remote.md](plugins-and-remote.md)). Its
`tools/list` always has the host tools too, which refuse while the session is
on this computer. The schemas are `src/machine_tools.json`. `list_machines`
is read-only, the others are not. Each returns one JSON object as the text of
its result; a failure is `{error, message}` with `isError` true.

A session is on this computer or on one machine. On a machine the front sends
the runtime `X-Endeavor-Host: <name>` (so it lists and allows the host tools
for this session) and `X-Endeavor-Browser-Port: <the connection's port on this computer>`
(see [Session identity](#session-identity)), and gives it the session's folder
with `endeavor/set_session_folder` each time it attaches to a runtime there.
The session keeps its key when it moves to another runtime, and nothing is
told to the old one: a notebook binding is each runtime's own, so a session
starts with no notebook on a machine it has not worked on, and finds the
notebook it made on one it comes back to, if that notebook is still open (the
runtime still has the binding, unless it was restarted or the record was
forgotten after a week). A binding to a notebook that is no longer open, closed
or stopped for being idle, counts as none: it is dropped when it is found, by
`list_notebooks` or by a call that `one_notebook` would have refused, and the
session can make or open another. A
call's route (port, token, host) is taken from the target when the call is
made, so a call that races a move goes whole to the old runtime or whole to
the new one. `use_machine` does everything that can fail, and the request to
start or attach, first; only when the request was taken does the session move
and the project remember the machine. A call that fails, and `needs_job`,
leave the session and `projects.json` as they were.

Each machine tool call has one 45 s deadline from the moment it arrives. Waiting
for the one machine tool call that may run at a time and every wait for a machine
(`Session::ensure`'s `wait` is what is left) come out of it; a call that can't get
the lock in time says another call is still running and has changed nothing. A
wait that runs out is a result that says what step it is at and to call again.

| Tool | Arguments | Result |
| --- | --- | --- |
| `list_machines` | none | `machines`: each `{name, host, cluster, state, this_session}` (`state` is `not connected`, or, for a machine this session is connected to, the connection's: `connecting`, `connected`, `starting`, `queued`, `ready`, `failed`, `needs_install`, with `error`); `local` `{name, state, this_session}`; `this_session.machine`; `ssh_hosts_not_added`; `message`. Starts nothing and connects to nothing: a machine this process has no connection to is listed as saved, `not connected`, which says nothing about whether Julia runs there |
| `add_machine` | `host` (an ssh alias or `user@host[:port]`, through `valid_host`), `name`, `julia`, `slurm` (boolean), `install` (boolean, only after the user agreed; covers the helper only) | `{machine, host, state, saved, updated, node, home, os, arch, slurm, cluster, runs_in, partitions: [{name, default, max_hours, cpus, memory_gb}], scratch, found, message}`. `slurm` is whether Slurm was found; `cluster` and `runs_in` (`slurm_jobs` or `directly`) are what is used. `state` is `connecting` when 45 s ran out; call again. `needs_install` when this build's helper isn't on the machine and `install` wasn't given: `install` `{items: [{kind: "helper", name, size_mb, place}], os, arch, update, running}` (`size_mb` is null for a server of another platform than this computer's; `running` is `{process}` or `{slurm_job}` when a runtime is recorded and alive there, and `{process_recorded}` or `{slurm_job_recorded}` when it is recorded and alive but `ps` or `squeue` couldn't say more, else null), `saved` (true only for a machine added before; a new one is not saved), and a `message` that tells the agent to ask the user. `found` (`[{name, version, path}]`, Julia once a runtime has been started there) is empty until then |
| `use_machine` | `machine` (a name, or `"local"`), `folder`, `install` (boolean, only after the user agreed); on a cluster `partition`, `cpus`, `memory_gb`, `hours`, `gpus`, `account`, `extra_sbatch_flags` | `{machine, state, ready, message, …}`. `ready`: `browser_url` (works while this session is connected), `node`, `remote_port` (the runtime's own port on `node`; null when an older helper didn't say), `folder`, `already_running`, and for a cluster `job` `{id, summary, node, ends_at, ends_in_minutes}`. `starting`, `queued`: `step`, `queue` `{state, reason, reason_text}`, `job`. `needs_job`: a cluster with nothing running and no resources given; `defaults`, `partitions`; nothing was submitted and the session did not move. `needs_install`: the machine lacks this build's helper (an update when an older one is there), or what the start needs, such as Julia when none was found (`install` `{items: [{kind, name, size_mb, place}], …}`); nothing was installed and the session did not move; call again with `install: true` after the user agreed. One yes covers everything this call needs, the helper and then Julia if none is found; the helper's agreement from `add_machine` covers the helper only. `gpus` 0 is no GPU (it overrides and clears the saved default); each `extra_sbatch_flags` entry starts with `-`, and `--wrap` and line breaks are refused |
| `stop_machine` | `machine`, `force`, `install` (boolean, only after the user agreed) | `{machine, stopped, message}`; `needs_install` (`stopped` false) when the machine has only an older build's helper, since stopping needs this build's: the same `install` as above, and nothing was stopped; refused without `force` with `active_sessions` and `active_seconds_ago` (how many other sessions working in an open notebook made a tool call in the last 15 minutes, and how long ago the latest did); with `state` `starting`/`queued`, `job` and `queue` when Julia is starting or a job is queued (waiting sessions can't be seen); or as an error when the runtime doesn't answer the check for 5 s |

`stop_machine` with `"local"` takes `start.lock` first, as the helper's stop does,
and waits for it up to 20 s (`ENDEAVOR_STOP_LOCK_SECS` sets it for tests); the lock is
held only for a look and a spawn, so it does not wait behind a whole start. If the lock
isn't had it stops nothing and says so (an error). A start under way, this session's or
another process's, is not stopped: without `force` the result on a machine names what would be cancelled,
and with `force` it is an error that says Julia is still starting; on this computer even
the result without `force` only says Julia is still starting and to stop it once it
is up, since a stop never cancels a start there. It marks
the stop as made from a connection, so a client that finds the runtime gone is told "It
was stopped from another connection." `endeavor stop` waits for the lock in the same way
and keeps its own words ("It was stopped with `endeavor stop`.").

No call waits longer than 45 seconds (`ENDEAVOR_START_WAIT_SECS` sets it for
tests): the deadline is taken once where a call arrives (`Relay::handle`), and every
wait comes out of what is left of it, the attach, the start after it, the try again
after a connection that didn't answer, the check for other sessions in
`stop_machine` (at most 5 s) and the stop itself.

`stop_machine` marks the session stopped before it ends the runtime, under the
target's lock that `route` takes to check that and issue a start, so a start can't
come between; a stop that fails, even after the call answered "taking a while", puts
the session back. It uses the connection this front holds whatever the machine's
settings are now, so a runtime in use through old settings can be stopped. A notebook call while the target's runtime isn't up fails with a plain
message built from the `Outcome` of asking the connection; `pluto_session_status`
is answered by the front from it (`{machine, state, ready, step, error, queue,
job, message}`), and when the runtime is up it is relayed with `machine` (and for
a cluster `job`, and `remote_port`) added to its JSON. A queued job is not waited
for, and neither is a start: the status tool waits only a few seconds (10) for a
connection that was just made to say what is there.

**A server's page and the runtime's port.** `browser_url` is on a port of this
computer that the session's connection serves, so it works while the session is
connected and stops when the front ends; the runtime and its notebooks run on.
`remote_port` is the runtime's own port on the machine where it runs
(`ToApp::Ready`'s `port`, from the runtime's record; an older helper's answer has
none). On a plain server, `ssh -L <port>:127.0.0.1:<port> <host>` on the user's
computer reaches it with the same token, and `use_machine` says so in its message.
On a cluster the runtime is on a compute node behind the login node: the result
gives the node and port and promises no command.

**What a project remembers.** `<state home>/endeavor/projects.json`, which the
binary owns, maps a project folder (the front's `--folder`, canonical) to
`{machine, folder}`. It is written whole and renamed, owner-only, under a lock.
`use_machine` writes it once its request was taken; `"local"` removes the entry. A front that starts in a
project with an entry targets that machine and starts nothing. On its first
runtime call it makes a connection to that machine and attaches only to a
runtime that is already there (`Want::Attach`): a notebook call on a plain server then starts one
if none runs, `list_notebooks` and `pluto_session_status` start none, and a cluster submits nothing, and says so with the defaults to
ask the user about.
An entry whose machine is gone from the machines file is ignored, and the first
result says so once.

**The connection is a library type, and the front holds its own.**
`client::Session` (one for each machine) connects, starts the runtime or
attaches, retries and attaches again after a drop, and keeps one listener port
through all of it. `Session::ensure(Want, wait)` answers with an `Outcome`
(`Ready`, `Queued`, `NothingRunning`, `NeedsInstall`, `Failed`, `StillWorking`);
`status()` reads its fields.
Asked for what is already wanted or under way, `ensure` only waits, so any
number of callers can ask at once; `install: true` is an agreement and upgrades
a start that lacked it; an attach never replaces a start. A failure is kept and
given to every call until one passes `retry`, which begins again. An attach asks the machine
each time nothing is known to run. `close()` and
dropping the session detach from the helper and end its thread, and never stop
the runtime.

`endeavor mcp` holds one `Session` for each machine it uses, in its own process
(`Connections` in `src/standalone/machines.rs`, by machine id), made when a
tool or a runtime call first needs it: `use_machine`, `add_machine`,
`stop_machine`, or the first runtime call of a project that remembers a
machine. `open_session` is the one place that makes one (the helper to send,
the test variables below, the listener's words). When the front's input ends,
every session is closed: the helper is told to detach, and no runtime is
stopped. Each front has its own `ssh` for each machine, and the page's address
works only while its session is connected. `list_machines` shows the state of
the machines the front is connected to and connects to nothing.

What a machine tool says comes from the `Outcome`: `ready` (`RuntimeInfo`),
`queued`, `connected` (nothing running), `needs_install`, `failed`, or, when
the call's time ran out, `StillWorking(step)`, which is "still connecting" or
"still starting, call again". A failure that has settled stays what the notebook
calls and the status tool say until a call asks to try again (`use_machine`, or
the notebook call after the one that reported it).

`add_machine` connects with a `Held` that is not `saved`, in the same map as the
others, and marks it saved only after `machines.json` was written; an `Unsaved`
guard ends the connection on every way out that doesn't save the machine. A result
that says "call again" (still connecting, partitions not listed yet) leaves it for the
next `add_machine` with the same connection settings (`Server::same_connection`);
`use_machine` and `stop_machine` end it. A connection that is replaced by one with new
settings is ended at once, so a failed update leaves the machine's record as it was
and its connection to be made again by the next call. If the machine's settings change
(its address, port, Julia or whether it is a cluster) and a runtime is in use
through its connection, the change is refused in plain words; with nothing in
use the connection is replaced.

**Installing needs the user.** A connection is made with the helper's install
not allowed (`Config::allow_install` false), and starts without installing what
the start needs (`ToHelper::StartRuntime`'s `install` false). A machine without
this build's helper is then only looked at: the bootstrap reports its platform,
where the helper would go, whether a complete helper of another build is there,
and whether a runtime or a Slurm job is recorded in the state folder the helper
would use (`runtime.json`, and `job.json` on a cluster), read with `sh`, `cat`,
`tr`, `cut`, `kill`, `ps` and `squeue`. A process counts as running when it is
alive and its command looks like a core's (`ps -p PID -o args=` has `core` and
`--state-dir`), and as recorded when `ps` can't say; a job counts when `squeue`
lists it as pending, running or configuring, and as recorded when there is no
`squeue` or it didn't answer. The connect ends with `ConnectError::needs`, the
session's state is `needs_install` (not a failure, not retried by itself) and
the outcome is `NeedsInstall`. Nothing is fetched for the look: the size is
known only for this computer's own platform, and the helper for another is
fetched once the install is allowed.

There are two agreements. The helper's belongs to the connection:
`Config::allow_install` (what `add_machine` passes when it makes a connection
for `install: true`, so that it connects once), `Session::allow_install`, or a
`Want` with `install` while there is no connection allow it, and the session
keeps that for as long as it lives, so that a reconnect to a machine that lost
the helper asks nothing. What a start needs belongs to the start: a `Want::Start`
with `install: true` sends that one `StartRuntime` with `install` true (the same
for a job's `StartRuntime` on a cluster), and any other start sends false. A
helper that finds the start needs something (Julia, when none is found) then
answers `ToApp::NeedsInstall` with the items, and the session goes to
`needs_install` on the same connection. A session with no helper and a start
with `install: true` therefore installs the helper and then what the start
needs, in one call: one yes covers both. So `use_machine` without `install`
installs nothing a start needs, however the helper was agreed to, and a
remembered project's first notebook call never installs. The helper's agreement
from `add_machine` covers the helper only.

The items are `wire::Item`: `kind` (a plain string: `helper` for Endeavor's own
files, `runtime` for a language runtime such as Julia), `name` with its version,
`size_mb` and `place`, both optional. The front's question names every item
with its size and place, whatever its kind; a kind it knows adds a note (the
helper's platform, update and what runs there; the `julia` setting for a
runtime). A new engine that needs something answers with its own items and
needs no new message, library error or sentence. `StartRuntime` names the
engine (`wire::ENGINE_PLUTO`, the only one); the helper decides what that
engine needs at one place (`prepare` in `src/lib.rs`).

**Plain or cluster.** The user chooses whether Julia runs in Slurm jobs or
directly on the machine, since a host can have Slurm's tools without being a
cluster. `add_machine`'s `slurm` argument sets it: true needs Slurm there,
false runs directly, and left out a new machine takes what was detected and a
machine connected before keeps how it was saved. `add_machine` connects with the
machine's record and writes `machines.json` only after the connect has
succeeded: a call that is still connecting, that needs the helper installed, or
that fails leaves the file as it was, and the agent calls `add_machine` again
with the same arguments. An updated machine keeps its old record until the new
settings have connected. Changing a machine between the two is refused while
its connection has a runtime, a start or a job.

## Approval

A small change, about 1–2 days.

- **In the app**, nothing changes. The core already decides which calls run
  code (`runs_code` in `crates/endeavor-mcp`), and in Manual the calls
  that change the notebook (`asks_first`), and holds them until the app's
  card is answered (`asks` in `/endeavor/events`, `endeavor/answer_run`).
- **Standalone**, nothing in the core asks the user: a client without the
  app uses its own per-tool approval, guided by the core's read-only markers.
  Claude Code asks in its own permission prompt for any tool not allowed.
- **Plan / ask / auto** gets a default from config or an environment
  variable, not only the app-only call.

Standalone users don't get the run card's detail (what reruns, the package
list); that's the app's interface.

## Open questions

- **Component or product.** A component the app depends on is about a week.
  A product other people install is mostly the setup, transport and public
  API work, about a month.
- **The control API.** The app-only calls (`/endeavor/call` `endeavor/*`, and
  `/endeavor/events` with authorship and previous code) are an internal protocol
  today, changed alongside the app. Either document and version them, or keep
  them as an Endeavor extension the product carries without promising
  stability.
