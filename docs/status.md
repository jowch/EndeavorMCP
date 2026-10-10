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
- CI (`ci.yml`) builds and tests on Linux, macOS and Windows. `e2e.yml` runs
  `e2e_julia` and `e2e_serve` against real Julia on Linux. `helpers.yml`
  publishes the binaries for Linux, macOS and Windows to the `helpers` release
  under a key computed from the helper's source (`scripts/helpers.sh`), and
  `LATEST`, the newest key, for builds from `main`. Every build gets that key
  (`ENDEAVOR_RELEASE_KEY`). All five platforms were published for the first
  time on 2026-10-08 (key `ed283c702b22`). GitHub allows 1000 files on one
  release and each build adds six, so `scripts/prune-helpers.sh` removes the
  builds nothing needs any more: it keeps `LATEST`, every branch head's pin
  here and in Endeavor, main's last 40 plugin pins and Endeavor's last 10,
  and anything from the last 3 days (7 until 2026-10-10, when the release
  was gaining about 100 files a day); of the rest, the newest 150 builds
  keep their Linux files and checksums. The Prune helpers workflow runs it by hand,
  listing only unless asked to delete; Helpers runs it once the release holds
  more than 700 files, when the repository variable `PRUNE_HELPERS_AUTO` is
  true. Not yet run with deleting on (dry runs only, 2026-10-09). An installed
  binary whose build has lost its Linux files can't set up a new server (gaps.md).
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
  never stops it, `session_status` says it every time (`other_version`,
  since 2026-10-09), and `client::Session` reports it as trouble (serve.md,
  "Update it"; endeavor-mcp.md, "A runtime from another build").
- The status tool is `session_status` since 2026-10-10 (interface 2); it was
  `pluto_session_status`, a name that told agents every notebook is Pluto's.
  The old name still runs the tool but isn't listed, so an agent calls the new
  one; the old name only keeps working where an agent read it in text written
  before (an older skill copy, an app from before its pin, the user's notes). A
  permission rule that names the old tool doesn't match the new name. Drop the
  old name in the release after (`mcp::RENAMED_TOOLS`). Runtimes started before
  this build offer interface 1, so they show the "another version" notice until
  they restart. Error kinds the Pluto adapter returns
  (`pluto_multi_expression`, `pluto_not_running`) keep their names: they are
  Pluto's rules and say so.
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
  It needed `COMPUTERNAME` set to the DNS host name's case, since Endeavor
  compared the NetBIOS name with Julia's; fixed in #31, which reads the DNS
  host name.
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
  app asks for still lists only Pluto's, and agents open and make R notebooks
  (`.R`) like Julia ones, except on Windows (until Ember runs there) and in
  the app (until its pane shows Ember's page), where they are refused as
  `unsupported`. The core routes calls to more than one engine and serves
  `/ember/`. R's adapter (`runtime/r/adapter.R`) is built: the core starts
  it, with Ember in it, the first time an R notebook is opened, and follows
  its notifications by long-polling. The core makes Ember's page secret and
  hands it to R. `new_notebook` makes one from an `.R` path, `read_cell`
  shows Ember's own `stale` and `not_run` (every cell after a restart), and a
  warning in a cell's output starts with `Warning:`. The skills route R
  notebooks to `reference/ember.md`, and smoke task R1 runs one through a
  real agent. R notebooks still need Julia: the core starts it first
  (#76). R is what
  `--r` or
  `--r-shell` (serve, mcp, `connect`, a machine's `r` in machines.json) names,
  else Endeavor's own R once it's installed, else the login shell's `Rscript`.
  On the user's own Mac (a core started with `--own-r`: `serve`, `mcp`, and
  the app's local runtime; never a server) with no R, the open answers
  `r_not_found` offering Endeavor's own R 4.6.1 (`r::install_own`, #85). A Mac
  that has R uses it. With the user's yes (`use_machine` for `local` with
  `install: true`, which calls `endeavor/allow_r_install`, or the core's
  `--install-r`) the core downloads CRAN's installer, checks its SHA-256, and
  unpacks it into `~/.cache/endeavor/R-4.6.1` without running it, the way rig's
  user mode does but without editing shell startup files. It runs with its own
  `R_LIBS_USER` and its own Ember library inside that folder, and is on no PATH.
  Its fonts cache is inside that folder too. On Linux and servers Endeavor installs no R; `r_not_found` says to install
  it with rig or the system's packages, or to use a cluster's module.
  Ember comes from its r-universe repository (`core::EMBER_REPOSITORY`), which
  builds Ember's latest main: a binary where r-universe has one, else the
  source package (Linux today, and Apple Silicon until r-universe builds it).
  Each time the core starts R, `runtime/r/install.R` installs it, or updates it
  when there's a newer build, into `~/.cache/endeavor/r/ember`
  (`$SCRATCH/endeavor/r/ember` on clusters; `ember` inside Endeavor's own R's folder for that R): one library per build, named by
  the SHA256 of its file, and one with any package Ember needs that R lacks or can't load.
  The call waits up to 30 seconds for that. The first install then returns
  `r_installing` until it's done; an update goes on while R starts with the
  installed Ember. Offline, R starts with the installed Ember. A build that fails to
  install or load, or that R's adapter doesn't start with, isn't tried again;
  R starts with the build before it, and the agent gets an `ember_previous`
  warning. A `--r-library` is used as it is, never updated. The E2E workflow's
  R job runs every day against a fresh install of Ember's latest build and fails
  on a fallback (#82). The app passes a server's and Settings'
  `r` through, with no field to set them yet. The engine
  name a client asks a runtime for (`wire::ENGINE_PLUTO`, a string) is still
  a string; it becomes `Backend` later.
- Julia starts only when it's needed (#76). With `--julia-when-needed` the
  core writes `runtime.json` and answers at once, finds Julia itself (and
  downloads it only with `--install-julia`) the first time a Julia notebook is
  opened or made or a page of Pluto's past the runtime's link is asked for, and answers
  `julia_starting` while Julia starts. The plugin's `serve`, `mcp` and machines
  pass the flag; on Slurm the login node still finds or downloads Julia, as
  before, and only starting it waits. A session whose folder has Pluto
  notebooks, or whose caller says it is a Julia session (`kind`), gets Julia
  started ahead in the background. That never downloads Julia; Pluto's
  packages install on the first start in a fresh depot, as before. `e2e_r` runs an R notebook with no Julia.
  The app doesn't pass the flag yet, so its runtimes start Julia at once; its
  step is an app issue. The interface number is 3.
- Endeavor's own Julia (#96). `--julia own` uses only the pinned Julia
  (1.12.6), never the PATH's. Where the computer has juliaup, Endeavor's own
  Julia is juliaup's channel for that version, which Endeavor adds
  (`juliaup add 1.12.6`) instead of downloading a second Julia; without
  juliaup it downloads into `~/.cache/endeavor/`, as before. On Windows it
  installs juliaup first when there is none (from the Microsoft Store, else
  juliaup's App Installer file), so `--julia auto` with no `julia.exe` on the
  PATH now offers that download too. `julia::own_installed`, `install_own` and
  `remove_own` are public for the app's Settings; removing takes away the
  download, and juliaup's channel only if Endeavor added it (decided before
  adding, from juliaup's own list). On a juliaup with no channels, Endeavor's
  becomes juliaup's default, which juliaup won't remove; `OwnFrom::Juliaup`
  says so (`default`) and removing explains what to do. On Linux and macOS a
  failed `juliaup add` falls back to the checked download. `tests/own_julia.rs`
  covers these with real downloads (ignored by default); run on Linux only so
  far.
- marimo after Ember.
