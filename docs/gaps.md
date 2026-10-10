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

- No marketplace install of the Claude Code plugin has listed its skills
  ([Releases and plugins](#releases-and-plugins)).
- The Codex plugin has not been installed from GitHub
  ([Releases and plugins](#releases-and-plugins)).
- No person has run the released macOS or Windows binaries
  ([Releases and plugins](#releases-and-plugins)).
- A Mac computer reaching a Linux server is wired and not run, and a Windows
  one only from a debug build ([Releases and plugins](#releases-and-plugins)).

## Releases and plugins

- [P2] **Only `endeavor mcp` and `client::Session` check the runtime they
  use.** A front of `mcp` compares `runtime.json`'s `interface` with its own
  (and the build when the record has no number). On this computer it replaces
  a runtime of another interface that a front started, with no notebook open,
  on the first call that may start one, and otherwise tells the agent once.
  Another session can open a notebook between that check and the stop. On a
  machine (since 2026-10-09, from the build and interface the helper's `Ready`
  carries) it only tells the agent once, in `use_machine` or the next notebook
  call, and `client::Session` reports it as trouble: a machine's runtime is
  never stopped for the agent, since on a cluster that gives up the job.
  A machine's runtime whose `Ready` gives neither its build nor its
  interface is from before runs waited for the user's answer, so for a
  client whose runs wait for it (the app, which sends a policy) the listener
  refuses its code runs (`Messages::no_run_gate`); reading and editing still
  work. The plugin leaves it off: it sends no policy, so no runtime holds its
  runs, old or new.
  `serve` doesn't need it: the agent talks to the core itself.
- [P2] **Old builds are never removed from servers or from the plugin's
  folder.** Every build a server was sent stays in `~/.cache/endeavor/<build>/`
  there (the bootstrap only notes that one is there), and every build the
  plugin fetched stays in `bin/<key>/`. Each is about 5 MB, and a merge that
  changes the build key adds one, so a cluster's home quota fills slowly.
  To close: remove a server's folders of other builds that no recorded runtime
  or job runs from, and the plugin's folders other than the pinned or newest
  build. Careful: removing the folder a running Slurm job's helper runs from
  ends that job's relay.
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
- [P1] **A Mac computer reaching a Linux server is wired and not run, and a
  Windows one only from a debug build.** The release logic is tested with a fake
  release, but `open_session`'s call to it is not exercised across platforms,
  because the tests' ssh stand-in always reports this computer's platform. On
  2026-10-09 a debug `endeavor mcp` on Windows 10 ran `add_machine`,
  `use_machine` and `stop_machine` against a Linux server with Slurm, through
  Windows' OpenSSH 9.5: it fetched the Linux helper from a local `file://`
  release (a debug build has no release key), sent it, and started and stopped
  Julia there. A release build fetching from the real release, the Slurm
  launcher, and a Mac were not run.
- [P2] **The Codex manifest says `"version": "0.1.0"` and never changes.**
  Claude Code's manifest has had no version since 2026-10-10, so Claude Code
  tracks this repository's commits: `claude plugin update`, or the
  marketplace's auto-update (off by default for a marketplace like this one),
  brings each new commit. Codex's manifest and the app's `plugin/` still carry
  `0.1.0`; whether Codex updates a plugin whose version doesn't change is not
  checked. Antigravity's has no version. To act on at the first tagged release:
  give each release its own version.
- [P2] **Every commit that changes the helper's source needs a new pin and a
  new build.** `release-key` must equal `scripts/helpers.sh --key`
  (`plugins.sh check`, run in CI), and the key covers `crates/`, `runtime/` and
  `plugin/`, so a skill edit or a code comment changes it. Each such push to
  `main` publishes five builds, and until Helpers finishes (1 to 6 minutes) a
  plugin updated to that commit says the release doesn't hold its build and to
  reconnect in a few minutes. A
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
- [P2] **The Antigravity plugin was tried on Windows only.** On Windows 10 with
  agy 1.3.2 (2026-10-09), `agy plugin install` from a local `antigravity-plugin`
  folder and from its GitHub `tree/main/antigravity-plugin` URL both worked;
  `agy plugin validate` passes. The plugin landed in
  `~/.gemini/config/plugins/endeavor` (the second folder `mcp_config.json`
  looks in; `~/.gemini/antigravity-cli/plugins` was not used). agy loaded the
  three skills, named the server `endeavor_endeavor` and listed its 34 tools;
  `agy -p` made a notebook, edited and ran a cell and read its output, with
  absolute paths (`--no-folder`). The first start installed Pluto in about
  two minutes; calls answered "starting" after 45 s and the agent called
  again. The repository's own URL installs every plugin it finds and the
  Claude Code one overwrites this one (same name), leaving skills without
  `launch/` or `mcp_config.json`; the README gives the folder's URL. The
  entry passes `--skills plugin` as the others do (since 2026-10-09; not yet
  tried under agy). Not tried: macOS, Linux, interactive agy, the desktop app
  and a marketplace.
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
  running session may use one and the launcher can't tell which. About 5 MB
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

- [P2] **A runtime that is alive and doesn't answer stops new starts until
  someone stops it.** A start asks it again for 30 s (`SILENT_WAIT`) and then
  fails with a message naming its pid and how to stop it; no second runtime is
  started beside it, and it is not stopped for the user, since it may be busy
  with a large computation. Until someone stops it, the session has no
  notebooks. The helper's `check` and an attach-only request report it as not
  running.
- [P3] **A dead core's leftover process group is ended by pid.** Before a
  start, the group of a core that died is stopped, checked by the core's start
  time and boot. On the same boot, a reused pid whose new process made its own
  group and exited leaving members would have that group signalled. On macOS a
  large forward clock jump makes the boot check skip the cleanup.
- [P3] **A stop forces Julia after 15 s.** A stop asks Julia to shut down and
  waits 10 s, then signals the group and waits 5 s, then kills it. A cell that
  ignores the interrupt, or a slow network folder, can be cut off. Pluto writes
  a notebook in place (`write(path, content)`, not a temporary file renamed
  over it), so a kill during that write can leave the file cut short.
- [P2] **A start that hangs, with no client left, is ended only by a forced
  stop.** Clients that wait for it never stop it, and a stop without `force`
  says it is still starting. `endeavor stop --force`, and `stop_machine` with
  `force` on this computer or a machine, end it. There is no deadline for a
  start. (A machine whose helper predates the `force` on its `Stop` still
  says it is still starting; run `endeavor stop --force` there.)
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
  core.** `endeavor stop --force` and `stop_machine` with `force` end the
  core the file names, if the lock is held, the file names
  the computer the stop runs on and a start time, and that process started then (on this
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
- [P3] **A call that reaches the listener just as the helper goes is closed
  with no answer.** The agent sees a bare connection error ("fetch failed")
  instead of "reconnecting by itself"; its next call gets the wording. This
  happens when the helper's channel has ended but the listener hasn't heard
  yet, or when the helper died and the relay can't write to it. A fix would
  wait briefly for the listener to hear of the end and answer from that.

## Slurm

- [P2] **Queued and starting states have not been seen on real Slurm through
  the tools.** The queue here is empty and a job runs at once, so their wording
  is checked against the fake Slurm only. `e2e_machines_slurm` now holds a job
  in the queue (`--begin=now+90`) to give a real one, also for a session that
  attaches to the queued job; it has not yet been run. No agent has followed
  the `endeavor-machines` skill through a cluster.
- [P3] **`use_machine` checks and makes a session's folder on the login
  node.** A folder on a disk only the login node has (`/tmp/...`, a node-local
  scratch) passes the check, or is made there, and is still missing on the
  compute node, so `new_notebook` fails as it did before the check. To close:
  have the runtime check the folder once the job runs, or name node-local
  paths in the refusal.
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
- [P3] **Changing a saved machine between Slurm jobs and direct takes a second
  connection.** A new machine is connected the way it will be saved (the
  `auto` launcher), but a saved one is connected the way it is saved, so a
  runtime running that way is seen and the change refused; once allowed, the
  next call connects again the other way. One computer used both ways would be
  two machines with two names (not tried; [launcher-spike.md](launcher-spike.md)).
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

On Windows only the Antigravity plugin has been run, on one computer, and
the README says so. The Claude Code plugin there was not tried, and Codex is
not supported. These gaps stay open.

- [P2] **Endeavor has run on Windows only by hand.** CI runs `cargo test` on
  `windows-latest`, but every test in `tests/` but `version.rs` is
  `cfg(unix)`, so little more than the unit tests run there (issue #9). The
  first try on Windows 11 found two bugs no test saw: `--julia auto` asked
  `/bin/sh` for julia, and the runtime's log couldn't be emptied (opened to
  append, os error 5). With both fixed, by hand with juliaup's Julia 1.13.1:
  `endeavor serve` started a runtime and `endeavor stop` stopped it, and under
  the Antigravity CLI `endeavor mcp` made a notebook, edited cells and read
  their output. There `new_notebook` started the local runtime itself;
  `use_machine local` was not called with the fix. The console handler and
  the `taskkill` cancel path have not run. A real ssh from Windows to a Linux
  server was run once by hand ([Releases and plugins](#releases-and-plugins)).
  Windows has no Julia download: with no `julia.exe` on the PATH, Endeavor
  says to install juliaup, and with one too old, it says so. With juliaup the
  core starts the `julia.exe` in that Julia's own `Sys.BINDIR`, not juliaup's
  launcher: started through the Store app's alias, the launcher and Julia ran
  outside the core's job and outlived it (issue #55). Host names recorded by
  the runtime are compared without case on Windows (issue #54).
- [P2] **No integration test runs on Windows.** Every file in
  `crates/endeavor-mcp/tests/` but `version.rs` is `#![cfg(unix)]`, so on
  Windows `cargo test` runs only the unit tests: none of connect, session,
  machines, serve or Slurm (issue #9).
  Their ssh and Slurm stand-ins are `/bin/sh` scripts (`tests/common`). The
  channel unit tests also spawn `true`, which only Git's `usr\bin` provides (CI's
  runner has it on the PATH; a plain Windows shell doesn't). To close: stand-ins
  written in Rust (a small test binary), or the scripts run through Git's
  `sh.exe`, and the tests' `cfg(unix)` narrowed to what is Unix only.
- [P2] **On Windows the app can't ask ssh's questions yet.** Since 2026-10-09
  the askpass mode also reaches the app over loopback TCP with a random token
  per listener (`wire::askpass::ADDRESS_ENV`, `TOKEN_ENV`; `client::Asker` is
  the app's side), and `Auth::Env` sets `SSH_ASKPASS_REQUIRE=force` on Windows.
  The token keeps out other users' processes, not the user's own. That was
  tested on Windows 10 with Windows' OpenSSH 9.5, started as Endeavor starts it
  (no console window), against a test sshd: a key's passphrase and an unknown
  host's yes/no went through `client::Asker` and ssh signed in, and a wrong
  token got no answer. A password login was not tried; it is the same kind of
  question. Without `force` Windows' ssh ignores the askpass and waits on a
  console nobody sees, and it cuts a prompt at about 100 characters (a long key
  path). The app still listens only on its Unix socket. To close: the app uses
  `client::Asker` (on every platform, or at least on Windows).
- [P2] **On Windows a key with a passphrase needs ssh-agent, which is off.**
  `endeavor mcp` signs in in batch mode, with no one to ask, so a key with a
  passphrase works only from ssh-agent. Windows ships the ssh-agent service
  disabled, so `ssh-add` fails there until an administrator enables it. On
  Windows a refused sign-in now says how (`client/ssh.rs`, `ADD_KEY`), but the
  user still has to do it once, as administrator.
- [P3] **On Windows a computer name longer than 15 characters moved its state
  folder.** Since #31 the state folder (`serve/<name>`) uses the DNS host
  name, not the 15-character NetBIOS one, so a runtime an older build started
  there is not seen: a new one starts beside it, and the old one runs until
  its idle limit. Such a runtime was already unusable (it counted as another
  computer's). Names that differ only in case keep their folder, since NTFS
  ignores case.
- [P3] **On Windows the ssh can't be chosen.** Endeavor runs Windows' own
  `System32\OpenSSH\ssh.exe` when it is installed, and the PATH's `ssh` only
  when it isn't (`client/ssh.rs`, `ssh_program`), so a user who wants Git's
  ssh, or another build, for its config or agent has no setting for it. To
  close: a setting or an `ENDEAVOR_SSH` variable read before the default.
- [P2] **Under Codex on Windows a runtime would end with the session.** Codex
  puts an MCP server in a job object that its children cannot leave, and ends
  the job with the session (read from its source, not run). Endeavor asks to
  leave the job and starts inside it when refused. To close: start the runtime
  there another way, before Windows is offered.
- [P2] **On Windows the plugins need `sh` on the PATH.** Each plugin's command
  is `sh`, which Git for Windows installs but only puts on the PATH when its
  "optional Unix tools" are chosen. Under agy 1.3.2 started from PowerShell
  without it, the server didn't start and no error showed; with
  `C:\Program Files\Git\bin` added, the launcher downloaded the release and
  ran (the README says to add it, and the `endeavor-setup` skill tells the
  agent to ask for it). Antigravity's `mcp_config.json` has no
  per-system command and no plugin-root variable, so its plugin can't avoid
  `sh`. Claude Code's plugin on Windows was not tried. To close: a `.cmd` or
  PowerShell launcher where the agent allows one.
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
- [P3] **Windows' 260-character path limit can break a package install.**
  Julia's artifacts sit deep under `%LOCALAPPDATA%\Endeavor\serve-depot\artifacts\`.
  On a Windows test machine, with `LOCALAPPDATA` pointed at a folder about 120
  characters long, the first start failed: "Failed to install some artifacts:
  SystemError: opening file" on a file under `include\everest\kremlib\`. The
  same install from a shorter folder worked, and a normal `%LOCALAPPDATA%` is
  much shorter still, but the margin hasn't been measured. A very long user
  name might hit it. Turning on Windows' long paths (`LongPathsEnabled`) may
  help, but that hasn't been tried. PowerShell's `Remove-Item` can't delete
  such a tree; `rd /s /q "\\?\<folder>"` can.

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
  Since 2026-10-10 the smoke suite ([smoke.md](smoke.md)) repeats eight
  notebook tasks with Claude Code, among them a `stale_read`, a `run_conflict`
  and a second notebook asked for in one session; the machine tasks are still
  to write (#26). In its first runs, Claude asked to "open old.jl and tell me
  what it computes" read the file with its own `Read` and never called
  `open_notebook`, though the skill says to open a notebook the user names
  (#52; the task now says the user wants to see it).
- [P1] **A first package install looks like a hang** (#58). When a new
  notebook uses a package the depot hasn't installed yet (DataFrames, Plots),
  Pluto installs it before any cell runs, which takes minutes. Meanwhile
  `read_cell` shows every cell as queued, not running, with no output, and
  nothing in any reply says packages are installing. In the smoke suite
  Claude polled about ten times in under a minute, then ended its turn
  guessing that an install was under way; the user got no result. A new
  user hits this on their first notebook. The smoke suite's N9 runs on an
  empty depot and is expected to fail until this is fixed.
- [P2] **A tool call during a start ends after 45 seconds with `isError`
  true.** The text says Julia is starting and to call the tool again. Codex
  shows it as a failed call, as the trial saw. The 45 seconds are chosen to
  stay under the agents' tool timeouts. To close: decide whether a start still
  under way should be an error; nothing changed for now.
- [P2] **A waited run stops waiting at 45 seconds, and nothing stops a second
  run of the same cells.** The cap is `WAIT_SECONDS` in `notebooks/tools.rs`,
  counted from the start of the tool call (an approval card's time included),
  with a floor of 5 seconds for the wait itself. No argument changes it. It is
  chosen to stay under 60 seconds: Claude Code (2.1.280, the app's pin, and
  2.1.295, checked 2026-10-09) ends every MCP call after 60 seconds by default,
  over HTTP and stdio, whatever progress the server sends; `MCP_TOOL_TIMEOUT`
  or the server's `timeout` raises that. Codex's default is 60 seconds too.
  The call returns with `execution.still_running`
  and the run goes on, watched until it ends. A run accepted is no longer in
  `pending_run` while it runs, for an unwaited run too; so if its task fails
  before the cell runs, the cell is not staged any more. If the agent runs the
  still-running cells again, Pluto queues the run as it does for an unwaited
  one; `run_conflict` only covers another session's edits.
- [P3] **A first package install is waited for only through `read_cell`.**
  Pluto installs and precompiles a notebook's new packages before any cell
  runs, with the cells queued; the first time that takes minutes (#58). The
  runtime reports the step (`packages`: step, packages, seconds so far, the
  log's last line) in snapshots, `list_notebooks` and `pluto_session_status`,
  and a run's receipt and `read_cell` add a `message`. Only `read_cell` on a
  queued cell waits for it, up to 45 seconds a call, so the agent's polls are
  spaced out. Not covered: the new notebook's own Julia starting (seconds,
  not minutes), and a Pluto that changes its status tree's names, which the
  step names come from (`_PKG_STEPS` in `Adapter.jl`; an unknown name is shown
  as is; a change that breaks the read reports no step rather than failing).
  After 10 minutes the text tells the agent to wait only while the log's last
  line changes, since a step can hang (another notebook's install, a registry
  update on a node with no outside network). URLs in that line lose any
  `user:token@`. The runtime reads Pluto's package log, a plain `Dict` that
  Pluto writes from its own task; that is safe while Julia runs one thread,
  not if a user sets `JULIA_NUM_THREADS`.
- [P2] **A call that waits for the user's answer stops waiting at 45
  seconds, or 20 for `open_notebook` and `new_notebook`.** Before, it waited
  as long as the card was up, and Claude Code ended it at 60 seconds: in
  Manual, an unanswered "Let Claude create a notebook?" card outlived its call
  (seen 2026-10-09). The wait leaves room for the work after it: a run waits at
  least 5 seconds more, and the first open in a new Julia takes 20 to 25. The
  call then fails with `waiting_for_user`, and the card stays up. The same call
  made again by the same session, on the same notebook code, waits on the same
  card and gets at once an approval given meanwhile. Any other call by the
  session that changes or runs something takes the card down; reads leave it,
  and the notebook-code check keeps an approval from reaching changed code. So
  does a refusal while no call waits. `waiting_for_user` is an error on
  purpose, unlike `execution.still_running`: nothing happened, and the agent
  must not assume it did. Claude stops retrying after one to four tries, so
  the result also says to tell the user and make the same call when they write
  back. The app keeps a card no call waits on (`waiting: false`) past the
  turn. Approving it does nothing until the user writes. An approval no call
  has taken yet never expires; it stays until the session's next change or
  run.
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

Before the app adopts the library, in this order (assessed 2026-10-08; the
launcher per start, architecture-review.md P6, was decided against on
2026-10-08 and an `auto` launcher built instead; the compatibility rule for
builds that share a runtime was built on 2026-10-08, `interface` in
`runtime.json`, and the `Config` settings and events on 2026-10-08,
`Config::auth`, `exit_idle` and `on_event`; a machine's runtime reports its
`build` and `interface` in `Ready` since 2026-10-09): one real run from a
Mac to the Linux test VM through `client::Session` (the P1 in Releases and
plugins). The rest of this file can wait or go alongside.

- [P2] **The app still compares builds for a runtime it shares.** The
  library compares `runtime.json`'s `interface` with its own
  (`CORE_INTERFACE`) since 2026-10-08, so a front of another build uses a
  runtime that offers the same tools and calls as it is. The app's own rule
  compares the exact build, and once the app and the plugin share a state
  folder a mismatch is the usual case. To close when the app adopts: compare
  `interface` as the library does (`RuntimeInfo::usable_as_is`, which a
  machine's runtime has too, or `endeavor_mcp::usable_as_is` for a record
  the app reads itself).
- [P2] **The app becomes a second pinned consumer of the Helpers release.** The
  app pins this repository by commit, so its helper key is that commit's, and
  servers can only fetch it once Helpers has published it. Push this
  repository and wait for Helpers before an app release that moves the pin. A
  check in the app's CI that the pinned key is published would catch it.
- [P2] **A pruned build stops the binaries that came from it.**
  `scripts/prune-helpers.sh` removes builds nothing recent needs. Every
  installed binary (plugin, install.sh or `endeavor update`, on any OS)
  fetches the Linux helper of its own build to set up a server
  (`release::helper_for`), so once its build's Linux files are gone it can't
  set one up. The Linux files of the newest 150 builds stay, which at
  2026-10-09's pace (19 builds) is about 8 days and at a quieter pace a month
  or more. A plugin installed from an older commit, on a computer that hasn't
  downloaded its binary yet, can't get its binary once the build is past the 3
  days and the last 40 pins (two or three days at that pace). Since
  2026-10-10 both say so: the helper's error says the build is old and was
  removed and to update (when the release's `LATEST` names another build), and
  the launcher says to reconnect in a few minutes if the plugin was just
  updated and otherwise to update it. The launcher can't tell a build not yet
  published from one removed. To close: tagged releases that are never
  pruned, once the plugin is stable enough to release.
- [P3] **The layers under a session still write a few lines to stderr.** A
  session's own progress and trouble go to `Config::on_event` (since
  2026-10-08). Below it, ssh's stderr (`ssh: ...`), lines a login script prints
  before the bootstrap's, the channel's "helper exited" and "sent an unreadable
  message" lines and the listener's first failed accept still go to the
  process's stderr. A desktop app sees none
  of them, but they are diagnostics: a sign-in failure's reason also reaches
  the session's error. To close: an optional log sink in `Options`, the channel
  and the listener, which the session points at `on_event`.
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

- [P2] **CI does not run the end-to-end tests against real Julia or real
  Slurm.** The `e2e_*` tests are ignored by default and are run by hand, on the
  author's Linux workstation; CI runs the stand-in tests on Linux, macOS and
  Windows, and the runtime package's Julia tests on Linux and macOS.
- [P3] **Parallel builds in one target folder break each other's tests.** Test
  binaries from two worktrees have the same names, so a build in one replaces
  the binaries another is running. Give each worktree its own
  `CARGO_TARGET_DIR`.
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
- [P3] **Tests that write a script and run it can fail with "Text file
  busy".** A script written with `fs::write` can still be open in a child that
  another test thread has just forked, so an exec of it fails with ETXTBSY.
  That made a `core` test time out waiting for `runtime.json` about one run in
  thirty (seen as `honors_the_mcp_protocol_version_header` on 2026-10-06 and
  `a_held_call_answers_as_an_event_stream_at_once` in CI on 2026-10-08; the
  core had exited at once). The fake Julia is now written by a child `sh`
  (`common::write_executable`), the helper reports a core that exits before
  its record instead of waiting 20 s, and the fixed-port test retries a port
  another test took. The fake `ssh`, `squeue`, `uname`, `ps` and Slurm tools in
  `connect.rs`, `client.rs`, `launcher.rs`, `machines.rs`, `install.rs` and
  `src/client/ssh/tests.rs` are still written with `fs::write`; none has
  failed. To close: write them with `write_executable` too.
- [P3] **The launcher's tests don't pass under `busybox sh`.** Four launcher
  tests and one install test fail there because busybox runs its own built-in
  `uname`, `mkdir`, `timeout` and `wget` and ignores the fakes the tests put on
  the `PATH`. The scripts themselves were not run by hand under busybox.
- [P3] **The test for a runtime that survives its stop takes about 23 s**: the
  helper waits out its own shutdown and signal limits.

## Not checked or not designed

- [P2] **The MCP token still opens Pluto's page.** Tool results no longer carry
  the runtime's token (`browser_url` has none; the `mcp` front opens the
  notebook in the user's browser itself), but the bearer header an agent's MCP
  client sends is also accepted on Pluto's routes, past the notebook tools and
  the app's run gate. The app relies on it for exports (`pluto::fetch`). A
  separate page token would only help against a client that leaks its MCP
  config: an agent that runs shell commands as the user can read the token
  file either way. That case is concrete for Antigravity in the app: the app
  hands the bearer to its agent in the ACP `mcpServers` headers, and
  Antigravity keeps session data on disk. Revisit before the app ships
  Antigravity. To close: a page token apart from the bearer, given to the app
  with `page_url`, and the bearer refused on page routes.
- [P3] **Opening the browser is not tried on a real desktop by the tests.** A
  debug build never opens one (`ENDEAVOR_TEST_BROWSER` records the link), so
  `open` on macOS, `rundll32` on Windows and `xdg-open` on Linux have not been
  tried on a desktop yet. Each `new_notebook` or `open_notebook` opens a tab, also for a
  notebook that is open already, unless this session opened it in the last 10
  seconds. On Linux a stale `DISPLAY` (an agent started in a tmux session first
  made on the workstation's desktop, then reached over ssh) opens the browser
  on the workstation's screen, not the user's; this can't be told apart.

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

## R notebooks (Ember)

Ember support is being built (status.md); these are its known limits so far.

- **P2: R notebooks are open only to tests.** `open_notebook` refuses an
  Ember file unless a debug build has `ENDEAVOR_TEST_R_NOTEBOOKS`, and
  `new_notebook` makes only `.jl` files. The tool descriptions and skills
  don't cover R yet; they come after the agent-loop eval.
- **P2: the app can't say which R yet.** serve, mcp, `connect`, Slurm jobs
  and a machine's `r` in machines.json pass `--r` or `--r-shell` to the core,
  which otherwise uses the login shell's `Rscript`. The app's Settings and
  server dialog don't have the field yet.
- **P3: a new R setting waits for the runtime's next start.** The core is
  given its R when it starts, and R itself starts with the first R notebook.
  A runtime already running keeps the R it was given; nothing says so.
- **P3: Julia's login shell can't be csh or tcsh.** `julia.rs` asks the
  login shell with `-lc`, which tcsh refuses. With `--julia auto` that falls
  back to Endeavor's own Julia; with `--julia-shell` it fails. R's setting
  handles these shells (`r.rs`); Julia's could do the same.
- **P3: R from a shell line on Slurm is untried on a real cluster.** The job
  runs the line on the node when the first R notebook opens; only a fake
  `module` has been tried (`e2e_r`).
- **P2: Ember installs from source.** The first R notebook builds Ember, and
  any of its packages R lacks, which needs a C compiler and CRAN and GitHub
  (codeload.github.com). Tried only where R already had Ember's packages.
  Binaries from Ember's CI would remove the compiler (Ember's gaps).
- **P2: an R process that ends loses its notebooks without a word.** When
  R stops answering, the core drops it and its notebooks: they leave
  `list_notebooks` and the events, and the next R notebook opened starts R
  again. Nothing tells the agent or the user that they went, and R's own
  error is only in the runtime's log. Julia and its notebooks are never
  affected; Pluto's Julia ending still ends the whole runtime.
- **P2: R notebooks are refused on Windows.** Ember, its install and R's
  adapter haven't been tried there.
- **P3: Ember's code and Pluto's differ in small ways the tools can see.**
  Ember drops trailing blank lines and CRLF (the core compares code the same
  way, so authorship holds), runs a cell's stale ancestors first, leaves every
  cell unrun after a restart, and puts printed text and the value in one
  output. The adapter's header lists the rest.
- **P3: notebooks opened from Ember's own start page aren't seen.** Only
  notebooks the adapter opened are in `list_notebooks` and the events.
