# One port per runtime

Steps 1–3 built (2026-10-03); the live checks (step 4) and `endeavor serve`
(step 5) are not done yet. Each runtime exposes one port to clients
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
  relayed connection (`wire/src/relay.rs`, `endeavor-remote/src/lib.rs`
  `Route::Local([pluto_port, mcp_port])`, `slurm.rs`).
- The app's host listener has two loopback ports per host
  (`src/runtime.rs`). The web view loads `pluto_url` with Pluto's
  `?secret=`; the agent and the app use the bridge with the bearer token.
- The app adds Endeavor's page script (Point, cards, status) to the web view
  itself (`with_initialization_script` in `src/main.rs`).
- The core's HTTP code (`endeavor-remote/src/http.rs`) relays one request
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
- `endeavor-remote` core: route by path, WebSocket pass-through, the cookie,
  Pluto's secret added on the way through.
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
- **`runtime.json`** is `{launcher, node, pid, started, port, token, job}`.
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

Built on top, once the above works:

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

## To check

These need the app (step 4): This Mac, the OrbStack server, the Slurm VM.

- The web view takes the cookie from the `303` and sends it back: a
  `SameSite=Strict` cookie on `127.0.0.1`, and WebKit's `Sec-Fetch-Site`
  and `Origin` on the page's own requests and WebSocket are what the core
  allows.
- A runtime from before this change: the app shows the "older version"
  message, and Restart Julia (or Stop) from there ends it and starts a new one.
- Every request Pluto's page makes stays under paths the core passes
  through untouched (no absolute URLs to another host or port). Expected,
  since Pluto runs behind proxies; a real notebook in the web view and in a
  browser settles it.
- The web view keeps working when the page and the bridge share one origin
  (cookies, the page's own secret in `annotate.rs`).
- Pluto's "lost authentication" alert when a runtime restarts (see
  `close_page`) still behaves with the cookie in front.

## Order

1. Core: routes, WebSocket pass-through, private Pluto port and secret;
   tests against a real Pluto. Built.
2. Wire and helper: one port, `Target` removed; relay and Slurm relay. Built.
3. App: one listener port, page URL, bridge URL, guard. Built.
4. Live check: This Mac, the OrbStack server, the Slurm VM.
5. `endeavor serve` and the stdio form.
