# The front: `serve`, `mcp` and the plugin

`standalone.rs` and `standalone/`: the notebook tools without the app
([serve.md](serve.md) is the user's guide, and the design is in
[plugins-and-remote.md](plugins-and-remote.md)). The user starts the runtime
themselves, as they would Pluto or Jupyter, and points their agent and
browser at its one port.

- `serve` starts the runtime in the foreground, or uses the one running from
  its state folder, and prints the browser link, the MCP URL and token, agent
  configs and the `ssh -L` line. Ctrl-C (on Windows, also closing the
  console) stops a runtime it started.
- `mcp` is the stdio form for an agent on the same machine, which the plugins
  use. It starts or reuses the runtime in the background, where it outlives
  the agent and ends itself when idle, and relays MCP between stdin/stdout and
  the runtime's `/mcp`. It is called "the front" below.
- `stop` ends the runtime running from the state folder.
- `open` lets the user's browser in, with the link that holds the token.
- `status` says what Endeavor has on this computer and changes nothing.
- `hook` is Claude Code's PreToolUse hook (see "Claude Code's mode").

## The runtime's files

- `runtime/` is built into the binary and unpacked on first use to a folder
  named by its version, so the binary is all a user installs. Each unpack
  writes to a staging folder of its own per call, then renames it into place,
  so a folder by that name is complete. One staging folder per process wasn't
  enough: a server's precompile thread unpacks while the server does, and one
  call cleared it as the other moved it into place (#363). A full disk
  doesn't keep each try's partial copy.
- Whoever uses an unpacked folder holds a shared lock on its `IN_USE` file
  (`Lease`), and touches it now and then. A new version's unpack removes the
  folders nobody holds and nobody used for `KEEP_UNUSED`, and staging folders
  an unpack left more than `KEEP_PART` ago (#365). The lock alone may not
  show: a home folder a cluster's nodes share may have locks only their own
  node sees. A folder with no `IN_USE` came from a build before leases and is
  left alone, since a runtime may still run from it. `--cache` can name a
  folder that also holds the user's own files (#126), so only folders named
  as `unpack` names them (a crate version, then `-`) are touched.
- With no `--state-dir`, `mcp`, which lives long and starts its runtime again
  after a stop, picks between the plugin's folder and the app's older one
  before each start; the others pick once (#434). While a runtime is
  starting or in use, its folder is kept, so one alive in the other folder
  never makes it look gone.
- Julia is found and started only when something needs it, so an agent that
  only opens R notebooks never needs Julia.

## Precompiling after an update

After an install or an update, the front precompiles the runtime and
`EndeavorRuntimeWarm` in the background, at low priority, before any
notebook needs them. The runtime loads `EndeavorRuntimeWarm` only once it is
precompiled, and an update leaves it stale: until the runtime got round to it
(after 30 s quiet), the first opens ran without it, about 20 s slower each
(measured 2026-11-11).

- Only where Julia notebooks ran before (an earlier runtime's compiled code
  is in the depot). Someone who only opens R notebooks never needs Julia.
  It never downloads Julia, and a failure only loses the speedup.
- The Julia process outlives the server, so a short session doesn't throw
  the work away. It takes the lock (`.precompiling`: one precompiles, the
  rest exit), and writes `.precompiled` when done, or `.precompile-failed`,
  after which it waits a day. A folder done once for a depot and a Julia
  version is never done again; `juliaup update` does it again.
- It leaves a folder whose packages aren't all installed to the runtime's
  own `Pkg.instantiate`, since two installs into one depot at once aren't
  safe, and removes compiled files of ours that can't be read
  (`runtime/cache_guard.jl`), before and after: a runtime folder removed
  while it ran leaves the file Julia was writing unreadable (#383).
- It runs under `nice` where there is one; on Windows it lowers its own
  priority class. Its flags must keep the compile cache's key the same as
  the runtime's (`core::julia_command`): a flag such as `-O` on one side only
  wastes the work.

## Starting and stopping

- `DIR/start.lock` is held by `serve`, `mcp` and `connect` while they find or
  start a runtime. A wait for another process's start gives up after
  `start_lock_limit()`, well past a first start's Julia download and Pluto
  precompile. A stop waits `stop_lock_limit()` for it; the stop then takes up
  to about 20 s more, within the client's 60 s.
- `serve` takes Ctrl-C, SIGTERM and SIGHUP on a thread of its own, set up
  before any other thread starts so they all inherit the mask. On Windows the
  console closing (or a log-off) ends the process a few seconds after it
  says so, too soon to ask Julia to shut down. The runtime has a console of
  its own, so none of these reach it.
- A runtime another build started: one that offers this build's interface is
  used as it is. Otherwise, once per runtime, it is stopped if a front started
  it in the background (it ends itself when idle, so nobody relies on its
  port), no notebook is open in it, and the call may start one. Any other is
  kept and the agent is told; doing this once keeps two fronts of different
  builds from taking turns. A runtime whose Julia was removed is handled the
  same way (#347). A machine's is never stopped for the agent: on a cluster
  that would give up the job's allocation.
- A runtime found running here with other network settings than this
  session's is mentioned once by variable name, since its downloads use the
  settings of the session that started it (#93).
- The user hears on stderr where the notebooks are, never the token: agents'
  clients keep stderr in their logs.

## The relay

- Each agent session has one key (`X-Endeavor-Session`) on every runtime it
  uses. Claude Code gives its MCP servers `CLAUDE_CODE_SESSION_ID` and keeps
  it when a conversation is resumed, so a resumed conversation is the same
  session, with its reads, edits and notebook (#176). A process Claude Code's
  shell started (`CLAUDE_CODE_CHILD_SESSION`) isn't that conversation.
- A front started with `--no-folder` takes the project folder from Codex's
  `_meta["x-codex-turn-metadata"].workspaces` (undocumented, #99): Codex
  starts a plugin's server in the plugin's folder and names no project. Only
  a single existing absolute folder counts; with several, a relative path
  would be a guess.
- To learn whether a client that serves several conversations on one
  connection names them, the front logs the keys of each call's `_meta`, and
  for the first calls a salted fingerprint of each value, never the value.
- The front lists its own build's tools, so it checks every call against
  them, runtime or not. A host-tool refusal comes before the check of the
  arguments, as the runtime's does; a path the runtime would refuse is
  refused before a runtime starts; only a call of a tool this build has
  starts one.
- Each call goes on its own thread: a run can take minutes, and a
  cancellation must get through meanwhile. A call's whole time counts from
  its arrival, and every wait for a runtime comes out of it.
- A relayed call waits `start_wait()` for a runtime still starting before it
  says so, under the time agents give a call.
- The front's headers: `X-Endeavor-Client: plugin`, `X-Endeavor-Consent:
  front`, `X-Endeavor-Mode: auto` in Claude Code's auto mode,
  `X-Endeavor-Front-*` naming its process (app #232), and `X-Endeavor-
  Notebooks: several` for a client that draws views. See
  [mcp-server.md](mcp-server.md).
- A call and its route are taken together, so a call that races a move of
  the session goes whole to the runtime it was routed to.
- Answers go out one JSON-RPC message per line, as stdio needs; a pretty-
  printed reply is put on one line.

## The browser

- The link that lets a browser in carries the runtime's token, so it goes
  from this process to the browser and never through the agent. Tool results
  carry the link without it.
- `new_notebook` and `open_notebook` open the notebook in the browser each
  time, so asking the agent to open it again works, except within
  `REOPEN_AFTER` of the same one: agents call `open_notebook` on an open
  notebook to get its id, sometimes several times a turn.
- A client that draws views (Claude's desktop app) shows a notebook on this
  computer beside the chat, and no tab opens. A notebook on a machine still
  opens a tab. Python notebooks have no view page yet (marimo's editor is
  21 MB of lazily loaded modules).
- This process shows a browser unless it was started over ssh, or on Unix
  other than macOS has no display. Where no browser opened (a display that is
  set but gone, as in a tmux session left from a desktop login, #299), the
  agent is told the address first and the port forward only if it doesn't
  load: a virtual machine that shares its ports reaches `localhost` as is,
  and one entered with `orb run` has the same ssh variables as a real ssh
  login, which doesn't. The agent can't forward the port, since the forward
  starts on the user's computer; `endeavor open` lets the browser in.
- `endeavor open` prints the link only to a terminal: an agent that runs it
  in its shell would read the token.
- A machine's notebooks come through this process's own port for the
  connection, with the machine's key. When the agent's note is for them, this
  process records the key for that port in `forwarded/<port>.json` in its state
  folder (owner-only, with its pid), and the note names
  `endeavor open --port <port>`. That command refuses a record whose process
  has ended, so a port that is reused later doesn't get the key.
- The opener gets the URL as one argument and no shell reads it (`start` in
  cmd would split it at `&`). On Windows `rundll32` always exits 0, so there
  "opened" means it started. A debug build never opens a real browser.

## Asking the user

Nothing downloads or installs unless the user says yes themselves. The app
holds `approve_install` for the user in every mode. The plugin has no app,
and the agent was trusted to ask first; Haiku 4.5 often didn't ("Let me
approve that for you", #285).

- **In a dialog.** Where the client can show a form (MCP elicitation; an
  empty `elicitation` is the form alone in 2025-06-18), the front asks before
  it passes the call on. The tool's description and the runtime's asking
  step then say that Endeavor asks, so the user isn't asked twice.
- **On a page.** A client that can't show one has the user asked on a page in
  their browser (#388). A client may say it shows forms and not (#381): one
  that answers with an error, or closes the dialog before anyone could read it
  (`DIALOG_GONE`), is taken for one without dialogs from then on. A
  later error, from a dialog that broke while up, isn't. A dialog with no
  answer after `dialog_silent` may not be up at all (Claude Code's `-p` takes
  it and never answers, #396), so the page asks as well and the first answer
  counts.
- **Nowhere.** With no browser, nothing downloads, and the agent is told
  where it can be approved (the app, or a Claude Code session in a terminal),
  since the notebook's page has no button for it (#402).
- The question names the notebook by its whole path, so a yes on one
  notebook is never another's (#389), and the packages, which are all a yes
  approves (#418). A call nothing waited for names no packages, so a package
  held since approves nothing unseen. R's download goes by what downloads, not
  by names. A name a letter or two from a common package's says so (#416).
- A yes at once is a yes (a client may answer for its user, as `-p` with a
  hook does), and so is a no; only a close is too fast to be a person's. A
  cancelled call downloads nothing and gets no reply, even on a later yes.
- A runtime the front can't reach now may be reached by the call itself, so
  the front can't tell nothing waits, and asks without the sizes.
- The machine tools ask the same way before `install: true` (#378). The call
  is made first without `install`, which installs nothing and says what
  would be; only then is the user asked, in a dialog that names it, whether
  or not the agent set `install`: an agent that was to call again for the
  install often didn't (#380). Without dialogs the page opens only once the
  agent sets `install`, or a page would open for each question the agent
  asks. The click and the install fit in `PAGE_CALL`, under the 60 s Codex
  gives a call, and the install gets at least `MIN_INSTALL`.

### The install and sign-in pages

The yes comes through something the agent doesn't get: a token in a tool's
reply wouldn't help, since anything the agent reads it can pass back. The
front opens a page in the user's browser at a link with a one-time secret,
which goes from this process to the browser only.

- Served on 127.0.0.1 only. It answers only its own links, and only a request
  addressed to that loopback port (a page elsewhere that points a name at
  127.0.0.1 sends its own name). The answer is a POST from the page's own form:
  every browser sends its origin and says it came from the same site, so a
  fetch, another host, another page's form or no origin answers nothing. The
  referrer policy isn't "no-referrer", which sends `Origin: null` with the
  page's own form. It can't be shown inside another page.
- A POST's body is read before any answer, the early 404 and 403 too:
  closing a socket with its body unread sends a reset, which the browser
  shows instead of the page (#538).
- The user is asked no more than needed, so an agent that keeps calling can't
  wear them down: after Don't install, the same question is refused without a
  page for `SETTLED`; an unanswered page opens again at most once, a minute or
  more later; a yes holds for the same question for `SETTLED`, so a call that
  stopped waiting isn't asked again. A no may become a yes with the page's own
  button. A question too vague to stand for later calls holds for one call.
- When the dialog beside a page is answered, the page goes, and a click on it
  answers nothing.
- ssh's sign-in questions for the machine tools (#106) use a page too. The
  plugin used to connect in batch mode, so a server that asks for a password
  or a second factor couldn't be reached. Not an MCP dialog: those are for
  what an agent may know, and must not ask for passwords. The answer goes from
  the page to ssh once and is forgotten. A question waits less than sshd's
  2-minute `LoginGraceTime`, so one nobody answers ends as unanswered, which a
  reconnect tries again. Only a connection a machine tool asked for within
  `SIGN_IN_WINDOW` puts questions to the user; once they answered, a second
  question (a code after the password) is theirs too. Without a browser, ssh
  runs in batch mode and signs in with keys only.
- What the pages don't stop: an agent with a shell on this computer can find
  the link (in the opener's command line, or the browser's history) and
  answer first. Such an agent could install things without Endeavor anyway,
  and a wrong sign-in answer only spends a try. The pages stop an agent that
  says yes for the user, not one that works against them.

## Claude Code's mode

In Claude Code's auto mode (and bypassPermissions) the user has said not to
be asked, so a Julia run adds a new package without asking, as in the app's
Auto mode (#397). Claude Code tells an MCP server nothing of its mode, but it
gives it to its hooks: the plugin's PreToolUse hook (`endeavor hook`)
writes it down before each of Endeavor's calls, and the front reads it.

- The hook writes two files: one named by the conversation (the hook's
  `session_id`, which the front knows from `CLAUDE_CODE_SESSION_ID`), and one
  named by the Claude Code process it runs under, `by-process/<pid>`. The
  front registers that file at start with its own pid, and the hook writes it
  for the nearest ancestor whose registered front is still its child. Claude
  Code runs one front per process (subagents share it), so that is one per
  front. A dead front's file under a pid a shell now has is passed over, and a
  Claude Code started from another's shell writes its own front's file.
  `/clear` starts a new conversation in the same front, whose
  `CLAUDE_CODE_SESSION_ID` still names the old one (#505), so the process
  file carries the mode there. Windows has no parent lookup and only the
  conversation's file. The front reads whichever was written last.
- A file older than `FRESH` doesn't count: the hook writes before every call,
  and in auto mode only Claude Code's own check comes between. Anything
  missing, unreadable or old is not auto. Files are written whole and
  renamed, and the hook never fails a call.
- Like `X-Endeavor-Consent`, the files are a declaration, not a check. An
  agent outside auto mode could write "auto" itself, but only with the user's
  OK for a shell command, and only between its call's hook and the call. No
  text an agent reads names them.

## Machines

The front's `target` says where a session's notebooks run: this computer or
one machine. `Local` finds, starts and ends the runtime in the state folder;
a machine's is the front's own `client::Session` to it, one per machine.
Calls to a machine's runtime carry `X-Endeavor-Host` and
`X-Endeavor-Browser-Port`.

- One machine tool runs at a time: they switch, start and stop things. A
  call that doesn't get its turn has changed nothing.
- The helper sent to a machine is this program when the platforms match,
  else the release's. ssh asks through this program (`SSH_ASKPASS`): on Linux
  this process's own link, not `/proc/self/exe`, which in ssh is ssh.
- A connection made with other settings isn't reused. One asked for another
  launcher may run Slurm jobs where the user said not to. A connection is
  replaced only when nothing hangs on it, so an open notebook page's address
  through it doesn't break. An `add_machine` is saved only once it connects;
  every way out that doesn't save it ends the connection. Closing a
  connection runs on its own thread, since a machine may not answer.
- `use_machine` checks everything that can fail first; only when the start
  was taken does the session move and the project remember the machine. A
  remembered folder that is gone can't be fixed by leaving `folder` out. The
  folder is made when only it is missing, before a job is asked for.
- A cluster's job node often has no internet, so Julia is downloaded on the
  login node and the job's core is told where it is (#80). A cluster isn't
  started like a plain server: it needs a job the user agreed to.
- `stop_machine` marks the runtime stopped before ending it, under the lock
  that routing takes, so a notebook call meanwhile doesn't start it again.
  It asks the runtime which other sessions are active; without `force`, a
  start or a queued job is refused, since sessions waiting for it can't be
  seen.
- What each project remembers (the machine its sessions last used, and the
  folder there) is `projects.json` in the state home, keyed by the project
  folder's canonical path, written whole and renamed, readable by this user
  only, under a lock. A file that can't be read is an error that names it and
  is never replaced. A session without a project folder remembers nothing.
- A machine's name is letters, digits, `-`, `_` and `.`, starting and ending
  with a letter or digit.

## `endeavor status`

It only reads: no folder or file made, nothing started or stopped, no ssh or
network beyond up to two local pings of at most 5 s (`look`). It never prints
the token. Folders with more than `SIZE_LIMIT` entries aren't sized, and what
the app and servers install into, and the depot, are named but never sized.

## Test variables

Read only by debug builds (`test_var`), so they can't redirect a shipped
binary: `ENDEAVOR_TEST_SHELL` runs a machine's helper on this computer
through `sh`, with no sshd; `ENDEAVOR_TEST_ROOT`, `_STATE` and `_DEPOT` set its
folders (`{id}` is the machine's id); `ENDEAVOR_TEST_ASK` runs before each
connect; `ENDEAVOR_TEST_JULIA_AT_START` starts Julia with the runtime;
`ENDEAVOR_TEST_BROWSER`, `_INSTALL_PAGES`, `_PAGE_WAIT_MS`,
`_PAGE_SETTLED_MS` and `_SIGN_IN_WAIT_MS` stand in for the browser and its
timing. `ENDEAVOR_START_WAIT_SECS`, `ENDEAVOR_START_LOCK_SECS` and
`ENDEAVOR_STOP_LOCK_SECS` set the start and stop waits.
