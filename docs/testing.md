# Testing

`cargo test` runs the unit and integration tests. Two tests start real Julia
and are ignored by default. Run them after a change to the runtime, the core,
the MCP server or `serve`. Endeavor's own checks, including the smoke test in
the app, are in
[Endeavor's testing.md](https://github.com/jowch/Endeavor/blob/main/docs/testing.md).

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
5. A fourth session that names its client opens the first session's
   notebook by path. It joins: the same `notebook_id`, `already_open`, nothing
   run again. Each session sees the other in `other_sessions`, and the
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
   joins it, and each sees the other in `other_sessions`; an agent also joins
   the notebook opened from the browser. The browser link sets the cookie and
   Pluto's page loads. Ctrl-C ends the core and Julia's process group.
2. `mcp` over stdio starts a runtime in the background. A second `mcp` with
   another folder shares it, and `stop` ends it.

## The client library over real ssh

`crates/endeavor-mcp/tests/e2e_client.rs` runs `endeavor_mcp::client` the way
the link will: `ssh` with `Auth::Batch`, the helper installed into a folder of
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
