# The client

`crates/endeavor-mcp/src/client*`: the client half of a remote connection
([plugins-and-remote.md](plugins-and-remote.md)), which the app and the
plugin's front both use. It runs `ssh` to a server with a bootstrap script,
speaks the wire protocol ([wire.md](wire.md)) to `endeavor connect` there,
starts or attaches to the runtime, and serves the runtime's port on a
loopback port of this computer. It uses std threads and blocking I/O, and
needs no window and no prompt.

## The parts

- `ssh.rs`: one connect. ssh runs the bootstrap script, which becomes the
  helper; its stdin and stdout carry the frames from then on.
- `channel.rs`: the client's end of a helper's stdin and stdout, on this
  computer or over ssh. It lasts as long as the helper: runtimes start, die
  and stop on it.
- `listener.rs`: the loopback port for one server's runtime.
- `session.rs`: one machine's connection and everything that hangs on it,
  including getting the connection back when it drops.
- `askpass.rs`: the app's side of askpass over loopback TCP.
- `machines.rs`: the machines file and its records.

## ssh

ssh is started with:

- `-T`, and a keepalive every 10 s of silence (`ServerAliveInterval=10`,
  `ServerAliveCountMax=2`), so a dead link ends the helper's channel within
  about 30 s even while nothing else is sent.
- `ConnectTimeout=20`, except on Windows. Windows' ssh with a timeout stops
  at the first address that refuses (IPv6 `localhost`, say) instead of trying
  the next (#35). There, Windows' own connect gives up on each address that
  doesn't answer after about 21 s. A server that takes the connection and
  then says nothing is covered by the connect's own deadline, below.
- On Windows, `ControlMaster=no` and `ControlPath=none`: its ssh has no
  connection sharing, so a ControlMaster line in a config copied from a Mac
  would make the connect fail.
- `BatchMode=yes` with `Auth::Batch`, so ssh fails instead of asking. Keys,
  the agent and `~/.ssh/config` do the signing in. A caller that answers
  ssh's prompts itself passes `Auth::Env` with its askpass variables, which
  set `SSH_ASKPASS_REQUIRE=force`: Windows' ssh uses an askpass only then, and
  otherwise waits on a console nobody sees.

The connect keeps its own deadline of 60 s, on every platform, from starting
ssh to the bootstrap's first `ENDEAVOR` line. It ends there, or as soon as ssh
asks a question through the caller's askpass (`Auth::Env`), so a user typing a
password or a code is never cut off. Past it, ssh is ended through the cancel
path, and the error says the server didn't answer. A login script that hangs
for a minute before the bootstrap runs times out too.

Which ssh: on Windows, Windows' own OpenSSH when it is installed, else the
first `ssh` on the PATH. In Git Bash that is Git's, which signs in with the
Windows user name as it is spelled (`JDoe`) where Windows' lowercases it,
and may read another `~/.ssh` than `%USERPROFILE%\.ssh`. On Windows the
client starts ssh, and the curl or wget that fetches a server's helper,
without a console window (`no_window`), as the core starts Julia: from the app, which has no console,
each would open one. The app uses it for its own console programs.

ssh runs in its own process group, which ends with it: the askpass it may
wait on, or the shells of the test transport. The connect keeps ssh's pid
from the start until the channel's reader has reaped it, so a cancel never
kills another process that got the pid.

### The bootstrap script

It is one line: the server's login shell (sh, bash, zsh, fish or csh) gets
it inside single quotes, so it holds no quote, backslash, `!` or newline.

- It reads eight lines first: the install root, the state folder, the depot,
  the helper's Julia flag and value, its launcher, and its R flag and value.
  Login scripts may print things before the script's own line.
- It settles a launcher of `auto` as the helper would (`PICK_LAUNCHER_SH`):
  `slurm` when `sinfo` is a file in a folder of `$PATH` or of
  `wire::slurm::FOLDERS`, else `process`.
- It prints `ENDEAVOR <os> <arch> have`, or, when this build's helper isn't
  installed, `ENDEAVOR <os> <arch> need <seen> <older|first> <folder>`.
- Unless the user agreed to install the helper (`allow_install`), a server
  without it is only looked at: nothing is written there, and the connect
  ends with what it found (`NeedsInstall`). Installing what a start needs,
  such as Julia, is asked for with each start instead.
- A server's core is never started with `--own-r`. The core also reads it
  as "this is the user's own computer" and then lets Python notebooks start
  there (`core::MarimoStarter`), which a server's runtime isn't set up for
  (#354). Offering Endeavor's R on a server needs its own flag first (#423).

### Explaining a failure

`explain` turns ssh's stderr into plain words. ssh exits with 255 for its
own failures (sign-in, network, host key) and otherwise with the remote
command's status, so the sign-in explanations apply only to a 255 or an
unknown status, and never once the script has answered. In batch mode ssh
can't ask, so they say what to do instead. Windows ships the ssh agent's
service turned off, so `ssh-add` fails there until an administrator turns it
on.

Each failure says whether trying again by itself could help. It can't when
the user has to act first (a key to add, a host to accept) or the request is
wrong. It can when the network or the server was the trouble, a name that
doesn't resolve (a laptop that just woke up) included.

An askpass that ends because the server stopped waiting for the sign-in
(sshd's LoginGraceTime) writes `SIGN_IN_GAVE_UP` on ssh's stderr. That is
said first, since ssh's own words after it depend on what it asked
("Permission denied" after an unanswered passphrase, a closed connection
after a password), and a reconnect tries again either way (#64).

### Test connection

`test` connects, checks the runtime answers through a listener, then stops
it, or leaves it running if it already was. A runtime found running may
start Julia only once a notebook needs it, so the test has it start Julia
now (`endeavor/start_julia`) and fails with why Julia can't start (app #169).
On a cluster it only asks Slurm about itself, since starting Julia there means
a job. A cancel kills ssh; the helper then stops the runtime this test was
starting, and leaves one that was running for someone else.

## The channel

- The helper says `Hello` first. Its protocol must equal this client's.
- Requests carry ids, and their answers go to whoever waits on that id. An
  answer with no waiter (it gave up) is dropped, so it never reads as the
  answer to a later request. Control messages that answer no request go to
  whoever waits on the helper now: its hello, a start, or the runtime's
  watcher.
- Only one start is under way at a time: a second one would take the first
  one's progress and its watcher.
- A stop waits up to about 60 s: the helper waits up to 20 s for the start
  lock, then stopping takes up to 20 s more. Each stop under way keeps the
  runtime's watcher quiet, so the client doesn't hear of a runtime going away
  that it stopped itself. `NotStopped` lets go of that hold before anything
  after it is heard. A stop that times out leaves it unknown whether the
  runtime stopped, and its answer is dropped from then on.
- When the client quits, it sends `Stop { unless_shared }`: Julia goes on if
  the plugin works in it too (app #232).
- A channel nobody holds closes the helper's input, as a client that vanished
  does. The helper applies its own rule (`--quit-with-client`), so ssh and the
  helper don't outlive a failed connect or a panic.
- The channel waits for the helper to exit without reaping it, and calls
  `exited` before reaping, so the pid is still the helper's then.
- Why Julia stopped, for "Julia on <host> stopped.", is how a cluster job
  ended, else how the process exited, plus a cause from its log when one is
  recognized. A crash's log is mostly Pluto's startup banner, so nothing else
  of it is shown.

## The listener

One loopback port per server's runtime. Pluto's page, the agent's MCP and the
app's calls all go through it, and each connection is relayed to the runtime
of the moment. The port stays the same while the runtime goes away and comes
back, so the MCP URL in the agent's config stays right.

- While the runtime is being got back, a connection waits for it up to 2 s (`HOLD`),
  so a short drop goes unnoticed. Then an MCP request is answered with why
  (`Messages`), and anything else is closed. A runtime that is already away
  keeps its reason: a drop never replaces a deliberate one.
- `Messages` speaks in the app's words (its buttons) by default. A caller with
  other controls passes its own.
- `hand_over` closes the listener but keeps its socket, for a new listener to
  take connections on. The port is never free in between, so nothing else can
  have it, and a connection that waits to be taken is taken by the new one.
  Closing and binding the port again would fail on macOS while an agent's
  keep-alive connection is still open.
- A failed accept (no file descriptors left, a connection reset before it was
  taken) doesn't end the listener. It waits 50 ms, doubling at each failure in
  a row up to a second, and reports only the first of a run.
- Pluto's start page carries the pages' token. The core trades it for a
  cookie and drops it from the URL.

### The guard

Connections are relayed without reading their HTTP unless the client asks
for something that needs it:

- `serve_guarded` fails agent calls that `refuse` gives a reason for.
- `set_snapshot`: a `view_cell_output` the runtime has no picture for gets the
  client's. On Windows the guard doesn't notice an agent hanging up during a
  stream (`wait_readable` isn't ported) until the wait times out, so call it
  only where the client takes pictures.
- `set_restart`: the agent gets `request_restart`, and only the user's yes
  restarts anything.
- A runtime whose `Ready` gives neither its build nor its interface is from
  before runs were held for the user's answer (EndeavorMCP b0cab29). When the
  client holds runs for the user's answer, as the app does with
  `endeavor/set_policy`, the listener refuses its code runs
  (`Messages::no_run_gate`). The rule is set with the runtime, so no call
  reaches it first.
- A runtime another build started, which doesn't offer this build's
  interface, is told to each agent session with its first tool result and in
  every `session_status` (`guard::Older`). The app's agent has no other way
  to know (#239); the plugin's front tells its agent itself, so it turns this
  off.

### How the guard serves a connection (`guard.rs`)

- Requests on a connection are served one at a time, each checked the same
  way. Everything but an agent's tool call that is refused or answered here
  (the app's own calls and event stream, Pluto's page and its WebSocket, the
  agent's other messages) passes through as it is. A `100 Continue` from the
  runtime is passed on before the real response.
- **A snapshot** replaces a `view_cell_output` the runtime has no picture for.
  The client gives none when the notebook isn't open in the app, or the output
  is empty or not drawn yet, which would give the agent a blank picture. In
  safe preview the runtime can't render the value but the notebook still shows
  it, so a snapshot is asked for any output there, text too. A picture from
  the output's HTML may be only part of it, so the notebook's picture wins; a
  value's own picture stands, as does an error a picture wouldn't help with.
  A picture taller than the client can show says so, and the agent is told
  the rest is missing.
- **Another build's runtime**: the notice goes as `other_version` in a
  `session_status` that worked, which tells the session, else as a note after
  the session's first tool result, once per session for as long as that
  runtime is the one relayed to. A call that may wait on the user (a run, an
  install, an edit in Manual) keeps its stream, so the session's next call
  that can't wait says it. A runtime from before R notebooks refused an R
  notebook's path saying only that it must end in .jl; the guard says why
  instead.
- **`request_restart`** is answered here, so it works whatever build the
  runtime is and while it is gone. It waits up to 50 s for the user, under the
  60 s agents give a call; the question stays on the user's screen after
  that. Its words follow those for a kept older runtime: small models take a
  named way to stop something as a recipe, so it says plainly that only the
  user's yes restarts anything. The answer can wait on the user, so it is
  streamed to an agent that takes a stream, and the connection closes after
  it: once the user restarts, the old connection leads to the runtime that
  stopped.
- **Between requests**, the guard checks every second whether the runtime's
  end closed. If it did (the runtime stopped or restarted), the agent's
  connection closes as an unguarded one would, and the agent's next request
  comes through the listener to the runtime of the moment instead of failing
  unanswered (#292).

## The session

A caller says what it wants (`Want::Attach` or `Want::Start`) and hears how
it stands (`Outcome`). The work goes on in a thread of the session's, which
ends when the session is closed or dropped. Closing detaches from the helper
and never stops the runtime.

- What is asked for is judged from what the session holds, not from who
  asked. With a start or an attach under way, or a runtime attached, a call
  only waits for it, so any number of callers can ask at once.
- An `Attach` never replaces a `Start` that is wanted or under way, and a
  `Start` is never answered `NothingRunning`. A cluster has one job per user,
  so a `Start` with a job on a session that already has one gets what is
  there.
- Agreement to install the helper is kept for the session's life, so a
  reconnect to a machine that lost the helper doesn't ask again. Agreement to
  what a start needs, such as Julia's download, is asked for by each start.
  With a connection, a start's `install` is only for what the start needs.
- A reconnect waits 1 s after its first failure, doubling up to 30 s (`RETRY_FIRST` to `RETRY_LAST`). A connection that stays lost for 10 minutes (`RETRY_GIVE_UP`) is `Failed`.
- After a reconnect, the session attaches to the runtime that was asked for
  only if the helper says it is still there (running, starting, or a job that
  waits). A start would otherwise begin a new runtime, or on a cluster submit
  a job nobody asked for.
- The first helper's settling of `Launcher::Auto` is kept: a reconnect asks
  for the same, so the session never changes how it runs the runtime.
- Stops are counted before the helper is asked, since that takes a while: the
  end of a start that a stop cut short is no failure, and a connection that
  comes later mustn't attach to what was stopped.
- `restart` is a stop for every client of the runtime, the plugin's too, then
  a start. Notebooks that were open are not opened again. Calls meanwhile are
  told Julia is restarting, and, if the start fails, that it couldn't start.
- `close` tells the helper to let go before ssh is ended, or it would only
  see its input end, and waits only a short time, so a dead connection holds
  nothing up.
- Events come on a session thread after the session has recorded them, with
  no lock held, so a caller may read `status` and see the state the event
  brought. The callback must return quickly and must not close or drop the
  session. Not every change of state has an event (a runtime that is ready,
  or found not running, doesn't), so a caller that shows the state reads
  `status`.

## Askpass over loopback TCP

ssh runs the helper as `SSH_ASKPASS` with the prompt as its only argument.
The helper passes the prompt to the app, only on this computer, over its
Unix socket or loopback TCP (Rust's std has no Unix sockets on Windows), and
prints the answer for ssh, or exits 1 if the user cancelled. A notice ("touch
your security key") needs no answer: ssh takes it down itself.

`askpass::Asker` is a port on 127.0.0.1 and a random token, one per
listener, that a connection must send first. It works on every platform, and
is the only way on Windows.

- 127.0.0.1 is open to every user's processes. The token, which only ssh's
  environment holds, keeps other users' out. It doesn't keep out this user's
  own processes, which can read that environment, as they can a Unix socket.
  The token is compared in constant time.
- A connection has `READ_LIMIT` in all to send the token and one question,
  and at most `MOST_BYTES`. One without the token is closed unanswered, and
  only so many are served at once.
- The listener shuts its write half first, then reads what the client still
  sends for a moment, before closing. A socket closed with unread bytes sends
  a reset instead of an end, which the client may get before the answer, as
  Windows' does (#112).
- `Reply::GaveUp` is nobody answering (no one there, or not in time); ssh's
  error then says the sign-in wasn't answered, which a reconnect tries again.
  `Reply::Refused` is the user's Cancel, which ssh takes as a refused
  sign-in.

## The machines file

`machines.json` lists the machines besides this computer, and the app reads
and writes it as well: `{"schema": 1, "machines": [Server, ...]}`.

- A bare list, the shape before the schema number, is read as schema 1 and
  written as an object. A file with a higher schema is an error for a writer.
- It is written whole to a temporary file that is renamed, readable by this
  user only, under a lock so two processes don't lose each other's change.
  A file that can't be read or parsed is an error that names it, and is never
  replaced.
- Fields this version doesn't know are kept on a rewrite, at every depth:
  in the file, a machine's record, and its cluster, job defaults and
  partitions (matched by name).
- `save_expecting` writes a machine worked out from an earlier read only if the
  list still picks the same record for it.
- An id may become a folder's name, and the file can be edited by hand, so
  `valid_id` refuses capitals (Windows and macOS give two ids that differ only
  in case one folder), names Windows keeps for devices (`nul`, `com1`, even as
  `nul.txt`) and a trailing dot.
- A machine's Julia is a path or a shell line such as `module load julia`;
  unset looks on the login shell's PATH, then downloads Endeavor's own. R is
  the same, but Endeavor doesn't download R. A cluster's depot defaults to
  `$SCRATCH/endeavor/depot` when the cluster sets `$SCRATCH` (home quotas are
  small).
