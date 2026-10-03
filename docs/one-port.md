# One port per runtime

Planned (2026-10-03), not built. Each runtime exposes one port to clients
instead of two. The runtime core answers it, passes Pluto's page through
at `/`, and serves Endeavor's own endpoints under a reserved prefix. The
app's relay, listener and state then carry one route instead of two. A
standalone command (`endeavor serve`) can then print one link that a user
forwards with one `ssh -L`, the way Pluto or Jupyter is used on a
workstation today.

There are no users to move over, so there is no two-port fallback. An app
that meets an older runtime treats it as an older build (the build check)
and asks for a restart of Julia.

## Today

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
| `/endeavor/…` (events, call, set_policy, answer_run, …) | the core | bearer token header |
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
   tests against a real Pluto.
2. Wire and helper: one port, `Target` removed; relay and Slurm relay.
3. App: one listener port, page URL, bridge URL, guard.
4. Live check: This Mac, the OrbStack server, the Slurm VM.
5. `endeavor serve` and the stdio form.
