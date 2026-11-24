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
- `crates/wire/`: the protocol between the app and the helper
  ([docs/wire.md](docs/wire.md)). `backend.rs`
  is the notebook-engine enum (Pluto only today).
- `runtime/`: the Julia side (`EndeavorRuntime`, the Pluto adapter,
  [docs/pluto-adapter.md](docs/pluto-adapter.md)), embedded
  into the binary by `build.rs`.
- `plugin/skills/`: the skills. `claude-plugin/`, `codex-plugin/` and
  `antigravity-plugin/` hold copies; `scripts/plugins.sh sync` refreshes them.
- `docs/`: start with `runtime-core.md` (architecture), `status.md` (where
  things stand), `testing.md`. Known gaps are tracked as GitHub issues, with a
  priority label (P1, P2, P3).

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
- **Don't edit `release-key`.** The plugins' pinned build
  (`scripts/release-key` and its copies) is written on `main` by the Helpers
  workflow once a build of `main` has finished on every platform. PRs leave it
  alone and need no re-pin when `main` moves. If a pin is missing, run Helpers
  again on `main`.
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
  ended detached process stays a zombie. The runtime counts a zombie as gone
  (`unixproc::start_time`); test helpers that use `kill -0` must too.
- marimo is installed as a uv tool (`marimo` on PATH, not importable from the
  system Python). For scripts, use `uv run --with marimo==0.25.1 ...`.

## No comments

Don't write comments, and that includes `///` and `//!` doc comments and
Julia docstrings: neither crate goes to a registry (the app takes them from
git), so they only ever reach someone reading the source. Comments go stale;
code doesn't. When you want to explain something, change the code so it says
it: a better name, a smaller function, a type, an `assert!`, or a test that
fails if someone undoes it. When the reason can't live in code (a platform
quirk, why the obvious way was rejected), put it in the commit message, where
`git blame` finds it, or in `docs/` if it's design. Known gaps and shortcuts
are GitHub issues.

Only these stay, because a tool or a reader outside the source needs them:

- license headers, shebangs, lint and format directives (`#[allow]`,
  `# shellcheck`), and build-tool pragmas;
- `// SAFETY:` on an `unsafe` block, which states the invariant the compiler
  can't check;
- comments that are a file format: Pluto's notebook markers
  (`### A Pluto.jl notebook ###`, `# ╔═╡`, `# ╟─`) in fixtures and in notebook
  text built by tests, and marimo's PEP 723 `# /// script` header;
- user content: the files under `smoke/tasks/*/project/` stand in for a
  user's folder, and their comments are the user's.

Triple-quoted Julia strings that aren't docstrings are code. UI text, MCP tool
descriptions and skill text are product strings, not comments. Leave copied
and generated files alone: the app's view, which changes in the app and is
synced here, the plugin copies, and `release-key`. `scripts/cloud-setup.sh`
is a copy of the app's; change both the same way.

The rule covers every file: Rust, Julia, shell, YAML, TOML. Reviewers flag
every new comment that isn't one of the kept kinds above.

## Style

Commit subjects name the area and say what changed in plain words
("The start lock and recorded pids: fixes from the review"). Docs are written
in plain, short sentences for someone who wasn't there; keep `status.md`
true when a change affects it, and open, update or close the GitHub issue
for a gap the change finds or fixes.
