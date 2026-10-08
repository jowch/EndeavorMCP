# Testing

`cargo test` runs the unit and integration tests. Two tests start real Julia
and are ignored by default. Run them after a change to the runtime, the core,
the MCP server or `serve`. Endeavor's own checks, including the smoke test in
the app, are in
[Endeavor's testing.md](https://github.com/jowch/Endeavor/blob/main/docs/testing.md).

## Trying the plugin before a release

The plugin's launcher gets its binary from the Helpers release, which holds no
build of a branch. `ENDEAVOR_BIN` names a binary to run instead:

```sh
ENDEAVOR_BIN=$PWD/target/debug/endeavor claude --plugin-dir claude-plugin
```

The launcher then runs `$ENDEAVOR_BIN mcp ...` and fetches nothing. Run
`scripts/plugins.sh sync` first if `scripts/` or `plugin/skills/` changed;
`scripts/plugins.sh check` (which CI runs) fails when a plugin folder's copy
differs.

### A first run on another computer (a Mac)

No person has run the plugin on macOS; CI only runs the tests there. To try it
before a release, with Claude Code and git installed:

```sh
# 1. Rust, if you don't have it. The repository pins its toolchain.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 2. The branch and a build (a few minutes).
git clone https://github.com/jowch/EndeavorMCP && cd EndeavorMCP
git checkout client-library
cargo build --release -p endeavor-mcp
export ENDEAVOR_BIN=$PWD/target/release/endeavor
export ENDEAVOR_PLUGIN=$PWD/claude-plugin

# 3. Claude Code with the plugin, in a scratch project folder.
mkdir -p ~/endeavor-try && cd ~/endeavor-try
claude --plugin-dir "$ENDEAVOR_PLUGIN"
```

`ENDEAVOR_BIN` must be set in the shell that starts `claude`. Codex does not
pass it to a plugin's server; put it in the entry's `env`. (The Codex trial gave
the server with `-c 'mcp_servers.endeavor.command=...'`, not as a plugin.) Julia is found on
your `PATH`, or downloaded into `~/.cache/endeavor/` if there is none. The
first notebook installs Julia packages into `~/.cache/endeavor/depot`, which
takes a few minutes; the Endeavor app uses the same folder. Don't open a
notebook that the app has open: the two don't share a runtime yet.

Then, in Claude Code:

1. `/mcp` shows `plugin:endeavor:endeavor` as connected, and `/skills` lists
   three `endeavor:` skills.
2. "Make a notebook called hello.jl with a cell showing 1 + 1, and give me the
   address." Open the address in a browser.
3. "Change it to 2 + 2." The page changes while you watch.
4. Quit Claude Code, start it again the same way, and ask "what does hello.jl
   show?" It should find the notebook still running, without a new start.
5. In a terminal: `$ENDEAVOR_BIN status`, then `$ENDEAVOR_BIN stop`, then
   `$ENDEAVOR_BIN status` again (not running).

What to send back if something fails: the output of `$ENDEAVOR_BIN status`,
the file it names as the log (`runtime.log`), what `/mcp` shows for the
server, and the agent's last message. This was run on Linux on 2026-10-08
with the same command (the server connected, the skills were listed, a
notebook was made); the macOS-only parts are the process start time
(`unixproc`) and whatever Gatekeeper says about a binary built locally, which
should be nothing.

`ENDEAVOR_RELEASE_URL` replaces the release's address in `install.sh` and the
launcher. It is for tests and development only: the launcher keeps what it
fetches from such a release in `bin-from/<address>/`, apart from the folder a
normal start uses. `ENDEAVOR_TEST_SH` makes `tests/launcher.rs` and
`tests/install.rs` run the scripts under another shell (`dash`, `bash`,
`busybox sh`).

## Runtime tests against real Julia

`crates/endeavor-mcp/tests/e2e_julia.rs` starts the helper and the core
with the real Julia adapter, the way the app starts This Mac's runtime. The
test then talks to the runtime the way Claude Code and the app do. It sends MCP
over `POST /mcp` with the `X-Endeavor-Session` and `X-Endeavor-Host` headers,
makes the app's `/endeavor/call`s, reaches Pluto's page and WebSocket as a
browser does, and sends the helper's file requests. Plain
`cargo test` skips it. To run it:

```sh
cargo test -p endeavor-mcp --test e2e_julia -- --ignored --nocapture
```

It takes about 40 s and prints how long each step took. Julia starts once, and
the test goes through these steps in order:

1. The MCP handshake. The server session gets the host tools and the This Mac
   session doesn't.
2. `new_notebook`, then add a cell, edit it, run it, and read its output (`42`).
3. Pluto's page as a browser reaches it: a `?token=` link sets the cookie and
   redirects; with the cookie the page loads and a WebSocket ping gets
   Pluto's pong through the core. Pluto's own secret never comes back.
4. `list_notebooks` marks `this_session` right for two sessions. A second
   notebook for the same session is refused, and so is a change to the other
   session's notebook.
5. A fourth session opens the first session's
   notebook by path. It joins: the same `notebook_id`, `already_open`, nothing
   run again, and both see it as `this_session`. The
   `/endeavor/call` `open_notebook` of the app gets the notebook the same way.
6. The run policy. `endeavor/run_preview` says what an asked run would run,
   including a dependent cell. Plan mode refuses edits and runs but allows
   reads.
7. Uploads through the helper's `Place` and `Write`. The same file is reused.
   A different file with the same name becomes `decay (2).csv`.
8. Restart, as the app's Restart Julia does it. The test stops and starts the
   runtime, then reopens each notebook. The unchanged notebook runs again. The
   notebook whose file changed opens in safe preview.
9. A notebook in safe preview doesn't run code until `allow_execution`.
10. A notebook's own Julia killed during a run that `execute_cell` waits
    for. The call fails with `process_exited` and "Julia stopped unexpectedly
    while running `rates`. …", and `list_notebooks` has `exited` with that cell.
11. Idle stop with a limit of about two seconds, seen on the app's
    `/endeavor/events` stream. `ENDEAVOR_IDLE_CHECK_SECS` makes the core
    check every second instead of every five minutes.

The test looks for Julia in this order. The first one found is used.

1. `ENDEAVOR_E2E_JULIA`.
2. The app's own Julia, at `~/Library/Application Support/endeavor/julia-*`.
3. `julia` on the login shell's PATH.

If none is found, the test prints `SKIPPED` and passes. With the app's Julia,
the app's depot supplies the packages. The test puts its own depot in front of
it, under `target/tmp/e2e-julia`, so Julia writes there and not into the app's
folder.

## `serve` and `mcp` against real Julia

`crates/endeavor-mcp/tests/e2e_serve.rs` runs `endeavor serve` and
`mcp` as a user without the app runs them. It finds Julia the same way as
`e2e_julia`, and keeps its state and project folders under `target/tmp/e2e-serve`.
To run it:

```sh
cargo test -p endeavor-mcp --test e2e_serve -- --ignored --nocapture
```

It takes about a minute and starts Julia twice:

1. `serve` in a terminal. The agent connects over HTTP with only the bearer
   header, makes a notebook in the project folder, runs a cell and reads it.
   A notebook from disk opens in safe preview, through the tools and through
   Pluto's `/open`. A second agent opens the first one's notebook by path and
   joins it; an agent also joins
   the notebook opened from the browser. The browser link sets the cookie and
   Pluto's page loads. Ctrl-C ends the core and Julia's process group.
2. `mcp` over stdio starts a runtime in the background. A second `mcp` with
   another folder shares it, and `stop` ends it.

## The client library over real ssh

`crates/endeavor-mcp/tests/e2e_client.rs` runs `endeavor_mcp::client` the way
a session does: `ssh` with `Auth::Batch`, the helper installed into a folder of
the test's own, the runtime started there, and the agent's MCP calls through
the local listener's port. It needs a host this user can `ssh` to with a key,
named in `ENDEAVOR_TEST_SSH_HOST` (`localhost` works), and prints `SKIPPED` and
passes without it or without Julia. It finds Julia as `e2e_julia` does, which
has to be at the same path on the host. The install folder, state folder and
depot are under `target/tmp/e2e-client`; the depot is kept between runs, so
the first run takes several minutes. Those are this checkout's paths, so the
test is meant for `localhost` or a host that shares this filesystem: on any
other host it would create them there.

```sh
ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_client -- --ignored --nocapture
```

The test connects, expects the helper's hello, starts the runtime, does an MCP
`initialize` and `tools/list` with the bearer token (and gets 401 with a wrong
one), stops the runtime through the channel, and checks the channel ends as a
detach and not as a drop. `tests/client.rs` covers the same code with a local
`sh` for ssh and no Julia, in plain `cargo test`.

## The connection library with a local `sh`

`crates/endeavor-mcp/tests/session.rs` drives `client::Session`, the one type
that connects, starts or attaches, retries and attaches again, against the
helper binary with a local `sh` for ssh (`Transport::Shell`) and the stand-in
Julia under the real core, in plain `cargo test`, with every folder under
`target/tmp`:

- `ensure` answers `Ready` with the listener's port, and again the same; an
  attach with nothing running says so and starts nothing; an attach never
  replaces a start, and a start is never told that nothing runs.
- A machine without the helper needs the user's agreement, and the agreement
  (given to a start that waits for it, or alone) installs it. A reconnect
  installs it again when it was agreed to; an agreement given to a start with
  the helper there is for what the start needs and is not kept for the helper.
- A failed sign-in is kept and told to every call; only a call that asks to
  retry tries again, and asking again while connecting makes no other attempt.
  A runtime that ended while connected is not running, and a start after it
  starts another; one that ended while the connection was down is a kept failure.
- A start that takes long answers `StillWorking`, and the next call gets it.
- A stop ends the runtime and not the connection, and a start after it works on
  the same port; a stop during a start is no failure.
- The helper killed: the connection is made again on the same listener port and
  attaches to the same runtime; a start the drop cut short is resumed after the
  reconnect; a runtime that ended meanwhile is told and not started again.
- Closing, or dropping the session, ends its thread, helper and port and stops
  nothing, also during a start or a connect; two sessions in one process leave
  each other alone.

## The machine tools

`crates/endeavor-mcp/tests/machines.rs` drives `endeavor mcp` over stdio as an
agent's harness does, in plain `cargo test`: the helper with a local `sh`
for ssh (`ENDEAVOR_TEST_SHELL`; the front holds its own connection), the stand-in Julia under the real core, fake
`sbatch`, `squeue`, `scancel`, `srun` and `sinfo` for a cluster, and a second
runtime for "this computer". The front gets nothing but its own variables: HOME,
`XDG_STATE_HOME`, `XDG_CONFIG_HOME` and `XDG_CACHE_HOME` are folders under
`target/tmp`, which is also where `list_machines` finds the ssh config (it
reads `$HOME/.ssh/config`), and `ENDEAVOR_TEST_ROOT`, `_STATE` and `_DEPOT`
keep the helper's folders there (`{id}` in them is the machine's id, for a test
that uses two machines). `ENDEAVOR_START_WAIT_SECS` shortens the front's waits. The `ENDEAVOR_TEST_*`
variables work only in a debug build, so `cargo test --release` would run real
`ssh lab` and `ssh hpc` with the machine's default folders. `machines.rs`
(`Place::bare`), `e2e_machines.rs` and `e2e_machines_slurm.rs` call
`common::require_debug_build()` first and panic in a release build: "these
tests need a debug build: a release build ignores ENDEAVOR_TEST_*, and would use
real ssh". A machine with Slurm
installed reports `slurm: true` to every helper (`wire::slurm::has` looks in
`/usr/bin`), so the `add_machine` test uses the fake Slurm and the other
tests save their machines. It covers:

- `tools/list` has the host tools and the four machine tools.
- `list_machines` connects to nothing and shows no state for a machine it
  isn't connected to; `add_machine` reports and saves; adding again updates; a
  failing one leaves no record (or keeps the old one) and no connection.
- `use_machine` to ready; notebook calls reach the machine's runtime with the
  host header and the connection's browser port; `list_folder`, `read_file` and
  `run_shell` work there and refuse after `use_machine("local")`.
- A second front in the project comes up on the remembered machine; a notebook
  call on a plain server with nothing running starts a runtime there, and a
  status or list call starts none; a remembered machine that is gone is said
  once.
- A call while the runtime is held starting returns the status within the limit.
- A cluster: no job is submitted without resources, with them the queue state
  and reason show in `pluto_session_status`, then the job's node and end
  time; a remembered cluster with no job submits nothing; `stop_machine`
  cancels the job.
- `stop_machine` says another session was active, stops with `force`,
  and a later call says to call `use_machine`; the same for this computer,
  which is one kind of target with a machine.
- This computer's runtime starts at the first notebook call and not before,
  a second front finds it, a start goes on without the front that asked for
  it, a record that can't be used is a failure and starts none, and a stop
  waits for a start under way.
- A session keeps its key when it moves and finds its notebook where it made
  it; the front's exit tells the runtime nothing and leaves it running; a new front attaches to it with the notebook still open; a dropped
  connection is made again on the same browser address; one front uses two
  machines, each with its own connection and runtime; `remote_port` is the
  runtime's recorded port.

The same path over real ssh and Julia is `e2e_machines.rs`, ignored like the
other `e2e_` tests:

```sh
ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_machines -- --ignored --nocapture
```

It uses `e2e_client`'s depot, and `HOME` stays yours so `ssh` finds its keys.
Through `endeavor mcp` it runs `add_machine`, `use_machine`, `new_notebook`, a
cell (`42`), `read_file` and `run_shell` on the server, fetches the
`browser_url` over HTTP, kills the helper to see the connection made again on
the same address with the runtime and notebook still there, and `stop_machine`,
and checks that nothing is left running.

The same path on a cluster is `e2e_machines_slurm.rs`, which also needs Slurm on
the host (`localhost` on a single-node cluster) and submits one job of 1 CPU,
2 GB and 15 minutes to `LocalQ` (`ENDEAVOR_TEST_SLURM_PARTITION` names another):

```sh
ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_machines_slurm -- --ignored --nocapture
```

Through `endeavor mcp` it runs `add_machine` with `slurm: true` (the partitions
and their limits), `use_machine` with no resources (`needs_job`, nothing in
`squeue`), then with resources (the job's id, node and end time), a cell that
reads `SLURM_JOB_ID` and `run_shell` on the node, and the page through the
front's port. A second and a third `endeavor mcp` in the same project attach to
the same job with no `use_machine` and no second job; the job outlives each
front; `stop_machine` is refused while another session has called lately and with
`force` the job leaves `squeue`. The job's id is recorded so a failed step
cancels it.

## The Slurm launcher over real ssh and real Slurm

`crates/endeavor-mcp/tests/e2e_slurm.rs` runs the cluster path of
`endeavor_mcp::client` against a real scheduler: `ssh` with `Auth::Batch` to a
login node, the helper submitting a job with `sbatch`, the runtime starting in
it, and the relay to its node. It needs a host this user can `ssh` to with a
key, named in `ENDEAVOR_TEST_SSH_HOST`, that has Slurm; a single-node
cluster that is also this machine, with `localhost` as the host, works. It
prints `SKIPPED` and passes without the variable, without `sinfo`, or without
Julia. Julia is found as `e2e_client` finds it, and the depot is that test's
(`target/tmp/e2e-client/depot`), so run `e2e_client` first or expect several
minutes for the first run. The install and state folders are under
`target/tmp/e2e-slurm`, and the host has to see them at the same path.

```sh
ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_slurm -- --ignored --nocapture
```

It submits three small real jobs, one after another, of 1 CPU, 2 GB and 15
minutes each, to partition `LocalQ` (`ENDEAVOR_TEST_SLURM_PARTITION` names
another), and takes about a minute. It cancels any job it submitted that is still
listed, also when it fails, and a job left by an earlier run in its state
folder. Other jobs of the user are not touched. The test goes through these
steps:

1. Connect with a cluster `Server`: the hello has `slurm: true`. Start the
   runtime: the client hears `Submitted`, then `Ready` with the job id. `squeue`
   lists the job as running, named `endeavor`, and ours. The helper's own
   `Runtime` check finds the running job. `runtime.json` has the job id, and
   `job.json` is gone.
2. MCP through the local listener: `initialize`, `tools/list`, 401 for a wrong
   token, then `new_notebook`, a cell, and its output (`42`).
3. A second client connects and starts with the same state folder: `Ready`
   with `reattached`, the same job and process, no second job, and its own
   listener reaches the first client's notebook.
4. The second client detaches. Its helper and relay are gone, and the job and
   the first client go on.
5. The first client stops the runtime: the channel ends as a detach, no
   notice is sent, the job ends, `runtime.json` is gone, and no process of the
   job (the core, Julia, Pluto's worker, the relays) is left.
6. A third client starts a new job, and the test cancels it with `scancel`
   while the client is attached. The client gets `Died` with "Its Slurm job was
   cancelled.", the state is cleaned, and the processes are gone.

The test prints the node names it sees (`squeue`'s, `runtime.json`'s, the
helper's), how the helper reached the node (`srun` or `ssh`), and the time from
submit to ready.
