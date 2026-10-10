---
name: endeavor-machines
description: >-
  Use when the user wants notebooks to run on a server or a cluster (a
  machine reached over ssh), asks to add, use, switch or stop a machine, or a
  notebook tool says the machine isn't ready, is queued or was stopped.
---

# Notebooks on a server or cluster

Without these tools the notebooks run on the user's own computer. With them (`list_machines`, `add_machine`, `use_machine`, `stop_machine`; only `endeavor mcp` has them) a session can work on a machine the user reaches over ssh: a plain server, or a Slurm cluster's login node, where Endeavor runs Julia in a job. The notebook tools work the same, on the machine.

## Rules

- **Only when the user asks.** Don't add a machine, switch machines, submit a job or stop Julia unless the user asked. A project remembers the machine and folder its last session used, so a new session may already be on one: `list_machines` says which.
- **Never a password.** Endeavor signs in with the user's own ssh keys and agent. Never ask for a password or passphrase, never type one, and never run `ssh` yourself to get past a failed sign-in. When `add_machine` or `use_machine` fails, tell the user what it says: usually the user runs `ssh-add` once in a terminal, or `ssh <host>` once to accept the server's identity. Servers that ask for a password or a code at every login can't be used yet.
- **Never install without a yes.** See below.
- **Never submit a job with resources the user hasn't agreed to.** See step 3.
- **A call that fails leaves the session where it was.**
- **R notebooks.** Endeavor's runtime is Julia, so a machine needs Julia for R notebooks too. They use the R found there (`Rscript` on the PATH of a login shell), unless the user set another R for that machine. If none is found, opening one fails with `r_not_found`: tell the user. Not on Windows machines.

## Installing needs the user

Looking at a machine installs nothing. When a tool returns `needs_install`, nothing was changed. `install.items` lists what would be installed, each with a `kind`, a `name` (with its version), `size_mb` and `place` (the folder); `message` says the same in words. Tell the user what is named, where and about how large, and ask.

- `kind` `helper`: Endeavor's program on the machine, missing or older than this plugin's (`install.update`). `install.running` says whether Julia already runs there.
- `kind` `runtime`: Julia, which Endeavor would download because none was found.
- On `use_machine`, one yes covers everything that call needs, including Julia if none is found once Endeavor's program is there. A yes given to `add_machine` covers Endeavor's program only.

Only after the user agrees to what the result names, call the same tool again with the same arguments and `install: true`. It covers that call only: ask again each time. Never set `install` on a first call, and never to get past an error. The user may instead tell you where Julia is: `add_machine` with `julia` set to its path, or to a shell line such as `module load julia`. A notebook tool called on a remembered machine never installs; it says what is missing. Outside Slurm jobs, Julia is looked for only when the first Julia notebook is opened or made, so that call can return `julia_not_found` instead of `use_machine` asking: ask the user the same way, and only if they agree call `use_machine` again with `install: true`, then open the notebook again.

## The first time

1. `list_machines`: the machines already added, and the `Host` names in the user's `~/.ssh/config` that are not added yet. Use it to find a machine; don't read `~/.ssh` yourself. If the user named a machine that is listed, skip to step 3.
2. `add_machine(host)`: connects and reports the node, home folder, and for Slurm the partitions and their limits. It waits up to 45 seconds; if it says it is still connecting, call it again with the same host (nothing is saved until a call has connected). Use the host the user named; if you use another name from `list_machines`, say which. Never say a machine was reached unless a tool result says so. If it returns `needs_install`, see above.
   Whether Julia runs in Slurm jobs or directly on the machine is the user's choice. If the user said, pass `slurm: true` (jobs) or `slurm: false` (directly). If not, leave `slurm` out: the result says whether the machine has Slurm (`slurm`) and which way it was saved (`cluster`, `runs_in`). If it says Slurm jobs, tell the user and check they agree before step 3: a workstation can have Slurm's tools without being a cluster. Pass `slurm: false` only for the user's own workstation, or once the user confirms the machine isn't a shared cluster: on a cluster it runs Julia on the login node, which other people share.
3. `use_machine(machine)`: puts this session on it and starts or attaches to the Julia there. On a plain server that is all. On a cluster with no job running it submits nothing and returns `needs_job` with the saved default resources: propose the returned `defaults` to the user, with its numbers (if `defaults` held 8 CPUs, 32 GB and 8 hours on partition `shared`: "8 CPUs, 32 GB, 8 hours on `shared`?"), and once they agree call `use_machine` again with those values.
4. On a cluster, tell the user the queue state and when the job ends. Then work as in **endeavor-notebooks**: the session has no notebook on a machine it hasn't worked on, so create or open one there; it usually opens in the user's browser (`opened_in_browser` says), and give them its `browser_url` too. The result's `folder` is where new notebooks go and where a notebook tool's relative path starts, and by default that is the machine's home folder: before making a notebook, if the user hasn't said where it should go, ask, or pass `folder` to `use_machine`. Pass only a folder the user named or agreed to, with the machine's path, not one from the user's computer. A new folder is made when the folder it goes in exists; tell the user when the result says a folder was made. When `use_machine` says the folders above it are missing too, the path is likely mistyped or from another computer: ask the user, and don't guess another. On one it worked on before it is still in its notebook there if that is open (`list_notebooks` shows `this_session`).

## Waiting

No call waits longer than 45 seconds. If `use_machine` returns `starting` or `queued`, tell the user. To wait, call the notebook tool you want again: each call waits up to 45 seconds for Julia, and a first start takes a few minutes. A queued Slurm job can wait minutes or hours: after a few tries, stop and let the user say when to check again. `session_status` answers at once and only shows the step: the state, the queue's state and reason, the job, and when the job ends. Don't call it repeatedly, and don't use `run_shell` to wait.

## Files are on the machine

On a machine, the notebook and the files are there, not on the user's computer. Use `list_folder`, `read_file` and `run_shell` for them, with the machine's paths: `~` is its home, `list_folder` and `read_file` take a relative path from the home folder, and `run_shell` runs in the session's folder unless given `cwd`. On a cluster `run_shell` runs inside the job, on the job's node, so only once the job has started; on a plain server it runs on the machine itself, which others may share, so ask before a long or heavy command. A project checked out on both computers has the same paths in both places, so your own file tools would read the local copy and show no error. Change notebooks only with the notebook tools.

## After the session

Notebooks keep running on the machine after the session ends, until the idle limit (48 hours by default; `session_status` shows `idle_stop_hours` and `exits_when_idle`). If `session_status` has `other_version`, Julia there was started by another version of Endeavor: some tools may not work as described, and stopping it needs the user's agreement. `browser_url` works only while this session is connected. `list_machines` knows a machine's state only while this session is connected to it.

On a cluster the job keeps its node and the user's allocation until its time limit (`ends_at`) or the idle limit, whichever comes first. When the user is done for now, make sure they know that, and that `stop_machine` ends the job and gives the node back. Stop it only if they ask.

## Stopping

`stop_machine` ends Julia for every client and on a cluster cancels the job, so every notebook there ends, other people's too. Call it only when the user asks, such as to give a node back. Without `force` it refuses when another session was active lately, when Julia is still starting or a job is queued, and when it can't check; tell the user and use `force: true` only if they agree. After a stop the session stays pointed at the machine: notebook calls say it was stopped, and `use_machine` starts it again.

Each tool's arguments and results: [machine-tools.md](reference/machine-tools.md).

## Common mistakes

| Mistake | Fix |
|---------|-----|
| Set `install: true` before the user agreed | Install nothing without a yes. One yes to `use_machine` covers what that start needs, including Julia if none is found; a later call asks again |
| `use_machine` with made-up resources on a cluster | Show the user the defaults it returned and have them confirm or change them |
| `use_machine` on a machine saved for Slurm jobs before the user agreed to jobs | When `add_machine` says Julia runs in Slurm jobs and the user hadn't said, check with them first |
| `slurm: false` on a cluster to skip the queue | That runs Julia on the shared login node. Only for the user's own workstation, or once they confirm it isn't a shared cluster |
| Read a server's files with your own file or shell tools | `read_file` and `run_shell`; your own tools see the user's computer |
| Ask the user for a password or passphrase | Never. Tell the user the failure and what it says to do in a terminal |
| Call `session_status` again and again while Julia starts | It answers at once and does not wait. Call the notebook tool you want again: each call waits up to 45 seconds |
| `stop_machine` to fix a problem in one notebook | Restart or fix that notebook; stopping ends everyone's work on the machine |
| Treat `ready` as a sign the notebook is there | The session starts empty on a machine: `new_notebook` or `open_notebook` there |

## Additional resources

- **Tools and results:** [reference/machine-tools.md](reference/machine-tools.md)
- **Notebooks once the session is on a machine:** [endeavor-notebooks](../endeavor-notebooks/SKILL.md)
