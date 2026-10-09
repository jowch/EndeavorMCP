# Status

Where EndeavorMCP stands, and what is open. Updated 2026-10-08. This
repository was split out of [Endeavor](https://github.com/jowch/Endeavor) on
2026-10-03.

## Where things stand

- The notebook runtime, its MCP server and the skills live here. The
  Endeavor app depends on `wire` and `endeavor-mcp` as a Cargo git
  dependency pinned in its `Cargo.lock`, and takes `runtime/` and `plugin/`
  from the crate's embedded copies (`endeavor_mcp::embedded`).
- The binary is `endeavor` (package `endeavor-mcp`). It is the app's helper
  on servers (`endeavor connect`, `endeavor core`), the standalone command
  (`endeavor serve`, `endeavor mcp`, `endeavor stop`, see the
  [serve.md](serve.md)), and the Slurm relay.
- The connection library (`client::Session`) is built and tested with a local
  shell for ssh (`tests/session.rs`) and, over real ssh and Julia, in
  `e2e_client` ([plugins-and-remote.md](plugins-and-remote.md)). There is no
  background process for a server. The machine tools in
  `mcp` (`list_machines`, `add_machine`, `use_machine`, `stop_machine`), what a
  project remembers (`projects.json`) and the `endeavor-machines` skill are
  built and tested with a local shell for ssh, a stand-in Julia and fake
  Slurm (`tests/machines.rs`), and over real ssh and Julia in `e2e_machines`.
- One port per runtime is built and checked live on a Mac, a Linux server
  and a Slurm job ([one-port.md](one-port.md)).
- CI (`ci.yml`) builds and tests on Linux, macOS and Windows. `helpers.yml`
  publishes the binaries for Linux, macOS and Windows to the `helpers` release
  under a key computed from the helper's source (`scripts/helpers.sh`), and
  `LATEST`, the newest key, for builds from `main`. Every build gets that key
  (`ENDEAVOR_RELEASE_KEY`). All five platforms were published for the first
  time on 2026-10-08 (key `ed283c702b22`).
- `scripts/install.sh` (tested against a fake release and the public one) and
  `scripts/install.ps1` (not run) install the newest build.
- `endeavor update` replaces the binary from the Helpers release with the
  newest build on Linux, macOS and Windows (tested for Linux's logic and the
  Windows rename on Linux; not run on a Mac or Windows). `endeavor --version`
  prints the version and a build hash of the source, on a release build a
  line `release <key>`, and last a line `interface <n>`, the number for what
  its core offers callers (`core::INTERFACE`). A runtime of another build is
  used as it is when it offers this build's interface. `serve`, `mcp` and
  `endeavor update` say when the running Julia doesn't (`update` reads the new
  binary's number; it compares builds with a binary from before the line was
  printed). A machine's runtime is checked too, since 2026-10-09: the helper's
  `Ready` carries its build and interface, the front tells the agent once and
  never stops it, `pluto_session_status` says it every time (`other_version`,
  since 2026-10-09), and `client::Session` reports it as trouble (serve.md,
  "Update it"; endeavor-mcp.md, "A runtime from another build").
- A Mac or Windows computer reaches a Linux server: `endeavor mcp` fetches the
  release's helper for the server's platform by the build's key, checks its
  SHA-256 and keeps it (tested against a fake release; the wiring is
  read, not run across platforms).

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
- The Antigravity plugin on Windows 10 (agy 1.3.2, 2026-10-09), installed
  from its GitHub folder URL with Git's `bin` on the PATH and Julia from
  juliaup. agy loaded the three skills, the launcher downloaded the pinned
  release, and `agy -p` made a notebook and read a cell's output (`5050`).
  It needed `COMPUTERNAME` set to the DNS host name's case
  ([gaps.md](gaps.md), Windows).
- The app sending its bundled macOS helper to a macOS server, with this Mac
  as the server over `ssh localhost`. The app picked
  `Resources/helpers/darwin-aarch64/endeavor`, the server's copy matched it
  byte for byte, Julia started through it, and `list_notebooks` answered
  over `/mcp`. Endeavor's ignored test
  `remote::tests::a_real_server_gets_its_helper_and_runs_julia` repeats it.

Not yet checked:

- The machine tools against a real cluster (`e2e_slurm` covers the helper
  and Slurm, `tests/machines.rs` the tools with fake Slurm). On 2026-10-07 an
  agent drove the machine tools through `add_machine`, `needs_install`,
  `use_machine` and switching back; the `endeavor-machines` skill was loaded
  by name in one run only, and no agent has followed it through a cluster.

- `endeavor update` replacing a binary with a newer build from the real
  Helpers release, and on macOS and Windows. The first build with `update` is
  the newest one, so nothing older can update yet. On 2026-10-04 the install steps (now in serve.md)
  fetched build `14eb0a67bda7` through `LATEST` in the Linux VM, the
  checksum passed, and `endeavor update` said it is up to date. The replace
  step is tested against a local server.
- The check of a runtime from another build against a real Slurm job. On
  2026-10-09 a real Julia started by a build from before the interface number
  was checked by `mcp` on the same computer and on a machine over real ssh
  (localhost), with the current wording and `other_version`: the agent was
  told once, the status said it on every call, and the runtime kept running.
  Not yet against a job, and `endeavor update` reading the new binary's
  interface only against a local stand-in.
- Gemini against `endeavor serve` and `endeavor mcp`, and Codex against
  `serve`. Their config lines in serve.md come from each tool's documentation.
  (Codex against `endeavor mcp` over stdio was tried through `codex exec` on
  Linux; see [gaps.md](gaps.md).) Over HTTP, that
  each sends back the `Mcp-Session-Id` from `initialize`, which gives it a
  notebook of its own. The spec requires it; no client is checked live yet,
  Claude Code included.
- An agent following the skills without the app. The notebook skill
  (`endeavor-notebooks`, rewritten 2026-10-06 from the three Pluto skills)
  keeps what holds only in the app in `reference/app.md`, and
  `guide::STANDALONE` tells the agent to skip it. Short Claude Code runs on
  2026-10-07 (`endeavor mcp` over stdio, the plugin loaded with `--plugin-dir`)
  made a notebook, edited and re-ran cells and handled a `stale_read`
  collision; the new text has not been compared with the old.

Known gaps:

- `serve` on Windows has started Julia and been stopped by `endeavor stop`,
  by hand (`gaps.md`, Windows). It also stops Julia on Ctrl-C and when its
  console closes, but only `cargo check` and clippy for the Windows target
  have seen that code.
- Unpacking a new version removes older folders that no runtime holds and
  nobody has used for a day (`unpack`, `lease`). Folders unpacked before this
  have no `in-use` file and stay. The app still has to hold a `lease` on its
  plugin folder while Claude Code runs from it.

Next:

- Ember (R notebooks) is in development in its own repository. It joins as
  another adapter behind the core, with its own path prefix on the one port
  ([runtime-core.md](runtime-core.md), [one-port.md](one-port.md)). So far
  `wire` knows its files (`Backend::Ember`: `.R` with Ember's first line, a
  page at `/ember/edit`, a preview of the first cells), the folder scan the
  app asks for still lists only Pluto's, and `open_notebook` refuses an Ember
  file as `unsupported`. Next: the core running one adapter per engine, then
  the R adapter. The engine name a client asks a runtime for
  (`wire::ENGINE_PLUTO`, a string) becomes `Backend` with the first of those,
  where the core starts engines.
- marimo after Ember.
