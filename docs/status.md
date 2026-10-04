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

Checked live on 2026-10-03:

- Pluto's page in a browser through an `ssh -L` tunnel. `endeavor serve
  --port 41873` ran in the OrbStack Linux VM, and headless Chromium on the
  Mac went through `ssh -L 51873:localhost:41873`: the token link set the
  cookie, Pluto's WebSocket connected, and a cell typed and run in the page
  showed its result. The local port differs from the runtime's because
  OrbStack already forwards the VM's ports to the Mac's `localhost`, which
  would have skipped ssh.
- The Claude Code plugin, installed with the README's two `claude plugin`
  lines into an empty `CLAUDE_CONFIG_DIR`. Claude Code lists the three
  skills, the `skills` symlink is copied as a folder, `${CLAUDE_PROJECT_DIR}`
  becomes the project folder, and the `endeavor` server connects with its
  tools and no `notebook_guide`. The same `endeavor mcp --skills plugin`
  command, driven over stdio, made a notebook and ran a cell (`42`). No
  model turn was run, because the empty config isn't logged in.
- The app sending its bundled macOS helper to a macOS server, with this Mac
  as the server over `ssh localhost`. The app picked
  `Resources/helpers/darwin-aarch64/endeavor`, the server's copy matched it
  byte for byte, Julia started through it, and `list_notebooks` answered
  over `/mcp`. Endeavor's ignored test
  `remote::tests::a_real_server_gets_its_helper_and_runs_julia` repeats it.

Not yet checked:

- Codex and Gemini against `endeavor serve` and `endeavor mcp`. Their config
  lines in the README come from each tool's documentation.
- An agent following the skills without the app. They mark which parts
  hold in the app and which without it, and `guide::STANDALONE` only names
  the setting; no agent session has run against that text yet.

Known gaps:

- `endeavor update` is planned, not built (README, "Planned: an update
  command").
- Over plain HTTP (no `X-Endeavor-Session`), `this_session` is false for
  every notebook.
- `endeavor stop` from another terminal ends a `serve` that started Julia
  with status 1 and "Julia stopped. Its log is …", the same as when Julia
  crashes.
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
