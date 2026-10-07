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

## Installing needs the user

Looking at a machine installs nothing. When a tool returns `needs_install`, nothing was changed. `install.items` lists what would be installed, each with a `kind`, a `name` (with its version), `size_mb` and `place` (the folder); `message` says the same in words. Tell the user what is named, where and about how large, and ask.

- `kind` `helper`: Endeavor's program on the machine, missing or older than this plugin's (`install.update`). `install.running` says whether Julia already runs there.
- `kind` `runtime`: Julia, which Endeavor would download because none was found.
- On `use_machine`, one yes covers everything that call needs, including Julia if none is found once Endeavor's program is there. A yes given to `add_machine` covers Endeavor's program only.

Only after the user agrees to what the result names, call the same tool again with the same arguments and `install: true`. It covers that call only: ask again each time. Never set `install` on a first call, and never to get past an error. The user may instead tell you where Julia is: `add_machine` with `julia` set to its path, or to a shell line such as `module load julia`. A notebook tool called on a remembered machine never installs; it says what is missing.

## The first time

1. `list_machines`: the machines already added, and the `Host` names in the user's `~/.ssh/config` that are not added yet. If the user named a machine that is listed, skip to step 3.
2. `add_machine(host)`: connects and reports the node, home folder, and for Slurm the partitions and their limits. It waits up to 45 seconds; if it says it is still connecting, call it again with the same host (nothing is saved until a call has connected). If it returns `needs_install`, see above.
   Whether Julia runs in Slurm jobs or directly on the machine is the user's choice. When the machine has Slurm and the user hasn't said, ask: a workstation can have Slurm's tools without being a cluster. Pass `slurm: true` (jobs) or `slurm: false` (directly). The result says which it used (`cluster`, `runs_in`).
3. `use_machine(machine)`: puts this session on it and starts or attaches to the Julia there. On a plain server that is all. On a cluster with no job running it submits nothing and returns `needs_job` with the saved default resources: propose them to the user (for example "8 CPUs, 32 GB, 8 hours on `shared`?"), and once they agree call `use_machine` again with those values.
4. Tell the user the `browser_url`, and on a cluster the queue state and when the job ends. Then work as in **endeavor-notebooks**: the session has no notebook on a machine it hasn't worked on, so create or open one there. On one it worked on before it is still in its notebook there if that is open (`list_notebooks` shows `this_session`).

## Waiting

No call waits longer than 45 seconds. If `use_machine` returns `starting` or `queued`, tell the user and check with `pluto_session_status`: it gives the state, the queue's state and reason, the job, and when it is up, when the job ends. A notebook tool called too early fails with the same facts. Don't poll in a tight loop and don't use `run_shell` to wait: look again when the user says so or after real work in between.

## Files are on the machine

On a machine, the notebook and the files are there, not on the user's computer. Use `list_folder`, `read_file` and `run_shell` for them, with the machine's paths (`~` is its home, a relative path starts in the session's folder). A project checked out on both computers has the same paths in both places, so your own file tools would read the local copy and show no error. Change notebooks only with the notebook tools.

## After the session

Notebooks keep running on the machine after the session ends, until the idle limit (48 hours by default; `pluto_session_status` shows `idle_stop_hours` and `exits_when_idle`). `browser_url` works only while this session is connected. `list_machines` knows a machine's state only while this session is connected to it.

## Stopping

`stop_machine` ends Julia for every client and on a cluster cancels the job, so every notebook there ends, other people's too. Call it only when the user asks, such as to give a node back. Without `force` it refuses when another session was active lately, when Julia is still starting or a job is queued, and when it can't check; tell the user and use `force: true` only if they agree. After a stop the session stays pointed at the machine: notebook calls say it was stopped, and `use_machine` starts it again.

Each tool's arguments and results: [machine-tools.md](reference/machine-tools.md).

## Common mistakes

| Mistake | Fix |
|---------|-----|
| Set `install: true` before the user agreed | Install nothing without a yes. One yes to `use_machine` covers what that start needs, including Julia if none is found; a later call asks again |
| `use_machine` with made-up resources on a cluster | Show the user the defaults it returned and have them confirm or change them |
| Let a machine with Slurm tools become a cluster without asking | Ask whether Julia should run in Slurm jobs, and pass `slurm` |
| Read a server's files with `Read` or `Bash` | `read_file` and `run_shell`; your own tools see the user's computer |
| Ask the user for a password or passphrase | Never. Tell the user the failure and what it says to do in a terminal |
| Keep calling a notebook tool while the job is queued | Tell the user it is queued; check `pluto_session_status` later |
| `stop_machine` to fix a problem in one notebook | Restart or fix that notebook; stopping ends everyone's work on the machine |
| Treat `ready` as a sign the notebook is there | The session starts empty on a machine: `new_notebook` or `open_notebook` there |

## Additional resources

- **Tools and results:** [reference/machine-tools.md](reference/machine-tools.md)
- **Notebooks once the session is on a machine:** [endeavor-notebooks](../endeavor-notebooks/SKILL.md)
