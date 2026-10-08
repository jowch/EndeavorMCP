# One port per runtime

Steps 1–5 built and checked (2026-10-03). Step 5, the standalone command, is
`endeavor serve` (user guide: [serve.md](serve.md)). Each runtime exposes one port to clients
instead of two. The runtime core answers it, passes Pluto's page through
at `/`, and serves Endeavor's own endpoints under a reserved prefix. The
app's relay, listener and state then carry one route instead of two. A
standalone command (`endeavor serve`) can then print one link that a user
forwards with one `ssh -L`, the way Pluto or Jupyter is used on a
workstation today.

There are no users to move over, so there is no two-port fallback. An app
that meets an older runtime treats it as an older build (the build check)
and asks for a restart of Julia.

## Before this change

- Julia's Pluto server listens on `pluto_port`. The core listens on
  `mcp_port` (the bridge: MCP, `/events`, `/call`, `endeavor/*`). Both are
  in `runtime.json`.
- `crates/wire`'s `Target::{Pluto, Bridge}` picks one of the two for every
  relayed connection (`wire/src/relay.rs`, `endeavor-mcp/src/lib.rs`
  `Route::Local([pluto_port, mcp_port])`, `slurm.rs`).
- The app's host listener has two loopback ports per host
  (`src/runtime.rs`). The web view loads `pluto_url` with Pluto's
  `?secret=`; the agent and the app use the bridge with the bearer token.
- The app adds Endeavor's page script (Point, cards, status) to the web view
  itself (`with_initialization_script` in `src/main.rs`).
- The core's HTTP code (`endeavor-mcp/src/http.rs`) relays one request
  at a time. It has no WebSocket pass-through.
- [runtime-core.md](runtime-core.md) plans one more UI port per engine
  (`Target::NotebookUi(backend)`) for marimo and Ember.

## Design

**Routes on the one port:**

| Path | Goes to | Auth |
|---|---|---|
| `/mcp` | the core's MCP endpoint | bearer token header |
| `/endeavor/events`, `/endeavor/call` (JSON-RPC: `endeavor/set_policy`, `endeavor/answer_run`, …, `ping`, `tools/call`) | the core; the calls Julia answers (`endeavor/set_folder`, `endeavor/shutdown`) go on to Julia's `/call` | bearer token header |
| any other `/endeavor/…` | `404` | bearer token header |
| everything else (`/`, `/edit`, `/open`, `/static`, Pluto's WebSocket, …) | Pluto's private port, unchanged | token header, or the cookie |

Later engines get a prefix each (`/marimo/…`, `/ember/…`); Pluto stays at
`/` so it needs no base URL.

**One token.** The core makes it and accepts it two ways:
- as `Authorization: Bearer …`, from agents and the app;
- for browsers: `/?token=…` once, which sets an HttpOnly cookie and
  redirects to the page without the token in the URL.

The cookie opens Pluto's paths only, never `/mcp` or `/endeavor/…`.
Notebook outputs run their own JavaScript on the page's origin. With cookie
access to `/endeavor/answer_run`, a notebook could approve its own held
runs or change the policy. The page script gets what it needs from the app,
as today, not from those endpoints.

**Pluto's port becomes private, not gone.** It stays on loopback with its
own secret (`require_secret_for_access`), because other users on a shared
node can reach loopback ports. Only the core knows that secret and adds it
to what it passes through. The user never sees it.

**WebSocket pass-through.** When a request asks to upgrade, the core sends
it on to Pluto, and after Pluto's `101` it copies bytes both ways until
either side closes. This is the main new code.

**The page script for browsers** (later, optional). With the core in front
of Pluto, it could inject Endeavor's page script into Pluto's HTML for
browser users. The parts that talk to the app (Point to the chat, ⏎ on
cards, run cards) need the app, so the browser gets a reduced set or none.
Version 1 serves Pluto's page as is.

## What changes in Endeavor

- `runtime/boot.jl` and `EndeavorRuntime` `Lifecycle.jl`: Pluto on a private
  port the core picks; `runtime.json` records one public `port` (plus the
  private one for the core).
- The core (`crates/endeavor-mcp`): route by path, WebSocket pass-through,
  the cookie, Pluto's secret added on the way through.
- `crates/wire`: `Target` goes away (one route per connection); the frame
  format, relay and Slurm relay carry one port.
- App: one loopback port per host in the listener; the web view's URL,
  `close_page`/`blank_page` (which match the page's origin), and the guard
  (`guard.rs`) see one port; the bridge URL becomes `…/mcp` on that port.
- Docs: runtime-core (processes and the per-engine ports), remote-sessions,
  endeavor-mcp.

## As built

What came out differently from the design above, or wasn't settled by it:

- **Paths.** The app's calls stay JSON-RPC methods posted to one path,
  `/endeavor/call`; the event stream is `/endeavor/events`. `/health` is gone
  from the public port (nothing used it); Julia's own bridge still has one.
- **The cookie.** `?token=` on any of Pluto's paths answers `303 See Other`
  to the same URL without `token`, with `Set-Cookie: endeavor-<id>=<token>;
  Path=/; HttpOnly; SameSite=Strict`. `<id>` is the first 12 hex digits of
  the token's SHA-256, so each runtime has its own cookie: cookies ignore
  ports, so runtimes on 127.0.0.1 (several hosts in the app, several
  forwarded runtimes) share one cookie jar.
- **The cookie counts only from the page itself.** Since 127.0.0.1 with
  another port is the same site, another runtime's page could reach this
  port with this runtime's cookie (an `<img>`, a `fetch`, a WebSocket). So
  on Pluto's paths an `Origin` must be `http://<Host>`, and a
  `Sec-Fetch-Site` must be `same-origin` or `none`; anything else is `403`.
  Endeavor's own paths refuse any `Origin` and never take the cookie.
- **Pluto's secret** goes to Pluto as `Cookie: secret=…`, which Pluto
  accepts on plain requests and on the WebSocket upgrade alike. The core
  drops the client's `Cookie` and `Authorization` headers first, and drops
  Pluto's `Set-Cookie: secret=…` from what comes back, so the secret never
  reaches the browser. Requests keep the client's `Host`.
- **WebSocket.** After Pluto's `101` the core (`http::tunnel`) copies bytes
  both ways and closes both ends when either side closes. The app's guard
  (`guard.rs`, for a runtime from another build) does the same, and now sees
  every connection, Pluto's included; each request on a connection is still
  checked, and an upgraded connection was checked at its upgrade.
- **`runtime.json`** is `{launcher, node, pid, started, port, token, job, exits_when_idle}`.
  `started` is when the core's process started (a platform's own unit); a pid is
  trusted only while the process has that start time (and `boot`, the boot id, on
  Linux, where it counts ticks after boot). Both are absent from a record an older
  build wrote, and then the pid alone counts.
  Pluto's port and secret and Julia's bridge port stay in `julia.json`,
  between the core and Julia. Julia's side needed no change: it already ran
  Pluto and its bridge on ports the core picked.
- **A runtime from before this change** (`runtime.json` without `port`) is
  not attached to. `StartRuntime` fails with "Julia here was started by an
  older version of Endeavor, which this version can't connect to. Restart
  Julia to use it."; the host's check still shows it running; Stop ends it by
  its pid (on a cluster, cancels its job). There is no other fallback.
- **SSE is gone.** A runtime from before Streamable HTTP is also from before
  one port, so `McpTransport` and the app's `McpServer::Sse` registration
  went with the two ports.
- **The app.** One listener port per host. The web view loads
  `http://127.0.0.1:PORT/?token=…` (a notebook's page adds `/edit?id=…`);
  the agent's MCP URL is `http://127.0.0.1:PORT/mcp`; exports go with the
  bearer header. The URLs and token are the same for a host's next runtime,
  so the app tells runtimes apart by the core's pid. While a runtime is
  away, an MCP request is answered as before and anything else is closed.

## The standalone command

Planned as follows; what was built is under "As built" below.

- `endeavor serve` on a server or workstation starts or reuses the runtime
  in its state folder and prints the link (`http://localhost:PORT/?token=…`),
  the MCP URL and the token, and the `ssh -L PORT:localhost:PORT host` line.
  On a cluster the user runs it inside their own `salloc`/`sbatch`.
- A stdio form for plugin installs (Claude Code, Codex, Gemini) on the
  machine the agent runs on.
- Login, ssh, Duo and the tunnel are the user's or their agent's, not
  Endeavor's.
- The runtime holds no calls without the app; the agent's own permission
  prompts apply.
- The server-side file and shell tools (`host_tools.rs`) are turned on by a
  flag instead of only by the app's `X-Endeavor-Host` header, for an agent
  running on a laptop against a server's notebook.
- Packaging: the runtime binary, `runtime/` and the skills; Julia found as
  `julia.rs` does now, or downloaded.

### As built

User guide: [serve.md](serve.md). The binary is `endeavor`, in the
package `endeavor-mcp`; it was `endeavor-remote` until the rename.

- **Commands.** `endeavor serve` starts the runtime in the foreground,
  or uses the one running from its state folder, and prints the browser link,
  the MCP URL and header, configs for Claude Code, Codex, Gemini and generic
  JSON, and the `ssh -L` line (with `-J <SLURM_SUBMIT_HOST>` inside a Slurm
  job). Ctrl-C, SIGTERM or SIGHUP stops a runtime it started; one it reused
  keeps running. `endeavor mcp` is the stdio form. `endeavor
  stop` ends the runtime in the state folder. Code: `standalone.rs`.
- **No `--detach`.** A detached runtime needs a way to find and stop it, and
  `mcp` and `stop` already give that. For a terminal you leave, `tmux` or a
  batch job does what `--detach` would. Keeping `serve` in the foreground
  keeps one rule: the terminal that started Julia owns it.
- **Flags.** `--folder`, `--port`, `--julia`/`--julia-shell`, `--depot`,
  `--idle-stop`, `--host-tools` (serve only), `--skills plugin` (mcp only),
  `--state-dir`. Dropped: `--no-browser-hint` (the printout is the point) and
  `--detach` (above).
- **Defaults.** State in `~/.local/state/endeavor/serve/<host name>`
  (`$XDG_STATE_HOME`), per machine because cluster nodes share a home folder,
  and apart from the app's `~/.cache/endeavor/state`. The depot is the app's
  server depot, `~/.cache/endeavor/depot:` or `$SCRATCH/endeavor/depot:`, so
  packages are shared. Idle stop 48 hours, the app's default.
- **The runtime inside the binary.** `crates/endeavor-mcp/build.rs` embeds
  `runtime/` and names it `<package version>-<FNV hash of its files>`, the
  same walk and hash the app uses for server installs (`wire::tree`, shared by
  the build script through `#[path]`). It unpacks to
  `~/.cache/endeavor/serve/<version>/` (`$XDG_CACHE_HOME`): written to a
  `.part` folder, then renamed, so a folder by that name is complete and a
  second run leaves it alone. `scripts/helpers.sh`, `build-helpers.sh` and the
  Helpers workflow now count `runtime/` as helper source.
- **The core without the app.** The helper passes the settings in the core's
  environment (`ENDEAVOR_FOLDER`, `ENDEAVOR_PORT`, `ENDEAVOR_HOST_TOOLS`,
  `ENDEAVOR_IDLE_HOURS`, `ENDEAVOR_EXIT_IDLE`); none reach Julia or
  `run_shell`. With a folder the core is standalone (`mcp::Standalone`): it
  works in that folder, gives it to Pluto's page (`endeavor/set_folder`), uses
  it for any session the app gave no folder, records it in `runtime.json`,
  adds `browser_url` to `new_notebook`, `open_notebook` and
  `pluto_session_status`, and adds to its MCP instructions what differs
  without the app (`guide::STANDALONE`). `open_notebook` now resolves a
  relative path against the session's folder, in the app too.
- **No holds.** Policies come only from the app's `endeavor/set_policy`; a
  session without one never waits, so `no_app` can't happen standalone.
- **Sessions.** An agent over HTTP sends no `X-Endeavor-Session`, so the
  reply to its `initialize` gives it an `Mcp-Session-Id`, which is its
  session key from then on (2026-10-03; see
  [endeavor-mcp.md](endeavor-mcp.md#session-identity)). The stdio
  form sends its own session key and, once the runtime answers,
  `endeavor/set_session_folder` with its `--folder`, so agents in different
  projects share one runtime with their own folders.
- **The stdio relay.** It answers `initialize`, `ping` and `tools/list`
  itself, from the same code the core uses, so the handshake doesn't wait for
  Julia. Everything else goes to `/mcp`, one thread per message: a JSON
  answer is written as one line, an event stream event by event, a `202`
  not at all; `Mcp-Session-Id` and `MCP-Protocol-Version` are passed back. A
  call waits up to 45 seconds for a starting runtime, then fails with "still
  starting". A refused connection starts or finds the runtime again once.
- **Browser link for the agent.** In the tool results above rather than a
  new tool: the agent gets it at the moment it has something to show, without
  being told to ask. `mcp` also writes it to stderr.
- **Idle.** `--idle-stop` sets the notebooks' idle stop. A runtime `mcp`
  started also ends itself once no notebook has been open that long
  (`core::exit_when_idle`); one `serve` started stays until Ctrl-C.
  `runtime.json` says which (`exits_when_idle`).
- **Opening a notebook from disk** runs nothing, from the tools or from
  Pluto's start page (checked in `tests/e2e_serve.rs`); Pluto's
  **Run notebook code** runs it.
- **Packaging.** `claude-plugin/` is the Claude Code plugin: a copy of
  `plugin/skills` and an `.mcp.json` running `endeavor mcp --skills
  plugin --folder ${CLAUDE_PROJECT_DIR}`. `.claude-plugin/marketplace.json`
  makes the repository its marketplace. The app still loads `plugin/` alone,
  so it doesn't start that server. `cargo install --git
  https://github.com/jowch/EndeavorMCP endeavor-mcp` builds it (tried from a
  local clone of the branch).
- **`endeavor update`** replaces the binary with the newest Linux build on
  the Helpers release (`LATEST` names its key), checked against the
  release's SHA-256 file and renamed over the running binary. It leaves the
  app's copies and cargo installs alone, and says when a running Julia came
  from the older build (serve.md, "Update it").

## To check

Checked live on 2026-10-03 in a test copy of the app:

- This Mac, the OrbStack server and a Slurm job on the VM: the web view
  takes the cookie from the `303`; WebKit's `Origin` and `Sec-Fetch-Site`
  pass; Pluto's page loads in safe preview, Run notebook runs its cells over
  the WebSocket, and Claude's notebook tools answer through `/mcp`.
- Restart Julia on This Mac: the page reloads with no "lost authentication"
  alert and runs cells.
- A runtime from before this change (started on the VM from an older cached
  helper): the session shows "Julia here was started by an older version of
  Endeavor … Restart Julia to use it." Settings → Where notebooks run → Stop
  ends it, and Start then brings up a new runtime. The notebook pane offers
  "Restart Julia on <host>", which after a confirmation stops the old
  runtime and starts a new one.

Checked for step 5 on 2026-10-03:

- `tests/e2e_serve.rs` with real Julia on This Mac: `serve` in a temporary
  state folder and project folder, the agent over HTTP with only the bearer
  header, a new notebook in the folder, a cell run and read, a notebook from
  disk in safe preview through the tools and through Pluto's `/open`, the
  browser link's cookie and Pluto's page, Ctrl-C ending the core and Julia's
  process group; then `mcp` over stdio starting a runtime in the background,
  a second `mcp` with another folder sharing it, and `stop`.
- The OrbStack VM, a Linux binary from `scripts/build-helpers.sh --via
  endeavor-linux`: `serve --port 8456` found Endeavor's own Julia, and from
  this Mac through `ssh -L 18456:localhost:8456 endeavor-linux` (OrbStack
  already forwards 8456 itself) `initialize`, `new_notebook`, `add_cell`,
  `execute_cell`, `read_cell` (`55`), the `303` with the cookie and Pluto's
  page all worked; Ctrl-C left no process in Julia's group. Inside `sbatch`
  it printed the `-J` line, and `scancel` stopped Julia.

Still open:

- A browser on Pluto's WebSocket through `ssh -L` (curl reached the page; the
  WebSocket is covered by `e2e_julia` on loopback).
- The Claude Code plugin installed from the marketplace, and Codex and Gemini
  against `serve` and `mcp`: configs written from their documentation, not run.

## Order

1. Core: routes, WebSocket pass-through, private Pluto port and secret;
   tests against a real Pluto. Built.
2. Wire and helper: one port, `Target` removed; relay and Slurm relay. Built.
3. App: one listener port, page URL, bridge URL, guard. Built.
4. Live check: done (This Mac, the OrbStack server, the Slurm VM).
5. `endeavor serve`, `mcp` and `stop`. Built and checked.
