# Testing

`cargo test` runs the unit and integration tests. Two tests start real Julia
and are ignored by default. Run them after a change to the runtime, the core,
the MCP server or `serve`. Endeavor's own checks, including the smoke test in
the app, are in
[Endeavor's testing.md](https://github.com/jowch/Endeavor/blob/main/docs/testing.md).

## Runtime tests against real Julia

`crates/endeavor-remote/tests/e2e_julia.rs` starts the helper and the core
with the real Julia adapter, the way the app starts This Mac's runtime. The
test then talks to the runtime the way Claude Code and the app do. It sends MCP
over `POST /mcp` with the `X-Endeavor-Session` and `X-Endeavor-Host` headers,
makes the app's `/endeavor/call`s, reaches Pluto's page and WebSocket as a
browser does, and sends the helper's file requests. Plain
`cargo test` skips it. To run it:

```sh
cargo test -p endeavor-remote --test e2e_julia -- --ignored --nocapture
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
5. The run policy. `endeavor/run_preview` says what an asked run would run,
   including a dependent cell. Plan mode refuses edits and runs but allows
   reads.
6. Uploads through the helper's `Place` and `Write`. The same file is reused.
   A different file with the same name becomes `decay (2).csv`.
7. Restart, as the app's Restart Julia does it. The test stops and starts the
   runtime, then reopens each notebook. The unchanged notebook runs again. The
   notebook whose file changed opens in safe preview.
8. A notebook in safe preview doesn't run code until `allow_execution`.
9. A notebook's own Julia killed during a run that `execute_cell` waits
   for. The call fails with `process_exited` and "Julia stopped unexpectedly
   while running `rates`. …", and `list_notebooks` has `exited` with that cell.
10. Idle stop with a limit of about two seconds, seen on the app's
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

`crates/endeavor-remote/tests/e2e_serve.rs` runs `endeavor-remote serve` and
`mcp` as a user without the app runs them. It finds Julia the same way as
`e2e_julia`, and keeps its state and project folders under `target/tmp/e2e-serve`.
To run it:

```sh
cargo test -p endeavor-remote --test e2e_serve -- --ignored --nocapture
```

It takes about a minute and starts Julia twice:

1. `serve` in a terminal. The agent connects over HTTP with only the bearer
   header, makes a notebook in the project folder, runs a cell and reads it.
   A notebook from disk opens in safe preview, through the tools and through
   Pluto's `/open`. The browser link sets the cookie and Pluto's page loads.
   Ctrl-C ends the core and Julia's process group.
2. `mcp` over stdio starts a runtime in the background. A second `mcp` with
   another folder shares it, and `stop` ends it.
