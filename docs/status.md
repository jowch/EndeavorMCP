# Status

Where EndeavorMCP stands, and what is open. Updated 2026-10-03, when this
repository was split out of [Endeavor](https://github.com/jowch/Endeavor).

## Where things stand

- The notebook runtime, its MCP server and the Pluto skills live here. The
  Endeavor app depends on `wire` and `endeavor-mcp` as a Cargo git
  dependency pinned in its `Cargo.lock`, and takes `runtime/` and `plugin/`
  from the crate's embedded copies (`endeavor_mcp::embedded`).
- The binary is `endeavor` (package `endeavor-mcp`). It is the app's helper
  on servers (`endeavor connect`, `endeavor core`), the standalone command
  (`endeavor serve`, `endeavor mcp`, `endeavor stop`, see the
  [README](../README.md)), and the Slurm relay.
- One port per runtime is built and checked live on a Mac, a Linux server
  and a Slurm job ([one-port.md](one-port.md)).
- CI (`ci.yml`) builds and tests on Linux, macOS and Windows. `helpers.yml`
  publishes the Linux binaries to the `helpers` release under a key computed
  from the helper's source (`scripts/helpers.sh`).

## Changing this repository and the app together

Endeavor pins a commit of this repository. A change the app needs lands here
first; then Endeavor moves its pin (Endeavor's
[docs/testing.md](https://github.com/jowch/Endeavor/blob/main/docs/testing.md),
"Changing EndeavorMCP and the app together"). Push here first and wait for
the Helpers release before pushing Endeavor, or Endeavor's builds fail
(missing commit) and its servers get no helper (missing release).

Endeavor's `tests/helper_mode.rs` expects this crate's version to equal the
app's (both 0.1.0). Bump them together.

## Open

Not yet checked:

- The Claude Code plugin installed from this repository's marketplace
  (`claude-plugin/`).
- Codex and Gemini against `endeavor serve` and `endeavor mcp`. Their config
  lines in the README come from each tool's documentation.
- Pluto's page in a browser through an `ssh -L` tunnel. curl reached the
  page; the WebSocket is only tested on loopback (`e2e_julia`).
- The app sending the bundled macOS helper to a real macOS server.

Known gaps:

- `endeavor update` is planned, not built (README, "Planned: an update
  command").
- The skills still describe the app's pane and approval cards. The
  standalone MCP instructions (`guide::STANDALONE`) tell the agent to skip
  those parts; the skills themselves aren't adapted.
- Over plain HTTP (no `X-Endeavor-Session`), `this_session` is false for
  every notebook.
- On Windows, `endeavor serve` doesn't catch Ctrl-C, so Julia keeps running
  until `endeavor stop`.
- Unpacked runtime and plugin folders (`serve`'s cache, the app's
  `runtime-files/` and `plugin/`) are kept per version and never removed.
- The warning "field `started` is never read" in `lib.rs` (read only on
  Windows).

Next:

- Ember (R notebooks) is in development in its own repository. It joins as
  another adapter behind the core, with its own path prefix on the one port
  ([runtime-core.md](runtime-core.md), [one-port.md](one-port.md)).
- marimo after Ember.
