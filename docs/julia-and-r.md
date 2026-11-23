# Finding Julia and R

`julia.rs`, `juliaup.rs` and `r.rs`: which Julia starts the runtime and which
R runs R notebooks, and Endeavor's own copies of each. The app's Settings use
the same functions (`own_installed`, `install_own`, `remove_own`), without a
runtime, so the app and a server pick the same way.

## Julia

The runtime starts with a path the user gave (`--julia PATH`), the one a
shell line of theirs sets up (`--julia-shell`, such as `module load julia`),
the login shell's (`--julia auto`; on Windows the PATH's, since Windows has
no login shell), or else Endeavor's own (`--julia own`). A julia called
`own` is `./own`.

- **The oldest Julia** is `MIN_JULIA`: the runtime's Manifest.toml is
  resolved on 1.12, and 1.11's Pkg can't instantiate it (#370). `auto` passes
  over an older one for Endeavor's own. The app reads it from here
  (`min_version`).
- **Endeavor's own** is the pinned version, `JULIA_VERSION`, with the
  official tarballs' SHA-256 and size. The app reads it from here; bump it
  with the cloud script (`scripts/cloud-setup.sh` and the app's copy, which a
  test checks) and e2e.yml's `version`.
- **The login shell** is asked for julia's path, after the line when there is
  one. The last absolute path it prints counts, since profiles can print other
  things first. `command -v` fails when it finds nothing, so a shell that ended
  well without a path never ran the lookup: a `#` in the line commented it
  out, or `julia` is a shell function or alias. csh and tcsh take `-l` only as
  their one flag, so they run a plain `-c`, which still reads ~/.cshrc or
  ~/.tcshrc, where module setup goes.
- **juliaup's launcher.** juliaup's `julia` (its launcher, or on Windows the
  Store app's alias) only starts the real one, from the channel juliaup picks.
  Started through it, the version checked and the Julia run can differ, and
  on Windows the launcher and the Julia under it ran outside the core's job,
  so ending the core left Julia running (#55). So the core starts the `julia`
  in that Julia's own `Sys.BINDIR`. A wrapper script someone put on the PATH
  is left alone. On Windows a julia counts as the real one when libjulia.dll
  is beside it.
- **A juliaup with no channel yet.** Its launcher's first run would install
  juliaup's latest Julia and make it the default, which the person never
  chose and Endeavor would then count as theirs (#512). So juliaup is asked
  first, which installs nothing, and Endeavor's channel, added next, becomes
  its default. With a channel, the launcher still picks, so `JULIAUP_CHANNEL`
  and `juliaup override` hold.

### Endeavor's own Julia

- **With juliaup**, Endeavor adds the pinned version as a channel
  (`juliaup add 1.12.6`) rather than download a second Julia, so the person
  keeps one Julia manager, and `juliaup update` can't move notebooks off the
  tested version. Their default and other channels stay as they were, except
  on a juliaup with no channels yet, where the first channel added becomes the
  default. juliaup is asked through `juliaup` itself, never its launcher.
- Endeavor marks the channel as its own only if juliaup said it had none.
  When juliaup can't say, the channel may be the person's already. If the
  channel later goes some other way (`juliaup remove`), the mark is deleted,
  so a channel added after is the person's and Remove leaves it (#118).
- **On Windows** juliaup is the only way, and Endeavor installs it for this
  account first when there is none, with no admin: from the Microsoft Store
  with winget, else from its App Installer file for a computer whose Store is
  blocked. Each route has a time limit, since a Store install can sit queued
  behind other updates for good, and is tried once more after a failure that
  wasn't the time limit (once in sixteen CI runs on Windows Server 2022, cause
  unknown).
- **`juliaup add`** may take a while (about 300 MB on a slow connection). A
  failure that wasn't the time limit is tried once more: on Windows a first
  install failed once with "Access is denied", and a retry after clearing
  juliaup's folder worked (#132). The folder is the person's, so it is never
  cleared here. A failure like that can still pass later, so a failed add is
  tried again on the next install.
- **Elsewhere without juliaup**, Endeavor downloads the tarball into
  `~/.cache/endeavor/julia-<version>`, checks its size and SHA-256, and
  unpacks it beside the target, then renames it, so a half-unpacked Julia is
  never used. A download resumes what an earlier try left, and fails when it
  stalls (under 1 kB/s for 5 minutes). Where juliaup is there but fails (a
  locked config, an old juliaup, a mirror that's down), the download is used,
  which costs a second Julia rather than none.
- **Commands** run without a window or input, with both pipes read while they
  run, and with the core's signal mask cleared, so a Ctrl-C stops a download
  or `juliaup add` with the core (#181). juliaup's errors end in a chain
  ("Error: …", "Caused by:", then numbered causes); the last line alone
  ("Access is denied. (os error 5)") doesn't say which step failed, so the
  step that failed is shown first.
- **Remove** deletes the download, and juliaup's channel if Endeavor added it.
  It refuses while a process here runs a program from that Julia: a runtime
  the app doesn't know about (`serve`, the plugin) may run it, and its
  notebooks break once it is gone (#117). The app stops its own runtime first,
  so its Julia gets a few seconds to end. juliaup keeps one folder per Julia
  version for all its channels, so when another channel is the same Julia the
  files stay, and a REPL on that channel doesn't hold Remove up. juliaup won't
  remove its default channel, so Remove says what to do instead.

## R

R notebooks run an Rscript the user gave (`--r PATH`, an Rscript or the R
beside one), the one a shell line sets up (`--r-shell`, such as
`module load R`), or with `--r auto` Endeavor's own R when it's installed,
else the login shell's (on Windows the PATH's).

- **Not found once.** Unlike Julia, R isn't looked for when the runtime
  starts: the core starts it the first time an R notebook opens. And it isn't
  found once and then run by its path: the shell line and the login shell run
  each time R starts, so what they set up besides the PATH (a module's
  libraries, the compiler Ember's install builds with) is there too. Someone
  who installs R after Endeavor's gets theirs from the runtime's next start.
- **Through the user's shell**, the arguments are passed in the environment,
  which every shell reads the same way (sh, bash, zsh, fish, csh and tcsh),
  not as the script's own arguments, which they don't. A shell that found no
  Rscript ends with 127; one that ended well without starting R had its line
  end in a comment (`module load R  # 4.4`), which comments out the rest.
  csh and tcsh run a plain `-c` as for Julia; their `exit` doesn't end a `-c`
  early and their `exec` of a missing program ends with 1, so sh starts R
  after the line.
- **Endeavor's own R** is installed only on a Mac, only when no R is found,
  and only after the user agrees. CRAN's installer, pinned with the oldest
  macOS it runs on, its SHA-256 and size, is unpacked into
  `~/.cache/endeavor/R-<version>` rather than run, as rig's user mode does:
  no admin rights, nothing on the PATH, no shell startup file edited. On Linux
  and on servers, people install R themselves (rig, the system's packages, a
  cluster's module).
  - Its R is made to run from there, not `/Library/Frameworks`: R's own
    script and its libraries name the framework folder, and each Rscript
    becomes a script that sets RHOME. Older Rs carry libraries that shadow the
    system's and crash once the libraries come from DYLD_LIBRARY_PATH, so
    they are removed. Packages built from source link to its library there.
  - Its packages go in its own library, never the user's, and the user's R
    settings (~/.Renviron, ~/.Rprofile, R_LIBS) don't point it at another R's.
  - Fonts for plots are installed as R's installer does; R still runs if that
    fails.
  - Remove deletes it with its packages and the Ember in it, and what an
    install that didn't finish left. The runtimes that use it are stopped
    first.
