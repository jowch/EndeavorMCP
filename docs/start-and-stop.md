# Starting and stopping a runtime

`runtime.rs` is the one place that finds the runtime in a state folder,
starts one, and stops it, for the helper (`endeavor connect`,
[helper.md](helper.md)), `serve` and `mcp`. The callers differ only in what
they say while it goes on and how they end it (`Hooks`). The start lock and
its rules are in [endeavor-mcp.md](endeavor-mcp.md) ("A start, once begun,
finishes without the process that asked for it"); this is what the code adds
to them.

## Which state folder

With none named, a runtime on this machine uses `Env::state_dir`, the
plugin's and now the app's. While a runtime alive on this machine is
recorded in the app's older folder (`Env::apps_old_state_dir`), that one is
used, so the app's open notebooks aren't opened again by a second runtime.
Once it stops, the next start is in `state_dir` (#110). A long-lived process
asks again before each start, not only when it begins, so a restart over the
same connection moves on once the older runtime stops (#434).

## Looking

`look` says what a state folder holds without starting or stopping anything:
no record, a record whose process is gone, a runtime that answers, one that
is alive and doesn't, one from a build before one port per runtime, or one
another machine of the same home folder recorded. A record of another node is
taken as this machine's only with `--any-node`: the folder belongs to one
machine, which was renamed.

- A ping is tried twice: a busy runtime can be slow to answer once.
- A runtime that is alive and silent is asked again for up to 30 s
  (`SILENT_WAIT`) before it counts as stuck: long enough for a computer that
  just woke from sleep or a moment of heavy load, and well within the 45 s
  an agent's call waits. No runtime is started beside one that is alive, since
  both would open the same notebooks.
- A runtime whose core was killed may have left Julia running, which would
  open the same notebooks as a new one.

## Starting

`find_or_start` finds the runtime, or starts one, and waits until it
answers. Callers asked at once start one runtime, and the rest wait for it.

- The start lock is held for a look and a spawn. The look comes after the
  lock is taken, so a core that records itself and lets go in between is
  found by it.
- Finding or installing Julia can take minutes, and a notebook may be opened
  meanwhile, so the runtime it would replace is looked at again after.
- A start that was stopped is not started again for whoever waited for it.
- A runtime is replaced at most once per call: one that comes back as an
  older one after that is used as it is. If a start can't go ahead, the one it
  would replace is kept rather than leave none.

### Replacing an older runtime

`replaces` stops the runtime a state folder records, and starts this build's,
when the call asks for it, no notebook is open in it, and either it offers an
older interface than this build's (or none), or this build would use it as it
is but its Julia was removed, so no notebook could start in it (#347).

- One that offers a newer interface is left alone, so two versions that share
  a state folder never take turns stopping each other's. So is one whose
  notebooks can't be counted.
- "No notebook open" is a count taken just before the stop, not a guarantee.
  The start lock doesn't hold back the calls of clients already attached, so
  a notebook another session opens between the count and the stop is cut off
  (its file is on disk).

## The core's lock file

- The core takes `starting.lock` before anything else, writes its pid and
  start time in it, and holds it until it has recorded itself. The file is
  opened close-on-exec, so Julia doesn't inherit it and hold it for as long as
  it runs.
- When it lets go, the pid is blanked first. A file that still named this
  process once a later core holds the lock would have a forced stop end a
  runtime that is up.
- `lock_state` is the one reading of it, asked with a shared lock so that two
  readers never make each other see it held. It makes no file: a core makes
  `starting.lock` before it holds it, so none means no start. It is read
  before the record, since a core writes the record before it lets go. On a
  home folder shared by several machines the lock may not reach them all.
- On Windows a byte-range lock is mandatory: a lock over the whole file would
  keep another process from reading the pid. So the core's lock is one byte
  past what the file holds, and writing the file needs no shared lock over
  it.
- After spawning a core, the starter waits until the core holds its lock and
  names itself in the file, so whoever takes the start lock next finds a core
  it can see and can stop.

## Stopping

`end` stops the runtime in a state folder, leaves a note (`stopped`) for the
clients still attached, and removes the record of one whose process is gone.
A runtime that is alive and doesn't answer is stopped as well. The caller
holds the start lock, so no start begins meanwhile. A start under way is left
alone unless `force` is given; then its core is ended with Julia and its
children, as described in endeavor-mcp.md. The lock of a core that was ended
is waited for a moment: the OS lets it go when the core's last handle closes,
and a process the core was starting may hold the file a little longer.

### The note a stop leaves (`stopped.rs`)

`DIR/stopped` tells the others still attached to the runtime, or waiting for
its job, what was stopped and how: "ID HOW", where ID is the runtime's pid,
or `job:` and the job's id for a job not yet running, and HOW is `stop`
(`endeavor stop`) or `connection` (a client's Stop). A pid alone, which an
older `endeavor stop` writes, is `stop`. One note at a time; a stop that
didn't happen takes it back. A starting runtime clears a pid's note, since the
pid may come again, but not a job's: job ids aren't reused, and a helper
still waiting on that job may not have read it.

## Telling a process from a later one

A pid alone doesn't name a process for long: pids come round, and a reboot
reuses them. `runtime.json` and `starting.lock` record a start time with each
pid, and a recorded pid counts only while the process now has that start
time.

- **Linux.** Field 22 of `/proc/PID/stat`, clock ticks after boot, read from
  after the last `)`, since the name in field 2 may hold spaces and
  parentheses. Since that counts from boot, the boot id goes with it, and a
  start time from another boot is not the same process.
- **macOS** gives an absolute time, so no boot id is needed. Other Unix
  systems record no start time, and a record without one is trusted as
  before.
- **Zombies.** A zombie has ended and only waits to be reaped, which never
  happens in a container whose PID 1 doesn't reap, so it counts as gone even
  though `kill(pid, 0)` still finds it. The same goes for a process group's
  members. On macOS a zombie doesn't last: the core's parent waits on it, and
  launchd reaps orphans.
- **Windows** hands pids out again quickly, so a pid is trusted only through
  a handle opened while the start time matches (`winproc::Process`). An open
  handle keeps an exited process's record, so it asks whether the process
  has exited rather than whether it can be opened. Unix keeps a runtime
  together as a process group; on Windows the core puts itself in a Job
  Object that ends every process in it once the core ends, however it ends.
  Windows 8 and later nest jobs, so this works inside a job the app or a
  terminal runs in. Processes started from then on join the job, and none
  may break away.

## Reading the log

`Log` reads the runtime's log line by line as it grows, keeping a line
written in part for the rest, and masks Pluto's secret.
