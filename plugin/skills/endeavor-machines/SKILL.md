---
name: endeavor-machines
description: >-
  Use when the user wants notebooks to run on a server or a cluster (a
  machine reached over ssh), asks to add, use, switch or stop a machine, or a
  notebook tool says the machine isn't ready, is queued or was stopped.
---

# Notebooks on a server or cluster

Without these tools the notebooks run on the user's own computer. With them (`list_machines`, `add_machine`, `use_machine`, `stop_machine`; only `endeavor mcp` has them) a session can work on a machine the user reaches over ssh: a plain server, or a Slurm cluster's login node, where Endeavor runs Julia in a job. Nothing else in a notebook session changes: the notebook tools work the same, on the machine.

## Only when the user asks

Don't add a machine, switch machines, submit a job or stop a runtime unless the user asked for it. A project remembers the machine and folder its last session used, so a new session may already be on one: `list_machines` says which.

## The first time

1. `list_machines`: the machines already added, and the `Host` names in the user's `~/.ssh/config` that are not added yet. If the user named a machine that is listed, skip to step 3.
2. `add_machine(host)`: connects, and reports the machine's node, home folder, whether it has Slurm, its partitions and their limits. It waits up to 45 seconds. If it says it is still connecting, call `add_machine` again with the same host: the machine can't be used until a call has connected.
   Whether Julia runs in Slurm jobs or directly on the machine is the user's choice. When the machine has Slurm and the user hasn't said, ask: a workstation can have Slurm's tools without being a cluster. Pass `slurm: true` (jobs) or `slurm: false` (directly). Left out, a new machine with Slurm gets jobs; one added before stays as it was saved. The result says which it used (`cluster`, `runs_in`) and how to change it; changing is refused while Julia runs there.
3. `use_machine(machine)`: puts this session on it and starts or attaches to the Julia there. On a plain server that is all. On a cluster with no job running it submits nothing and returns `needs_job` with the saved default resources, and the session stays where it was: propose the resources to the user (for example "8 CPUs, 32 GB, 8 hours on `shared`?"), and once they agree call `use_machine` again with those values (`gpus: 0` means no GPU; `extra_sbatch_flags` entries are one string each, such as `"--constraint=a100"`). Never submit a job the user hasn't agreed to. A call that fails leaves the session where it was.
4. Tell the user the `browser_url`, and on a cluster the queue state and when the job ends. Then work as in **pluto-session**: the session starts with no notebook on the new machine, so create or open one there.

## Waiting

No call waits longer than 45 seconds. If `use_machine` returns `starting` or `queued`, say so, and check with `pluto_session_status`: it gives the state, the queue's state and reason, the job, and when it is up, when the job ends. A notebook tool called too early fails with the same facts. Don't poll in a tight loop and don't use `run_shell` to wait: tell the user it is queued, and look again when they say so or after real work in between.

## Files are on the machine

On a machine, the notebook and the files are there, not on the user's computer. Use `list_folder`, `read_file` and `run_shell` for them, with the machine's paths (`~` is its home, a relative path starts in the session's folder). A project checked out on both computers has the same paths in both places, so your own file tools would read the local copy and show no error. Change notebooks only with the notebook tools.

## Sign-in

Endeavor signs in with the user's own ssh keys and ssh agent. Never ask for a password or passphrase, never type one anywhere, and never run `ssh` yourself to get past a failed sign-in. When `add_machine` or `use_machine` fails, relay what it says: usually the user runs `ssh-add` once in a terminal, or `ssh <host>` once to accept the server's identity. Servers that ask for a password or a code at every login can't be used yet.

## Stopping and other people

- `stop_machine` ends the runtime for every client, and on a cluster cancels the job. Call it only when the user asks, such as to give a node back.
- If another session was active in a notebook on the machine in the last 15 minutes, it refuses and names who. Tell the user; call again with `force: true` only if they say to go ahead.
- It also refuses, without `force`, while Julia is still starting or a job is queued (it names the job), since sessions waiting for it can't be seen, and when it can't check who else is active. Same rule: tell the user, and use `force: true` only if they agree.
- Before you change a notebook, `list_notebooks` shows `other_sessions`; if one was active in the last few minutes, say so first.
- After a stop the session stays pointed at the machine. Notebook calls say it was stopped; `use_machine` starts it again.

Details of each tool and its result: [machine-tools.md](reference/machine-tools.md).

## Common mistakes

| Mistake | Fix |
|---------|-----|
| `use_machine` with made-up resources on a cluster | Show the user the defaults it returned and have them confirm or change them |
| Let a machine with Slurm tools become a cluster without asking | Ask whether Julia should run in Slurm jobs, and pass `slurm` |
| Read a server's files with `Read` or `Bash` | `read_file` and `run_shell`; your own tools see the user's computer |
| Ask the user for a password or passphrase | Never. Relay the failure and what it says to do in a terminal |
| Keep calling a notebook tool while the job is queued | Tell the user it is queued; check `pluto_session_status` later |
| `stop_machine` to fix a problem in one notebook | Restart or fix that notebook; stopping ends everyone's work on the machine |
| Treat `ready` as a sign the notebook is there | The session starts empty on a machine: `new_notebook` or `open_notebook` there |

## Additional resources

- **Tools and results:** [reference/machine-tools.md](reference/machine-tools.md)
- **Notebooks once the session is on a machine:** [pluto-session](../pluto-session/SKILL.md)
