# Launcher per start (P6): what it changes for Slurm

A spike for architecture-review.md P6, written 2026-10-08 from the code on
`main` at 65fbe36. Nothing was built or run for it.

## Recommendation

Don't build P6 as proposed. The problem it was meant to fix is real but
narrow: adding a cluster costs a second connection, so a second sign-in. A
smaller change fixes that without touching `StartRuntime`: the client may
send the launcher as `auto`, and the helper picks Slurm when it finds Slurm.
Keep the launcher fixed for a connection's life. On a shared login node that
is a safety property, not an accident.

Separately, fix a bug found on the way: the Slurm launcher ignores
`attach_only`, so a start meant only to attach can submit a job (below).

## What fixes the launcher today

The client writes the launcher into the bootstrap's preamble from the machine
record (`client/ssh.rs:228`, `Server::launcher`): `process` for a record with
no cluster, `slurm` for one with a cluster. The helper reads `--launcher` once
(`lib.rs:251`), and it decides four things for the whole connection:

1. The state folder. `process` uses `~/.local/state/endeavor/serve/<hostname>`,
   one per host. `slurm` uses `~/.local/state/endeavor/cluster`, shared by every
   login node (`lib.rs:266`, `paths.rs`).
2. What `StartRuntime` does: a process here, or a job (`lib.rs:329`).
3. What `files::Request::Runtime` reports (`check`, `lib.rs:765`).
4. What a `Stop` with nothing attached ends: the recorded process, or the
   recorded job (`stop_recorded`, `lib.rs:741`).

The bootstrap also uses it before the helper exists: `PICK_STATE_DIR_SH` picks
the folder from it, and the probe asks `ps` or `squeue` accordingly.

## Where it costs something today

- **Adding a cluster takes two connections.** A new machine's record has no
  cluster yet, so `add_machine` connects as `process`, sees `Hello.slurm`,
  saves the machine as a cluster, and drops the connection because it was
  made the other way (`standalone/machines.rs:1074`). The first job opens a
  second ssh. With keys that is a second or two. On a cluster with Duo or
  another second factor it is a second prompt on the user's phone. It happens
  even when the agent passed `slurm: true`, because the record is still empty
  when the connection is made.
- **Switching an existing machine between jobs and direct.** Refused while a
  runtime runs or starts (`machines.rs:1036`), else the connection is dropped
  and the next call reconnects. This is rare and already explained to the
  agent.

## What P6 would change for Slurm

1. **Three wire messages, not one.** `StartRuntime` gains the launcher. So must
   `Stop`, or a stop with nothing attached can't know which folder to look in.
   `files::Request::Runtime` must name a launcher or answer for both.
2. **The helper holds two state folders.** `--state-dir` (and
   `Options::state`) names one folder today, used for whichever launcher. It
   would need one per launcher. The app's own layout already has two
   (`state` and `cluster-<id>` under its root; `Server::launcher()[1]`, which
   the library ignores at `ssh.rs:228`). The bootstrap's probe would read both
   folders, inside the one-line script that is already about 1,400 characters.
3. **A new state.** A connection attached to a process runtime is asked for a
   job, or the reverse. Today that can't happen. It needs its own answer, and
   `client::Session` has to track which kind it is attached to.
4. **Less protection on login nodes.** Today a connection made for a cluster
   has no code path that starts Julia on the login node. Per start, only the
   client's choice stands between a tool call and Julia on a node other people
   share. The record would still decide, so it isn't unsafe by design, but one
   bug in one place becomes enough.
5. **Two folder rules in one helper.** The process folder is per hostname, the
   cluster folder is shared. On a cluster with load-balanced login nodes, a
   direct runtime started from `login1` is invisible from `login2`, while a job
   is visible from both. That isn't new, but one helper answering for both
   makes status and `Stop` depend on where ssh landed for one kind and not the
   other.
6. **Version skew is not the cost.** The bootstrap always runs this build's
   helper, so client and helper match. The cost is code: wire, `lib.rs`,
   `slurm.rs`, the bootstrap, `session.rs`, `add_machine`, and `e2e_slurm`,
   which needs a real Slurm. A few days.

What it buys: no second connection when adding a cluster, and a mode switch
could stop the old kind's runtime through the same connection instead of
refusing.

The Julia setting stays per connection either way. It changes only when the
user edits the machine, and a reconnect then is fine.

## The smaller change

- The preamble's launcher line may be `auto`. The helper resolves it once, at
  start: `slurm` when `wire::slurm::has("sinfo")`, else `process`. That is the
  same test that fills `Hello.slurm`, and the same answer `choose_mode` gives a
  new machine with no `slurm` argument (`machines.rs:1429`). `Hello` says which
  launcher it chose, in a new field with a default.
- `PICK_STATE_DIR_SH` resolves `auto` the same way (`command -v sinfo`, plus the
  fixed folders `has` checks), tested against `has` the way the state folder
  rule is tested against `paths.rs`.
- `add_machine`: a new machine with `slurm` unset connects `auto`, `slurm: true`
  connects `slurm`, `slurm: false` connects `process`. A saved machine connects
  as saved. `connected_as_cluster` comes from `Hello`, so the connection is
  dropped only on a real mode switch.
- `StartRuntime`, `Stop` and `files::Request` don't change.
- Left as it is: switching a saved machine's mode still reconnects.

Size: under a day, with tests.

## A bug found on the way

`slurm::attach` takes no `attach_only` (`lib.rs:331`); the wire says the helper
answers `NotRunning` when nothing runs. The client asks `files::Request::Runtime`
first and sends an attach-only `StartRuntime` only when a job runs, waits or
starts (`client/session.rs:652`). If the job ends between the two, or the start
under way fails, the helper submits a new job with `JobRequest::default()`: the
Small preset (2 CPUs, 8 GB, 2 hours) on the default account. That spends the
user's allocation without asking, which the skills tell agents never to do.
Found by reading the code, not reproduced. The fix: pass `attach_only` to
`slurm::attach` and answer `NotRunning` instead of submitting.

## To decide

1. The `auto` launcher instead of P6 (recommended).
2. P6 as proposed.
3. Neither: keep the second connection.
