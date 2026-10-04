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

Not done: a notebook the user already opened in the browser can't become an
agent's notebook, because `open_notebook` on an open path is an error
(`notebook_already_open`) and binds nothing. The pluto-session skill tells
the agent, without the app, to work in such a notebook by its `notebook_id`
when the user names it and the session has no notebook yet (the core holds an
unbound session to nothing), and otherwise to suggest a new agent session.

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
