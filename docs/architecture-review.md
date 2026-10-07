# Architecture review

A look back at the plugin and remote design ([plugins-and-remote.md](plugins-and-remote.md))
after building it, before the app adopts it. It is for deciding, not a plan:
nothing here is done until it is chosen.

_Written 2026-10-09 against `client-library` at 7f1e6dc. The second part is an
independent review by a separate agent that had not worked on the branch and
was told that proposing removals was welcome. The first part is the view of
the session that built it._

## Decided

2026-10-09: go with the recommendations below.

- **Now, before the skills trial and the app:** P1, P2, P4 (with the
  `machines.json` fixes), N3, N4, and P5a.
- **P1 is done:** request ids on `Stop` and `StartRuntime`, `wire::PROTOCOL` in `Hello`, and `link::PROTOCOL` in `link.json`; fronts compare the protocol, not the build.
- **P2 and N4 are done:** `StartRuntime { engine, install }`, `ToApp::NeedsInstall { items }` with `wire::Item { kind, name, size_mb, place }`, `StartError::NeedsInstall`, `link::InstallInfo { items, helper }`; one `connect`, one `start` (with `StartOptions`) and one `status`/`start`/`install`/`attach` each, taking the wait. The compatibility code of P1 (`InstallWhat::Unknown`, `lenient`, the refuse-unknown-fields rule, `download_julia`'s default) is gone; `State::Unknown` stays for reading a link of another protocol.
- **P4 is done:** `link::ensure(&Server)` writes the record to `links/<id>/server.json` and the link keeps it; `machines.json` is `{"schema": 1, "machines": [...]}` (a bare list still reads), written only after a connect succeeded, keeps unknown fields, and is never rewritten when its schema is higher. `PROVISIONAL`, `restore` and `undo` are gone.
- **P5a is done:** the local runtime starts at the first call that needs it; `list_notebooks`, `pluto_session_status` and `notebook_guide` do not start it.
- **2026-10-07, after three more review rounds: the link goes, and this computer and a server get one treatment.** Each `endeavor mcp` holds its own connection in process, as the app does; finding or starting a runtime is one library function that the helper wraps and the front calls directly here; connect, retry and re-attach become one library type that answers with an outcome (P3, brought forward); no session sign-out and no other-sessions list. This reverses "the link as a separate process" under "Left alone on purpose", and replaces P5's longer-term item. The design is in [plugins-and-remote.md](plugins-and-remote.md#the-revision-decided-2026-10-07-planned). N3 follows it.
- **P5b: no question on this computer.** Installing the plugin is the
  agreement to Julia and the packages it needs here. The question stays for
  servers.
- **With the skills rewrite:** N1.
- **Before real users:** N2, N5.
- **With the app:** P3. **At the first release from `main`:** P7.
- **P6:** a short spike first.

## The proposals

Ordered by how much they matter before the app takes the library. "Before
the app" means the change touches the wire protocol, the library's API or a
file the app will share, so doing it later means doing the app's side twice.

| # | Proposal | Before the app? | Cost | My read |
|---|---|---|---|---|
| P1 | Request ids on `Stop` and `StartRuntime`, and a protocol number in `Hello` and `link.json` | Yes | Days | Do it. It removes the stop queue in the client and the replay logic in the helper, where two of the untested races live, and it is what makes builds of different ages work together. Today every build looks incompatible with every other |
| P2 | One engine-neutral shape for "what this start needs installed" | Yes | About a week | Do it. It is the direction already stated for R and marimo, and today each engine would add a wire message, a library error and tool text |
| P4 | No provisional marker: the front hands the link its server record; `machines.json` is written only after a connect succeeds | Yes | Days | Do it. The app reads `machines.json` and cannot see the marker, so it would show half-added machines. With it: keep unknown fields on rewrite and add a schema field |
| P5a | Start the local runtime at the first notebook call, not when `endeavor mcp` starts | No, but soon | Small | Do it. Every agent session with the plugin starts Julia today, used or not |
| P5b | Ask before downloading Julia on this computer too | No | Small | Yours to decide. Installing the plugin was agreed to cover the binary; a 289 MB Julia download is a second thing. I would ask, for the same reason as on a server |
| P3 | Move connect, retry and re-attach into the library as one type; the link process becomes a thin wrapper; the link's state becomes one enum | Decide who supervises before the app; the rewrite can follow | One to two weeks | Agree with the direction. The app must otherwise write the supervisor a second time, and that is the part with the races. It is the largest item, so decide it first and schedule it with the app work |
| P6 | The launcher (process or Slurm) chosen per start, not per connection | Yes if done | Unclear | Not sure. It would end the two connections for a new cluster and the link restart on a mode change, but the helper would have to look in two state folders. Worth a short spike before deciding |
| P7 | Drop the launcher's unpinned mode once every release pins its build | No | Small | Do it at the first release from `main`. Until then the unpinned mode is what makes the plugin usable |
| N1 | Hide "link" and "helper" from what an agent reads; say "runtime" and name the engine only when it matters | No | Small | Do it with the skills rewrite |
| N2 | `endeavor status` (links, runtimes, cached binaries, paths, logs) and keeping the previous `link.log` | No | Days | Do it before real users. It is most of what support would need |
| N3 | One public module for every on-disk path, used by the scripts through a hidden `endeavor paths` | Yes for the module | Days | Do it. The server state folder is already defined twice, in Rust and in the bootstrap script, and the two differ |
| N4 | Collapse the near-duplicate library calls (`connect`/`connect_checked`, `start`/`start_with`, the `_within` twins) | Yes | Small | Do it with P1 to P3 |
| N5 | Gate the 14 test-only `ENDEAVOR_*` variables behind one switch | No | Small | Do it. Two of them make the link run a local shell instead of ssh |

Left alone on purpose, and the review agrees: the link as a separate
process, its separate control port, `projects.json`, the 45-second limit,
the three plugin folders with checked copies, `endeavor update`, and the
helper being started by build so link and helper always match.

## What I would add to the review

- **The review is right that the hardest code is working around missing ids
  and versions.** Looking back at the fix rounds, the stop path took three
  of them, and each added bookkeeping (`Stops`, `Owed`, `Said`) to match
  answers by arrival order. With an id on each request that code goes.
- **The first plugin update is the riskiest moment and has not been lived
  through once.** A new front meets an old link, the new link wants its own
  build's helper on every server, and `stop_machine` needs an install
  first. P1's protocol number is what turns most of that into "same
  protocol, carry on".
- **Windows should not be offered yet.** Everything there is compile-checked
  or CI-tested at the unit level only, and the launcher as an MCP command
  is undocumented. Say so in the README until one real session has run.
- **Order.** P1, P2, P4, N3 and N4 together are one piece of work on the
  library's edges and should come before the skills rewrite's trial, since
  they change tool results and states the skills describe. P5a is
  independent and small. P3 is the one to schedule with the app.

## The independent review

Read-only review. I read the eight docs, `link.rs`, `link/run.rs`, `client.rs`, `wire/src/lib.rs`, the launcher and the plugin files in full, and the relevant parts of `standalone.rs`, `standalone/machines.rs`, `lib.rs`, `client/*.rs` and `release.rs`. I did not read `mcp.rs`, `notebooks*`, `core.rs` bodies, `slurm.rs` bodies, `install.sh`/`install.ps1` or the workflows line by line. Nothing was run.

Size: the branch adds 20,566 lines over `main`. The Rust is 31,340 lines, of which about 13,200 are tests.

### Bottom line

The three roles and the one-ssh wire protocol are right and should stay. The problems are in four places:

1. The wire protocol and the link's control calls have no version and no request ids. Much of the hardest code exists to work around that.
2. Julia is written into the wire format, the library API and the machines file, which works against the R and marimo plans.
3. The logic the app most needs (reconnect, reattach, add and use a machine) is not in the library. It sits in the link process and the front.
4. The local path breaks the ask-before-installing rule that the server path follows.

Items 1 to 3 change formats the app will share, so they should be decided before the app adopts the library.

### 1. Does the shape hold?

Mostly yes. Three drifts:

**a. The library is not where the behaviour is.** `client` (1,848 lines without tests) gives the app ssh, the channel, the listener and the machines file. The connect, retry, resume and reattach rules are in `link/run.rs` (`supervise`, `reattach`, `serve_connection`, lines 488 to 829), which is private. Adding and using a machine is in `standalone/machines.rs`, also private. The design doc says the app "keeps its own connection", so the app must write the supervisor a second time. That is the part with the known untested races.

**b. The front does the link's job.** The front decides whether a link of another build may be replaced (`link_rule`, machines.rs:676), refuses to send it starts (`ask`, :701), and works out "settled" by polling with a 3 s grace (`settle`, :1095; `machine_route`, :610). The link should answer "this is the outcome of your request". Today the front infers it from state words.

**c. The front is also a local launcher.** `Relay::start` and `start_or_reuse` (standalone.rs:390, :918) repeat what the helper's `attach` does (lib.rs:594): lock, find existing, find Julia, token, start, wait. So there are two targets with two status types (`Status` in standalone.rs:816 and `link::Status`), two stop paths (`stop_local`, `stop_machine`), and `Target::Local { stopped }` beside `Machine.active`.

### 2. What is extra, and what I would change

**P1. Give control messages ids and a protocol number. (High confidence; before the app.)**
- What: `Stop{id}` answered by `Stopped{id}` or `NotStopped{id}`; the same for `StartRuntime`. Add `protocol: u32` to `Hello` and to `link.json`.
- Why: `Files` already has ids; `Stop` does not, so answers are matched by arrival order. That alone needs the `Stops` queue, `Owed`, `Pending` and the `starting` marker in `client/channel.rs` (:89 to :117, :201 to :222, :377 to :417), and `Said` with its counter and re-sort in `lib.rs` (:483 to :539). gaps.md lists two untested races there.
- Version: `Hello.version` is `CARGO_PKG_VERSION`, always `0.1.0` (lib.rs:300). The only comparison anywhere is `BUILD_VERSION` equality, a source hash. So every release looks incompatible with every other. That drives `link_rule`, "send no start to another build's link", `State::Unknown`, `InstallWhat::Unknown`, the `lenient` decoder, the refuse-unknown-fields rule in `/link/start`, and the `skip_serializing_if` on `download_julia`. With a protocol number, most build differences need no action.
- Cost: a few days, both ends. Lost: nothing a user sees.

**P2. Make "what a start needs installed" engine-neutral. (High confidence; before the app.)**
- What: replace `download_julia: bool`, `ToApp::NoJulia`, `FoundJulia`, `InstallWhat::{Helper, Julia}`, `JuliaInfo` and `StartError::NoJulia` with one shape: the start names an engine and a list of allowed installs; the helper answers with a list of needed items `{kind, engine, description, size, where}`.
- Why: the owner wants R and marimo, installed all at once or on demand. Today each new engine means a new wire variant, a new `InstallWhat`, a new library error and new tool text. `Server.julia` in `machines.json` has the same problem.
- Cost: a week, mostly renaming and tests. Lost: nothing.

**P3. Move the supervisor into the library; the link process becomes a thin wrapper. (High confidence on the direction; before the app, or at least decide who supervises.)**
- What: a public type, say `client::Session`, that owns connect, retry, reattach and start, and reports one typed state. `endeavor link` is that type plus the HTTP control. The app uses the same type in-process, or uses the link.
- Why: see 1a. Also `Inner` (run.rs:62 to :100) is not a state machine. It has 8 public states plus `starting`, `connecting`, `kick_pending`, `gone_early`, `check_first`, `nothing_running`, `wanted`, `resume`, `ended`, `epoch` and `conn`. The real state space is the product. A single enum carrying its data (`Connecting`, `NeedsInstall(info)`, `Connected(ch)`, `Starting(ch, wish)`, `Ready(ch, runtime)`, `Failed(why)`) with one transition function would remove most of the flags.
- Cost: one to two weeks, with the existing 694-line `tests/link.rs` as the safety net. Lost: nothing.
- Also decide whether `pub mod link` is public API. Only tests use it outside the crate.

**P4. Remove the provisional marker. (High confidence; before the app.)**
- What: the front passes the server record to the link when it starts it (argument or stdin). Save to `machines.json` only after a connect succeeds.
- Why: the marker exists only because the link reads its target from `machines.json` by id (run.rs:158, :669). So `add_machine` must save first, then mark, then undo. That is the `restore` and `undo` closures and eight `inspect_err(|_| undo(self))` calls in a 208-line function (machines.rs:860 to :1068). The app reads `machines.json` and cannot see a marker in `links/<id>/`, so it would show half-added machines as real.
- Lost: an interrupted first `add_machine` leaves nothing behind, so the agent calls it again from scratch. That is the same as today from the user's side.

**P5. Local start: make it lazy and ask before downloading Julia. (High confidence; can be done any time, but soon.)**
- `relay()` starts the runtime at process start when the target is local (standalone.rs:861). `start_or_reuse` calls `julia::find(&options.julia, true, …)` (:414), so a missing Julia is downloaded without a question. Every session in every project with the plugin enabled does this, even if it never opens a notebook. It is one runtime per computer, not one per session, but it contradicts the "Installing on a server" row of the Decided table in spirit.
- Change: start on the first notebook call (`Relay::runtime` already handles `Status::Idle`), and return `needs_install` for Julia as the server path does. (Decided otherwise for the second half: no question on this computer. See "Decided".)
- Longer term: treat `local` as a machine reached through the same channel (the helper over a local pipe). That removes drift 1c. Moderate confidence; a week or more; not urgent.

**P6. Take the launcher choice out of the connection. (Moderate confidence; wire change, so decide before the app.)**
- The launcher (`process` or `slurm`) and the Julia setting are fixed in the bootstrap preamble (ssh.rs:228). That is why adding a cluster takes two connections and why the front quits the link when the mode changes (machines.rs:1021 to :1024). If `StartRuntime` carried the launcher, one helper could serve both.
- Lost: the helper would have to look in both state folders. I am less sure of the Slurm side effects.

**P7. Drop the launcher's unpinned mode. (Moderate confidence; can wait.)**
- `.newest`, `.checked`, the daily background fetch and half the lock logic in `scripts/endeavor-mcp.sh` (183 lines) exist only for an empty `release-key`. If every release pins its key, the launcher is: have the binary, or fetch it under a lock. `ENDEAVOR_BIN` covers development.
- Lost: a plugin installed from an unreleased commit does not work without `ENDEAVOR_BIN`.

**Candidates I looked at and would leave alone:**
- **The link as a separate process.** It buys one ssh, one sign-in and a stable browser address across sessions, and it outlives a session. The front holding the connection loses all three.
- **The separate control port.** Merging it into the runtime port means parsing HTTP on what is now a byte relay. Not worth it.
- **`projects.json`.** 149 lines, and it is what makes "the next session" work. Keep.
- **The 45 s deadline.** The need is real (Codex's 60 s). `Deadline` is small. Its awkward part, running link calls on a spare thread and abandoning them, goes away if the link answers outcomes (1b).
- **Three plugin folders with copied skills.** `plugins.sh check` in CI makes the copies safe. Keep until the harnesses are tested.
- **`endeavor update`.** Needed for `serve` users on servers. The guard for plugin-managed binaries is small.
- **Helper cache pruning.** Slightly wrong (gaps.md) but cheap. Can wait.
- **`--exit-idle`.** The mechanism is fine. The problem is that idle policy depends on who started the runtime (gaps.md, "Idle exit"). Record the policy in `runtime.json` and show it; decide one default.
- **Test-only variables.** 14 of 29 `ENDEAVOR_*` variables are for tests (`LINK_SHELL`, `LINK_ROOT`, `LINK_STATE`, `LINK_DEPOT`, `LINK_ASK`, `LINK_IDLE_SECS`, `FRONT_PING_SECS`, `START_WAIT_SECS`, `START_LOCK_SECS`, `STOP_LOCK_SECS`, `SLURM_POLL_MS`, `IDLE_CHECK_SECS`, `JOB_TEST_PIDS`, `JOB_TEST_ROLE`). They work in release builds. `ENDEAVOR_LINK_SHELL` and `ENDEAVOR_LINK_ASK` make the link run local shell commands instead of ssh. Low risk, since anyone who can set them can already run code, but they should be gated behind one switch or a debug build.

### 3. What is missing

- **Protocol versions.** See P1. The app's `/endeavor/call` methods have none either (the docs say so).
- **A command to see and clean up.** There is `serve`, `mcp`, `stop`, `update`, `--version`, and hidden `link`, `connect`, `core`, `relay`. Nothing lists links, runtimes, cached binaries or state. A user with a stuck link must find `~/.local/state/endeavor/links/<id>/link.json` and kill a pid. `endeavor status` (read-only) and `endeavor link stop <machine>` would cover most support cases.
- **Logs a user can find.** `link.log` is opened with `truncate(true)` each time a link starts (link.rs:363). After a crash, the next front erases the reason. The front logs to stderr, which the harness may or may not keep. Keep the previous log (`link.log.1`) and print the paths in `endeavor status`.
- **Failures outside a tool call.** A link that gives up after 10 minutes sets `failed` and waits. The user learns of it at the next tool call, or from a dead browser page. The listener's "not connected" page is the only signal. Acceptable for now; say so in the docs.
- **Uninstall.** Nothing removes `~/.local/share/endeavor/bin/*`, the helpers cache, the state folders, or the per-build helper folders on servers. I found no pruning of `<root>/<build>/` in the bootstrap script, so each plugin update leaves another helper folder on every server. I did not check whether the helper prunes them itself.
- **One place for paths.** `Env` has seven path functions (standalone.rs:122 to :180), `machines_path` is in `client/machines.rs`, the binary store is defined only in shell, and the server state folder is defined a second time in the bootstrap script (ssh.rs:216: `${XDG_STATE_HOME:-$HOME/.local/state}/endeavor/serve/$(uname -n)`). The two copies already differ: Rust accepts `XDG_STATE_HOME` only if absolute and uses `hostname()`; the shell takes any value and uses `uname -n`. `Env::depot` uses `~/.cache` and ignores `XDG_CACHE_HOME`, unlike `Env::cache`. Put all of them in one public module the app can call, and add a hidden `endeavor paths` the scripts and tests can read.
- **Engine-neutral installs.** See P2.
- **`machines.json` has no schema field and drops unknown fields** (gaps.md). Must be fixed before the app writes the same file.

### 4. Risk ranking: first month

1. **The first start through the launcher.** Codex and Antigravity entries are built from documentation only. `release-key` is empty. A download can exceed the harness's 30 s start limit. On Windows the MCP command is `sh`. Failure here means no tools at all, and the user sees only "server failed".
2. **The first plugin update.** The new front finds an old link with a runtime and sends it no start. The new link needs its own build's helper, so every server asks for an install again, and `stop_machine` cannot stop the old runtime without installing first. This path is tested only with a faked build string (gaps.md).
3. **Link lifetime on a laptop.** Sleep, network change, a stale `SSH_AUTH_SOCK` from the first session, and the flag-based state in `run.rs`. Three gaps.md entries here have no forcing test.
4. **Local eager start.** Julia starts, and may download, when a session opens. On a first run this is minutes of work and hundreds of MB that nobody asked for.
5. **Real clusters.** Queued and starting states have not been seen on real Slurm through the tools. The pre-install look parses `runtime.json` with shell patterns. The ssh-to-node relay route is not exercised.

Windows is not on this list because it should not be offered yet: every Windows path is compile-checked only.

### 5. Complexity budget

| File or function | Lines | Verdict |
|---|---|---|
| `standalone/machines.rs` | 1,527 | Accidental in large part. `add_machine` is 208 lines with manual rollback (P4, P6). `use_machine`, `settle`, `machine_route` re-derive link outcomes (1b). About 300 lines are message text and JSON shaping, which is fine but should be its own file. |
| `link/run.rs` | 1,017 | The problem is hard, but the flag set is accidental (P3). |
| `lib.rs` | 1,327 | The helper's main loop, process control, state file, HTTP calls and `Said` in one file. `Said` goes with P1. Split process control and the state file out. |
| `standalone.rs` | 1,290 | Holds `Env`, argument parsing, runtime unpacking and leases, start locks, `serve`, `stop` and the whole stdio relay. Split by those headings. |
| `client/channel.rs` | 554 | About a third is stop bookkeeping that P1 removes. |
| `bootstrap_script` line 216 | 1 line, about 1,400 characters | Essential constraint (one line, no quotes), but it is unreadable and duplicates path rules. Keep the look minimal: report platform and whether the helper exists; drop the runtime probe, or accept "unknown". |

The library API also has near-duplicate pairs: `connect`/`connect_checked`, `start`/`start_with`, `start_runtime`/`start_runtime_with`, four `ensure*`, and `status`/`start`/`install`/`attach` each with a `_within` twin. `Options.root`, `state` and `depot` are free strings used mainly by tests. Collapse each pair to one function before the app depends on both.

### 6. Names and concepts

An agent meets: machine, local, link, helper, runtime, Julia, job, session, notebook, project, folder, plus the states `connecting`, `connected`, `starting`, `queued`, `ready`, `failed`, `needs_install`, `needs_job`, `unknown`, `not yet connected`, `no link running`.

- **Hide "link".** It appears in `list_machines` (`"no link running"`), in `link_rule`'s note, and in error text ("The link to X isn't answering"). The agent cannot act on it. Say "not connected" and "the connection".
- **Hide "helper" where possible.** "Endeavor's program on the machine" is what the skill already says. Use one phrase everywhere.
- **Pick "runtime" or "Julia".** Tool text says "Julia on X stopped"; states say runtime. With a second engine, "Julia" is wrong. Use "runtime" for the process and name the engine only when it matters.
- **"provisional" and `not yet connected`** go away with P4.
- **`unknown`** goes away with P1.
- **"front" and "core"** are internal and do not leak. Good.

### 7. Sequencing

**Decide before the app adopts the library** (they change the wire, the API or shared files):
- P1: request ids and a protocol number.
- P2: engine-neutral installs.
- P3: who supervises a connection, and whether `link` is public.
- P4 and the `machines.json` fixes: keep unknown fields, add a schema field, no half-added records.
- P6: launcher per start or per connection.
- One public module for every on-disk path, and the idle policy recorded in `runtime.json`.
- Collapse the duplicate API pairs.

**Can wait:**
- The internal rewrite of `run.rs` once its public shape is fixed.
- P5's "local as a machine" (but do the lazy start and the Julia question now).
- P7, cache pruning, uninstall, `endeavor status`, log rotation, gating the test variables, splitting the large files.

### What is good and should be left alone

- The bootstrap runs `<root>/<build>/endeavor`, so link and helper always match. That removes one whole compatibility axis.
- One ssh carrying multiplexed streams, with the browser on the same loopback port as the agent.
- The one-port core and its token and cookie rules.
- Whole-file write, rename and lock for `machines.json` and `projects.json`.
- Two separate agreements for the helper and for the engine download.
- "No client makes another exit", and stop taking the start lock.
- `use_machine` doing everything that can fail before the session moves.
- gaps.md itself. It is accurate, and most of what I found is already named there.

### Could not determine

- Whether Codex and Antigravity expand plugin variables, start the server in the project folder, or load the skills.
- Any behaviour on real Windows or macOS.
- What the app's current client code needs from the API beyond what the docs say. The app is not in this repository.
- Whether old per-build helper folders on servers are ever removed.
- The Slurm side effects of P6.
