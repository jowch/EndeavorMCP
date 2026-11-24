# Scripts

What each script in `scripts/` does, and why it works as it does. How the
workflows use them is in [ci.md](ci.md).

## Installing (`install.sh`, `install.ps1`)

```sh
curl -fsSL https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.sh | sh
curl -fsSL .../install.sh | sh -s -- --dir /some/folder
```

```powershell
irm https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.ps1 | iex
```

`install.sh` is for Linux and macOS (and Git Bash on Windows); `install.ps1` is
for Windows, PowerShell 5.1 or newer. Neither needs sudo or administrator
rights.

- `install.sh` reads `LATEST` from the release, or takes the build from
  `--key`, or from main's pin (below). It downloads `endeavor-<key>-<platform>`
  and `endeavor-<key>.sha256`, checks the SHA-256, and puts the binary in DIR
  as `endeavor` (`endeavor.exe` under Git Bash), replacing one that is there.
  DIR is `--dir`, else `$ENDEAVOR_INSTALL_DIR`, else `~/.local/bin`. No shell
  file is edited: if DIR isn't on the PATH, the line to add is printed.
- `install.ps1` puts `endeavor.exe` in `-Dir`, else `$env:ENDEAVOR_INSTALL_DIR`,
  else `%LOCALAPPDATA%\Endeavor\bin`, and adds it to the user's PATH if it isn't
  there. With `irm | iex` there is no way to pass `-Dir`, so set
  `ENDEAVOR_INSTALL_DIR` first. Everything runs in a script block, so
  `irm | iex` leaves no variables, functions or preferences in the session. A
  copy that is running is renamed to `endeavor.exe.old-<pid>`, which the next
  install or `endeavor update` removes once nothing runs from it.
- The user's Path is read as stored, with its `%VARIABLES%` unexpanded, so
  writing it back keeps them. Setting and clearing a variable makes Windows
  tell running programs that the environment changed.
- The SHA-256 is .NET's rather than `Get-FileHash`, which Windows PowerShell
  5.1 defines in a script module that doesn't load when `PSModulePath` names
  PowerShell 7's modules (a 5.1 started from pwsh).

### Which build, checked against what

Run by hand, both read `release-key` from main, the same place the script
comes from. When that holds checksums, they install the build it names (not
`LATEST`'s) and check it against them: the build the plugins run, checked the
way they check it. If `release-key` can't be read or holds no checksums,
nothing is installed. `ENDEAVOR_PIN_URL=none` skips the pin; then `LATEST` is
read and the checksum file comes from the same release as the binary, so the
check catches a damaged download and not a tampered release. With `--key`,
main's checksums are used when main pins that build; for another build it says
so.

- `--sums FILE` gives the checksums to check against, in sha256sum's form
  (other lines are ignored). The launcher passes its `release-key`, where the
  Helpers pin job writes each platform's SHA-256 under the key. Then the check
  doesn't depend on the release, and catches a release changed after the pin
  as well as a damaged download. A file with no checksums in it (a pin from
  before they were written) counts as none. A Windows checkout can give
  `release-key` CRLF line ends.
- `ENDEAVOR_RELEASE_URL` replaces the release's address (for tests and
  development; `file://` and, for `install.ps1`, a folder work), and
  `ENDEAVOR_PIN_URL` the address of main's `release-key` (none or empty: no
  pin). With only the first set, no pin is read.
- A build's key is hexadecimal, since it is used as a folder name.

### Downloads

- A connection that doesn't open in 15 s is given up on, and so is a file that
  takes over 15 minutes (a build is about 5 MB). Without `timeout`, a watchdog
  ends the command and then ends itself and its sleep.
- From https, a redirect is followed only to https. curl refuses any other.
  GNU Wget has no such switch, so it prints the headers (`-S`), and a download
  that went through one is thrown away; a relative redirect keeps the scheme
  of the address it came from. Wget2 prints no headers under `-q`, and is told
  to keep to https instead. PowerShell 7 (.NET's HttpClient) refuses any
  other itself; Windows PowerShell 5.1's `Invoke-WebRequest` has no such
  switch, so there each redirect (up to 10) is followed by hand and one away
  from https is refused. An error status comes as a WebException, inside
  PowerShell's own, holding the response.
- Exit 3 tells the launcher that the release answered but doesn't hold this
  build for this platform: either it is newer than the release (a build from
  main is published a few minutes after the push) or it is old and was removed
  (`prune-helpers.sh` removes the macOS and Windows files days before the
  rest). The server answered "not there" when curl got an HTTP error or a
  `file://` path that can't be read, or wget an error response, rather than
  the network failing.

## The plugin's launcher (`endeavor-mcp.sh`)

The plugin's MCP command in every agent: it runs `endeavor mcp` with the
arguments it is given, getting the binary first if this computer has none for
the plugin's build.

```sh
endeavor-mcp.sh --skills plugin --folder DIR   # run the server
endeavor-mcp.sh --fetch-only                   # only get the binary
endeavor-mcp.sh --hook                         # Claude Code's PreToolUse hook
```

- The hook gives the binary the hook's input, so the server learns Claude
  Code's permission mode. It runs only a binary that is already here, prints
  nothing, and always exits 0: it never fetches, and never stops a tool call.
  A build from before the hook says it doesn't know `hook`, and that is no
  failure either.
- The build is the first line of `release-key`, next to the script. Empty or
  missing, it is the newest build: one that is already here is used without
  asking the network, and `--fetch-only` looks for a newer one. Unpinned, a
  start also asks for a newer one once a day after starting the server, in a
  process of its own that neither holds the server's input and output nor
  delays its start; the result applies from the next start. The lines after
  the key, written by the Helpers pin job, are each platform's SHA-256, and a
  pinned download is checked against them rather than the release's own
  checksum file.
- Binaries live in `${XDG_DATA_HOME:-~/.local/share}/endeavor/bin/<key>/`, the
  same for every agent (`XDG_DATA_HOME` counts only if it is an absolute
  path). `install.sh --quiet --into BASE` fetches them: the binary goes to
  `BASE/<key>/endeavor` only if it isn't there already, and with no `--key`
  `BASE/.newest` is set to the key `LATEST` named, the build the plugin runs
  when it isn't pinned. `--quiet` writes everything to stderr.
- stdout belongs to the MCP server: everything goes to stderr until the exec,
  which gets stdout back.
- A start runs only a binary in its own folder (or `ENDEAVOR_BIN`), so a copy
  installed elsewhere wouldn't help, and the advice is to fetch into that
  folder. `ENDEAVOR_BIN` names a binary to run instead, to try the plugin
  before a release holds its build ([testing.md](testing.md)).
  `ENDEAVOR_RELEASE_URL` (tests and development) fetches from another release
  into `bin-from/<its address>/` beside `bin/`, so that it can't put a binary
  where a normal start would run it.
- Two starts download once, under a lock: a folder made when the lock is
  taken, so its age is the lock's, with the owner's pid in it. A lock is stale
  when it has no pid two minutes after it was made, or its owner is gone, or
  it is 20 minutes old. One start at a time (`.lock.take`) removes a stale
  lock, and looks at it again first, since by then it may be another start's,
  just taken, with no pid in it yet; that one is never moved or removed. A
  `.lock.take` left by a start killed while removing a lock goes after a
  minute. Only a lock this start made is removed. A download cut short leaves
  its temporary folder behind, which goes after an hour.
- Each agent's copy of the plugin has its own manifest beside `launch/`.

## The plugins' copies (`plugins.sh`)

Keeps the plugin folders in step with their one source each: `plugin/skills/`
(the skills), `scripts/endeavor-mcp.sh` and `install.sh` (the launcher and its
installer), and `scripts/release-key` (the pinned build). `sync` writes the
copies; `check` fails if any copy differs. Each plugin holds the launcher files
in `launch/` and a copy of the skills, not a link: an agent may not follow a
link out of the plugin's folder, and a checkout without symlink support turns
a link into a text file.

`release-key` names the newest build the Helpers workflow published from main,
and only that workflow's pin job writes it, after every platform built, so a
PR doesn't touch it and PRs don't have to be re-pinned each time main moves.

## Helpers for servers (`helpers.sh`, `build-helpers.sh`)

`helpers.sh` puts the runtime helpers for Linux servers (x86_64 and aarch64)
in `target/helpers/<platform>/`, or in `$HELPERS_OUT`, built from this
checkout's source. Endeavor's `scripts/helpers.sh` runs it in a checkout of
the commit Endeavor's `Cargo.lock` pins, with `HELPERS_OUT` set to its own
`target/helpers`.

```sh
scripts/helpers.sh               # keep what's there if it matches, else download it, else build it
scripts/helpers.sh --fetch-only  # keep or download, never build (Endeavor's git hooks run this)
scripts/helpers.sh --key         # print the helper source key
```

- Downloads come from the `helpers` release, which the Helpers workflow fills
  for every change to the helper's source, with plain curl and no sign-in. A
  checkout with uncommitted helper changes, or one GitHub hasn't built yet, is
  built here instead: with cargo-zigbuild if it's installed, else on the Linux
  machine named by `ENDEAVOR_HELPER_HOST` (`build-helpers.sh --via`, its
  architecture only).
- The key hashes the committed helper source, its dependencies (the crates
  the helper is built from, as "name version" lines read from `Cargo.lock`)
  and the version it reports. The repository's owner and name come from
  origin's URL.

`build-helpers.sh` builds the helper into `target/helpers/<os>-<arch>/`, or
into `$HELPERS_OUT` (an absolute path):

```sh
scripts/build-helpers.sh             # cross-build x86_64 and aarch64 (musl) with cargo-zigbuild
scripts/build-helpers.sh --via HOST  # build on HOST, a Linux machine reachable by ssh, for its own architecture
```

Neither way installs anything on this computer. `--via` sends the workspace
and the folders the binary carries (`plugin/` and `runtime/`) on stdin, so
nothing on HOST may read stdin, and installs rustup (and a C compiler, with
sudo) on HOST if they're missing.

## Publishing and pruning (`publish-helpers.sh`, `prune-helpers.sh`)

`publish-helpers.sh` is the Helpers workflow's Publish step, kept as a script
so a test can run it with a fake `gh` (`tests/publish_helpers.rs`). It runs
from the checkout's top folder, with the builds in `dist/<platform>/`
(`endeavor`, or `endeavor.exe` on Windows), `KEY` set to the release key, and
`gh` signed in with contents: write. It uploads `endeavor-<key>-<platform>` and
`endeavor-<key>.sha256`, and on main (`GITHUB_REF`) `LATEST` naming the key. A
manual run on another branch builds that branch's helpers but doesn't make
them the newest. The Linux builds must be there; macOS and Windows are
published when they are. With `GITHUB_OUTPUT` set, the checksum file is also
the step's output `sums`, for the pin job.

A binary already published under the key is kept, binary and checksum line:
the plugins pin each build's checksums, so a binary under a key must never
change, and builds aren't byte-for-byte reproducible. A build this run made
for a platform the release lacks (an earlier run's macOS or Windows build
failed) is added.

`prune-helpers.sh` lists the release's files for builds nothing needs any
more, and with `--delete` removes them. GitHub allows 1000 files on one
release, and every build adds six, so without it the Helpers workflow would
start failing.

```sh
scripts/prune-helpers.sh                     # list what would go (changes nothing)
scripts/prune-helpers.sh --delete            # delete it
scripts/prune-helpers.sh --delete --above N  # delete only if the release holds more than N files
```

A build (its key, as in `endeavor-<key>-<platform>`) is kept when:

- `LATEST` names it (`install.sh`, `install.ps1`, `endeavor update`);
- a `release-key` file names it at the head of any branch here, or it is one
  of the last `PLUGIN_PINS` (40) builds main's `release-key` has named: a
  plugin installed from such a commit downloads that build, and
  `endeavor mcp` fetches its Linux helper for a server. A key Helpers never
  built (pinned on a branch, then merged) uses up none of the count;
- Endeavor's `Cargo.lock` pins the commit it was built from, at the head of
  any of Endeavor's branches or as one of the last `APP_PINS` (10) pins on its
  main (the app's `scripts/helpers.sh` downloads it to bundle). Each is keyed
  by that commit's own `helpers.sh`;
- it was published in the last `NEW_DAYS` days (3).

Of the other builds, the newest `LINUX_BUILDS` (150) keep their Linux files
and checksum file, and lose only the macOS and Windows ones. Every installed
binary, from a plugin, `install.sh` or `endeavor update`, fetches the Linux
helper of its own build when it sets up a server, so this is what keeps a
binary nobody has updated working; the Linux files are half a build.

Pins and builds are counted rather than dated so that what is kept stays
bounded however often main moves: at most about 240 files for the pins and
450 for the Linux files. At 2026-10-09's pace (19 builds, about 15 pins) the
40 pins cover two or three days, so a first download of an older plugin's
macOS or Windows binary rests on the 3 days, and 150 builds of Linux files
keep an installed binary setting up servers for about 8 days. When the kept
builds' files alone pass 800, it warns: pruning can't help then.

`LATEST` itself and any file whose name holds no key (the old
`endeavor-remote-<key>` names count as keys) are never deleted. If any of this
can't be worked out (a clone, a pinned commit, the release), the script stops
before deleting anything. It needs git, curl and jq, and clones both
repositories itself (no file contents beyond what it reads), so it runs the
same in CI and in any folder. Deleting needs `GH_TOKEN` with contents: write;
listing works without one, but a token avoids the API's rate limit.
`ENDEAVOR_MCP_REPO` and `ENDEAVOR_APP_REPO` name other repositories
(owner/name).

## The rest

- `smoke.sh` runs the agent smoke suite ([smoke.md](smoke.md)): it builds the
  binary and the runner, then runs the tasks in `smoke/tasks` with Claude
  Code. Its arguments go to the runner (`--only`, `--out`, `--julia`, `--r`,
  `--skip-r`, `--depot`, `--retries`, `--model`, `--long`).
- `smoke-timeline.py ATTEMPT_DIR [--top N] [--csv OUT]` (needs numpy) shows
  how a smoke attempt's context filled, request by request, and what filled
  it. `ATTEMPT_DIR` is a run's `<task>/attempt-<n>` folder. From Claude's
  transcript it prints, for each user message, the requests it took and the
  context at its first and last; then what is in the context at the end, by
  source; then the largest single items.
  - The context of each request comes from Claude's own usage. What filled it
    is estimated: the growth between two requests is the agent's last message
    (the code and text it wrote, and its thinking, which the transcript holds
    only as a signature) plus what came back (tool replies, pictures). A
    least-squares fit over all requests gives tokens per character for each
    kind, and per picture. Text counts by its length, anything else but a
    picture (ToolSearch's tool references) by its JSON's. A message's own
    usage can't be used for this: its output count stops at its first part.
  - When Claude Code compacted the conversation, the earlier part is gone from
    the context: the fit leaves out the step across each compaction, the table
    of the end context counts only what came after the last one, and an item
    before it counts as re-read only until it. A compaction Claude Code didn't
    report still shows as the context shrinking by a lot.
- `view-frontend.sh`: the notebook view's look and Point
  (`crates/endeavor-mcp/src/views/endeavor-view.js`) is a copy of the Endeavor
  app's `frontend/dist/view.js`, which the app's repo builds from its
  `frontend/src/view.ts`. The app's repo is its source: change it there, then
  copy it here. `sh scripts/view-frontend.sh check` fails if the copy differs
  from the app's main; `sync [file]` copies the app's main (or a local build)
  here.
- `cloud-setup.sh` is a copy of Endeavor's, which its `docs/cloud.md`
  describes; change both. `.claude/hooks/session-start.sh` runs it in a cloud
  session with only this repository; a session with several repositories (a
  Claude project) doesn't run repo hooks, and there the environment's setup
  script runs it.
