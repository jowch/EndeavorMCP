# CI and releases

The workflows in `.github/workflows/`. No check is required to merge.

## CI (`ci.yml`)

Builds the workspace and runs its tests on Linux, macOS and Windows. The
real-Julia tests (e2e_julia, e2e_serve) are ignored by default and run in
`e2e.yml` instead ([testing.md](testing.md)).

- It runs on main and on pull requests only. A push to any branch used to run
  it too, so every push to a PR's branch ran the whole matrix twice (once for
  the push, once for the PR, in different concurrency groups), four macOS jobs
  each time.
- A PR that changes only `docs/` runs nothing: no job reads `docs/`. If a check
  is ever required, a docs-only PR would wait on it forever, so drop
  `paths-ignore` then.
- Each job has a time limit well above a normal run (a few minutes), so a test
  that hangs fails the job instead of holding it, and the PR's checks, for
  GitHub's default of six hours.
- Checkouts keep LF line endings: tests compare checked-in text byte for byte.
- A PR may not change `scripts/release-key`: only the Helpers workflow's pin
  job changes the pinned build, on main, and a PR that changed it could put a
  build that was never published on main. The check compares with the merge's
  first parent, since the checkout is the PR merged into its base's current
  tip, and the event's `base.sha` can be older.
- Clippy checks one lint outside tests: on Windows a socket's `try_clone` is
  an inheritable copy, and a child started while it is open keeps the
  connection open (#154). `clippy.toml` forbids it; tests may copy sockets.
- The notebook view's look and Point are built in Endeavor's repo
  (`frontend/src/view.ts`), and the check that this repo's copy matches runs
  last, so a copy that waits on an Endeavor PR doesn't hide the build and
  tests.
- The runtime package's own tests (`runtime/EndeavorRuntime/test`) run on
  Linux and macOS only, since the runtime runs only on Linux and macOS
  servers, with the Julia Endeavor downloads (`JULIA_VERSION` in
  `crates/endeavor-mcp/src/julia.rs`).

## Real Julia, R and Python (`e2e.yml`)

The tests against real Julia (e2e_julia, e2e_serve), real Julia and R (e2e_r)
and real marimo (e2e_marimo), which `ci.yml` skips because they are ignored by
default. They take a few minutes once the packages are cached and much longer
the first time, so CI's checks don't wait for them. Linux only for now.

- They run on a PR or push that changes what goes into the binary (the
  crates, the runtime, the skills it embeds, the Cargo files), and on request.
  Because of those path filters, don't make this check required: a PR that
  changes none of them would wait for it forever. A run on main isn't
  cancelled, so each merge gets a result.
- The R job also runs every day: R notebooks install Ember's latest build from
  r-universe, so a push to Ember's main reaches users at their next R start,
  and a change there that breaks the adapter should show up here first.
- The Julia tests also run every day on the newest Julia (`julia-newest`):
  the app runs the user's own Julia when they have one, which juliaup moves to
  each new release, so a release that breaks the runtime should show up here
  before users hit it. `'1'` is the newest release (juliaup's `release`),
  `'pre'` the newest pre-release. These start with fresh depots, since a new
  Julia compiles everything anyway.
- Each test installs the runtime's packages into a depot of its own under
  `target/tmp`. The depots are cached between runs, saved even when a test
  fails, so the next run doesn't install everything again. A run whose tests
  passed marks its depots complete; one restored without the mark came from a
  failed run and may lack packages. A run saves only when the restored depots
  weren't complete: a new entry each time would fill the cache, and complete
  depots have nothing to add.
- A test that finds no Julia prints SKIPPED and passes. Here that would hide a
  broken setup, so the step fails on it.
- R and most of Ember's packages come from Ubuntu's archive. The first R
  notebook builds what's missing (RcppMsgPack) and Ember itself from
  r-universe's source package, as it would for a user. A stalled package
  mirror once hung `apt-get update` until the job's 45-minute limit, with no
  test run (#235); the step's limit and apt's network timeouts make it fail
  in minutes instead, and say so.
- The R job caches the Julia depot (e2e_r starts no Julia, so it makes the
  depot itself) and the folder Ember installs into (`ember_folder` in
  paths.rs). A build's library is only there when its install finished, so a
  restored one is whole. Every run installs Ember's latest build afresh
  (keeping the packages it needs), so a build that doesn't install or load
  fails the test instead of falling back to the cached one; it takes seconds.
  A cached build was only saved by a run that found its depot incomplete, so
  it could be weeks old: then R started with it while the update ran, and the
  notebook view's step failed on an Ember from before `view_page()`. Only a
  passing run saves, so the Ember library saved is one that worked.
- After e2e_r, `runtime/r/test/test.R` runs R's adapter on its own, every
  method over HTTP, against the Ember build e2e_r just installed. It ends
  non-zero when a check fails, after printing the adapter's log.
- Python notebooks need only uv: it fetches Python, marimo
  (`runtime/marimo/requirements.txt`) and each notebook's packages.

## Helpers (`helpers.yml`)

Builds `endeavor` for Linux, macOS and Windows (the helper Endeavor installs on
servers, and the standalone binary) and publishes it to the `helpers` release
as `endeavor-<key>-<platform>` (`.exe` on Windows), with
`endeavor-<key>.sha256`. `<key>` is `scripts/helpers.sh --key`, a hash of the
helper's source. Every build gets the key in `ENDEAVOR_RELEASE_KEY`, so
`endeavor --version` can show it and `endeavor mcp` can fetch the Linux helper
for a Mac or Windows computer. `scripts/helpers.sh` downloads the Linux ones,
and `scripts/install.sh` and `install.ps1` the newest build for a computer.
`LATEST`, uploaded after them, holds the newest key, for those scripts and for
`endeavor update`.

- One run at a time, in push order, so `LATEST` ends up naming the newest
  push. The group doesn't cancel, so each leg has a time limit: a hung leg
  would otherwise hold every later release for six hours.
- Checkouts keep LF line endings: `build.rs` embeds `runtime/` and `plugin/`
  as checked in, and the key covers LF.
- The macOS x86_64 build is built on Apple Silicon, which can't be counted on
  to run it, so it isn't run. Every build must carry the key its file is
  named by.
- Publish (`scripts/publish-helpers.sh`) publishes the legs that built, as
  long as both Linux ones did (the app's servers need them), and ends red if
  any leg failed.
- GitHub allows 1000 files on a release and refuses uploads to a full one.
  Before publishing, once the release passes 700 files, builds nothing needs
  go (`scripts/prune-helpers.sh`), if the repository variable
  `PRUNE_HELPERS_AUTO` is true (set it after a deleting run by hand has been
  checked). A failure there doesn't hold the build back; it leaves a warning
  on the run. The publish job's limit is long enough for that pruning run (two
  blobless clones) too.

### The pin job

On main, once every platform has built, the pin job points the plugins at this
build: it commits the key to `scripts/release-key` and the plugins' `launch/`
copies, with each platform's SHA-256 as published under it on the lines
after, so the plugins check their download against main rather than against
the release (#98). PRs never re-pin.

- Only when every platform built, because the plugins run on macOS and Windows
  too; otherwise the plugins keep naming the last complete build. Every
  platform the plugins run on must have its checksum.
- The commit touches no helper source, so it doesn't start this workflow
  again, and a push made with the workflow's own token doesn't start CI.
- It pins only a build of newer source than the one pinned now (the commit
  its "Built from" line names, or the commit that set it by hand), so a re-run
  of an old run can't put an older build back. Merges that land while it
  builds don't stop it, since their own runs pin after it. A merge landing
  between the fetch and the push makes the push fail; then it starts again.

## Pruning by hand (`prune-helpers.yml`)

Runs `scripts/prune-helpers.sh` by hand: it lists the Helpers release's files
for builds nothing pins any more and, with "delete" ticked, removes them. Read
the list from a run without "delete" first; a deleted file can't be brought
back.

It has its own concurrency group: in Helpers' group, a run queued here would
cancel a Helpers run that is waiting (GitHub keeps one waiting run per group),
and that build would never be published. Pruning needs no lock: it deletes
only files from its own listing, which a build published meanwhile isn't in.

## The feedback form

`.github/ISSUE_TEMPLATE/feedback.yml` is a copy of Endeavor's: the same
questions and details as the form on Endeavor's feedback page. The fields are
listed in Endeavor's `site/feedback-fields.md`; change them there and copy the
file.
