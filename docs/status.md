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
- CI (`ci.yml`) builds and tests on Linux, macOS and Windows. Most integration
  tests are Unix only (their stand-in Julia is a shell script); on Windows,
  `tests/windows.rs` runs the local start with a stand-in `julia.exe`
  (`examples/stand_in_julia.rs`): `serve` with a given Julia and with the one
  on the PATH, `starting.lock`'s pid read while the core holds it, a Julia that
  ends, `endeavor stop`, and an agent's call through `endeavor mcp` (#9).
  Not covered there yet: the cancel of a start, and real Julia. `e2e.yml` runs
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
  binary whose build has lost its Linux files can't set up a new server ([#77](https://github.com/jowch/EndeavorMCP/issues/77)).
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
  `Ready` carries its build and interface, the front tells the agent once,
  `session_status` says it every time (`other_version`,
  since 2026-10-09), and `client::Session` reports it as trouble (serve.md,
  "Update it"; endeavor-mcp.md, "A runtime from another build"). Since
  2026-10-22 (#226) the helper (`endeavor connect`, not in a Slurm job)
  replaces an older version's runtime that has no notebook open with its own
  build's, and a refusal of an R notebook's path by a runtime from before R
  notebooks ("must end in .jl") reaches the agent as why, and how to restart
  it, from the front and from the app's relay (`client::Listener`). A kept
  older runtime still isn't told to the app's agent up front, and a pathless
  `new_notebook` in an R session on one makes a Pluto notebook (#239).
- The status tool is `session_status` since 2026-10-10 (interface 2); it was
  `pluto_session_status`, a name that told agents every notebook is Pluto's.
  The old name ran the tool, unlisted, for one release (`mcp::RENAMED_TOOLS`);
  since interface 7 (the release that let R notebooks into the app) it is an
  unknown tool. A permission rule that names the old tool doesn't match the new
  name. A runtime started before a rise in the interface shows the "another
  version" notice until it restarts; one from before interface 7 also refuses
  the app's R notebooks. Error kinds the Pluto adapter returns
  (`pluto_multi_expression`, `pluto_not_running`) keep their names: they are
  Pluto's rules and say so.
- The app can put back what an agent's turn changed since 2026-10-16
  (interface 10, Endeavor #79): `endeavor/restore_cells` sets the turn's edited
  cells back to their earlier code and deletes the cells it added, all or
  nothing, and only if every cell still has the code the turn left. Nothing
  runs; the code is the user's, a cell whose code isn't what last ran shows
  unrun, and an agent reads a restored cell before a run of it or of what
  needs it (`run_conflict`). Cells the turn deleted can't come back yet, and
  a cell to delete isn't checked by the engine itself (#146). The app's Restore
  button is the next step.
- The pages have a token of their own since 2026-10-15 (interface 8, #100). The
  bearer token, which an agent's MCP config carries, opens `/mcp` and
  `/endeavor/…` only; the pages' token (`page_token` in the state folder and
  `runtime.json`, in `Ready` and `page_url`) opens Pluto's and Ember's pages
  only. Julia's and R's bridges take a third key the core makes at each start
  and keeps in no file, so the bearer token can't run cells past the run gate
  through `/adapter` either. A client meeting a runtime from before sends the bearer token to its
  pages, which that runtime still takes. An older app that attaches to a newer
  runtime gets its pages refused until Julia restarts from the app's build.
- A session that attaches to the runtime on this computer is told once when
  that runtime was started with other network settings (#93, 2026-10-17).
  `runtime.json` records a short hash of each proxy and CA bundle variable
  that is set (`network`), not its value. A runtime still downloads with the
  settings of the session that started it; the notice says so, and says that
  stopping it lets the next call start one with this session's. The record
  gained a field that older callers ignore, so the interface stays 23
  (endeavor-mcp.md, "A runtime started with other network settings").
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
  Linux; see [#17](https://github.com/jowch/EndeavorMCP/issues/17).) Over HTTP, that
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
  by hand ([#9](https://github.com/jowch/EndeavorMCP/issues/9)). It also stops Julia on Ctrl-C and when its
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
  app asks for lists every engine's notebooks (the app leaves R ones out on
  Windows), and agents open and make R notebooks (`.R`) like Julia ones, in
  the app too, except on Windows (until Ember runs there), where they are
  refused as `unsupported`. The core routes calls to more than one engine and serves
  `/ember/`. R's adapter (`runtime/r/adapter.R`) is built: the core starts
  it, with Ember in it, the first time an R notebook is opened, and follows
  its notifications by long-polling. The core makes Ember's page secret and
  hands it to R. `new_notebook` makes one from an `.R` path, `read_cell`
  shows Ember's own `stale` and `not_run` (every cell after a restart), a
  warning in a cell's output starts with `Warning:`, and `read_notebook_code`
  marks cells as an Ember file does (`# %% id=`). `edit_cell` can fold the
  cell it edits, in either engine (#162), and so can each edit in `edit_cells`
  (#168; the interface number is 13), whose receipt says each cell's fold after the
  edits (`mutation.code_folded`; #174, interface 16). While an edited cell is
  still running, the edit's result gives its old output and error as
  `previous_output`, `previous_errored` and `previous_error`, and leaves it out
  of `outputs.changed` (#190, interface 17). In Ember's lazy mode, a run's result warns `lazy_mode` and lists the
  notebook's stale cells as `stale_cells`, and `list_notebooks` and
  `open_notebook` show `on_cell_change` (#196, interface 19). A hand-written
  R notebook's short cell ids (`# %% id=B`, which Ember keeps) name its cells
  in every tool that takes a cell id (Ember #82, interface 20). An agent can restart a notebook with
  `restart_notebook`, as the page's Restart does (#202, interface 21). The notes Ember shows under a cell's last run (`changed_in_place`, `settings_found`, `load_note`) reach the agent: as `warnings` on the run that made them, and as `notes` in `read_cell` (#207, interface 22). The tools also show an R notebook's `package_date`, and `preview_package_update` and
  `apply_package_update` move it, as the Packages tab's Update does: Ember's
  `preview_date()`, then its `set_date()` (#201, interface 23). A waited run counts the dependents that reran with it as its own cells, so a dependent that failed makes it `errored`, and a run not waited for names the dependents it will rerun in `execution.will_run` (#192, interface 24). R's adapter
  names each run, as Pluto's does, and in autorun counts the cells downstream of
  the run's cells as part of it, since Ember queues each one as its parent's
  result comes in. So a run goes on, and `read_cell` waits for a dependent of the
  agent's run, until those finish (#151; interface 12). That includes a cell that
  read a name the run's cells dropped (a rename), which Ember reruns though the
  graph no longer links it; the adapter names such cells to the core, for
  `will_run` and the waited run's cells (#228). A waited `submit_changes` with
  nothing staged reports the cells that ran while it waited, and `errored` if one failed (#171; interface 14).
  `allow_execution`'s run of the notebook is the agent's run too, so a read or a waited submit
  right after it waits for it, in both engines (#170; interface 15). A read whose
  wait runs out with the cell still running or queued says so in a `message`, with
  how long it waited (#203): agents that saw only `running` reported that it
  hadn't waited. When R ends, its
  notebooks close with it: the agent's next call on one answers
  `notebook_closed` with why (how R's process ended and its last error), and
  `/endeavor/events` lists them under `engine_ended` until each opens again
  or the app's Stop notebook dismisses it (#101). The app doesn't show
  them yet (Endeavor #120). The skills route R
  notebooks to `reference/ember.md`, and smoke task R1 runs one through a
  real agent. R notebooks don't need Julia: a core started with
  `--julia-when-needed` runs them with no Julia (#81, #87). R is what
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
  Its fonts cache is inside that folder too. In the app, whose agents have no
  `use_machine`, `r_not_found` tells the agent to have the user install it from
  the notebook pane's Install R or Settings instead; the app does that with `r::install_own` (and `own_offered`,
  `own_installed`, `remove_own`), as it does Julia with `julia`'s. On Linux and servers Endeavor installs no R; `r_not_found` says to install
  it with rig or the system's packages, or to use a cluster's module.
  Ember comes from its r-universe repository (`core::EMBER_REPOSITORY`), which
  builds Ember's latest main: a binary where r-universe has one, else the
  source package (Linux today). r-universe has no Apple Silicon build (Ember
  #66), so Ember's own CI builds one for R release and oldrel on each push to
  main and publishes it as a one-build repository in a GitHub release per R
  version (`core::EMBER_MAC_ARM64`, `macos-arm64-r4.6`). An Apple Silicon Mac
  installs from that first, then from r-universe; an R version with no such
  release gets r-universe's source. A Mac without the developer tools that has
  an Ember skips r-universe's source, which it couldn't build: when GitHub
  can't be reached it keeps its Ember and says nothing. With the tools, it
  installs r-universe's source build then, and goes back to GitHub's at the
  next start that reaches it. The downloaded file must have the SHA256 the
  index lists, or it isn't installed (and isn't counted as a failed build).
  Each time the core starts R, `runtime/r/install.R` installs it, or updates it
  when there's a newer build, into `~/.cache/endeavor/r/ember`
  (`$SCRATCH/endeavor/r/ember` on clusters; `ember` inside Endeavor's own R's folder for that R): one library per build, named by
  the SHA256 of its file, and one with any package Ember needs that R lacks, can't load,
  or has at a version older than Ember or a package it needs asks for (#90).
  The call waits up to 30 seconds for that. The first install then returns
  `r_installing` until it's done, and each call made meanwhile waits up to 30
  seconds for it too, as the answer says (#175: before, only the call that started
  it waited, and agents looped). An install that fails, of Ember or of
  Endeavor's own R, is what every call is told for a minute; the call after that
  tries again (#183: before, each call after a failure started a new install). An
  update goes on while R starts with the installed Ember. Offline, R starts with the installed Ember, and for an hour after none of Ember's repositories answered it doesn't ask them again: where connections hang rather than being refused, each place waited 20 s at every start (#90). A build that fails to
  install or load, or that R's adapter doesn't start with, isn't tried again;
  R starts with the build before it, and the agent gets an `ember_previous`
  warning. A `--r-library` is used as it is, never updated. The E2E workflow's
  R job runs every day against a fresh install of Ember's latest build and fails
  on a fallback (#82). The app passes a server's and Settings'
  `r` through, with no field to set them yet. The engine
  name a client asks a runtime for (`wire::ENGINE_PLUTO`, a string) is still
  a string; it becomes `Backend` later.
- A tool call that finds something still starting answers as a normal result, not an
  error, keeping its code (`julia_starting`, `r_installing`, `notebook_opening`), and so
  does a machine's runtime that is starting or queued (#105). `open_notebook` and
  `new_notebook` wait at most what is left of the call's 45 s, Julia's start included;
  an open that takes longer goes on, the call answers `notebook_opening`, and the same
  call again gets its result (#16). `allow_execution` is waited the same way: leaving
  safe preview starts a new Julia process, which with packages to install can take
  longer than a call (#128). `run_shell` too: a command still running when the call
  has waited returns `still_running` with its output so far and goes on, up to its
  `timeout_seconds`; the same call again waits for it, without asking the user again,
  and gets its result (#16). A precompile workload runs a notebook once while
  its package precompiles, so a cold first open went from about 33 s to about 13 s
  here, and a later open on Windows from about 19 s to about 11 s (#16). Most of
  what is left is starting the notebook's worker process. The workload is its own
  package, `EndeavorRuntimeWarm`, because it adds about 30 s to precompiling on
  Windows: setup precompiles `EndeavorRuntime` alone, the runtime precompiles the
  warm package in a low-priority process once nothing has been opening or running
  for 30 s, and the next start loads it. So a first-ever notebook doesn't wait
  for it, and opens get faster from the next start.
- Julia starts only when it's needed (#76). With `--julia-when-needed` the
  core writes `runtime.json` and answers at once, finds Julia itself (and
  downloads it only with `--install-julia`) the first time a Julia notebook is
  opened or made or a page of Pluto's past the runtime's link is asked for, and answers
  `julia_starting` while Julia starts (a normal result, not an error: #105). An open or
  creation the user allowed on a card that is told Julia (or R's setup) is still
  starting doesn't ask again when the agent makes the same call again: only that
  session's, only the same tool and arguments, only while nothing came between
  (another call, a Stop, a change of mode), and for 5 minutes after the user's yes
  (Endeavor #90). Under Claude Code, `endeavor mcp`'s session key comes from the
  conversation's id, so a resumed conversation keeps its reads and its notebook
  (#176). The plugin's `serve`, `mcp` and machines
  pass the flag; on Slurm the login node still finds or downloads Julia, as
  before, and only starting it waits. A session whose folder has Pluto
  notebooks, or whose caller says it is a Julia session (`kind`), gets Julia
  started ahead in the background. A session said to be R's never does, and
  its `new_notebook` without a path makes an R notebook (#86; interface 6). That never downloads Julia; Pluto's
  packages install on the first start in a fresh depot, as before. `e2e_r` runs an R notebook with no Julia.
  The app doesn't pass the flag yet, so its runtimes start Julia at once; its
  step is an app issue.
- The app can ask where Julia is (#96): `endeavor/julia_status` answers
  `not_started`, `starting` (the step in words, such as "Downloading Julia
  1.12.6… 42%", and `quiet_seconds` since the last sign of progress), `ready`,
  or `failed` (the error's code and why), and never starts Julia. Reading a
  failure there leaves it for the agent's next call to hear; the app's Retry
  is `endeavor/start_julia`, which starts Julia afresh (or ahead, before
  anything needs it). A start ends: Julia that makes no progress for 30
  minutes (no new step, nothing new in the runtime's log) is stopped and the
  start fails, and a download under 1 kB/s for 5 minutes fails and resumes on
  the next try. The interface number is 5.
- The app can ask where R notebooks' setup is (Endeavor #93): `endeavor/r_status`
  answers `starting` with the step in words ("Installing Endeavor's own R …",
  "Installing Ember for R notebooks", "Updating Ember", "Starting R"),
  `ready` while R runs (with `updating` while a newer Ember installs for the
  next start), `failed` with the error's code and why (until the next start),
  or `not_started`, whoever is waiting, the agent included. It starts nothing.
  After a first install that finished with no call waiting it reads
  `not_started` until the agent's next call (#148). The interface number is 9.
- An agent can stop a run with `interrupt_notebook` (#161; interface 11), as the
  notebook's Stop button does. Before it, an agent asked to "stop if it takes too long"
  had no way to, and its edits queued behind the run. It asks nothing in any mode and
  plan mode refuses it. In R, a cell in compiled code that ignores SIGINT is reported
  `still_running`, and only the page's restart ends it. Until then no R cell stopped
  for an interrupt at all, from the page either: R's adapter inherited the core's
  signal mask, which blocks SIGINT, SIGTERM and SIGHUP for the core's own thread, and
  passed it to Ember's workers. R now starts with the mask the core was started with,
  as Julia does. So do `run_shell`'s commands (#165), which before ignored SIGINT and
  SIGTERM, as did everything they started, and the curl, wget or juliaup the core starts to
  download Julia, Endeavor's own R or a helper (#181), which before went on after
  a Ctrl-C had stopped the core.
  In Julia, an interrupt while the notebook's process is still starting stops its
  run before any cell, and the answer lists them all as `dropped` at once (#179).
  Before, it reported them dropped, or none when the start took longer than its
  wait, and they all ran once the process was up.
  A notebook whose process ended while it started can be restarted, from the app
  or from Pluto's own restart on its page, and its cells run (#194). Before, the restart failed with the start's error and
  its cells stayed queued until the notebook was stopped and opened again.
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
  says so (`default`) and removing explains what to do. A failed
  `juliaup add` is tried once more (#132: a first install on Windows failed
  once with "Access is denied", and a retry after clearing juliaup's folder
  worked; a retry in the same folder hasn't been seen yet). If that fails too,
  Linux and macOS fall back to the checked download, and on Windows the error
  says trying again may work. The error names the step juliaup says failed,
  and the runtime's log has all of juliaup's error (#135: on Windows it's the
  move of the unpacked files, which juliaup doesn't retry). `tests/own_julia.rs`
  covers these with real downloads (ignored by default); run on Linux only so
  far.
- marimo after Ember.
