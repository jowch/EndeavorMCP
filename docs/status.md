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
  Where the client can show a dialog (MCP elicitation), a machine tool never
  installs on the agent's word: the front makes the call without `install`,
  and if that names something to install (or, on `use_machine` with
  `install: true` and a runtime running, leave to download Julia or install R
  later), it asks the user then and there in a dialog that names it, and
  installs only on their yes (`install_declined`, `install_not_answered`
  otherwise). The agent needn't call again: when it had to, Haiku 4.5 often
  said a dialog would come and stopped (#380's smoke runs). There the tools'
  descriptions tell the agent not to ask in the chat as well. Haiku 4.5 installed before the user's yes in 2 of 3 smoke
  runs before this (#378). A client that can't show a dialog doesn't take the
  agent's word either (#381): there a call with `install: true` opens a page
  in the user's browser (`standalone/install_page.rs`, on 127.0.0.1 behind a
  one-time secret that never reaches the agent) that names what it would
  install, and installs only once they click Install. Click and install fit
  in 55 s (Codex gives a tool call 60 s), so the call waits 35 s for the
  click; a later call finds it, and a yes holds for the same question for 10
  minutes. A no can become a yes with the answered page's own button. A no holds as long, without
  a new page, and an unanswered page opens again at most once, so an agent
  that keeps calling can't wear the user down. A look without `install` opens
  no page. Without a browser to open (a front over ssh or with no display),
  the tools and `needs_install` say nothing can be installed from that app,
  and `install: true` fails with `install_cant_ask`. The page stops an agent
  that says yes for the user, not one working against them: an agent with a
  shell on the same computer can find the link (the opener's command line,
  the browser's history) and could install things itself anyway. The front's
  log says whether each client shows dialogs (`[ Info: Client …: shows
  dialogs: yes|no`). A client can say it shows them and not: one that answers
  the dialog with "Method not found", or with another error or a close in
  under 0.7 s, before anyone could read it (`claude -p` closes it so), is taken for one without dialogs from then on,
  and that call asks on the page instead (if it would have opened one). A
  client can also take the dialog and never answer it (`claude -p` does): a
  dialog with no answer in 20 s gets the page as well, where a browser can be
  opened, and whichever the user answers first counts (the other is taken down);
  the dialog stays up until then, for the dialog's full 30 minutes. An
  answer on the page that still holds is given without a new dialog. Which clients really show the
  dialog is still to be checked by hand (#26).
  A server's sign-in that asks something (a password, a code, a key's
  passphrase, or whether to trust a new server) is asked on a page in the
  user's browser the same way (`standalone/signin_page.rs`, #106): the answer
  goes from the page to `ssh`, never through the agent. Cancel refuses the
  sign-in; a page nobody answers in 100 s, or a question more than 2 minutes
  after a tool last used that connection (which gets no page), ends as a
  sign-in nobody answered, which the next call tries again. Without a browser,
  `ssh` stays in batch mode and such a server fails with a message saying so.
  Checked with a stand-in `ssh` in the tests, and by hand with real OpenSSH
  on Linux (a key with a passphrase, and a new host key); not yet against a
  server that asks for a password or a code, nor on macOS or Windows.
  A Julia notebook's run whose cells use a package the notebook doesn't
  have yet (not in its Project.toml, not a standard library) stops before
  Pluto adds it (#397): its cells stay queued (though nothing runs, so
  other cells still read and view as usual, #506), `packages.step` is `asking`
  with `new`, and `approve_install` is the user's yes, asked about in the
  same dialog or page; its yes holds for those packages while the notebook
  is open. An edit that drops the package releases the cells: they show as not
  queued, and run with the notebook's next run, unless they ran another
  way first or a Stop came between (#523). The yes covers only the packages the user was shown: the dialog,
  the page and the app's card pass their names on (`approve_install`'s
  `packages`, interface 43), and a package the notebook came to need after
  the question asks again (#418). A call the front passes on with no yes to
  names (nothing waited when it looked, or it couldn't tell what did) names
  none, so it approves nothing. Python notebooks ask the same way (#355).
  Packages the notebook already has install as before. The same
  gate holds when the notebook leaves safe preview (`allow_execution`) and
  when `open_notebook` runs it, so a file with no environment of its own adds
  nothing unasked. Runs in the
  app's Auto mode don't ask, and neither do runs in Claude Code's own auto
  mode (or bypassPermissions) through the plugin. Claude Code doesn't tell an
  MCP server its permission mode, but it gives it to hooks: the plugin's
  PreToolUse hook (`endeavor-mcp.sh --hook`, then `endeavor hook`) writes it
  to a file named by the conversation and one named by the Claude Code
  process, and the front reads the newer with each call
  (`standalone/claude_mode.rs`). The process's file carries it past `/clear`,
  which starts a new conversation in the same server (#505); on Windows only
  the conversation's is written. Anything missing or older than 30
  seconds counts as not auto, so the user is asked. Other plugin clients'
  auto modes (Codex) can't be seen, so those runs ask. The app's card for a Julia `approve_install`
  still uses R's download wording (an app follow-up).
  The same page asks before `approve_install` passes an R notebook's large
  download on, where the client can't show a dialog (#388). Its question
  names the notebook by its whole path, so a held yes is for that notebook
  only, even beside another with the same file name; when the front
  can't read what downloads, the question says so and its yes holds for one
  call. With no browser either, `approve_install` downloads nothing
  (`download_cant_ask`), and it can't be approved there: the agent says so and
  sends the user to the Endeavor desktop app or to a new Claude Code session in
  a terminal. The notebook's page has no approve button (#402). A client connected straight to `endeavor serve` over HTTP has no
  front to ask for it, so the runtime refuses its `approve_install` the same
  way (#391); only a caller that says it is the app (`X-Endeavor-Client:
  app`, its card asks) or a front that asks (`X-Endeavor-Consent: front`)
  gets the call through. A caller that says neither is refused even on a
  runtime the app started. These headers are a declaration, not a check:
  they stop an agent saying yes for the user (a model can't add headers to
  its client's requests), not a client someone configured to send them, and
  no text an agent reads names them. Asking such a client in
  a dialog would need server-to-client requests on `/mcp`, which aren't built.
- One port per runtime is built and checked live on a Mac, a Linux server
  and a Slurm job ([one-port.md](one-port.md)).
- The notebook view (`views.rs`, interface 36; R notebooks interface 38): a client that draws MCP App
  views (Claude Desktop and claude.ai say so in `initialize`) gets
  `open_notebook` and `new_notebook` with a `ui://endeavor/notebook` view, and
  no browser tab. The view is the notebook's own live editor (Pluto's, or Ember's
  for an R notebook), shown only beside the
  chat (the Code tab's panel, after one click on its card) or full screen
  (Claude's chat, without a click). It reaches its engine through tools only it
  calls: `notebook_page` reads the notebook's page, made by Julia
  (`endeavor/page_bundle`) or for an R notebook by R's adapter
  (`page_bundle`, Ember's `view_page()`, made again each time R starts), and
  `notebook_socket_*` relay the engine's WebSocket, which the core holds on
  loopback. Checked with `scripts/notebook-view-check.cjs` (`--r` for R;
  [testing.md](testing.md)) and a step of `e2e_julia` and of `e2e_r`. An
  Ember from before `view_page()` gets a card that says it updates when R
  next starts. An R notebook's widgets (htmlwidgets: plotly, DT) get their
  files through `notebook_file`, which reads `deps/<name>-<version>/<file>`
  from Ember on this computer, for Ember's `window.ember_dep_file` (Ember
  #629, interface 39; `--widget` in the check). A box the user draws with Point goes
  to Claude as a picture: the view draws what it can of the box (images, SVG
  and canvases, not text or WebGL), keeps it with `notebook_crop` in the core
  (32 pictures, an hour, 8 MB each), and the quote names it; the agent sees it
  with `view_cell_output`'s `crop` (interface 47). What a widget fetches from
  the internet (Leaflet's map tiles, a CDN script, web fonts) stays blocked
  by the view's sandbox. A socket no view asks about for 60 s, or with 256 MB unread,
  closes itself; a message over 1 MB comes in parts. A card that Claude draws
  again for an old call waits for a click. It has the Endeavor look and Point
  (`views/endeavor-view.js`, a copy of the app's `frontend/dist/view.js`,
  which CI keeps equal to the app's `main`): ↩ in Point's comment bar
  passes the picks and the comment to the chat as the user's message, with any
  that ⌘↩ added (kept in the view until then). Claude's
  desktop app puts it in its message box for the user to send (checked
  2026-11-11); its Code tab gave a view's context to Claude on no turn, so the
  view doesn't use context. Desktop adds the message at the end of its box and
  trims its leading whitespace, so it starts with ↓ (explained in its note);
  it refuses one for about 5 s after other input on the page, or when the same
  press was already used, and then Point gets its picks and comment back for
  the user to press ↩ again (checked 2026-11-12). A host that
  takes no messages from views gets the look without Point. A notebook's output
  scripts run in the view and can post to the chat too (#304); Desktop takes a
  view's message only right after a press in the view and never sends it, and
  the README tells users to read the box before sending. The view fills Claude's
  panel: Pluto's header, footer and floating tabs give way to one slim toolbar
  (the file's name, what Pluto is doing, Docs and Point), and Pluto's header
  shows only as its band when the notebook needs the user (Safe preview,
  Restart). The "Open notebook" card uses Claude's style variables, on a
  transparent background. Links in cells open through the host's
  `ui/open-link` (the frame can't open windows; Desktop asks first). Point's figures and boxes go without a picture, which
  the app takes natively. Desktop's Chat tab draws no card (its tool list lacks the app-only tools,
  checked 2026-11-12), and the server can't tell it from the Code tab: so the
  agent says the notebook should show beside the chat, and if the user doesn't
  see it, opens it again with `in_browser`, which opens a browser tab (#311).
  Desktop also serves all its conversations on one connection, so its session
  may work on several notebooks, and one conversation can change a notebook
  another opened once it opens it (`X-Endeavor-Notebooks`, docs/endeavor-mcp.md).
  Whether a session is the app's or the plugin's is the session's own, not the
  runtime's: the front sends `X-Endeavor-Client: plugin` and the app's agents
  `app`, so on one runtime that both use, each gets its own guide, browser
  links, host tools and offer of Endeavor's own R. A caller that sends neither
  is taken as the runtime's starter was (#110). Until the plugins' pinned build
  has this, a runtime the plugin started ignores the app's header and still
  answers the app's agents as the plugin's.
  Quitting the app with Keep running off leaves a runtime that both use running
  while a plugin session works in an open notebook and its front still runs.
  It ends 10 minutes after the plugin is out of it, and the app takes it back
  when it opens again (app #232, interface 45, `docs/plugins-and-remote.md`).
  A runtime the app leaves running (any quit, or a killed app) goes on until
  its notebooks have each been idle for the idle limit, and ends 10 minutes
  after none is open (`connect --exit-idle-after-client`,
  `endeavor/end_when_idle`, app #274, interface 46). The app's Keep running
  setting goes. An older-build runtime is stopped as a quit used to stop it.
  Until the plugins' pinned build moves past this change, their fronts don't
  say which process they are, so for them the only rule is "called in the
  last 30 minutes".
  `close_notebook` ends a session's own notebook's process when the user asks,
  as the app's Stop notebook does, so a sitting no longer needs `endeavor stop`
  (interface 37). Closing it doesn't free the session for another notebook:
  the session stays bound to it and may only open it again (#520). Not yet: a notebook on a machine, which still opens in a browser
  tab (the chat's view reaches another `endeavor mcp` process, which works on
  this computer). A client that draws views but can't move one beside the chat
  gets only the card. Claude ignores the documented automatic side panel for a
  local server (checked 2026-11-11).
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
  `scripts/install.ps1` (run in CI under Windows PowerShell 5.1 against a
  release behind http redirects) install the newest build. Under 5.1,
  `install.ps1` follows redirects by hand and refuses one from https to
  anything else (that refusal isn't tested: it needs a TLS server).
- The plugins check their download against checksums pinned in this
  repository: the Helpers pin job writes each platform's SHA-256 under the
  key in `release-key`, and the launcher passes them to `install.sh`
  (`--sums`), so a release changed after the pin is caught, not only a
  damaged download (#98). A binary published under a key is never replaced. The install
  scripts run by hand and `endeavor update` install the build `main` pins,
  checked against its checksums, and install nothing when `main` can't be
  read, unless `ENDEAVOR_PIN_URL=none` says to use `LATEST` and the release's
  checksum file (#422). A server's helper is pinned only when the build
  embeds its sums (`embedded::PINNED_SUMS`, which a build of the pin commit
  has) or `main` still pins its key. Otherwise, and when `main` can't be read,
  it is checked against the release's own checksum file, which catches a
  damaged download and not a tampered release.
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
  it, from the front and from the app's relay (`client::Listener`). Since
  2026-11-23 the app's relay to a kept older runtime, on a server or on this
  computer, also tells each agent session once, with its first tool result,
  and adds `other_version` to every `session_status`, as the front does
  (`guard::Older`; `Messages::tell_other_build`, which the front turns off).
  These texts no longer name `stop_machine` as the remedy: the agent asks the
  user first, and calls it only on their yes (#239). One part is the app's to
  do, since only it knows a session's kind: a pathless `new_notebook` in an R
  session on such a runtime still makes a Pluto notebook (Endeavor#326).
  Since 2026-11-20 `connect`, `serve`, `mcp`, `status` and `stop` with no
  state folder named use the plugin's folder, or while a runtime the app
  started in its older `~/.cache/endeavor/state` is alive on that machine,
  that one, so the app and the plugin on a server can share one runtime (#110).
  A connection or an `mcp` that began while that runtime was alive starts its
  next runtime in the plugin's folder once it stops, not in the older one (#434).
  The app still names its older folder until it moves its pin.
  Since 2026-10-30 a client that sets `Listener::set_restart` (the app) gives
  its agent `request_restart`: the listener adds it to `tools/list` and answers
  it itself, whatever build the runtime is and while the runtime is away, by
  asking the client, which shows the user the agent's reason with Restart
  (`guard::AskRestart`). Nothing restarts without the user's press. The R
  refusal then names that tool instead of telling the agent to restart Julia
  (Endeavor #174).
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
  (interface 18, Endeavor #79): `endeavor/restore_cells` sets the turn's edited
  cells back to their earlier code and deletes the cells it added, all or
  nothing, and only if every cell still has the code the turn left. Nothing
  runs; the code is the user's, a cell whose code isn't what last ran shows
  unrun, and an agent reads a restored cell before a run of it or of what
  needs it (`run_conflict`). Cells the turn deleted can't come back yet. Pluto's
  and marimo's engines check a cell to delete against the turn's code, as they
  do an edited one (#146); Ember's doesn't yet, so there a user's edit in the
  moment before the delete can still be lost. The app's Restore button is the
  next step.
- When someone else changed a cell since a session last read it (the user,
  another session, or this conversation before it was resumed), that session's
  next `read_cell` says so in `changed_since_your_read` (with the code it had
  read), and `read_notebook_code` lists such cells, each with that code. That message and
  `stale_read`'s ask the agent to tell the user before it replaces the change.
  Haiku 4.5 replaced another session's edit without a word in three runs of
  smoke N5 out of three (#504). A session that had never read the cell is told
  too, on its first read, when another agent session changed it in the last ten
  minutes (`edited_by_another_session`, with how many seconds ago): it would
  otherwise take that fresh edit for the notebook as it is (#108). The user's
  own changes aren't told this way.
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
  read, not run across platforms). Endeavor's app does the same through
  `release_helper` for a server whose helper it doesn't ship (the Windows
  app ships none), with its own words for a build the release dropped.

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

- Stop clearing a held card on Windows (#300, fixed by #581). The Windows
  tests cover the hang-up itself; the app picks it up once it pins a build
  with it. Check, in the next Windows app check after that pin: open a
  notebook in safe preview, have the agent run a cell so the run-approval
  card shows and its call waits, press Stop before answering. The card should
  go within about 2 s, and the state dump should show no ask waiting.
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
- Older builds on a server (`~/.cache/endeavor/<build>/`) and in the plugin's
  `bin/<key>/` are removed after a week unused (`old_builds`,
  [#97](https://github.com/jowch/EndeavorMCP/issues/97)). A server build a
  runtime record names stays, wherever that runtime runs; records are read
  from the server root's folders, each node's `serve` folder and the
  cluster's, not from a state folder given as a path elsewhere. Checked on
  Linux with a fake server folder and plugin folder, not on a real cluster. A server
  where `squeue` doesn't answer keeps all of them. The plugin's folder stays
  after an uninstall. On a home folder without file locks (NFS mounted
  `nolock`, Lustre without `flock`), no lease can be taken or tested, so a
  build from this version on stays there for good; only builds from before
  leases are removed, by the other rules.

Next:

- Python notebooks run in marimo (app #1), in the library and the plugin only so
  far: the app and the notebook view beside the chat don't show them yet. `wire`
  knows their files (`Backend::Marimo`: a `.py` with `import marimo` and
  `app = marimo.App(`). The core starts Python notebooks' adapter
  (`runtime/marimo/adapter.py`) through uv the first time one is opened or made,
  drives it with R's protocol (`notebooks/r.rs`), serves marimo's page at
  `/marimo/` (not its terminal or file listings), and stops it on exit. Its first
  start may download Python and marimo: a call waits up to 30 s of it, then
  answers `python_installing`, and the same call again waits on (#354). marimo
  runs every notebook in its own uv environment from the notebook's header; a new
  notebook's header pins marimo and an `exclude-newer` package date, and a
  package a run imports is installed and written there at its exact version.
  An opened notebook whose header doesn't name marimo (or that has no header)
  gets marimo pinned at the server's version when it goes live, not the
  `marimo>=<newest>` marimo's sandbox would write (#352).
  `preview_package_update` and `apply_package_update` move a Python notebook's
  date as they do an R notebook's (#352): uv resolves the header at the new date,
  with each exactly pinned package let go to that date's newest and nothing built,
  and the apply pins those versions, moves `exclude-newer`, and restarts a live
  notebook so its sandbox is built again. Packages move only with the date (no
  `package`), and a notebook with no date gets one this way. The date can't be
  earlier than the marimo the server runs. A day is held as its end in UTC; with
  no day (today), it is the moment of the preview, which the apply writes, so it
  matches what the preview showed.
  A package new to the notebook (not in its header) waits for the user's OK
  first, as in Julia (#355): the cells stay queued, `packages.step` is
  `asking`, and `approve_install` installs it; the app's Auto mode doesn't ask,
  nor does Claude Code's own.
  A name a letter away from a widely used package's (a short list in the
  adapter, no network; same first letter, not a prefix of it) gets a hint in
  the question and the agent's message (#416).
  marimo runs them lazily, so opening one runs nothing and a run from marimo's
  page marks the cells below stale; an agent's run takes them too, as in Pluto.
  marimo allows one editing page per notebook, so
  the adapter watches as a read-only viewer and the user's page takes the notebook
  over as its editor; a second page is read-only until its Take over. The page
  and its WebSocket open only for a notebook that is open and allowed to run,
  since opening one builds its environment; the proxy decodes a path before it
  matches the refusals, as marimo's server does. A new notebook's header also
  holds Python to the minor version it was made with (interface 42). An app
  session opens and makes Python notebooks only once the app says it shows them
  (`shows_python` in `endeavor/set_session_folder`, interface 49, Endeavor #1);
  an app session that doesn't say so is refused. A server's runtime refuses
  them for now. The refusal goes by the caller, so a plugin session that shares
  the app's runtime on this computer opens them either way (#358). In safe preview an
  edit is written with marimo's own save and no session, so nothing runs or installs, and when the
  notebook goes live the cells marimo loads are given the ids the preview handed out (#351). A file
  written by hand is saved once in marimo's own form when it goes live (a heading's
  `mo.md("...")` becomes triple-quoted), so marimo pairs its cells by code when the file changes
  (#442); that save shows as a diff with no edit. A markdown cell that marimo will write in
  another form (a backslash at the end of its text, `"""` with a backslash, or keyword arguments
  to `mo.md`) carries a `markdown_in_file` note saying how to keep it (#449). A deleted cell
  leaves marimo's kernel too, so what it defined is gone from the session as it is from the file,
  and the cells that read it turn stale (#410). marimo doesn't keep cell ids in the file, so
  each time the file is read again (a change from another editor, going live, a restart) the
  cells are paired with the ones the agent knows by code, and keep their ids (#353). On
  Windows the core keeps uv, the adapter and marimo in a Job Object of their own, so stopping
  them ends all three; there the user installs uv themselves, since Endeavor has no uv of its
  own for Windows yet. marimo is started at the root of the drive the first Python notebook resolves
  to (a subst drive's own drive, a mapped drive's share), and until Endeavor restarts a notebook on another drive is refused
  with a plain message (#574). Known gaps: `set_input` and
  `view_cell_output` of HTML aren't there (#354). Where no uv is found, the agent
  is told Endeavor can download its own uv 0.11.32 (from uv's GitHub release,
  checked against its SHA-256, into `~/.cache/endeavor/uv-0.11.32`) if the user
  agrees; their yes comes as `use_machine` with `install: true` on this computer,
  which asks them first and passes `endeavor/allow_uv_install` (interface 48). In the
  Endeavor app the agent is told instead that the user starts a new Python
  session and clicks Install uv in its notebook pane, which sends the same call.
  The pane offers Install uv only when the app's own open fails, so the message
  can't point at the current pane: an agent's call from a Julia session, or in a
  Python session whose notebook is already open, has no button there.
  Python notebooks also run on servers, through `use_machine`, as Julia and R
  ones do; marimo's page is reached through the same forwarded port. On a
  server with no uv the offer names that machine, and `use_machine` with
  `install: true` there passes the user's yes to download uv along with
  Julia's. In a Slurm job Endeavor doesn't download uv: the agent is told to
  ask the user to install uv on the cluster (`tests/e2e_machines.rs` covers a
  server with no uv).
- In R and Python notebooks, as in Julia ones (#393), `view_cell_output` tells the
  agent when an output is interactive, so it doesn't describe controls it hasn't
  seen work. In R that is HTML with a script, canvas or control, or with a
  dependency that brings a script (an htmlwidget). In Python it is the same, plus
  marimo's own controls and Vega-Lite charts (altair); a table, math, or a layout
  such as an accordion or tabs, counts only by what it holds (#398).
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
  shows Ember's own `stale` (a result from before an ancestor ran, or from code
  the cell no longer has, such as an edit on Ember's page that hasn't run, #291;
  Ember doesn't reload an edit made to the file outside it, Ember #636) and
  `not_run` (every cell after a restart), a cell's error has Ember's `call` (R's
  own call, or a text cell's `` `r ...` `` span, with its `line`, #288), a
  warning in a cell's output starts with `Warning:`, and `read_notebook_code`
  marks cells as an Ember file does (`# %% id=`). `edit_cell` can fold the
  cell it edits, in either engine (#162), and so can each edit in `edit_cells`
  (#168; the interface number is 13), whose receipt says each cell's fold after the
  edits (`mutation.code_folded`; #174, interface 16). While an edited cell is
  still running, the edit's result gives its old output and error as
  `previous_output`, `previous_errored` and `previous_error`, and leaves it out
  of `outputs.changed` (#190, interface 17). So does a staged edit's result,
  whose cell hasn't run since (#435, interface 44). In Ember's lazy mode, a run's result warns `lazy_mode` and lists the
  notebook's stale cells as `stale_cells`, and `list_notebooks` and
  `open_notebook` show `on_cell_change` (#196, interface 19). A hand-written
  R notebook's short cell ids (`# %% id=B`, which Ember keeps) name its cells
  in every tool that takes a cell id (Ember #82, interface 20). An agent can restart a notebook with
  `restart_notebook`, as the page's Restart does (#202, interface 21). The notes Ember shows under a cell's last run (`changed_in_place`, `settings_found`, `load_note`) reach the agent: as `warnings` on the run that made them, and as `notes` in `read_cell` (#207, interface 22). The tools also show an R notebook's `package_date`, and `preview_package_update` and
  `apply_package_update` move it, as the Packages tab's Update does: Ember's
  `preview_date()`, then its `set_date()` (#201, interface 23). A day Package Manager hasn't published yet (today, early in the day in UTC) is previewed at the newest day it has, and the preview's `date` and message say which (#247). A notebook with no packages yet isn't told that nothing would change because every package is at its version: its preview says it has none, and that applying still moves the date every package added later comes from (#269, interface 30). A waited run counts the dependents that reran with it as its own cells, so a dependent that failed makes it `errored`, and a run not waited for names the dependents it will rerun in `execution.will_run` (#192, interface 24). Code the agent already has isn't sent again: `add_cell`'s reply leaves out the code it was sent, and `read_cell` leaves out code that is what this session read or wrote within its last ten calls, with `code_unchanged` and `code_sha` (12 hex of its SHA-256) instead; `include_code=true` gets it anyway (interface 25). In a long analysis that text was about a tenth of the agent's context. A run's result cuts each output's text at 2000 bytes, and at 200 for a cell that only ran along with the run's cells (a dependent, or a stale cell they needed) unless the agent edited it since it last ran or it failed: in that analysis dependents' outputs were another tenth. A package that isn't on CRAN or Bioconductor is named with an install call in a cell, as users do (Ember #79): the call doesn't run, and its cell gets a `from_lock` note, which reaches the agent like the others (#125, interface 26). An install that
  stops before a large download (Ember #37) shows as package step `asking`, with
  the packages and their sizes; reads and waited runs don't wait on it, the agent
  is told to ask the user, and `approve_install` gives the OK (#220, interface 27). While an install builds packages from source for want of a binary for this R (Ember #19), `read_cell` names them, and `view_cell_output` on a cell that hasn't run yet says so rather than calling its output text (#265, interface 28). `preview_package_update` also takes a `package` and `version` instead of a date, for Ember's `preview_update()`: the date that puts one package at a version, moving the others as little as it can, as the Packages tab's row Update does (#266, interface 29). A text cell's `output` is its prose with each inline value in place of its `` `r ...` `` span, as the page shows it, rather than the values alone (#270, interface 30). A cell that waits at a console prompt (`readline()`, `menu()`, `askYesNo()`; Ember's worker runs interactive since Ember #186) ends a waited run's wait at once: the run's result and `read_cell` show `waiting_for_input` (the cell, the prompt's id, what it asked, `menu()`'s choices), and `answer_prompt` answers it, or `interrupt_notebook` stops the run (#261, interface 31). A cell that reads variables through `get()` on a computed name, `eval()` or a function that does carries an `untracked_read` note, which Ember shows from the code, before the cell runs too (Ember #357, #365, interface 32). `set_input` sets an input from emberui (Ember #36), as moving its widget does, and `read_cell` lists a cell's inputs (interface 33). In the app, `approve_install` waits for the user's answer in every mode but plan, Auto included (#278, interface 34). With the plugin, the front (`endeavor mcp`) asks the user itself in a dialog (MCP elicitation) before it passes `approve_install` on, where the client can show one, as Claude Code can; a no or a closed dialog downloads nothing, and a call the client cancels while the dialog is up downloads nothing either. It asks only when the runtime's `list_notebooks` shows that notebook's install `asking`, and names the packages and sizes from it; where it asks, `approve_install`'s description tells the agent not to ask in the chat as well (#285). In `claude -p` there is no one to ask, and Claude Code closes the dialog at once. A cell whose reader got something naming no file Ember can watch, such as a connection of a kind it doesn't know, carries an `untracked_file` note, and the skill says that Ember watches the data files a cell reads, however the path is built (Ember #513; #264, interface 35). A Julia notebook's `read_cell` lists its `@bind` widgets under `inputs` too, as the notebook's process describes them, and a cell whose result used a widget the user moved from its default carries a `uses_moved_input` note, which the `message` of a run or a read repeats; the tools still can't set a Julia widget (interface 40). An R notebook's cells get the same note when an emberui input is off its default, from Ember's graph (#343). PlutoUI's `Clock` and `CounterButton` count as moved once they've ticked or been pressed, and a ticking `Clock` has the runtime ask the notebook's process again after each tick. R's adapter
  names each run, as Pluto's does, and in autorun counts the cells downstream of
  the run's cells as part of it, since Ember queues each one as its parent's
  result comes in. So a run goes on, and `read_cell` waits for a dependent of the
  agent's run, until those finish (#151; interface 12). That includes a cell that
  read a name the run's cells dropped (a rename), which Ember reruns though the
  graph no longer links it; the adapter names such cells to the core, for
  `will_run` and the waited run's cells (#228). A disabled cell, or one
  downstream of it, never reruns, so `will_run` and lazy mode's `stale_cells`
  leave it out, and a read of it doesn't wait for the run: R's snapshot marks it `off`, and Pluto's tools hide
  such cells (#232). A waited `submit_changes` with
  nothing staged reports the cells that ran while it waited, and `errored` if one failed (#171; interface 14).
  `allow_execution`'s run of the notebook is the agent's run too, so a read or a waited submit
  right after it waits for it, in both engines (#170; interface 15). So is the run an agent's `open_notebook` starts with `run_notebook`, in both engines, and the session's Stop interrupts it (#331, interface 41). A read whose
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
  else the login shell's `Rscript`, else Endeavor's own R if it's installed
  (looked for once, when the runtime starts).
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
  source package (Linux today). Apple Silicon Macs get r-universe's binary
  too: until r-universe's arm64 builds worked (Ember #66, #205), they
  installed one Ember's own CI published as GitHub releases. An Apple
  Silicon Mac without the developer tools that has an Ember skips
  r-universe's source, which it couldn't build: when the binary can't be
  reached it keeps its Ember and says nothing. The downloaded file must have
  the SHA256 the index lists, or it isn't installed (and isn't counted as a
  failed build).
  Each time the core starts R, `runtime/r/install.R` installs it, or updates it
  when there's a newer build, into `~/.cache/endeavor/r/ember`
  (`$SCRATCH/endeavor/r/ember` on clusters; `ember` inside Endeavor's own R's folder for that R): one library per build, named by
  the SHA256 of its file, and one with any package Ember needs that R lacks, can't load,
  or has at a version older than Ember or a package it needs asks for (#90).
  The checks after installing, of those packages and of Ember, run in a new R,
  as the adapter loads them (#216: before, they ran in the R that had already
  loaded an old copy while deciding what to install, so on a machine with an
  old site library the first start could fail with "Couldn't install" and the
  next one work).
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
  error, keeping its code (`julia_starting`, `r_installing`, `python_installing`, `notebook_opening`), and so
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
  for it, and opens get faster from the next start. After an install or update,
  `endeavor mcp` also starts that low-priority precompile itself, of both
  packages, once per runtime folder, Julia and depot, but only where Julia
  notebooks ran before and Julia is already installed. So the first open after
  an update doesn't run without the warm package: about 24 s instead of 45 s here.
  That precompile isn't part of the runtime, so `endeavor stop` leaves it to
  finish, a minute or two, as a Julia process with no runtime around it. That
  is meant, so a short session doesn't throw the work away. It holds a lock in
  the runtime folder, so a server started meanwhile doesn't start a second one,
  and it writes its marker when it's done.
  Before either loads anything, a compiled file of those two packages in our own
  depot that the user can't read is removed, so Julia compiles it again. One left
  `--w-------`, once another runtime version had compiled there too, made every
  start fail with "Permission denied" (#372). Only our first depot is looked at,
  and never `~/.julia`. Such a file comes from a precompile whose runtime folder
  was removed while it ran: Julia gives the new file its source's mode, and a
  missing source's is 0 (#383). The smoke runner did that on every task; it now
  waits for the precompile, and `endeavor mcp`'s precompile runs the check again
  when it's done. The same check removes a native image of those packages with
  no `.ji` beside it, once it is an hour old: Julia never uses or removes one
  (#382). Each runtime folder (one per build) compiles its own set; once the
  folder is gone (a day after its last use), a start removes the set whose
  `.ji` names its source, so old sets don't pile up to Julia's limit of 10 per
  package. Each start marks the sets whose source it sees, and a set is removed
  only after 3 days with no mark, so a cluster whose cache folder is per node
  but whose depot is shared keeps the sets another node still uses (#528).
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
  (#176). Its subagents share that key, and Claude Code passes nothing per
  subagent (a tool call's `_meta` has only its `toolUseId`), so two that work in
  one notebook at once count as one reader and get no `stale_read` or
  `run_conflict` from each other. The skill tells agents not to do that (#252).
  The plugin's `serve`, `mcp` and machines
  pass the flag. On Slurm the login node looks for Julia before it submits the
  job and passes it on when it finds one; when it doesn't, the job is submitted
  without asking about Julia, which an R notebook never needs (#80). The job's
  core then looks for Julia on the node when a Julia notebook first needs it,
  and never downloads it there (compute nodes often have no internet): the
  user's yes (`use_machine` with `install: true`) has the login node find or
  download Endeavor's own (`files::Request::Julia`), and the core is told its
  path (`endeavor/use_julia`). A download that takes longer than the call
  says so, and the next `use_machine` with `install: true` finishes it.
  Checked with the stand-in Slurm in the tests; not yet on a real cluster. Test connection (`client::test`) that
  finds a runtime already running has it start Julia and waits for it, so it
  fails when that server's Julia can't start, rather than pass on a ping
  (Endeavor #169). It downloads Endeavor's Julia only when the test may install
  (the app's may: an empty Julia field means Endeavor's own); otherwise a server
  with no Julia says "Julia isn't installed there". A session whose folder has Pluto
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
  The interface number is 9. An install (Endeavor's own R, or Ember the first
  time) that finishes after the agent's call stopped waiting goes straight on
  to start R, as Julia's start does, so the pane goes from the install's step
  to "Starting R" and the agent's next call finds R ready (#148).
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
- A stop signal to the core (SIGTERM, SIGINT, SIGHUP) asks a ready Julia to shut
  down through its bridge, and passes the signal on only if that fails or Julia
  still runs 10 s later; 10 s after that it kills Julia. A second signal is passed
  straight on. A SIGTERM that reaches Julia while it compiles can hang its exit for
  good (#509). A Julia still starting has no bridge yet and gets the signal. The
  core still ends as the signal would end it.
- The shutdown stops every notebook's process itself, all at once, each within
  about 2 s (an exit, then SIGTERM, then SIGKILL), before Julia exits. Malt's own
  stop at exit gives a process busy in a cell 15 s to take its exit, one process at
  a time, so a stop during a long cell took 12 to 15 s in two tries of three. Now
  they take 1.3 to 4.2 s. It then closes Pluto's server and the bridge, so `exit`
  doesn't tear down a connection mid-write (each stop logged HTTP.jl's "stream not
  initialized" error), and a notebook's process it stopped isn't logged as one
  that ended by itself.
- When the core dies abruptly (an OOM kill, `kill -9`), a small shell the core
  started with Julia stops it. It waits on a pipe only the core holds, so it costs
  nothing while the core lives and ends with Julia. On Linux, Julia has had SIGTERM
  already (`PR_SET_PDEATHSIG`); on macOS the shell sends it. 10 s later the shell
  kills Julia if it still runs. A notebook's process busy in a cell goes on until
  the cell ends. On Windows the core's job ends Julia and its notebook processes
  with the core, at once.
- Stop on a Julia cell that waits (`sleep`, a download, a lock) stops the cell and keeps the
  notebook's process and its values (#139). On Julia 1.12 a Ctrl+C sent to the process
  never reaches a waiting cell, so Pluto pressed it until Julia ended, which lost every value
  and read as a crash. The runtime now notes each cell's task in the notebook's process and
  stops that task, for the agent's `interrupt_notebook` and for the page's Stop button alike;
  only a cell that never yields still gets Pluto's Ctrl+C. It is a hook into PlutoRunner
  (`_STOP_HOOK` in `Lifecycle.jl`), only on the Pluto releases it was checked on: on any
  other, Stop is Pluto's own, as before.
  In Julia, an interrupt while the notebook's process is still starting stops its
  run before any cell, and the answer lists them all as `dropped` at once (#179).
  Before, it reported them dropped, or none when the start took longer than its
  wait, and they all ran once the process was up.
  A notebook whose process ended while it started can be restarted, from the app
  or from Pluto's own restart on its page, and its cells run (#194). Before, the restart failed with the start's error and
  its cells stayed queued until the notebook was stopped and opened again.
- The user's own Julia and R first (#367, #370). The app on this computer now
  runs `--julia auto` like servers and the plugin: the user's Julia (login
  shell, juliaup) when it's 1.12 or newer, else Endeavor's. The app's
  Settings give a `julia::Preference` and `julia::choose` decides, so the
  choice lives here, not in the app. The user's depot goes behind ours
  (`paths::with_user_depot`): a trailing `:` alone gave Julia only its bundled
  depots, so a LocalRegistry and the user's packages were never found. R
  Pkg updates only the first depot's registries, so the runtime updates the
  ones in the user's depot itself (at most hourly) before Pluto opens a
  notebook (`Adapter.update_user_registries`). R notebooks read the user's
  `~/.Renviron` and `~/.Rprofile` (no more `--vanilla`; on Windows R's own
  rules for `.Renviron`). The adapter runs the profile itself
  (`run_user_profile`), so an error in it is logged instead of stopping R, and
  R's stdout goes to the log, never to the MCP channel. The user's R runs ahead
  of Endeavor's own R. juliaup's launcher gives way to the Julia it would start
  (its `Sys.BINDIR`) on every platform, not only Windows. On a juliaup with no
  channel yet (its listing has no default), the launcher isn't run, and there is
  no Julia of the user's, so Endeavor's channel goes in as juliaup's default. Run,
  the launcher would have installed juliaup's latest Julia (1.13.4) and
  notebooks would have run on it (#512). Julia 1.11 is refused
  (the runtime's manifest doesn't instantiate on it). E2E runs the Julia tests
  daily on the newest release and pre-release.
  `endeavor/julia_status` says which Julia a start found (`julia`: `path`,
  `version`, `endeavors`) and `endeavor/r_status` which R runs (`r`:
  `rscript`, `endeavors`), so the app shows the runtime's choice instead of
  making it again.
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
- A Julia that juliaup removes while the runtime runs on it (`juliaup update`
  removes the version it updates past) leaves the runtime unable to start a
  notebook's process, which is that same Julia started again. Restart, open,
  new and Run notebook then refuse with `julia_removed`, before they stop
  anything, and Pluto's page Restart does nothing; restarting Julia fixes it
  (#346). A client that finds such a runtime on this computer with no notebook
  open stops it and starts a new one on the Julia there now, as for an older
  version's (#347): the app's helper always, a plugin's front once, when the
  runtime ends when idle and the call may start one. With a notebook open it
  is kept, and its calls say why.
- marimo after Ember.
