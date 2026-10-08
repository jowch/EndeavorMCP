# Gaps

Known limits in the plugin and remote work
([plugins-and-remote.md](plugins-and-remote.md)), kept here to come back
to. Each says what happens, why it was left, and what would close it. Remove
an entry when it is fixed.

Each entry has a priority. **P1** breaks a user's work or blocks offering the
plugin to users. **P2** is wrong or confusing behaviour with a workaround, or
will be expensive to change later. **P3** is polish, a rare edge case, or nice
to have. Within a section, P1 comes first.

_Started 2026-10-06, on the `client-library` branch. Sorted by area and
priority on 2026-10-08._

## Priorities

Every P1:

- A front and its runtime can be of different builds after an update, and the
  agent can't see it ([Releases and plugins](#releases-and-plugins)).
- No marketplace install of the Claude Code plugin has listed its skills
  ([Releases and plugins](#releases-and-plugins)).
- The Codex plugin has not been installed from GitHub
  ([Releases and plugins](#releases-and-plugins)).
- No person has run the released macOS or Windows binaries
  ([Releases and plugins](#releases-and-plugins)).
- A Mac or Windows computer reaching a Linux server is wired and not run
  ([Releases and plugins](#releases-and-plugins)).
- On macOS a core that dies leaves Julia running, and a runtime that doesn't
  answer is replaced without being stopped ([Runtime lifecycle](#runtime-lifecycle)).

## Releases and plugins

- [P1] **A front and its runtime can be of different builds after an update,
  and the agent can't see it.** A runtime outlives the front that started it,
  so after a plugin update the new front attaches to the old build's runtime.
  Only stderr says so (`other_build`, when a front attaches), which the agent
  doesn't read. Each checks a call's argument names against its own build's
  tools, so a newer front's new argument is rejected by an older runtime with
  that runtime's message. It also goes the other way: before the release of
  2026-10-08 the launcher once fetched the then-newest build (key
  `14eb0a67bda7`) and ran it against a runtime of this branch, and it attached
  and answered read-only calls. Nothing compares the two builds before a front
  uses a runtime.
- [P1] **No marketplace install of the Claude Code plugin has listed its
  skills.** `claude-plugin/skills` is a synced copy of `plugin/skills`, like
  the other two plugins', so it survives a Windows checkout without
  `core.symlinks`. The trials loaded the folder with `--plugin-dir` or from an
  empty config directory; none installed from the marketplace after a release.
  To close: install from the marketplace once, now that the first release is
  out, and list the skills.
- [P1] **The Codex plugin has not been installed from GitHub.** With Codex
  0.161.0 on Linux (`codex exec`), `codex plugin marketplace add` of a local
  copy of the repository, which has `.agents/plugins/marketplace.json` (beside
  `.claude-plugin/marketplace.json`; Codex used the first), and `codex plugin
  add endeavor@endeavor` installed `codex-plugin/`, and two runs created and
  reopened a notebook through it. The form the README gives (`codex plugin
  marketplace add jowch/EndeavorMCP`) was not run; the plugin is on `main` now.
- [P1] **No person has run the released macOS or Windows binaries.** The
  Helpers workflow built and published all five platforms for the first time
  on 2026-10-08 (key `ed283c702b22`), with the key check passing on each. CI
  (`ci.yml`) builds and runs `cargo test` on `macos-15` and `windows-latest`,
  and the tests pass there, but CI does not cover real Julia, real ssh or the
  install scripts there. The `darwin-x86_64` build is cross-built on an Apple
  Silicon runner and its key is checked by searching the file for it, not by
  running it. The Linux x86_64 file was downloaded, its checksum checked and
  run; the launcher's `--fetch-only` and `install.sh` were run against the
  release on Linux.
- [P1] **A Mac or Windows computer reaching a Linux server is wired and not
  run.** The release logic is tested with a fake release, but
  `open_session`'s call to it is not exercised across platforms, because the
  tests' ssh stand-in always reports this computer's platform.
- [P2] **The plugin manifests say `"version": "0.1.0"` and never change.** The
  Claude Code and Codex manifests and the app's `plugin/` carry it;
  Antigravity's has no version. This is deliberate until the first plugin
  release. Claude Code caches a plugin by its version, so a new commit with the
  same version likely never reaches a user who installed it. To act on at the
  first release: change the version with each release, or drop it, so that
  updates are delivered.
- [P2] **Every commit that changes the helper's source needs a new pin and a
  new build.** `release-key` must equal `scripts/helpers.sh --key`
  (`plugins.sh check`, run in CI), and the key covers `crates/`, `runtime/` and
  `plugin/`, so a skill edit or a code comment changes it. Each such push to
  `main` publishes five builds, and until Helpers finishes (1 to 6 minutes) a
  plugin updated to that commit says the build is still being published. A
  branch's build is published only by a manual run, so a plugin installed from
  a branch waits for it. To ease: narrow the key to what the binary needs, if
  the skills embedded in it can be checked another way.
- [P2] **`endeavor update` on macOS and Windows has never run.** The logic is
  the Linux code with other asset names, and the Windows rename-aside is
  tested on Linux with the same function; `cfg(windows)` paths are
  compile-checked. The old binary moved aside (`endeavor.exe.old-<pid>`) is
  removed only by a later `update` or install, once nothing runs from it.
- [P2] **The checksum file comes from the same release as the binary.** It
  guards against a damaged or cut-short download and not against a tampered
  release, and the same holds for the install scripts, `update` and the
  fetched server helpers. To close: sign the release, or pin the key in the
  script. For plugin installs, `release-key` could also carry the SHA-256 of each
  platform's binary, which would make the plugin's check independent of the
  release; not built. It needs Helpers to refuse to replace a key's published
  files (a rerun with `--clobber` would break a pinned checksum) and a release
  step that writes the sums into `release-key` after publishing.
- [P2] **A partly failed Helpers run replaces the checksum file** with one that
  lists only the builds it published, so a macOS or Windows build published by
  an earlier run of the same key stops being found until the next successful
  run. The Linux builds are always in it.
- [P2] **The first start can outlast the agent's 30 s limit.** The launcher's
  download has run on Linux (about 4.5 MB for Linux x86_64 at key
  `ed283c702b22`); it was not timed against the limit. The download goes on in
  the background only if the agent doesn't kill the launcher's children;
  either way the next start finishes or redoes it, and `endeavor-setup` tells
  the agent to ask the user to reconnect. Claude Code's hook usually avoids it,
  unless it doesn't run before the server starts (undocumented).
- [P2] **Codex has no early download.** `codex-plugin/` has no hooks, so the
  first start downloads while the agent waits. The plugin's entry names
  `--no-folder`, which only builds from key `ed283c702b22` on understand.
- [P2] **Codex doesn't tell the plugin's server the workspace.** Codex starts
  the server in the plugin's cache folder and gives it no project folder
  (`roots/list` is empty; an entry's `cwd` may only be inside the plugin), so
  the entry passes `--no-folder` and the server refuses relative notebook
  paths. Nothing breaks: the agent gives absolute paths, or gets the refusal
  and retries. Each `tools/call` carries
  `_meta["x-codex-turn-metadata"].workspaces`, an object whose keys are
  workspace paths (undocumented). It is not used. To close: read it, or find a
  documented way, so that relative paths work.
- [P2] **`codex exec` needs `default_tools_approval_mode = "approve"` for the
  server.** Without it every tool that changes something fails with "MCP tool
  call requires approval, but approval policy is never"; read-only tools ran.
  The setting is per server (`[mcp_servers.endeavor]`, or for the plugin
  `[plugins."endeavor@endeavor".mcp_servers.endeavor]`, or `-c
  'mcp_servers.endeavor.default_tools_approval_mode="approve"'`) and keeps
  Codex's sandbox on. Today `tools/list` declares `readOnlyHint` (true for the
  read tools; false for the write tools, `open_notebook` and the machine tools
  but `list_machines`). Whether Codex's approval could be avoided by what the
  tools declare is not looked into. Interactive `codex`, where the user is
  asked, was not tried.
- [P2] **The Antigravity plugin folder was built to its documentation and tried
  on no real install.** The manifest and MCP file names, that a plugin's server
  starts in the project folder, the tool timeout and that it loads the skills
  all come from its documentation, which gives no shape for `mcp_config.json`.
  The README says Antigravity is not supported yet. To close: install it and
  call a tool.
- [P3] **Under Codex a session without a project folder remembers no machine.**
  A project is remembered by its folder, so `use_machine` works, and the next
  session starts on this computer.
- [P3] **Codex does not pass `ENDEAVOR_BIN` to a plugin's server.** It passes no
  part of its own environment (`XDG_*` too); only an `env` object in the plugin
  entry reaches the server. A pre-release trial through the Codex plugin route
  must put `ENDEAVOR_BIN` in that entry, and without it the launcher downloads.
- [P3] **A client of an earlier build of this branch cannot read
  `RuntimeState::Starting`.**
- [P3] **A build whose key the release doesn't hold** (a branch built by hand
  with the variable set, or `main` before the workflow finished) fails with
  "couldn't download" when it needs a helper of another platform, and tries
  again at the next connect.
- [P3] **The kept helper is checked against the checksum recorded beside it.**
  Someone who can write the cache folder can change both; it is owner-only.
- [P3] **Fetched helpers of other builds are removed** when a helper is fetched
  or reused, with no regard for a front of an older build that is about to
  send one: its connect fails ("couldn't open") and the next connect fetches
  it again. On Windows a folder holding a locked file stays until a later
  fetch.
- [P3] **Downloads follow redirects to plain http under wget and PowerShell
  5.1.** curl is pinned to https for redirects; `wget` (used only where there
  is no curl) and `Invoke-WebRequest` on 5.1 have no such switch, and only get
  time limits. The SHA-256 check still applies, though it comes from the same
  release. To close: follow redirects by hand and refuse a non-https one.
- [P3] **Uninstalling a plugin leaves the binary** in
  `~/.local/share/endeavor/bin/`. Old builds there are never removed, since a
  running session may use one and the launcher can't tell which. About 30 MB
  each. To close: delete all but the pinned and the newest at a start, when no
  `endeavor` from there is running.
- [P3] **A launcher killed while it holds the lock** leaves a lock the next
  start takes over once its owner's pid is gone, or once the lock is 20 minutes
  old whatever its pid says. Until then a start waits up to 25 s and fails with
  a message to reconnect. Two starts that take over one stale lock at the same
  moment are told apart by a rename, but a start that judged the old lock stale
  just as another made a new one can move the new one aside (it puts it back
  unless a third start has made one); the worst result is a second download,
  since each download has its own temporary folder.
- [P3] **`ENDEAVOR_RELEASE_URL` is honoured by the launcher and the hook.** A
  hostile value in one session can run whatever it points at in that session,
  but it can't leave anything for others: the binary goes to
  `bin-from/<its address>/`, which a normal start never reads. The variable is
  for tests and development.
- [P3] **`endeavor-setup` is also in the app's embedded plugin**, where it is
  never needed, since the app starts the server. It is left out of
  `notebook_guide`.

## Runtime lifecycle

Starting, stopping, idle exit and updates.

- [P1] **On macOS a core that dies leaves Julia running, and a runtime that
  doesn't answer is replaced without being stopped.** Linux ends Julia with its
  core (`PR_SET_PDEATHSIG`) and Windows with the core's job object; macOS has
  neither. The next start finds the core's pid gone and starts a new runtime
  while the old Julia still runs, so two runtimes hold the same notebook files
  (see "One notebook file open in two runtimes"). A runtime that is alive but
  doesn't answer its pings (`Looked::Silent`) is treated the same way: a start
  says "isn't answering; starting a new one" and leaves the old one running.
  What to do with a runtime that is alive and silent is not decided.
- [P2] **`endeavor update` while a `serve` or `mcp` from that binary runs breaks
  its later starts.** A start runs `endeavor core` from the program's own path
  (`current_exe`). After the binary is replaced, on Linux that path names the
  deleted old file, and on macOS it names the new build, so the core is not its
  front's build. Restarting the front avoids it.
- [P2] **A start that hangs, with no client left, is ended only by a forced
  stop.** Clients that wait for it never stop it, and a stop without `force`
  says it is still starting. On this computer `endeavor stop --force` ends it;
  on a machine see "On a machine, a stop cannot cancel a start another
  connection began". There is no deadline for a start.
- [P2] **On a machine, a stop cannot cancel a start another connection began.**
  The helper's `Stop` has no `force`, so `stop_machine` with `force` ends a
  start that this session's own connection is waiting on, and for another
  process's start the helper still says it is still starting. Run `endeavor
  stop --force` on the machine, with the helper's `--state-dir`. To close: a
  `force` on the `Stop` message.
- [P2] **`starting.lock` is a file lock, which a home folder shared by several
  machines may not carry between them.** A client on another machine may then
  not see a start under way and start a second runtime. A recorded runtime of
  another node is still refused by name (`runtime.json`); this is only for a
  start with no record yet.
- [P2] **The app's runtime on a server stays up when idle**, since the app does
  not pass `--exit-idle`. A runtime a session started exits when idle, so an
  app that attaches to one and leaves it with no notebook open for 48 hours
  sees it end. `runtime.json` and `pluto_session_status` now say which kind it
  is.
- [P3] **A forced stop cancels a start only when `starting.lock` names its
  core.** `endeavor stop --force` and `stop_machine` with `force` on this
  computer end the core the file names, if the lock is held, the file names
  this computer and a start time, and that process started then (on this
  boot). A core of an older build writes no pid, a core that has just taken
  the lock has not yet, and a platform that gives no start time records none:
  the stop then says it can't tell which process is starting Julia and stops
  nothing. Find the process with `ps` and end it, or try again in a moment.
- [P3] **A recorded pid is compared with its start time only where the
  platform gives one.** Linux (`/proc/PID/stat`, with the boot id) and macOS
  (`proc_pidinfo`) do. The macOS code is built, and its unit test run, only by
  CI (`macos-15`); nobody has run it by hand. Only a process that is not there
  counts as not the process when its start time can't be read (no
  descriptors, no memory, another user's process under `hidepid`):
  `kill(pid, 0)` decides then, so a stale pid that another program has may
  count in that moment. When a stale pid is another program's, a runtime's
  leftover notebook workers are not signalled, since their group id may be
  that program's.
- [P3] **A stop that fails while a start is under way** restores the start's
  state only roughly.
- [P3] **A helper started with `--quit-with-client` stops, when its input ends
  during a start, only a runtime it started itself.** One it was waiting for
  (another process began it) is left to finish. Once it is attached, the end
  of input stops it, as before.
- [P3] **A stop that gave up waiting** drops the helper's late answer, since
  its id is no longer waited for. The runtime is watched again at that point,
  so a late refusal leaves it watched; a late success sends no death notice.
- [P3] **A second `StartRuntime` while one is under way** is answered
  `StartFailed` ("already starting"), not queued. On one channel the client
  refuses it before sending anything.
- [P3] **A `Stop` said after a start that waited behind a stop** is not part of
  the first stop's outcome: it ends that start, by the order the client said
  them in.
- [P3] **A start's progress messages name no id.** Only one start is under way
  at a time, and the answer that ends it names its id.
- [P3] **A dropped channel closes the helper's input** instead of sending
  `Detach`. Intended: the helper then applies its own rule
  (`--quit-with-client`). Two effects to know: partly written uploads are not
  discarded on that path, and the drop can block while a send is stuck on a
  full pipe.
- [P3] **`stop()` says "didn't answer"** when its wait runs out at the moment
  the helper goes away, where "the connection closed" would be more exact.
- [P3] **`stop_machine` waits 5 s** for the runtime to say how many other
  sessions called lately, and without `force` refuses when it doesn't come. A
  runtime busy for longer than that needs `force`. A refusal also follows an
  answer that is not well formed, or a call that has no time left.
- [P3] **A session that has left still counts for 15 minutes while its notebook
  is open.** Nothing signs a session out, so `stop_machine` without `force` is
  refused for 15 minutes after the last tool call of another session that
  works in a notebook that is still open, whether the session is still there
  or not. It asks the user, who can say to go ahead. A session that only asked
  for status, or whose notebook has closed, does not hold up a stop.
- [P3] **A session idle for 7 days loses its notebook binding**, and another
  session's call is what prunes it. Intended; the session binds again when it
  next opens the notebook.
- [P3] **A session that goes away leaves its run policy and folder in the
  runtime's memory** until the runtime ends; the 7-day forgetting does not
  clear them. They are a few strings for each session. A session the app drops
  does not clear them either, since the app's call (`endeavor/end_session`) is
  gone.
- [P3] **A runtime already running keeps the idle settings it started with.**
  `mcp --idle-stop` and a changed `ENDEAVOR_IDLE_HOURS` do not reach it;
  `endeavor/set_idle_limit` does. A runtime that exits when idle waits for the
  limit it has now.
- [P3] **`ENDEAVOR_IDLE_CHECK_SECS` panics on a negative or non-finite value.**
  It is a variable for tests.

## Connections and machines

The connection library, the front's connections, the machines file and
sign-in.

- [P2] **Keys only.** A server that asks for a password or a code on every
  login is unsupported. A key with a passphrase needs `ssh-add` first, and a
  new host needs one `ssh <host>` in a terminal. There is no sign-in page.
- [P2] **`list_machines` knows only this process's connections.** A machine the
  front has not connected to is listed as saved and `not connected`, which
  says nothing about whether Julia runs there; listing connects to nothing and
  asks no server. To close: a read-only look at a server that needs its own
  `ssh`.
- [P2] **A server's browser address ends with the session.** The address is a
  port of this computer that the front's connection serves, so it works while
  the session is connected and stops when the front ends; the runtime and its
  notebooks go on, and a new session gives a new address. For a plain server
  the result names the runtime's own port (`remote_port`) and `ssh -L` reaches
  it between sessions. On a cluster the runtime is on a compute node behind the
  login node, and no command is given. A helper from before `remote_port` says
  no port, and the result has null.
- [P3] **Each front has its own `ssh`.** Several agent sessions on one server
  make several connections to it, each with its own helper, and each signs in
  on its own (a key with a passphrase needs the agent to have it already, in
  every session's environment). A subagent's front is one more. Many helpers
  attach to one runtime, which is built to allow it.
- [P3] **A front's status tool waits up to 10 s for a connection it has just
  made** to say whether a runtime is there, so the first
  `pluto_session_status` of a session on a remembered machine can take that
  long over a slow network. It never waits for a start.
- [P3] **A failure is told to every call that asks** until a call asks to try
  again (`use_machine`, or the notebook call after the one that reported it).
  A `list_notebooks` or `pluto_session_status` call on a machine whose
  connection failed (sign-in refused, say) says so each time and does not try
  again; only the next notebook call that needs a runtime does. A runtime that
  ended while the connection was down is kept as a failure with its reason.
- [P3] **A runtime that died while connected is "not running",** not a
  failure: its reason (the job's time limit, say) is in the connection's last
  step, which `pluto_session_status` shows as `step` until something else
  happens, and the next notebook call starts another one.
- [P3] **`list_machines` says "not running" for a local runtime recorded on
  another node** (a state folder shared between computers). A notebook call
  then says where it is running. To close: a third state for "running on
  another node".
- [P3] **A connection keeps the settings it was made with, and a tool that
  names the machine replaces it when the saved settings differ.**
  `use_machine`, `stop_machine` and `add_machine` replace a connection whose
  address, port, Julia or Slurm mode is not the one asked for (the saved
  record, or for `add_machine` the new settings), or refuse in plain words if
  Julia is in use through it. A session that is already working through a
  connection is not checked on each call, so a change made to `machines.json`
  meanwhile reaches it at its next `use_machine`. A machine removed from the
  list keeps its connection, and keeps reconnecting after a drop, until the
  front ends.
- [P3] **A failed `add_machine` that was updating a saved machine ends that
  machine's connection** (it is made again by the next call), since one
  machine has one connection and the new settings took its place while they
  were tried.
- [P3] **An `add_machine` that said "still connecting" leaves its connection
  (not saved, in no list) until the next `add_machine` with the same settings
  continues it, a `use_machine` or `stop_machine` lets it go, or the front
  ends.** Any other end of the call, an error included, ends it at once.
- [P3] **A machine whose id is `local`** (only by editing `machines.json` by
  hand: `add_machine` refuses the name) is taken for this computer, and the
  tools can't reach it.
- [P3] **Machine ids must be lower-case.** A hand-edited id with capitals is
  refused. Generated ids are lower-case already.
- [P3] **Unknown fields of a partition are kept only while the cluster still
  lists it.** A partition that disappears takes its extra fields along; the
  fields of the file, a machine, its cluster and its job defaults are kept.
- [P3] **A session that returns to a runtime is still in its old notebook there
  if that is still open.** It keeps one key for its whole run, so on a machine
  or on this computer it worked in before, `list_notebooks` shows
  `this_session` for that notebook, and `one_notebook` refuses a different
  one. If the notebook has closed or idle-stopped meanwhile, the binding is
  dropped when it is next found out, and the session can make or open
  another.
- [P3] **An install a start needs is not retried by the session by itself.**
  After `needs_install` for a runtime such as Julia it waits for
  `install: true` or for `add_machine` with a `julia` setting.
- [P3] **A start that is already under way takes no later agreement.** A
  `use_machine` without `install` that is still starting when a second one
  with `install: true` arrives keeps its own permission, so it can end with
  `needs_install` for what the start needs; the second call, made again, then
  goes through.
- [P3] **With the helper missing, the question can't name the runtime.** The
  helper is what looks for Julia, so the first question names the helper only;
  `use_machine`'s one yes then covers Julia too, and a start that finds Julia
  missing and had no yes asks a second time.
- [P3] **The look at a server knows the helper's size only for this computer's
  platform** (this program's size plus the runtime's files, not the
  transfer's after compression by ssh). For another platform the helper is
  fetched only once the install is allowed, so the question gives no size. A
  platform the release has no helper for is found out then too.
- [P3] **Julia is not known when a machine is added.** The helper has no call
  for it, so `add_machine` reports `found: []` until the first runtime start.
- [P3] **A runtime without the helper is looked for by `sh`.** The bootstrap
  reads `runtime.json` with shell patterns (the first `pid` followed by a quote
  and a colon, and the first `job` the same way), so a node name that holds
  `pid":` would confuse it. It doesn't check the runtime's port. A process
  counts as Julia's when it is alive and its command (`ps -p PID -o args=`)
  has `core` and `--state-dir`, which is the shape of `endeavor core`; a
  server whose `ps` has no `-p` (busybox) reports it as recorded and alive,
  not checked. A cluster's job counts when the user's `squeue` lists it as
  pending, running or configuring, and is reported as recorded when there is
  no `squeue` or it fails.
- [P3] **An attach is checked again after a reconnect.** Its wish stays an
  attach, so the helper is asked once more whether a runtime runs, where a
  start would be resumed on the reconnect's own check alone.
- [P3] **A closed session leaves three kinds of thread to end by themselves:**
  the one that waits for the helper to go, a start that was under way, and the
  one that asks Slurm for the partitions. They end when the helper does, but
  `close` doesn't join them.
- [P3] **`close` can take up to a minute.** It waits for the supervisor, which
  may be inside a call to the helper that takes up to 60 s (`Channel::files`
  in `reattach`). `reattach` looks at the closing flag between its tries only.
- [P3] **The front's exit can wait up to 5 s** for a connection that hangs (the
  connections close together).

## Slurm

- [P2] **Queued and starting states have not been seen on real Slurm through
  the tools.** The queue here is empty and a job runs at once, so their wording
  is checked against the fake Slurm only. No agent has followed the
  `endeavor-machines` skill through a cluster.
- [P3] **A queued job has no id in the session's status until it is submitted
  by this session.** A session that re-attaches to a job already queued shows
  `queue` but no `job` until it runs, because the library's queued event
  carries no id. A job already running shows its id, node and end time to a
  new session (`e2e_machines_slurm`).
- [P3] **The reason a job ended could come back empty.** `e2e_slurm` failed
  about one run in six on 2026-10-07: a job cancelled from outside was
  reported with no reason, because the helper asked while Slurm still listed
  the job as running or completing. The helper now waits up to 10 s for the
  job's final state and reads the job's log (last 64 KB) on every try; a job
  that ended is never reported with nothing, at worst "Its Slurm job ended."
  The failure was not reproduced in 14 runs, so the cause is inferred (a long
  shutdown trace below Slurm's line) and the fix is not shown by a run. If a
  job stays in COMPLETING past 10 s and its log never says why, the user is
  told only that it ended.
- [P3] **The ssh-to-node relay route is not exercised** by `e2e_slurm`; on this
  workstation the `srun` route is used. Running it would add a host key to
  `known_hosts`.
- [P3] **The end reason read from the job's log** (used where `sacct` is off)
  is not exercised by a real test.
- [P3] **Not run against a host with no Slurm.** On this workstation the helper
  finds Slurm in `/usr/bin`, so the "no Slurm" message and `slurm: true`
  without Slurm are covered by unit tests of the decision only.
- [P3] **A `serve` you started inside your own job** is recorded under the
  compute node's name and isn't found from the login node. Add the node as the
  machine instead.
- [P3] **Adding a cluster takes two connections.** The first connection is made
  as a plain server; `add_machine` ends it once Slurm is found.
- [P3] **Time waited in the queue is not reported**: the session doesn't know
  when a job from an earlier connection was submitted.
- [P3] **Saved extra `sbatch` flags written as a flag and a separate value**
  (`"--qos"`, `"normal"`), from a pasted line in an earlier build or by hand,
  are refused at submit time with a message to write `--qos=normal`.
  `parse_salloc` now writes one entry, but records already saved are not
  rewritten.
- [P3] **Changing a machine between Slurm jobs and direct** drops the saved job
  defaults (partition, resources, account) and is refused only while the
  connection shows a runtime, a start or a job. A runtime or job it doesn't
  know of is not seen.

## Windows

Windows is not offered until these close; the README says so.

- [P2] **No person has run Endeavor on Windows.** CI runs `cargo test` on
  `windows-latest` and the tests pass, but they use a stand-in Julia and fake
  ssh and Slurm. CI does not cover the runtime's detached start with real
  Julia, its console handler, the `taskkill` cancel path, or a real ssh from
  Windows.
- [P2] **Under Codex on Windows a runtime would end with the session.** Codex
  puts an MCP server in a job object that its children cannot leave, and ends
  the job with the session (read from its source, not run). Endeavor asks to
  leave the job and starts inside it when refused. To close: start the runtime
  there another way, before Windows is offered.
- [P2] **The plugin's launcher has never run on Windows.** `install.sh` knows
  Git Bash's `uname` (tested with a fake one), but whether Claude Code can
  start a `sh` script as an MCP command on Windows is undocumented. To close:
  try it under Git Bash, and give Windows a `.cmd` or PowerShell launcher if
  not.
- [P2] **`install.ps1` has never run.** There is no PowerShell here. It is read
  against PowerShell 5.1's behaviour only. To close: run it on Windows against
  a fake release (`ENDEAVOR_RELEASE_URL`) and the real one.
- [P3] **The byte lock on `starting.lock` is not run on Windows.** Windows
  locks bytes against other processes' reads, and std's `File::lock` and
  `try_lock_shared` lock every byte (`LockFileEx` from offset 0 over the
  largest length, read in std's source), so the core locks one byte at offset
  4096, past the few hundred bytes (under 512) the file holds, with
  `LockFileEx` itself and the pid stays readable. It works for any lock from
  offset 0 longer than 4 KiB. CI compiles and tests this on `windows-latest`
  with a stand-in Julia, but the pid and the cancel are not tested there (the
  tests that use them are Unix only). A core that can't write its pid in the
  file does not start.
- [P3] **Windows has no script check.** The tests that run `endeavor-mcp.sh`
  and the bootstrap script against `paths` are `cfg(unix)`. Under Git Bash the
  launcher keeps binaries in `$HOME/.local/share/endeavor/bin`, and `endeavor
  update` finds them through `paths::Env::plugin_bin`, which starts from
  `HOME` if it is a Windows absolute path and else from `USERPROFILE`. Whether
  the two agree there was not checked. `install.ps1`'s
  `%LOCALAPPDATA%\Endeavor\bin` is not in `paths` at all.
- [P3] **No helper for a Windows server or a platform other than the five.**
  The server's `uname` is what is asked for, and the release has no Windows
  server helper.

## Folders and paths

- [P2] **`SCRATCH` is found by two rules.** `serve` and `mcp` read only the
  variable (`Env::from_vars`); a Slurm job (`wire::slurm::scratch()`) also
  asks a login shell. On a cluster that sets it only in the login profile, the
  two use different depots. To close: one rule in `paths`.
- [P3] **The server root ignores `XDG_CACHE_HOME`.** `~/.cache/endeavor`
  (`paths::server_root`, and `c=` in the bootstrap script) is the folder the
  app installs to, so it stays. `serve`'s runtime and the helpers cache do
  honour `XDG_CACHE_HOME`, and the default depot does not. To close: move the
  app too, then make all of them follow it.
- [P3] **Folders below the server root are written by hand where they are
  used:** `<build>` and `depot:` in the bootstrap script, `julia-<version>` in
  `julia.rs`. Only the root is shared with `paths` (and, in the script, pinned
  by a test). The script takes `$HOME` as it is, where Rust accepts only an
  absolute `HOME`.
- [P3] **The script's state folder is pinned on this machine only.** The test
  checks that `gethostname` and `uname -n` agree here. On a server where they
  differ, the script and Rust would name different folders.
- [P3] **The install scripts' default folder is only in shell.** `install.sh`
  (`~/.local/bin`) and `install.ps1` repeat it, and nothing in Rust or a test
  pins them.
- [P3] **`Env::from_vars` still reads the real process** for the home fallback
  (`home_dir`), the working folder and the host name, so a test that fakes the
  variables does not fake those three.
- [P3] **`endeavor status` is limited to this computer, and is read-only.** It
  shows the state folder it is given (the default one, or `--state-dir`), not a
  server's runtime or a machine's connection, and the cluster state folder only
  as the files it holds. It asks a running runtime for up to two pings, each
  limited to 5 seconds in all, so it takes about 10 seconds at most. A ping
  sends the recorded token to the recorded port on 127.0.0.1 (every pinging
  `look` does, including `endeavor status`), and only if the recorded pid is
  the process that started then (its start time is recorded, with the boot id
  on Linux, where it counts ticks after boot). That still leaves a record
  without a start time, which an older build wrote or a Unix system that gives
  none, and one without a boot id, which an earlier build of this branch wrote.
  In those cases a stale record whose port another local program now uses
  would show it the token. It reads `runtime.json` through `State`, while
  `standalone::recorded_folder` and `other_build_than` still read the file on
  their own (their tests write partial records), so the record has two
  readers. `bridge_rpc` (the app's calls) still has per-read timeouts only. To
  see whether a start is under way it takes `starting.lock` shared for an
  instant, as every `look` does. A folder with more than 20,000 entries is not
  sized. Nothing cleans up or uninstalls (see "Uninstalling a plugin").

## Skills and agent text

- [P2] **The skills were tried only in short runs.** On 2026-10-07 Claude Code
  created a notebook, edited and re-ran cells, handled a `stale_read`
  collision, and loaded the plugin's skills with `--plugin-dir`. The
  `endeavor-machines` skill was loaded by name in one run, and an agent drove
  the machine tools through `add_machine`, `needs_install`, `use_machine` and
  switching back. Still untested: the new text against the old (the trial in
  the skills audit) and whether a current model needs any rule that was cut.
- [P2] **A tool call during a start ends after 45 seconds with `isError`
  true.** The text says Julia is starting and to call the tool again. Codex
  shows it as a failed call, as the trial saw. The 45 seconds are chosen to
  stay under the agents' tool timeouts. To close: decide whether a start still
  under way should be an error; nothing changed for now.
- [P2] **A waited run stops waiting at 45 seconds, and nothing stops a second
  run of the same cells.** The cap is `WAIT_SECONDS` in `notebooks/tools.rs`,
  counted from the start of the tool call (an approval card's time included),
  with a floor of 5 seconds for the wait itself. No argument changes it, and a
  real agent client has not been run against it (it is chosen from the 45 to
  60 second tool timeouts). The call returns with `execution.still_running`
  and the run goes on, watched until it ends. A run accepted is no longer in
  `pending_run` while it runs, for an unwaited run too; so if its task fails
  before the cell runs, the cell is not staged any more. If the agent runs the
  still-running cells again, Pluto queues the run as it does for an unwaited
  one; `run_conflict` only covers another session's edits.
- [P2] **An agent can add a machine under a name the user did not give.** Told
  "the machine localhost", a Codex agent called `add_machine` with host
  `self`, an alias `list_machines` showed in `ssh_hosts_not_added`, and told
  the user that alias "reached the requested host", which no tool had said.
  The `host` description and the `endeavor-machines` skill now say to use the
  host the user named, to say which other name was used, and not to claim a
  machine was reached without a tool result. Text only; not tried again.
- [P3] **A new session on a machine starts in the machine's home folder**
  unless `use_machine` is given `folder`. In a trial an agent made a notebook
  there with `new_notebook` and no path. The `endeavor-machines` skill now
  tells the agent to ask the user where it should go (or to pass `folder`)
  before making a notebook, and says the result's `folder` is where new
  notebooks go. Nothing enforces it.
- [P3] **The host tools start Julia on a plain server when none runs.**
  `list_folder`, `read_file` and `run_shell` take the same route as a notebook
  call (`Need::Start` in `standalone/machines.rs`), while the notebook skill
  says the first notebook tool call starts it. Reading a file should not need
  Julia.

## The app and the code

The app (the Endeavor repository) still uses its own connection code, and its
pin of this repository was 84 commits behind `main` on 2026-10-08. That is the plan, not a
defect: the MCP side's design is to settle before the app moves to
`client::Session`.

- [P2] **`client::Session`'s public state lets impossible states through.**
  `Status` is a flat `State` with optional `runtime`, `job`, `queue`,
  `needs_install`, `error` and a `nothing_running` flag beside it, so, for
  example, `Failed` with a runtime or `needs_install` without `NeedsInstall`
  can be built. The single enum carrying its data that
  [architecture-review.md](architecture-review.md) (P3) proposed was not done.
  Cheaper to change before the app adopts it.
- [P2] **Adopting `Session` in the app needs three settings it fixes now:** the
  wording of the listener's messages (`Config::messages` names MCP tools in the
  front's case; the default speaks in the app's words), batch sign-in only
  (`Auth::Batch`) and `exit_idle: true` for a runtime this session starts. Each
  would become a `Config` field.
- [P3] **A session writes its progress and failures to stderr** (`eprintln!`).
  A caller that holds sessions in its own process gets them on its stderr.
- [P3] **The names of the entry points mislead.** `lib.rs`, the crate root,
  holds the helper's connect loop as `serve`, while `endeavor serve` is
  `standalone::serve`; `mcp.rs` is the runtime's MCP server, while `endeavor
  mcp` is `standalone::relay`. To close: move the helper into its own
  `helper.rs`.
- [P3] **Only Julia is installed through the item list.** The engine name picks
  what a start needs at one `match` (`prepare` in `src/lib.rs`); there is no
  registry. `machines.json`'s `julia` field is still Julia's.
- [P3] **`StartOptions::engine` is a string.** A misspelt engine name is caught
  only by the helper, when the start is asked for, not when the options are
  built.

## Tests and CI

- [P2] **CI does not run the tests against real Julia or real Slurm.** The
  `e2e_*` tests are ignored by default and are run by hand, on the author's
  Linux workstation; CI runs the stand-in tests on Linux, macOS and Windows.
- [P2] **The Julia tests are not in CI, and one fails.** `lifecycle: stop
  releases HTTP and Pluto ports` (`runtests.jl:191`) fails, with and without
  the waited-run change of 2026-10-07. `docs/testing.md` has no command for the
  Julia tests; they were run with a depot under `target/tmp`. Not looked into.
- [P3] **A failed or interrupted test run can leave its fake runtime behind.**
  One run left a fake `endeavor core` from the `machines` tests under
  `target/tmp`, which had to be ended by hand. The tests clean up when they
  pass; a panic or a kill skips that. To close: have each test's place end
  what it started when it is dropped, and check for leftovers at the start of
  a run.
- [P3] **Five variables for tests are read by release builds.**
  `ENDEAVOR_START_WAIT_SECS`, `ENDEAVOR_IDLE_CHECK_SECS`,
  `ENDEAVOR_START_LOCK_SECS`, `ENDEAVOR_STOP_LOCK_SECS` and
  `ENDEAVOR_SLURM_POLL_MS` only change a wait or a check interval, and are not
  gated. The variables that redirect ssh to a local shell
  (`ENDEAVOR_TEST_SHELL`, `_ROOT`, `_STATE`, `_DEPOT`, `_ASK`) are ignored by
  release builds, so the tests that set them (`tests/machines.rs` and the
  `e2e_machines*` tests) need a debug build and panic in a release build
  (`common::require_debug_build`).
- [P3] **A runtime that dies in the moment its start succeeds** is handled
  (`Run::Gone`, in `client/session.rs`) but has no test: the order can't be
  forced.
- [P3] **`Outcome::Queued` has no test.** The fake helper has no Slurm, so the
  queued outcome is read from the code and from the e2e Slurm tests.
- [P3] **A start that ended `NeedsInstall` and is begun again when the
  agreement came meanwhile has no test.** Julia is given to every fake machine,
  so the runtime item never appears; the helper item is tested.
- [P3] **The races behind the move counter, `active` and the ops lock** have
  no test that forces them. The counter is tested as a comparison of the
  target before and after a move away and back, the lock by a call that waits
  out its deadline, and the order in `stop_machine` (the target is marked
  stopped before the runtime is ended) by reading.
- [P3] **The Julia decision is tested through the tools** with a fake `curl`,
  not against a real download; its unit test would need a login shell that
  finds no Julia, which a developer's machine often has. That test skips
  itself where a login shell finds Julia.
- [P3] **`a_helper_that_ends_unexpectedly_is_a_drop_and_after_the_client_let_it_go_is_not`
  hung once** in a full workspace run on 2026-10-07 and passed alone and in
  the next full runs. Not reproduced.
- [P3] **`honors_the_mcp_protocol_version_header` failed once.** In one full
  workspace run (2026-10-06) the `core` test timed out after 20 s waiting for
  its core's `runtime.json`; five runs after it passed, three of them of that
  binary alone. Not understood. To close: if it recurs, keep the core's stderr
  in the failure message.
- [P3] **The launcher's tests don't pass under `busybox sh`.** Four launcher
  tests and one install test fail there because busybox runs its own built-in
  `uname`, `mkdir`, `timeout` and `wget` and ignores the fakes the tests put on
  the `PATH`. The scripts themselves were not run by hand under busybox.
- [P3] **The test for a runtime that survives its stop takes about 23 s**: the
  helper waits out its own shutdown and signal limits.

## Not checked or not designed

- [P2] **A second session's edit can replace a first session's edit made a
  moment earlier, and neither is told.** It happens when the second session
  read the cell after the first one wrote it: the edit is then valid for the
  cell as it is. Sessions leave each other no notes. Seen in the trial. Not
  covered by design.
- [P2] **The desktop app and the plugin on the same notebook.** Until the app is
  revised to take its folders from `paths`, it keeps its runtime's state in its
  own folders (its data folder on this computer, `~/.cache/endeavor/state` on a
  server), and the plugin in `~/.local/state/endeavor/serve/<host>`. Each would
  start its own runtime on the file. The README says not to. Not covered by
  design.
- [P2] **Codex beyond `codex exec` on Linux is not checked.** Interactive
  `codex` (where the user is asked to approve a tool), Codex subagents, the
  tool timeout (default 60 s) and Codex on Windows are not tried. Codex's
  end-of-turn SIGTERM to the server's process group was seen on Linux only.
- [P2] **`endeavor serve` over HTTP has not been tried with a real agent
  client.** The skills trial used `endeavor mcp` over stdio.
- [P2] **The app's side of sharing a runtime** is read from its code, not run:
  the shared state folder, detaching on quit, the build comparison that holds
  back runs, and a version on its `/endeavor/…` calls.
- [P2] **Gatekeeper and SmartScreen** behaviour for a binary installed with
  `curl` is not checked.
- [P3] **One notebook file open in two runtimes**, such as a shared disk opened
  on two servers. Each runtime saves over the other. Not covered by design.
- [P3] **`run_conflict` was not provoked in the trial** with the real Claude
  Code CLI. It is covered by the unit tests only.
