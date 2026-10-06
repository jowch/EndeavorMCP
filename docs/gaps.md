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
- **A server on another platform than this computer can't be used yet.** The
  link sends its own binary as the helper, so a Mac can't reach a Linux
  server. Closed by build step 5 (the release key in the build, download and
  SHA-256 check).

## The machines file

- **Rewriting `machines.json` drops fields `Server` doesn't have.** Harmless
  while only this binary writes it. Must be fixed before the app shares the
  file, or one side loses the other's additions. To close: keep unknown
  fields when rewriting.
- **Machine ids must be lower-case.** A hand-edited id with capitals is
  refused. Generated ids are lower-case already.

## Stopping and the client

- **Two `Stop`s sent from two threads at once**: queueing and sending are
  one step now, with no test, since it needs a controlled race.
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
- **The reason for a job cancelled from outside is sometimes not "cancelled".**
  `e2e_slurm` failed once on that check (the client was told the job ended,
  with another reason) and passed on the next six runs. `sacct` is off on
  this workstation, so the reason comes from the job's state and its log,
  and that probably races with Slurm marking the job. Not investigated. To
  close: capture the reason string when it happens, and wait for the job's
  final state before reading it.
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

## Not checked

- **Codex and Antigravity** facts in the design come from their
  documentation: the manifest and MCP file names, that a plugin's server
  starts in the project folder, the tool timeout, and that each loads the
  skills.
- **Gatekeeper and SmartScreen** behaviour for a binary installed with
  `curl`.
- **The app's side of sharing a runtime** is read from its code, not run:
  the shared state folder, detaching on quit, the build comparison that
  holds back runs, and a version on its `/endeavor/…` calls.

## Not covered by design

- **One notebook file open in two runtimes**, such as a shared disk opened
  on two servers. Each runtime saves over the other.

## The machine tools

- **`use_machine` without `folder` resets the session's folder.** A second
  session that calls it for a machine the project already uses moves to the
  server's home folder and the project forgets the folder it remembered. A
  session that attaches on its own keeps the remembered folder. Left because
  "no folder" means home by the tool's definition. To close: keep the
  remembered folder when the machine is the project's and no folder is given.
- **Julia is not known when a machine is added.** The helper has no call for
  it, so `add_machine` reports `julia: null` until the first runtime start.
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
