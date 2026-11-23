# Builds, releases and folders

`build.rs`, `release.rs`, `old_builds.rs` and `paths.rs`: what a build
carries, how a helper is fetched and checked, how old builds are removed,
and where Endeavor keeps its files.

## What a build carries (`build.rs`)

- `runtime/` (the Julia side) and `plugin/` (the skills as a Claude Code
  plugin) are embedded in the binary, so `serve` and `mcp` work from the one
  file, and the app takes both from this crate. Each one's version names the
  folder it is unpacked to: the package version and a hash of its files
  (`wire::tree`).
- `BUILD_VERSION` names the whole build the same way, hashing the Rust source
  (this crate's and `wire`'s) with both folders, since the package version
  alone doesn't change between builds.
- When `scripts/release-key` pins this build with checksums, the binary
  carries them. The Helpers pin job writes them on `main` after the build, so
  a build of the pin commit (the one the app's Cargo.lock usually pins)
  carries the sums of the release's binaries for its own key.

## Fetching from the release (`release.rs`)

- The Helpers release is on GitHub. `ENDEAVOR_RELEASE_URL` replaces it (tests
  serve one locally, and `file://` works too). `ENDEAVOR_PIN_URL` replaces
  main's release-key (`none` or empty: none). With only
  `ENDEAVOR_RELEASE_URL` set, no pin is read, since main doesn't pin that
  release's builds.
- A download is checked against checksums pinned outside the release when
  there are some for its build: the ones this binary carries, or the ones in
  main's release-key. Those catch a release changed after the pin. Otherwise
  the checksum file comes from the same release, which catches a corrupt or
  cut-short download but not a release that was tampered with. A release-key
  from before the pin job wrote checksums holds the key alone and doesn't
  count as a pin.
- Downloads use curl, or wget where there is no curl. A release file is
  about 5 MB, which a slow line needs minutes for. From https, a redirect is
  followed only to https: curl refuses any other (`--proto-redir =https`);
  GNU Wget has no such switch, so it prints its headers (`-S`) and a download
  that went through one is thrown away; Wget2 prints no headers under `-q`,
  so it is told `--https-enforce=hard` instead. A relative redirect keeps the
  scheme it came from.
- **A server's helper** for another platform than this computer's is fetched
  for the app, which doesn't ship every platform's, and kept in the helpers
  cache as `<key>/<platform>/endeavor` with its checksum beside it. A kept
  one whose checksum still matches is used again without the network. One
  process at a time checks or fetches (a lock file there, waited for up to
  20 minutes), a kept file is only replaced by renaming a checked download
  over it, and other keys' folders are removed after.
- **A build the release no longer has.** A release with no checksum file for
  a key, but whose `LATEST` names another build, removed this one
  (`prune-helpers.sh`), since an installed binary's build was on the release
  once. The error then says so.

## Updating (`update.rs`)

`endeavor update` replaces this binary with the build main's
`scripts/release-key` pins, checked against the checksums there: the build
the plugins run. When main can't be read, or its pin has no checksums,
nothing changes, unless `ENDEAVOR_PIN_URL=none` says to take the newest build
the Helpers release's `LATEST` names, checked against the release's own
checksum file. A release tampered with so that `LATEST` and its checksum file
agree still loses to main's pin.

- `endeavor --version` prints the package version and build, `release <key>`
  on a release build, and `interface <n>` (`core::INTERFACE`), which `update`
  reads from the new binary: a runtime that offers it is used as it is.
- A copy the app installed, or one cargo installed, is left to the app, to
  cargo or to a plugin. The app installs its helper as
  `~/.cache/endeavor/<version>/endeavor` and reinstalls only when that folder
  is missing, so a replaced file there would run under the wrong version's
  name. Run as the app's helper (`--helper update`), the binary is the app
  itself, and replacing it would swap the app for this build (Endeavor #200).
- The new binary goes in by a rename within its folder, which is atomic, and
  a process running the old one (a runtime's core) keeps the file it started
  from. Windows can rename a running exe but not replace it, so the old one
  is moved aside under a name of its own first, and put back if the new one
  can't be moved in. Asides no process runs from any more are removed later.

## Removing old builds (`old_builds.rs`, #97)

Two folders keep one build each: a server's `~/.cache/endeavor/<build>/`
(or the app's install folder there), which the bootstrap script installs
into, and the plugin's `bin/<key>/`, which its launcher downloads into.

- A process run from one holds a `standalone::Lease` on it for as long as it
  runs, which also marks the folder as used every hour. If another build
  removed the folder as this one started, it says so and exits; the next
  start installs it again. A filesystem without locks (NFS mounted nolock,
  Lustre without flock) fails the lock at once, and then no lease is held and
  no build is removed there.
- `connect`, `mcp` and `serve` look at the other builds in the background
  (`tidy`), and remove one only when all of these hold:
  - no one holds a lease on it, and it hasn't been used for a week
    (`KEEP_UNUSED`; a build from before leases counts from when it was
    installed);
  - no process here runs from it or names a file in it (Julia's `--project`);
  - on a server, no runtime record names it (a runtime on another login node,
    or in a job, runs from the build that started it). A record from this
    computer whose process is gone doesn't count, and one from before builds
    were recorded keeps them all;
  - on a server, no Slurm job of the user's that waits or runs starts from it
    (its job script names the build's binary). A server where `squeue`
    doesn't answer is left alone. The script read is `job.sh` as it is now,
    while Slurm runs the copy it took; they agree because a state folder has
    one job at a time.
- The plugin keeps the newest build its launcher fetched (`.newest`), which
  an unpinned start runs, whatever its use.
- The week covers what this computer can't see: a cluster's other login
  nodes, whose processes and locks don't show here, keep marking a folder they
  use. A mark is the marking node's time, so this relies on the nodes' clocks
  being within days of each other (a mark in the future counts as now).
- A folder is moved aside before it is removed, so a removal cut short leaves
  `<name>.removing.<pid>`, which the next look removes. A half-written install
  (`<name>.part.<pid>`) is kept a day, since its install may still be going
  on. On Unix the lock is held until the folder has moved, so a process
  starting from it meanwhile isn't told it may. Windows won't move a folder
  with a file open in it, so there a running binary keeps it instead, and no
  process list is needed.

## Folders (`paths.rs`)

Every folder Endeavor uses is defined in `paths.rs` and nowhere else. The app
links this crate and takes its folders from here, so the app, the plugin and
a server share one state folder.

- On Unix a variable (`XDG_*`, `HOME`) counts only if it holds an absolute
  path: a relative one would put Endeavor's files under whichever folder the
  process started in. On Windows the folders are under
  `%LOCALAPPDATA%\Endeavor`, and the machines file under `%APPDATA%`.
- The state folder is per machine, since a home folder is often shared by a
  cluster's nodes. A cluster has one for the whole cluster
  (`cluster_state_dir`), since a reconnect through another login node must
  find the same job.
- What the app and the bootstrap script install into is `~/.cache/endeavor`,
  ignoring `XDG_CACHE_HOME`, since the app installs to the same folder. The
  app's server installs use one depot, so packages installed for one serve
  the other; a cluster's is on its scratch folder, where home quotas are
  small. Ember is installed beside the depot, one library per build.
- **`JULIA_DEPOT_PATH`.** A trailing empty entry (`<ours>:`) gives Julia only
  its bundled depots, not the user's own, so their registries (a
  LocalRegistry, say), packages and compiled code went unseen (#367). So where
  the depot ends with one, the user's depot goes in before it: the first entry
  of their own `JULIA_DEPOT_PATH` that isn't already there, else `~/.julia`.
  Julia still installs only into the first entry, ours. A depot without a
  trailing empty entry is a list someone chose, and is kept.
- **Two scripts repeat a rule** because they run before the binary exists:
  the bootstrap script (`PICK_STATE_DIR_SH`, the state folder) and
  `scripts/endeavor-mcp.sh` (the binary store). Tests here and in
  `tests/launcher.rs` run the shell text and compare it with this module. The
  bootstrap's state folder is, like `runtime::default_state_dir`, the app's
  older folder while a runtime of this node recorded there is alive: while
  `kill -0` reaches its pid and `ps` shows it was started with
  `--state-dir` that folder, or can't say. A reused pid or a zombie shows
  other arguments, so it doesn't count. `uname -n` is what `gethostname`
  returns. Some `ps` cut `args` short without `-ww`, and some refuse `-ww`
  (#430).
