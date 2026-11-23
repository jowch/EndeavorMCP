# The helper

`lib.rs` is `endeavor connect`: the app's end on a machine. It answers the
app's questions about the machine's files at once. When the app asks, it
becomes a client of the machine's runtime (Pluto plus EndeavorRuntime): it
attaches to the runtime recorded in the state folder or starts one
([start-and-stop.md](start-and-stop.md)), then relays the app's streams to
the runtime's one loopback port over its own stdin and stdout (Endeavor's
docs/remote-sessions.md). On a server it runs over `ssh` as the `endeavor`
binary; on the user's own computer the app runs itself as the helper
(`endeavor --helper connect …`, `HELPER_ARGS`), so its helper can't go
missing or be from another build. Any number of helpers attach to one
runtime. The same binary is ssh's askpass program (`askpass`). On a cluster
(`--launcher slurm`) the runtime runs in a Slurm job, and the streams go on
through a second helper on the job's node (`endeavor relay`,
[slurm.md](slurm.md)).

## Flags

- With no `--state-dir`, the folder is decided each time no runtime is
  attached: the plugin's, or the app's older one while a runtime there is
  alive (#434). So the app's Restart Julia after the older runtime stopped
  starts in the plugin's folder, and a start after the plugin started one
  there finds it.
- `--julia-when-needed`: Julia isn't looked for before the runtime starts,
  only the first time something needs it. On a cluster it is still found on
  the login node, which has internet for a download; only its start waits.
- `--own-r`: the runtime is on the user's own computer, where the core may
  offer Endeavor's own R. Never on a server.
- `--quit-with-client`: stop the runtime when the app goes away without
  saying Stop or Detach. `--own-with-client` does so only for a runtime this
  connection started, and only for the process launcher: a cluster job is
  left, since a running job another helper submitted also attaches as not
  reattached. An explicit Detach wins over both: the app decides at quit, and
  the flags only cover an app that vanishes.
- `--any-node`: the state folder belongs to one machine, so another node
  name only means it was renamed.
- `--exit-idle`: the runtime ends once no notebook has been open for a while.
  The Slurm launcher passes it on to the job.
- `--exit-idle-after-client`: a runtime the app leaves running as it goes
  (Detach, or the end of input with nothing to stop) ends once idle from then
  on (`core::end_when_idle`, app #274). The app passes it for its own
  computer only. A runtime of an older build can't, and would never end, so
  it is left to the plugin if the plugin works in it, else stopped.
- `--launcher auto` is Slurm when `sinfo` is on the path, as the bootstrap
  script's `PICK_LAUNCHER_SH` settles it.

## The runtime's record

`runtime.json`, written by the core once the runtime is ready, holds its pid,
start time and boot ([start-and-stop.md](start-and-stop.md), "Telling a
process from a later one"), its one port, the pages' token, its Slurm job,
the build that started it, its interface (`core::INTERFACE`), a standalone
runtime's folder, whether it ends when idle, and its network settings. Each
field a later build added is optional, for a record from before.

- This build uses a runtime as it is when it offers this build's interface,
  or is this build. One from before one port per runtime can be stopped but
  not relayed to.
- The agent hears of a runtime another build started in words, never build
  keys, which mean nothing to the user. The way to stop it comes only after
  the user's yes: small models take a named call as a recipe.
- The network settings (`NETWORK_VARS`, the proxy variables) are recorded as
  names with a short hash of each value, since a proxy's address can carry a
  password. A runtime keeps those of the session that started it (#93).
- A runtime on this computer whose Julia was removed since it started can't
  start a notebook (#346, #347): juliaup removes a version when it updates
  past it, even while it runs. This is checked only where the core says which
  Julia it started.
- The bridge's bearer token and the pages' token are kept in the state
  folder, so they stay the same across runtimes: the agent's MCP config
  carries the first, and the pages' link the second. The pages' token opens
  Pluto's and Ember's pages and nothing else; the bearer token doesn't open
  the pages. Both files are readable by this user only (on Windows the state
  folder is under the user's own %LOCALAPPDATA%).
- The record's host name is Julia's `gethostname()`, the DNS name.
  `COMPUTERNAME` is the NetBIOS name (upper case, at most 15 characters), so
  a computer named otherwise once saw its own runtime as remote. Windows host
  names don't keep case, so they are compared without it (#54).

## The main loop

- Frames from the app are read on one thread: streams go to the attached
  runtime (its port here, or the relay on its job's node), file requests are
  answered each on its own thread, and other messages become events. Frames
  are binary, so output skips std's line-buffered stdout. Parts of files the
  app never finished sending are removed when it goes.
- The helper blocks SIGUSR1 in every thread before any starts: a helper of an
  older build sends it to the one named in `DIR/lock`.
- When the app quits and the plugin works in the runtime, the runtime is left
  to the plugin (app #232). One that doesn't answer is stopped, as before.
  When the app has the runtime again, it goes back to the lifetime it started
  with (`core::keep_for_app`), so no front takes it for the plugin's.
  This take-back is tried for about a minute, off the loop, and each try
  holds the lock that quitting sets, so a take-back can't land after a quit
  and undo it (#453).
- A relay to a job's node that ends is reconnected while the job runs (the
  connection inside the cluster dropped); otherwise the app hears how the
  job ended.
- The app's calls on the runtime's port are HTTP/1.0 JSON-RPC, with
  `BRIDGE_CALL_WAIT` for connecting, sending and reading the head all told,
  so a server that answers slowly, or a little at a time, can't hold it.

## Starting and stopping

- A start holds the start lock. While another helper holds it, the app is
  told once, and what it sends is still served. A `StartRuntime` during a
  start is refused so it is answered; a `Stop` ends the start, and is
  answered with it.
- A `Stop` that ended only this connection's wait for what it didn't begin
  (another connection's start, the start lock, a runtime that didn't answer)
  is handled as a `Stop` with nothing attached: an unforced one says Julia is
  still starting, a forced one cancels that start.
- During a start, Detach and the end of input exit and leave the start to
  finish. The end of input stops it where `--quit-with-client` says, or
  `--own-with-client` and this connection began it.
- A stop holds the start lock throughout, so a helper asked for a runtime
  meanwhile starts a new one after this one is gone, never attaching to one
  on its way out. Without the lock within its limit nothing is stopped. The
  streams are cut only once the lock is held, and restored if the process is
  still alive. What the app sends meanwhile is kept, not lost, and the stops
  said during one are answered by it. Events about the runtime are handled
  before what the app said, so a start asked for meanwhile doesn't find a
  runtime that has gone.
- The app's Stop for a host it only browsed stops the recorded runtime
  without attaching (on a cluster, cancels its job). Without the start lock,
  or for a runtime still starting, nothing is stopped.
- On a cluster only attaching is done: a new job would wait in the queue
  again.

## The lifeline

With `--own-with-client`, a runtime this helper starts gets the read end of a
pipe (`core --lifeline`). However the helper ends, SIGKILL included, the core
reads the pipe's end and ends with Julia, unless the helper said first that
the runtime is wanted (`keep_runtime`): once the app has it, or detaches
while it starts. Turning the flag off lets a start under way go too, so a
killed app leaves it (app #245). On Windows the core inherits the handle as
an fd is on Unix (#144); every inheritable handle goes to each child, so only
the read end is made inheritable, and only until the spawn.

## Starting the core

- `endeavor core` starts `julia boot.jl` with the Julia flags. `ps` shows
  `endeavor core`, not the binary's path. Secrets don't go in argv, which
  `ps` shows to every user.
- The core runs from the file this process runs from. `endeavor update` may
  have put another build at the program's path since it started, and that
  build's core would run with this build's `runtime/`. Linux keeps the file
  this process started from; elsewhere the program at the path is asked its
  build, and another build is refused. The app's copy is replaced only by the
  app's own update.
- A binary the bootstrap script or the plugin's launcher installed keeps its
  folder from being removed while it runs (#97).
- `runtime.log` takes the runtime's stdout and stderr, emptied first. On
  Unix it is opened to append, so nothing that writes it overwrites another.
  On Windows appending opens the file without the right to empty it (os
  error 5), so it isn't; the two handles share one position. The log shows
  Pluto's secret URL, so its tail is masked before the app sees it.
- **Unix**: the core starts in its own session, with no terminal and stdin
  from /dev/null, without the signals the starting process blocked for
  itself (`standalone` blocks Ctrl-C). The runtime's pid is its process
  group's. Once the core is gone, what is left of its group is ended and
  waited for: notebook workers, and on macOS, where nothing ties Julia to the
  core, Julia itself, which would go on saving the notebooks a new runtime
  opens. Only the group, since a group id isn't reused while a member is
  left, and not when the pid now belongs to another process.
- **Windows**: its own process group and a console with no window
  (`CREATE_NO_WINDOW`), stdin from NUL. A GUI-program core (the app's exe)
  gets no console at all, so the core also starts Julia with no window and
  Pluto's workers share Julia's hidden console. Breaking away from a job the
  app runs in lets the runtime outlive the app, as on Unix; a job that
  doesn't allow it refuses, and then the runtime ends with the app. Our
  standard handles came from whoever started us as inheritable handles, and
  Windows gives every inheritable handle to each child, so they are made
  non-inheritable first; otherwise the core and Julia would keep the app's
  log and our pipes open for as long as a kept runtime runs. The core's Job
  Object ends Julia and its workers.
- `ENDEAVOR_TEST_UNKILLABLE` (debug builds only) sends the runtime no
  signals, for a test that needs one still alive after its stop.
