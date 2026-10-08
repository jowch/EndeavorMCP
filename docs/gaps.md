# Gaps

Known limits in the plugin and remote work
([plugins-and-remote.md](plugins-and-remote.md)), kept here to come back
to. Each says what happens, why it was left, and what would close it. Remove
an entry when it is fixed.

_Started 2026-10-06, on the `client-library` branch._

## The connection library

- **A start cut short by a lost connection is reported as "not running".**
  After a reconnect the session only re-attaches to a runtime the helper can
  see. A core that was still booting when the connection dropped has no
  `runtime.json` yet, so the helper's runtime check says nothing is running,
  though the core goes on and records itself, and the next `StartRuntime`
  waits for it (`starting.lock`). The session never starts one by itself; the
  next `use_machine` does. To close: have the check report a start under way.
- **The test for a connection lost during a start doesn't force the order
  of messages that caused the bug.** It passes on the old code too. The fix
  is covered by reading, not by the test.
- **A runtime that dies in the moment its start succeeds** is handled
  (`Run::Gone`, in `client/session.rs`) but has no test: the order can't be forced.
- **A stop that fails while a start is under way** restores the start's
  state only roughly.
- **`Outcome::Queued` has no test.** The fake helper has no Slurm, so the
  queued outcome is read from the code and from the e2e Slurm tests.
- **A closed session leaves three kinds of thread to end by themselves:** the
  one that waits for the helper to go, a start that was under way, and the
  one that asks Slurm for the partitions. They end when the helper does, but
  `close` doesn't join them.
- **A session writes its progress and failures to stderr** (`eprintln!`). A
  caller that holds sessions in its own process gets them on its stderr.
- **`close` can take up to a minute.** It waits for the supervisor, which may
  be inside a call to the helper that takes up to 60 s (`Channel::files` in
  `reattach`). `reattach` looks at the closing flag between its tries only.
- **Adopting `Session` in the app needs three settings it fixes now:** the
  wording of the listener's messages (`Config::messages` names MCP tools in the
  front's case; the default speaks in the app's words), batch sign-in only
  (`Auth::Batch`) and `exit_idle: true` for a runtime this session starts. Each
  would become a `Config` field.
- **A start that ended `NeedsInstall` and is begun again when the agreement
  came meanwhile has no test.** Julia is given to every fake machine, so the
  runtime item never appears; the helper item is tested.
- **An attach is checked again after a reconnect.** Its wish stays an attach,
  so the helper is asked once more whether a runtime runs, where a start would
  be resumed on the reconnect's own check alone.

## The front's connections

- **`list_machines` knows only this process's connections.** A machine the
  front has not connected to is listed as saved and `not connected`, which says
  nothing about whether Julia runs there; listing connects to nothing and asks
  no server. To close: a read-only look at a server that needs its own `ssh`.
- **A server's browser address ends with the session.** The address is a port of
  this computer that the front's connection serves, so it works while the
  session is connected and stops when the front ends; the runtime and its
  notebooks go on, and a new session gives a new address. For a plain server the
  result names the runtime's own port (`remote_port`) and `ssh -L` reaches it
  between sessions. On a cluster the runtime is on a compute node behind the
  login node, and no command is given. A helper from before `remote_port` says no
  port, and the result has null.
- **Each front has its own `ssh`.** Several agent sessions on one server make
  several connections to it, each with its own helper, and each signs in on
  its own (a key with a passphrase needs the agent to have it already, in every
  session's environment). A subagent's front is one more. Many helpers attach to
  one runtime, which is built to allow it.
- **A front's status tool waits up to 10 s for a connection it has just made**
  to say whether a runtime is there, so the first `pluto_session_status` of a
  session on a remembered machine can take that long over a slow network. It
  never waits for a start.
- **A failure is told to every call that asks** until a call asks to try again
  (`use_machine`, or the notebook call after the one that reported it). A
  `list_notebooks` or `pluto_session_status` call on a machine whose connection
  failed (sign-in refused, say) says so each time and does not try again; only the
  next notebook call that needs a runtime does. A runtime that ended while the
  connection was down is kept as a failure with its reason.
- **A runtime that died while connected is "not running",** not a failure: its
  reason (the job's time limit, say) is in the connection's last step, which
  `pluto_session_status` shows as `step` until something else happens, and the
  next notebook call starts another one.
- **A failed `add_machine` that was updating a saved machine ends that
  machine's connection** (it is made again by the next call), since one machine
  has one connection and the new settings took its place while they were tried.
- **A machine whose id is `local`** (only by editing `machines.json` by hand: `add_machine` refuses the name) is taken for this computer, and the tools can't reach it.
- **On this computer a stop never cancels a start under way,** with `force`
  either (it says Julia is still starting); on a machine `force` cancels it.
- **The test variables are `ENDEAVOR_TEST_SHELL`, `_ROOT`, `_STATE`, `_DEPOT` and
  `_ASK`**; `{id}` in the last four stands for the machine's id, so that two
  machines in one test don't share a runtime. They work in release builds.

## The machines file

- **A connection keeps the settings it was made with, and a tool that names
  the machine replaces it when the saved settings differ.** `use_machine`,
  `stop_machine` and `add_machine` replace a connection whose address, port,
  Julia or Slurm mode is not the one asked for (the saved record, or for
  `add_machine` the new settings), or refuse in plain words if Julia is in use
  through it. A session that is already working through a connection is not
  checked on each call, so a change made to `machines.json` meanwhile reaches it
  at its next `use_machine`. A machine removed from the list keeps its
  connection, and keeps reconnecting after a drop, until the front ends.
- **An `add_machine` that said "still connecting" leaves its connection
  (not saved, in no list) until the next `add_machine` with the same settings
  continues it, a `use_machine` or `stop_machine` lets it go, or the front
  ends.** Any other end of the call, an error included, ends it at once.
- **Unknown fields of a partition are kept only while the cluster still lists
  it.** A partition that disappears takes its extra fields along; the fields
  of the file, a machine, its cluster and its job defaults are kept.
- **Machine ids must be lower-case.** A hand-edited id with capitals is
  refused. Generated ids are lower-case already.

## Stopping and the client

- **A start that hangs, with no client left, is ended only by ending its
  process by hand.** Clients that wait for it never stop it, and a stop says it
  is still starting. Find the core with `ps` and end it.
- **`starting.lock` is a file lock, which a home folder shared by several
  machines may not carry between them.** A client on another machine may then
  not see a start under way and start a second runtime. A recorded runtime of
  another node is still refused by name (`runtime.json`); this is only for a
  start with no record yet.
- **A helper started with `--quit-with-client` stops, when its input ends
  during a start, only a runtime it started itself.** One it was waiting for
  (another process began it) is left to finish. Once it is attached, the end
  of input stops it, as before.

- **A stop that gave up waiting** drops the helper's late answer, since its
  id is no longer waited for. The runtime is watched again at that point, so a
  late refusal leaves it watched; a late success sends no death notice.
- **A second `StartRuntime` while one is under way** is answered
  `StartFailed` ("already starting"), not queued. On one channel the client
  refuses it before sending anything.
- **A `Stop` said after a start that waited behind a stop** is not part of the
  first stop's outcome: it ends that start, by the order the client said them in.
- **A start's progress messages name no id.** Only one start is under way at
  a time, and the answer that ends it names its id.
- **A dropped channel closes the helper's input** instead of sending
  `Detach`. Intended: the helper then applies its own rule
  (`--quit-with-client`). Two effects to know: partly written uploads are
  not discarded on that path, and the drop can block while a send is stuck
  on a full pipe.
- **A session idle for 7 days loses its notebook binding**, and another
  session's call is what prunes it. Intended; the session binds again when
  it next opens the notebook.
- **`stop()` says "didn't answer"** when its wait runs out at the moment the
  helper goes away, where "the connection closed" would be more exact.
- **The test for a runtime that survives its stop takes about 23 s**: the
  helper waits out its own shutdown and signal limits.

## Idle exit

- **The app's runtime on a server stays up when idle**, since the app does not
  pass `--exit-idle`. A runtime a session started exits when idle, so an app
  that attaches to one and leaves it with no notebook open for 48 hours sees
  it end. `runtime.json` and `pluto_session_status` now say which kind it is.
- **A runtime already running keeps the idle settings it started with.** `mcp
  --idle-stop` and a changed `ENDEAVOR_IDLE_HOURS` do not reach it;
  `endeavor/set_idle_limit` does. A runtime that exits when idle waits for the
  limit it has now.
- **`ENDEAVOR_IDLE_CHECK_SECS` panics on a negative or non-finite value.** It
  is a variable for tests.

## Windows

- **Compile-checked only.** The runtime's detached start, its console handler,
  the `taskkill` cancel path and the machines and projects file locations
  have never run on Windows.

## Slurm

- **The ssh-to-node relay route is not exercised** by `e2e_slurm`; on this
  workstation the `srun` route is used. Running it would add a host key to
  `known_hosts`.
- **The end reason read from the job's log** (used where `sacct` is off) is
  not exercised by a real test.
- **The reason a job ended could come back empty.** `e2e_slurm` failed about
  one run in six: a job cancelled from outside was reported with no reason,
  because the helper asked while Slurm still listed the job as running or
  completing, and gave up after 1.5 s. The helper now waits up to 10 s for
  the job's final state, except when the runtime already said how it ended
  (then it tries three times, 1.5 s, as before). The slow case did not come up again in eight runs,
  so the fix is not shown by a run.
- **A queued job has no id in the session's status until it is submitted by
  this session.** A session that re-attaches to a job already queued shows `queue`
  but no `job` until it runs, because the library's queued event carries no
  id. A job already running shows its id, node and end time to a new session
  (`e2e_machines_slurm`). Queued and starting states have not been seen on
  real Slurm through the tools: the queue here is empty and a job runs at
  once, so their wording is checked against the fake Slurm only.
- **A `serve` you started inside your own job** is recorded under the
  compute node's name and isn't found from the login node. Add the node as
  the machine instead.

## Sign-in

- **Keys only.** A server that asks for a password or a code on every login
  is unsupported. A key with a passphrase needs `ssh-add` first, and a new
  host needs one `ssh <host>` in a terminal. Planned: a sign-in page on a
  loopback port.

## Releases and installing

- **`install.ps1` has never run.** There is no PowerShell here. It is read
  against PowerShell 5.1's behaviour only. To close: run it on Windows
  against a fake release (`ENDEAVOR_RELEASE_URL`) and the real one.
- **The workflow's macOS and Windows rows and the key check have never
  run.** They run on the next push to `main`; the YAML parses and was read
  through. The `darwin-x86_64` build is cross-built on an Apple Silicon
  runner and its key is checked by searching the file for it, not by running
  it. Nothing has run the macOS or Windows builds, so a Mac or Windows
  binary may fail to link or start. Until a release holds them, the install
  scripts and `endeavor update` on those platforms find no asset and say so.
- **`endeavor update` on macOS and Windows has never run.** The logic is the
  Linux code with other asset names, and the Windows rename-aside is tested
  on Linux with the same function; `cfg(windows)` paths are compile-checked.
  The old binary moved aside (`endeavor.exe.old-<pid>`) is removed only by a
  later `update` or install, once nothing runs from it.
- **The checksum file comes from the same release as the binary.** It guards
  against a damaged or cut-short download and not against a tampered
  release, and the same holds for the install scripts, `update` and the
  fetched server helpers. To close: sign the release, or pin the key in the
  script.
- **A Mac or Windows computer reaching a Linux server is wired and not run.**
  The release logic is tested with a fake release, but `open_session`'s call
  to it is not exercised across platforms, because the tests' ssh stand-in
  always reports this computer's platform.
- **A build whose key the release doesn't hold** (a branch built by hand with
  the variable set, or `main` before the workflow finished) fails with
  "couldn't download" when it needs a helper of another platform, and tries
  again at the next connect.
- **The kept helper is checked against the checksum recorded beside it.** Someone
  who can write the cache folder can change both; it is owner-only.
- **No helper for a Windows server or a platform other than the five.**
  The server's `uname` is what is asked for, and the release has no Windows
  server helper.
- **Fetched helpers of other builds are removed** when a helper is fetched or
  reused, with no regard for a front of an older build that is about to
  send one: its connect fails ("couldn't open") and the next connect
  fetches it again. On Windows a folder holding a locked file stays until a
  later fetch.
- **Downloads follow redirects to plain http under wget and PowerShell 5.1.**
  curl is pinned to https for redirects; `wget` (used only where there is no
  curl) and `Invoke-WebRequest` on 5.1 have no such switch, and only get time
  limits. The SHA-256 check still applies, though it comes from the same
  release. To close: follow redirects by hand and refuse a non-https one.
- **A partly failed Helpers run replaces the checksum file** with one that
  lists only the builds it published, so a macOS or Windows build published
  by an earlier run of the same key stops being found until the next
  successful run. The Linux builds are always in it.
- **The plugin's launcher has never run on Windows.** `install.sh` knows Git
  Bash's `uname` (tested with a fake one), but whether Claude Code can start
  a `sh` script as an MCP command on Windows is undocumented. To close: try
  it under Git Bash, and give Windows a `.cmd` or PowerShell launcher if not.
- **The Codex and Antigravity plugin folders were built to their
  documentation and tried on no real install.** Codex's page shows no stdio
  entry and no variable for an MCP command (`${PLUGIN_ROOT}` is documented for
  hooks only); Antigravity's gives no shape for `mcp_config.json`. Whether
  they start the server in the project folder, expand the variable, and load
  the skills is unknown. To close: install each and call a tool.
- **The pinned key can only be set after a release from `main` holds that
  build.** Until then `release-key` is empty and the plugin takes the newest
  build. Unpinned, a start uses the build it already has, and looks for a
  newer one in the background at most once a day (and Claude Code's
  `SessionStart` hook does at startup, on every session start and not once a
  day: one request for `LATEST`, seen in the trial with Claude Code), so a
  newer build applies from the start after it is found. If the agent kills the launcher's children, the
  background check may never finish, and it is retried only the next day. To
  close: pin the key when releasing.
- **The pinned-key file could also carry the SHA-256 of each platform's
  binary.** The plugin's check would then be independent of the release, and
  would close "The checksum file comes from the same release as the binary"
  for plugin installs. Not built, because no release holds this build yet.
- **Skills in a plugin can be newer than the pinned binary's embedded copy.**
  The binary's own `notebook_guide` and the skills in the plugin can then
  disagree about the tools. To catch it: have `mcp --skills plugin` compare a
  hash of the plugin's skills, passed by the launcher, with its embedded
  `PLUGIN_VERSION`, and say so in its instructions; or check in CI that
  `release-key` names a build embedding the same `plugin/skills`.
- **Uninstalling a plugin leaves the binary** in `~/.local/share/endeavor/bin/`.
  Old builds there are never removed, since a running session may use one and
  the launcher can't tell which. About 30 MB each. To close: delete all but
  the pinned and the newest at a start, when no `endeavor` from there is
  running.
- **The first start can outlast the agent's 30 s limit.** The download goes
  on in the background only if the agent doesn't kill the launcher's children;
  either way the next start finishes or redoes it, and `endeavor-setup` tells
  the agent to ask the user to reconnect. Claude Code's hook usually avoids it,
  unless it doesn't run before the server starts (undocumented).
- **A launcher killed while it holds the lock** leaves a lock the next start
  takes over once its owner's pid is gone, or once the lock is 20 minutes old
  whatever its pid says. Until then a start waits up to 25 s and fails with a
  message to reconnect. Two starts that take over one stale lock at the same
  moment are told apart by a rename, but a start that judged the old lock
  stale just as another made a new one can move the new one aside (it puts it
  back unless a third start has made one); the worst result is a second
  download, since each download has its own temporary folder.
- **`ENDEAVOR_RELEASE_URL` is honoured by the launcher and the hook.** A
  hostile value in one session can run whatever it points at in that session,
  but it can't leave anything for others: the binary goes to
  `bin-from/<its address>/`, which a normal start never reads. The variable
  is for tests and development.
- **`endeavor-setup` is also in the app's embedded plugin**, where it is
  never needed, since the app starts the server. It is left out of
  `notebook_guide`.

## Folders

- **Windows has no script check.** The tests that run `endeavor-mcp.sh` and the
  bootstrap script against `paths` are `cfg(unix)`. Under Git Bash the launcher
  keeps binaries in `$HOME/.local/share/endeavor/bin`, and `endeavor update`
  finds them through `paths::Env::plugin_bin`, which starts from `HOME` if it is
  a Windows absolute path and else from `USERPROFILE`. Whether the two agree
  there was not checked. `install.ps1`'s `%LOCALAPPDATA%\Endeavor\bin` is not in
  `paths` at all.
- **The install scripts' default folder is only in shell.** `install.sh`
  (`~/.local/bin`) and `install.ps1` repeat it, and nothing in Rust or a test
  pins them.
- **The server root ignores `XDG_CACHE_HOME`.** `~/.cache/endeavor`
  (`paths::server_root`, and `c=` in the bootstrap script) is the folder the app
  installs to, so it stays. `serve`'s runtime and the helpers cache do honour
  `XDG_CACHE_HOME`, and the default depot does not. To close: move the app
  too, then make all of them follow it.
- **Folders below the server root are written by hand where they are used:**
  `<build>` and `depot:` in the bootstrap script, `julia-<version>` in
  `julia.rs`. Only the root is shared with `paths` (and, in the script, pinned by
  a test). The script takes `$HOME` as it is, where Rust accepts only an
  absolute `HOME`.
- **The script's state folder is pinned on this machine only.** The test checks
  that `gethostname` and `uname -n` agree here. On a server where they differ,
  the script and Rust would name different folders.
- **`SCRATCH` is found by two rules.** `serve` and `mcp` read only the variable
  (`Env::from_vars`); a Slurm job (`wire::slurm::scratch()`) also asks a login
  shell. On a cluster that sets it only in the login profile, the two use
  different depots. To close: one rule in `paths`.
- **On Windows `endeavor update` finds the plugins' binary store from an
  absolute `HOME` before the profile folder**, as the launcher script does. It
  used the profile folder only.
- **`Env::from_vars` still reads the real process** for the home fallback
  (`home_dir`), the working folder and the host name, so a test that fakes the
  variables does not fake those three.

## The skills

- **`endeavor-machines` says "helper".** The file says "helper", which an
  agent should not have to read.
- **The notebook skill is untried.** No agent session has run against
  `endeavor-notebooks` or the shorter tool descriptions, and no rule that was
  cut has been tested for whether a current model needs it. To close: the
  trial in the skills audit, old text against new.
- **`notebook_guide` does not include the engine's reference.** An agent
  without skills gets the notebook skill, then has to ask for
  `endeavor-notebooks/reference/pluto.md` before its first cell. Every
  notebook is Pluto today, so this is one more call each session. Left so
  that the guide needs no change when a second engine arrives.

- **A waited run has no time limit.** `wait_for_completion=true` blocks until
  the whole run ends (`run_cells!` calls Pluto with `run_async=false`), so the
  60-second `TIMEOUT_SECONDS` never applies and the agent client's own tool
  timeout is the only bound. The skill and the tool descriptions say to wait
  only for a run that takes a few seconds. To close: run async and wait in
  `_wait_cells!`, which would give the cap and make `execution_timeout` real.

## Not checked

- **Codex and Antigravity** facts in the design come from their
  documentation: the manifest and MCP file names, that a plugin's server
  starts in the project folder, the tool timeout, and that each loads the
  skills (see "Releases and installing" for the plugin folders).
- **`run_conflict` was not provoked in the trial** with the real Claude Code
  CLI. It is covered by the unit tests only.
- **Gatekeeper and SmartScreen** behaviour for a binary installed with
  `curl`.
- **The app's side of sharing a runtime** is read from its code, not run:
  the shared state folder, detaching on quit, the build comparison that
  holds back runs, and a version on its `/endeavor/…` calls.

## Not covered by design

- **A second session's edit can replace a first session's edit made a moment
  earlier, and neither is told.** It happens when the second session read the
  cell after the first one wrote it: the edit is then valid for the cell as
  it is. Sessions leave each other no notes. Seen in the trial.
- **One notebook file open in two runtimes**, such as a shared disk opened
  on two servers. Each runtime saves over the other.

## The machine tools

- **A new session on a machine starts in the machine's home folder** unless
  `use_machine` is given `folder`. A notebook made there with `new_notebook`
  and no path lands in the home folder (seen in the trial). To close: tell the
  agent where the notebook went, or ask for a folder when the session has none.
- **The front and the runtime each check a call's argument names against their
  own build's tools.** A newer front's new argument is rejected by an older
  runtime with that runtime's message.
- **A runtime without the helper is looked for by `sh`.** The bootstrap reads
  `runtime.json` with shell patterns (the first `pid` followed by a quote and
  a colon, and the first `job` the same way), so a node name that holds
  `pid":` would confuse it. It doesn't check the runtime's port. A process
  counts as Julia's when it is alive and its command (`ps -p PID -o args=`)
  has `core` and `--state-dir`, which is the shape of `endeavor core`; a
  server whose `ps` has no `-p` (busybox) reports it as recorded and alive,
  not checked. A cluster's job counts when the user's `squeue` lists it as
  pending, running or configuring, and is reported as recorded when there is
  no `squeue` or it fails.
- **An install a start needs is not retried by the session by itself.** After
  `needs_install` for a runtime such as Julia it waits for `install: true` or
  for `add_machine` with a `julia` setting.
- **A start that is already under way takes no later agreement.** A
  `use_machine` without `install` that is still starting when a second one
  with `install: true` arrives keeps its own permission, so it can end with
  `needs_install` for what the start needs; the second call, made again, then
  goes through.
- **The Julia decision is tested through the tools** with a fake
  `curl`, not against a real download; its unit test would need a login shell
  that finds no Julia, which a developer's machine often has. That test
  skips itself where a login shell finds Julia.
- **The look at a server knows the helper's size only for this computer's
  platform** (this program's size plus the runtime's files, not the transfer's
  after compression by ssh). For another platform the helper is fetched only
  once the install is allowed, so the question gives no size. A platform the
  release has no helper for is found out then too.
- **Julia is not known when a machine is added.** The helper has no call for
  it, so `add_machine` reports `found: []` until the first runtime start.
- **Only Julia is installed through the item list.** The engine name picks
  what a start needs at one `match` (`prepare` in `src/lib.rs`); there is no
  registry. `machines.json`'s `julia` field is still Julia's.
- **`StartOptions::engine` is a string.** A misspelt engine name is caught only
  by the helper, when the start is asked for, not when the options are built.
- **With the helper missing, the question can't name the runtime.** The
  helper is what looks for Julia, so the first question names the helper only;
  `use_machine`'s one yes then covers Julia too, and a start that finds Julia
  missing and had no yes asks a second time.
- **Adding a cluster takes two connections.** The first connection is made as a
  plain server; `add_machine` ends it once Slurm is found.
- **Time waited in the queue is not reported**: the session doesn't know when a
  job from an earlier connection was submitted.
- **The front's exit can wait up to 5 s** for a connection that hangs (the connections close together).
- **Saved extra `sbatch` flags written as a flag and a separate value** (`"--qos"`,
  `"normal"`), from a pasted line in an earlier build or by hand, are refused
  at submit time with a message to write `--qos=normal`. `parse_salloc` now
  writes one entry, but records already saved are not rewritten.
- **Changing a machine between Slurm jobs and direct** drops the saved job
  defaults (partition, resources, account) and is refused only while the
  connection shows a runtime, a start or a job. A runtime or job it doesn't
  know of is not seen.
- **`stop_machine` waits 5 s** for the runtime to say how many other sessions
  called lately, and without `force` refuses when it doesn't come. A runtime
  busy for longer than that needs `force`. A runtime started by a build from
  before `endeavor/recent_sessions` doesn't know the question, so it is
  refused the same way until it is restarted or `force` is given. So is an
  answer that is not well formed, or a call that has no time left.
- **A session that has left still counts for 15 minutes while its notebook is
  open.** Nothing signs a session out, so `stop_machine` without `force` is
  refused for 15 minutes after the last tool call of another session that
  works in a notebook that is still open, whether the session is still there
  or not. It asks the user, who can say to go ahead. A session that only asked
  for status, or whose notebook has closed, does not hold up a stop.
- **A session that returns to a runtime is still in its old notebook there if
  that is still open.** It keeps one key for its whole run, so on a machine or
  on this computer it worked in before, `list_notebooks` shows `this_session`
  for that notebook, and `one_notebook` refuses a different one. If the
  notebook has closed or idle-stopped meanwhile, the binding is dropped when
  it is next found out, and the session can make or open another.
- **A session that goes away leaves its run policy and folder in the
  runtime's memory** until the runtime ends: the sign-out cleared them, and
  the 7-day forgetting does not. They are a few strings for each session.
  A session the app drops does not clear them either, since the app's call
  (`endeavor/end_session`) is gone.
- **A front from before this change, with a runtime from after it, loses the
  `stop_machine` check without saying so.** It reads `other_sessions` from
  `list_notebooks`, which is gone, so it sees no other session and stops
  without asking. Nothing was released before this change, so there is no
  code for it.
- **Not run against a host with no Slurm.** On this workstation the helper
  finds Slurm in `/usr/bin`, so the "no Slurm" message and `slurm: true`
  without Slurm are covered by unit tests of the decision only.
- **The races behind the move counter, `active` and the ops lock** have
  no test that forces them. The counter is tested as a comparison of the
  target before and after a move away and back, the lock by a call that waits
  out its deadline, and the order in
  `stop_machine` (the target is marked stopped before the runtime is ended) by
  reading.

## CI

- **CI runs on every push, on Linux, macOS and Windows**, and was red on macOS
  for most of this work without being looked at. It showed one product
  fault the Linux runs could not: when a runtime's end was reported before
  the connection dropped, the connection forgot the reason on reconnecting and
  said only "connected". Fixed; the order that shows it only happens on
  macOS, so the test for it is CI's.


## Tests

- **A failed or interrupted test run can leave its fake runtime behind.** One
  run left a fake `endeavor core` from the `machines` tests under
  `target/tmp`, which had to be ended by hand. The tests clean up when they
  pass; a panic or a kill skips that. To close: have each test's place end
  what it started when it is dropped, and check for leftovers at the start
  of a run.
- **`honors_the_mcp_protocol_version_header` failed once.** In one full
  workspace run (2026-10-06) the `core` test timed out after 20 s waiting for
  its core's `runtime.json`; five runs after it passed, three of them of that
  binary alone. Not understood. To close: if it recurs, keep the core's
  stderr in the failure message.
- **The launcher's tests don't pass under `busybox sh`.** Four launcher tests
  and one install test fail there because busybox runs its own built-in
  `uname`, `mkdir`, `timeout` and `wget` and ignores the fakes the tests put
  on the `PATH`. The scripts themselves were not run by hand under busybox.
- **Under Codex on Windows a runtime would end with the session.** Codex
  puts an MCP server in a job object that its children cannot leave, and
  ends the job with the session (read from its source, not run). Endeavor
  asks to leave the job and starts inside it when refused. To close: start
  the runtime there another way, before Windows is offered.
- **`list_machines` says "not running" for a local runtime recorded on another
  node** (a state folder shared between computers). A notebook call then says
  where it is running. To close: a third state for "running on another node".
