# EndeavorMCP

The notebook runtime and MCP server behind Endeavor, and a plugin that lets
Claude Code or Codex edit and run live Pluto notebooks without the app. One
binary, `endeavor` (package `endeavor-mcp`), is the app's helper on servers,
the standalone `endeavor serve` / `endeavor mcp`, and the Slurm relay.

## Where things are

- `crates/endeavor-mcp/`: the binary. `core.rs` is the language-neutral core
  (one port, MCP, tool rules); `notebooks.rs` and `notebooks/` the tool
  semantics; `mcp.rs` the MCP server; `standalone*` `serve`/`mcp`/`stop`;
  `client/` ssh, channel and machines; `slurm.rs` the cluster launcher;
  `paths.rs` every on-disk folder.
- `crates/wire/`: the protocol between the app and the helper. `backend.rs`
  is the notebook-engine enum (Pluto only today).
- `runtime/`: the Julia side (`EndeavorRuntime`, the Pluto adapter), embedded
  into the binary by `build.rs`.
- `plugin/skills/`: the skills. `claude-plugin/`, `codex-plugin/` and
  `antigravity-plugin/` hold copies; `scripts/plugins.sh sync` refreshes them.
- `docs/`: start with `runtime-core.md` (architecture), `status.md` (where
  things stand), `gaps.md` (known gaps, kept accurate), `testing.md`.

## Build, test, lint

```sh
cargo build --locked --workspace --all-targets
cargo test --locked --workspace --no-fail-fast       # what CI runs
cargo clippy --locked --workspace --all-targets       # a few warnings today; don't add more
sh scripts/plugins.sh check                           # CI fails if a plugin copy differs
```

- Run one test: `cargo test --locked -p endeavor-mcp --test machines <name>`.
- Real-Julia tests are ignored by default. Run them after touching
  `runtime/`, `core.rs`, `notebooks*`, `mcp.rs` or `serve`:
  `cargo test -p endeavor-mcp --test e2e_julia -- --ignored --nocapture`
  (about 40 s once packages are installed; the first run installs Pluto into
  the depot and takes minutes). `e2e_serve` likewise.
- The code is **not** rustfmt-formatted. Don't run `cargo fmt` on the tree;
  match the surrounding style (long lines are normal here).
- Tests are timing- and process-heavy. A test that fails once isn't a flake
  until you know why; see `docs/testing.md`.

## Rules that aren't obvious from the code

- **The app pins this repo by commit.** Endeavor depends on `wire` and
  `endeavor-mcp` through its `Cargo.lock`. A change the app needs lands here
  first, and the Helpers release (`.github/workflows/helpers.yml`, on `main`)
  must finish before Endeavor moves its pin. The crate version must equal the
  app's (both 0.1.0); bump them together. See `docs/status.md`.
- **Never write a notebook's `.jl` file directly.** All mutation goes through
  Pluto's session API, or reactivity breaks and Pluto's next save overwrites it.
- **Pluto stays on 127.0.0.1** behind its secret; the runtime's own port uses
  a separate bearer token. Never widen either.
- **Pins move together.** Julia 1.12.6 is pinned in `src/julia.rs`, in the
  app's `src/runtime.rs`, and in `scripts/cloud-setup.sh` (a test checks the
  last). `scripts/cloud-setup.sh` is a copy of Endeavor's; change both.
- `ENDEAVOR_TEST_*` variables are read only by debug builds. Don't make a
  release build depend on one.

## Working in a Claude Code cloud session

`scripts/cloud-setup.sh` installs Julia, marimo and clippy/rustfmt. In a
session with only this repo, `.claude/hooks/session-start.sh` runs it; in a
Claude project (several repos) the environment's setup script must. Details,
including the network hosts Julia needs, are in Endeavor's `docs/cloud.md`.

- Without `julialang-s3.julialang.org` and `pkg.julialang.org` allowed, there
  is no Julia, and the real-Julia tests can't run. Say so; don't work around it.
- The VM runs as root, and its PID 1 doesn't reap orphaned processes, so an
  ended detached process stays a zombie that `kill -0` still finds.
  `machines::a_forced_stop_cancels_a_start_another_process_began_and_the_next_call_starts_afresh`
  fails here for that reason (the runtime treats a zombie as alive). It passes
  on macOS and GitHub's runners. Don't chase it as a regression.
- marimo is installed as a uv tool (`marimo` on PATH, not importable from the
  system Python). For scripts, use `uv run --with marimo==0.25.1 ...`.

## Style

Commit subjects name the area and say what changed in plain words
("The start lock and recorded pids: fixes from the review"). Docs are written
in plain, short sentences for someone who wasn't there; keep `gaps.md` and
`status.md` true when a change affects them.
