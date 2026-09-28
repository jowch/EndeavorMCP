# EndeavorMCP

Notes on separating the runtime core into its own product, EndeavorMCP, that
the app depends on and that other agents can use through a plugin. Nothing
here is decided or built except where marked. It builds on the core in
[runtime-core.md](runtime-core.md).

_Drafted 2026-09-28_

## Summary

The core refactor did the hard part: the core is a self-contained Rust
process with its own tests, and Julia sits behind a narrow adapter interface.
What's left depends on how far the product goes:

| Scope | What it takes | Rough effort |
| --- | --- | --- |
| Separate crate, same repo | Move the core's modules (`core`, `http`, `mcp`, `host_tools`, `notebooks`) into an `endeavor-mcp` crate and binary; the helper and app depend on it | ~1 week |
| Usable by other MCP clients | Setup from the server itself, a standard transport, session identity without the app, approval, a public control API, the skills (below) | +3–5 weeks |
| Own repository and releases | CI, signed macOS and Linux binaries, versioning, the app pinning a version | +1–2 weeks |
| Windows | The core uses Unix processes and signals throughout | several more weeks |

Estimates, not measurements.

## What stays in the app

The notebook pane and the page script (annotations, change highlights), run
cards, ssh, Slurm and the relay, sign-in and session management. The
notebook page is Pluto's own, served by Julia, so standalone users can open it
in a browser; the product needs a tool or command that gives its address.

The Pluto adapter, the marimo adapter and turtleR's adapter live with the
product, since each implements its interface. turtleR itself stays its own
package.

## Deployment: a plugin

EndeavorMCP ships most likely as a Claude Code plugin: the MCP server entry,
the skills (today's `plugin/skills/`) and hooks (see
[Approval](#approval)).

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
(`crates/endeavor-remote/src/julia.rs`). The product drops the download: with
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

Decided: move the bridge from MCP's SSE transport to Streamable HTTP, after
the core lands, whether or not the product happens.

- The MCP spec deprecated SSE in 2025; clients may drop it.
- Endeavor's side supports it: the ACP crate has an HTTP server type
  (`McpServerHttp`), and the pinned adapter advertises
  `mcpCapabilities: { http: true }`.
- It's simpler: the tools are request and reply, so each POST returns its
  JSON directly, with no per-session event queue or keepalive thread.
- The app and the core ship together (servers get the helper from the app
  bundle), so the app switches in the same change; no period with both.

For the plugin, a stdio mode is a thin shim that starts or attaches to the
long-running core (the helper's `runtime.json` and lock already do this), so
notebooks keep running between client sessions.

## Session identity

The core tells agent sessions apart by the `X-Endeavor-Session` and
`X-Endeavor-Host` headers the app puts in each session's MCP config.
Standalone, the default is one session per connection.

## Approval

A small change, about 1–2 days.

- **In the app**, nothing changes: its hook and run cards stay.
- **Claude Code with the plugin**: the plugin ships the same `PreToolUse`
  hook. The decision of which calls run code (`runs_code`, today in
  `src/gate.rs`) moves into the core binary (`endeavor-mcp hook-pretool`), so
  Claude Code asks before notebook code runs, in its own permission prompt.
  The hook's message can carry the run preview ("this reruns 3 cells").
- **Other clients** use their own per-tool approval, guided by the core's
  read-only markers.
- **Plan / ask / auto** gets a default from config or an environment
  variable, not only the app-only call.

Standalone users don't get the run card's detail (what reruns, the package
list); that's the app's interface.

## Open questions

- **Component or product.** A component the app depends on is about a week.
  A product other people install is mostly the setup, transport and public
  API work, about a month.
- **The control API.** The app-only calls (`/call` `endeavor/*`, and
  `/events` with authorship and previous code) are an internal protocol
  today, changed alongside the app. Either document and version them, or keep
  them as an Endeavor extension the product carries without promising
  stability.
