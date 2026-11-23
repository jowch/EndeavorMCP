# The wire protocol

What the app and its helper (`endeavor`) say to each other over the
helper's stdin and stdout, in `crates/wire`. Over ssh this is one channel:
TCP streams to the runtime's port, multiplexed byte for byte, plus JSON
control messages.

## Frames

Every frame is a 4-byte big-endian length `n`, then `n` bytes: a kind byte
and its body.

| kind | frame     | body                                       |
|------|-----------|--------------------------------------------|
| 0    | `Open`    | stream id (u32 BE)                         |
| 1    | `Data`    | stream id (u32 BE), then the bytes         |
| 2    | `Close`   | stream id (u32 BE)                         |
| 3    | `Control` | one JSON message (`ToApp` or `ToHelper`)   |

- Only the app opens streams, so it picks the ids.
- `Close` ends a stream in both directions. Either side may send it, and
  frames for an id that's already closed are dropped.
- A frame longer than `MAX_FRAME` is corrupt input, not a message. An end of
  input between frames is a clean end; one inside a frame is an error.

## The protocol number

`PROTOCOL` numbers the frames and both message enums. The helper says it in
`ToApp::Hello`, and a client refuses a helper whose number differs from its
own. A helper that says no number is 0.

Raise it when a client and a helper of the previous number can no longer
work together: a message or field one of them needs and the other wouldn't
understand, or one whose meaning changed. Adding an optional field, or a
message a peer may ignore, does not raise it. That is why most new fields
are `#[serde(default)]`, and absent means what an older peer meant.

## Requests and answers

`StartRuntime`, `Stop` and `Files` carry an `id` the client picks. The
helper answers each request once, naming its id (`ToApp::answers`). Ids are
the client's to keep apart: one in use by a request still waiting must not
be used again.

A start's progress (`Progress`, `Found`, `Submitted`, `Queued`) names no id: only
one start is under way at a time, and the answer that ends it names it. The
relay on a cluster job's node, which has no request, says `Ready` with id 0.

### App to helper

- **`StartRuntime`**: attach to the runtime, starting it if it isn't
  running. `engine` names the notebook system (`ENGINE_PLUTO`). On a cluster,
  `job` says what to submit.
  - Without `install`, a start that needs something installed is answered
    `NeedsInstall` and installs nothing. With it, the helper may install what
    the start needs, such as a language runtime.
  - `attach_only` never starts one: the helper attaches to the runtime that
    runs, or waits for a start under way and attaches to what it records. It
    answers `NotRunning` when there is none, or the start ended without one.
- **`Stop`**: stop the runtime and stay connected: the attached one, else the
  one recorded in the state folder, or on a cluster the job waiting for it.
  - A start under way ends with its own answer (`StartCancelled`), then the
    stop is answered. That includes this connection's wait for a start another
    connection began.
  - Without `force`, that other start is left to finish and the answer is
    `NotStopped`. With it, it is cancelled as `endeavor stop --force` cancels
    it.
  - `unless_shared` is the app quitting. A runtime the plugin also works in is
    left running for it, and ends itself once idle. The helper exits as after
    `Detach`, without an answer.
- **`Detach`**: exit and leave the runtime running.
- **`OwnWithClient`**: sets what `--own-with-client` is from now on. With it
  on, a client that goes away without a word stops a runtime this connection
  started. The app sent it while it had a "Keep notebooks running" setting;
  the setting is gone (app #274) and today's app doesn't send it, but a helper
  still takes it from an older app.
- **`Files`**: a question about the host, or a write (see below).

### Helper to app

- **`Hello`**, as soon as the helper runs. File requests work from then on;
  the runtime starts only when the app sends `StartRuntime`. It says the
  protocol, the machine, whether Slurm's commands are there (probably a
  login node), whether it takes `Place` and `Write`, and the launcher it
  runs the runtime with for the whole connection (`process` or `slurm`, what
  `--launcher auto` became).
- **`Ready`** answers a start: the runtime is up and streams can open. It
  carries the launcher and node, the bearer token for every path on the
  runtime's port but the pages, the pages' own token (none from a runtime or
  helper older than that token, whose pages take the bearer token), whether
  the runtime was already running, the cluster job, the runtime's port on
  its node, and the build and `CORE_INTERFACE` its record names.
- **`StartFailed`**, **`NeedsInstall`**, **`StartDied`** (the runtime exited
  while the start waited), **`StartCancelled`** (a `Stop` ended it, sent
  before the `Stopped`), **`NotRunning`** (an `attach_only` found none):
  the other answers to a start. After each the helper stays connected, so the
  app can start again.
- **`Stopped`** or **`NotStopped`** (and why) answers each `Stop`.
- **`Died`**: the runtime exited with no request waiting for it.
- **`Error`**: the helper can't go on, and exits.

`Item` is something to install before a start can go on. `kind` is a plain
string (`runtime`, `helper`), so a kind a peer doesn't know still reads.
`needs_text` turns the items into the sentences the user sees.

## Files

`files::Request` is what the app asks a host before, or without, attaching to
its runtime: a folder's contents, the notebooks under a folder, a notebook's
first cells, Slurm's partitions, the Julia a cluster job would use, and
whether a runtime or its job is there. The helper answers all but `Julia`
and `Runtime` with `files::answer`; only it knows its Julia and its state
folder.

- Paths in replies follow the rules of the machine that answered. A server is
  Linux or macOS, so its paths follow `/` rules whatever the app's computer
  uses, and the app carries them as strings (`server_path`). On Windows a
  `PathBuf` would put `\` into them and read `C:` as a drive.
- `Folder` makes a session's folder only when the folder it goes in is there.
  When that one is missing too, the path is more likely mistyped, or another
  computer's, than new.
- An attached file goes into the folder's `data/`. `Place` picks the name:
  `data/<name>` if that's free or holds the same contents (size and SHA-256),
  else `name (2)`, and so on. `have` says there's nothing to send.
- `Write` sends the file in pieces, in order, into a hidden `.<name>.part`
  beside it. The first piece starts the part over; the last renames it, so a
  file cut short never looks finished. A piece that fails, or doesn't follow
  on, deletes the part. The part is made with `create_new`, so a link planted
  at its name isn't followed.
- Only a Windows machine refuses a name Windows can't hold (`\`, `:`, a
  trailing dot or space, a device name such as `CON`).

## The relay

`relay` carries many TCP streams over one frame channel, the same on both
ends: the app attaches the connections it accepts, the helper the ones it
dials for each `Open`.

- Each stream has a reader thread (socket to `Data` frames) and a writer
  thread fed by a bounded queue (frames to socket), so one slow socket never
  blocks the channel for the others.
- A queue that stays full for `stall` means its socket stopped reading. That
  stream is closed and the channel moves on, so memory stays bounded and the
  other streams wait at most `stall`.
- A stream is registered before its `Open` goes out, so the first reply finds
  it, and its reader starts after, so no `Data` precedes the `Open`.
- When the channel ends, every stream gets what was already queued, then an
  end. A connection relayed after that, as the app's listener can still do
  before it hears of the end, is refused rather than left with nobody to
  answer or close it.

## Sockets are shared, not copied

`Shared` is one connection shared between its reader and writer, or between
threads, through one handle. On Windows, std's `try_clone` makes an
inheritable copy of a socket, so a child started while the copy was open
(Julia, or `julia --version` while finding it) kept the connection open after
it was closed: a page waited for its end for as long as Julia ran, or was
reset when a short-lived child ended. Std opens the one handle
non-inheritable, so sharing it leaves no copy to inherit. `clippy.toml`
forbids `try_clone` outside tests (#157).

## Askpass

When ssh asks for a password, a two-factor code or a yes/no, it runs the app
(or the helper) as `SSH_ASKPASS`. That sends one `Ask` line to the app and
prints the `Answer` for ssh.

- It reaches the app on loopback TCP when `ADDRESS_ENV` is set, sending
  `TOKEN_ENV`'s token as its first line so the app answers no other local
  process. Otherwise, on Unix only, it uses the socket in `SOCKET_ENV`.
  Windows' Rust has no Unix sockets, so it uses TCP; TCP wins when both are
  set.
- An answer of `None` is a cancel. With `gave_up`, nobody answered (no one
  was there to ask, or not in time): the askpass says so on ssh's stderr, so
  the connect fails as one to try again, not as a refused sign-in.

## Shared lists

- `slurm::FOLDERS` is where the helper looks for Slurm's commands besides the
  PATH. The bootstrap script repeats it (`PICK_LAUNCHER_SH` in
  `endeavor-mcp`), and a test there checks they match.
- `tree` hashes the files of an install (`runtime/`, `plugin/`) with FNV-1a,
  which is stable across builds, unlike std's hasher. `endeavor-mcp`'s build
  script includes the file to embed those folders, so it uses only `std`.
- `backend::is_marimo` tells a marimo file by what it does near its start
  (imports marimo, makes `app = marimo.App(...)`), since a marimo file has no
  first line of its own.
