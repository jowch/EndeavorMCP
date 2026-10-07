# Gaps

Known limits in the plugin and remote work
([plugins-and-remote.md](plugins-and-remote.md)), kept here to come back
to. Each says what happens, why it was left, and what would close it. Remove
an entry when it is fixed.

_Started 2026-10-06, on the `client-library` branch._

## The link

- **The link keeps the first front's `SSH_AUTH_SOCK`.** The link is started
  by whichever agent session needs it first and lives for hours. If that
  session's ssh agent socket goes away, a later reconnect fails at sign-in
  and is not retried. Left because the link has no better source for the
  socket. To close: let a front pass its current socket with
  `POST /link/start`, or have the link look for the user's agent itself.
- **A start cut short by a lost connection is reported as "not running".**
  After a reconnect the link only re-attaches to a runtime the helper can
  see. A core that was still booting when the connection dropped has no
  state file yet, so the helper can't see it and the link says the runtime
  is not running any more. It never starts one by itself; the next
  `use_machine` does. To close: have the helper record a start under way.
- **The test for a connection lost during a start doesn't force the order
  of messages that caused the bug.** It passes on the old code too. The fix
  is covered by reading, not by the test.
- **A stale link record whose pid was reused.** On Unix `pid_alive` ignores
  the start time, so `ensure` sees a live pid that doesn't answer and says
  the link isn't answering, until `link.json` is removed. To close: check
  the recorded start time on Unix as on Windows.
- **A runtime that dies in the moment its start succeeds** is handled
  (`gone_early`) but has no test: the order can't be forced.
- **A stop that fails while a start is under way** restores the start's
  state only roughly.

## The machines file

- **A link keeps the machine record it was started with.** A change to a
  machine in `machines.json` that the front didn't make (the app's, or a hand
  edit) reaches a running link only when the link is replaced: it ends after
  8 hours idle, or `add_machine` with other settings quits it. Julia
  settings and addresses are the ones that matter; job defaults are read by
  the front.
- **An `add_machine` that is still connecting leaves a link running with a
  record that is in no list.** It ends with the idle limit; the next
  `add_machine` for the same name reuses it, or replaces it when the settings
  differ.
- **Machine ids must be lower-case.** A hand-edited id with capitals is
  refused. Generated ids are lower-case already.

## Stopping and the client

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

- **A runtime the link starts exits when idle**, as one `mcp` starts does
  (`connect --exit-idle`): once no notebook has been open for the idle
  limit. A runtime the app starts on a server stays up. If the app attaches
  to a link-started runtime and leaves it with no notebook open for 48
  hours, it ends under the app. Not yet confirmed as the wanted behaviour.
- **`ENDEAVOR_IDLE_CHECK_SECS` panics on a negative or non-finite value.** It
  is a variable for tests.

## Windows

- **Compile-checked only.** The link's detached start, its console handler,
  the `taskkill` cancel path and the machines and projects file locations
  have never run on Windows.
- **The detached link may inherit handles from the front.** std starts a
  child with handle inheritance on and no handle list, so if the front's
  own stdin or stdout pipes are inheritable the link holds copies for up to
  8 hours and the harness never sees the front's output end. Not verified.
  To close: check on a real machine; if so, clear the inherit flag on the
  front's handles or start the link with an explicit handle list.

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
- **A queued job has no id in the link's status until it is submitted by
  this link.** A link that re-attaches to a job already queued shows `queue`
  but no `job` until it runs, because the library's queued event carries no
  id. A job already running shows its id, node and end time to a new link
  (`e2e_machines_slurm`). Queued and starting states have not been seen on
  real Slurm through the tools: the queue here is empty and a job runs at
  once, so their wording is checked against the fake Slurm only.
- **A `serve` you started inside your own job** is recorded under the
  compute node's name and isn't found from the login node. Add the node as
  the machine instead.

## Sign-in

- **Keys only.** A server that asks for a password or a code on every login
  is unsupported. A key with a passphrase needs `ssh-add` first, and a new
  host needs one `ssh <host>` in a terminal. Planned: a sign-in page on the
  link's loopback port.

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
  The release logic is tested with a fake release, but `link/run.rs`'s call
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
  reused, with no regard for a link of an older build that is about to
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
  `SessionStart` hook does at startup), so a newer build applies from the
  start after it is found. If the agent kills the launcher's children, the
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
- **Gatekeeper and SmartScreen** behaviour for a binary installed with
  `curl`.
- **The app's side of sharing a runtime** is read from its code, not run:
  the shared state folder, detaching on quit, the build comparison that
  holds back runs, and a version on its `/endeavor/…` calls.

## Not covered by design

- **One notebook file open in two runtimes**, such as a shared disk opened
  on two servers. Each runtime saves over the other.

## The machine tools

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
- **An install a start needs is not retried by the link by itself.** After
  `needs_install` for a runtime such as Julia it waits for `install: true` or
  for `add_machine` with a `julia` setting.
- **A start that is already under way takes no later agreement.** A
  `use_machine` without `install` that is still starting when a second one
  with `install: true` arrives keeps its own permission, so it can end with
  `needs_install` for what the start needs; the second call, made again, then
  goes through.
- **The Julia decision is tested through the link and the tools** with a fake
  `curl`, not against a real download; its unit test would need a login shell
  that finds no Julia, which a developer's machine often has. That test
  skips itself where a login shell finds Julia.
- **The look at a server knows the helper's size only for this computer's
  platform** (this program's size plus the runtime's files, not the transfer's
  after compression by ssh). For another platform the helper is fetched only
  once the install is allowed, so the question gives no size. A platform the
  release has no helper for is found out then too.
- **A link of another build that has a runtime on it is not sent `install`.**
  `add_machine` says so in its result, and the user has to end the runtime, or
  wait for the link to end, to install through a new link.
- **A link of another protocol is read only for its machine, pid, state, runtime,
  job and queue.** Its `state` reads as `unknown` when this build doesn't know
  the word, and a field whose shape changed is dropped, not an error. That is
  enough to decide whether to replace the link (nothing hangs on it and its
  state is `connecting`, `connected`, `failed` or `needs_install`); an
  `unknown` state is never replaced. What such a link wanted installed
  (`needs_install`) is not shown.
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
- **Adding a cluster takes two connections.** The first link connects as a
  plain server; `add_machine` quits it once Slurm is found.
- **Time waited in the queue is not reported**: the link doesn't know when a
  job from an earlier connection was submitted.
- **The front's exit can wait up to 5 s** on a link that hangs.
- **One run of the failing-`add_machine` test failed** while waiting for the
  link to end. The wait was changed and it did not recur in 13 runs; the
  cause was not found.
- **Saved extra `sbatch` flags written as a flag and a separate value** (`"--qos"`,
  `"normal"`), from a pasted line in an earlier build or by hand, are refused
  at submit time with a message to write `--qos=normal`. `parse_salloc` now
  writes one entry, but records already saved are not rewritten.
- **Changing a machine between Slurm jobs and direct** drops the saved job
  defaults (partition, resources, account) and is refused only while the
  link shows a runtime, a start or a job. A runtime or job the link doesn't
  know of is not seen.
- **A link call or `ensure` that runs past a tool call's 45 s** is given up
  on, and the work goes on in the background (it only starts or asks a
  link).
- **`stop_machine` waits 5 s** for the runtime's list of notebooks, and
  without `force` refuses when it doesn't come. A runtime busy for longer than
  that needs `force`.
- **Not run against a host with no Slurm.** On this workstation the helper
  finds Slurm in `/usr/bin`, so the "no Slurm" message and `slurm: true`
  without Slurm are covered by unit tests of the decision only.
- **The races behind the route/key pairing, `stopped` and the ops lock** have
  no test that forces them. The pairing is tested as a snapshot taken before
  a move, the lock by a call that waits out its deadline, and the order in
  `stop_local` by reading.
- **A link of another build with a runtime on it** is used as it is and sent
  no start or attach. Not tested with a real older build, only with a link
  whose record names another build.

## CI

- **CI runs on every push, on Linux, macOS and Windows**, and was red on macOS
  for most of this work without being looked at. It showed one product
  fault the Linux runs could not: when a runtime's end was reported before
  the connection dropped, the link forgot the reason on reconnecting and
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
- **`pluto_session_status` says "not running" while another process is starting the
  runtime.** The start's record (`runtime.json`) is written when Julia is up, and
  the front doesn't look at the start lock. The next call after the start
  finds it. To close: report a held `start.lock` as starting.
