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
`/endeavor/events` stream doesn't count) and a label for its client. The label
is the `X-Endeavor-Client` header, else, for a client that got an
`Mcp-Session-Id`, the `clientInfo.name` from its `initialize`. It is cut to
printable characters, trimmed and at most 80 long. `endeavor mcp` sends
"<agent's name, else endeavor mcp> on <host name>". `list_notebooks` and
`pluto_session_status` give each notebook `other_sessions`: an entry for each
other session bound to it, `{client, active_seconds_ago}`, with null for
what isn't known. A session has a record from when it is bound, makes a
tool call or is given an id, and only sessions with a record are listed.
The record goes when the binding is cleared, and a week after the session's
last call (after it was made, if there was none). `endeavor mcp` sends
`endeavor/end_session` with its key when the agent's input ends. The core
unbinds the session and drops its record, folder and run policy, and from
then on a call of that session still under way binds nothing and leaves no
record. The key is unique to the process, so an ended session never returns;
the core forgets that it ended a week later. A front that is killed can't
send it, so its session drops out after the week.

**Where the browser reaches the runtime.** The results that name a notebook or
the session (`new_notebook`, `open_notebook`, `pluto_session_status`) carry a
`browser_url`. A request to `/mcp` may send `X-Endeavor-Browser-Port: <port>`,
a port from 1 to 65535, and then the link is on `http://localhost:<port>`, on
any runtime. That is how a runtime reached through a link (its loopback port
on the user's computer) gives a link that works there. Without the header, or
with one that isn't a port, only a runtime started by `serve` or `mcp` adds a
`browser_url`, on its own port, as before.

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
for this session) and `X-Endeavor-Browser-Port: <the link's runtime port>`
(see [Session identity](#session-identity)), and gives it the session's folder
with `endeavor/set_session_folder` each time it attaches to a runtime there.
When the session moves to another runtime the front ends its key on the old
one (`endeavor/end_session`) and makes a new key, since a runtime that has ended
a key ignores it afterwards, and a notebook binding means nothing on another
runtime. A session starts with no notebook on the machine it moves to. A call's
route (port, token, host) and the session's key are taken together and a move
replaces both together, so a call that races a move goes whole to the old
runtime or whole to the new one. `use_machine` does everything that can fail,
and the request to start or attach, first; only when the request was taken does
the session move, its old key end and the project remember the machine. A call
that fails, and `needs_job`, leave the session, its key and `projects.json` as
they were.

Each machine tool call has one 45 s deadline from the moment it arrives. Waiting
for the one machine tool call that may run at a time, finding or starting the
link and every call to it come out of it; a call that can't get the lock in time
says another call is still running and has changed nothing.

| Tool | Arguments | Result |
| --- | --- | --- |
| `list_machines` | none | `machines`: each `{name, host, cluster, state, this_session}` (`state` is `no link running` or the link's: `connecting`, `connected`, `starting`, `queued`, `ready`, `failed`, `needs_install`, with `error`); `local` `{name, state, this_session}`; `this_session.machine`; `ssh_hosts_not_added`; `message`. Starts nothing |
| `add_machine` | `host` (an ssh alias or `user@host[:port]`, through `valid_host`), `name`, `julia`, `slurm` (boolean), `install` (boolean, only after the user agreed) | `{machine, host, state, saved, updated, node, home, os, arch, slurm, cluster, runs_in, partitions: [{name, default, max_hours, cpus, memory_gb}], scratch, julia, message}`. `slurm` is whether Slurm was found; `cluster` and `runs_in` (`slurm_jobs` or `directly`) are what is used. `state` is `connecting` when 45 s ran out; call again. `needs_install` when this build's helper isn't on the machine and `install` wasn't given: `install` `{what: "helper", os, arch, folder, size_mb, update, running}` (`running` is `{process}` or `{slurm_job}` when a runtime is recorded and alive there, else null), `saved` true (not added), and a `message` that tells the agent to ask the user. Julia is null until a runtime has been started there once |
| `use_machine` | `machine` (a name, or `"local"`), `folder`, `install` (boolean, only after the user agreed); on a cluster `partition`, `cpus`, `memory_gb`, `hours`, `gpus`, `account`, `extra_sbatch_flags` | `{machine, state, ready, message, …}`. `ready`: `browser_url`, `node`, `folder`, `already_running`, and for a cluster `job` `{id, summary, node, ends_at, ends_in_minutes}`. `starting`, `queued`: `step`, `queue` `{state, reason, reason_text}`, `job`. `needs_job`: a cluster with nothing running and no resources given; `defaults`, `partitions`; nothing was submitted and the session did not move. `needs_install`: the machine lacks this build's helper (an update when an older one is there), or no Julia was found and Endeavor would download its own (`install` `{what: "julia", detail}`); nothing was installed and the session did not move; call again with `install: true` after the user agreed. `gpus` 0 is no GPU (it overrides and clears the saved default); each `extra_sbatch_flags` entry starts with `-`, and `--wrap` and line breaks are refused |
| `stop_machine` | `machine`, `force` | `{machine, stopped, message}`; refused without `force` with `other_sessions` `[{client, active_seconds_ago, notebook}]` when another session was active in the last 15 minutes; with `state` `starting`/`queued`, `job` and `queue` when Julia is starting or a job is queued (waiting sessions can't be seen); or as an error when the runtime doesn't answer the check for 5 s |

`use_machine` and `pluto_session_status` use the link's own words for what is
going on, and no call waits longer than 45 seconds (`ENDEAVOR_START_WAIT_SECS`
sets it for tests). A notebook call while the target's runtime isn't up fails
with a plain message built from the link's status; `pluto_session_status` is
answered by the front from the link's status (`{machine, state, ready, step,
error, queue, job, message}`), and when the runtime is up it is relayed with
`machine` (and for a cluster `job`) added to its JSON. A queued job is not
waited for.

**What a project remembers.** `<state home>/endeavor/projects.json`, which the
binary owns, maps a project folder (the front's `--folder`, canonical) to
`{machine, folder}`. It is written whole and renamed, owner-only, under a lock.
`use_machine` writes it once its request was taken; `"local"` removes the entry. A front that starts in a
project with an entry targets that machine and starts nothing. On its first
runtime call it asks the link to attach only to a runtime that is already
there (`only_running`): a plain server then starts one if none runs, and a
cluster submits nothing, and says so with the defaults to ask the user about.
An entry whose machine is gone from the machines file is ignored, and the first
result says so once.

**The link's `only_running`.** `POST /link/start` takes `{"job": …,
"only_running": true}`. After connecting, the link asks the helper whether a
runtime runs or a job waits (`Request::Runtime`, as a reconnect does). If so it
attaches as for any start. If not it starts nothing, and the status is
`connected` with `nothing_running` true until the next start.

**The link and the front.** While its target is a machine the front asks the
link for its status every four minutes (`ENDEAVOR_FRONT_PING_SECS` for tests),
which counts as activity, so the link's 8 hours run from the end of the last
session. A link that is gone is started again by the next call and attached to
whatever runs. A link of another build than the front's is quit and started
again only when no runtime hangs on it (a new link has a new port, and the
browser's page would break); otherwise it is kept and the result says so. One
function applies that rule to every link a front gets, for the tools and for a
notebook call, before any start or attach is sent; a link of another build that
is kept is sent no start, since it may not know `only_running` and would start
what was only to be attached to. The link refuses a start request with a field
it doesn't know (HTTP 400).

**Installing needs the user.** The link connects with installs not allowed
(`client::Options::allow_install` false). A machine without this build's helper
is then only looked at: the bootstrap reports its platform, where the helper
would go, whether an older build is there, and whether a runtime (a `process`
whose pid answers `kill -0`) or a Slurm job is recorded in the state folder
the helper would use (`runtime.json`, and `job.json` on a cluster), read with
`sh`, `cat`, `tr` and `cut`. The connect ends with `ConnectError::needs`, the
link's state is `needs_install` (not a failure, not retried by itself) with
`needs_install` in its status. `POST /link/start` with `"install": true`, or
`POST /link/install`, is the agreement: the link connects again with installs
allowed, and keeps that for as long as it runs, reconnects included. The helper
is also started with `--no-julia-download`; a start that finds no Julia then
fails with a message that starts with `julia::NOT_FOUND`, and the link goes to
`needs_install` with the download's size and place, until `install: true`
makes it drop the connection and connect again to download. A reconnect after
the agreement asks nothing. `use_machine` passes `install` on to the link;
a remembered project's first notebook call never does.

**Plain or cluster.** The user chooses whether Julia runs in Slurm jobs or
directly on the machine, since a host can have Slurm's tools without being a
cluster. `add_machine`'s `slurm` argument sets it: true needs Slurm there,
false runs directly, and left out a new machine takes what was detected and a
machine connected before keeps how it was saved. A machine whose first
`add_machine` was still connecting is saved but marked (an empty `provisional`
file in the link's folder, removed when a call has connected): a later
`add_machine` treats it as new, a failure removes it, `list_machines` shows it
as `not yet connected`, and `use_machine`, `stop_machine` and a project that
remembers it refuse it. Changing a machine between the two is refused while
the link has a runtime, a start or a job.

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
