# Machine tools

| Tool | Purpose | Notes |
|------|---------|-------|
| `list_machines` | The added machines and their state, `"local"`, which one this session uses, and `Host` names in `~/.ssh/config` not added yet | Starts no connection; safe anytime |
| `add_machine(host, name?, julia?, slurm?)` | Connect to an ssh alias or `user@host`, report and save what was found | Waits up to 45 s. Existing machine: updates it. Failure: nothing saved |
| `use_machine(machine, folder?, …)` | Put this session on a machine, or back on `"local"` | Cluster resources below. Remembered by the project |
| `stop_machine(machine, force?)` | Stop the runtime there for every client | Refuses and names who when another session was active in the last 15 minutes |

`machine` is a name from `list_machines`, or `"local"` for the user's own computer. A tool that fails says why in `message`.

## add_machine

Result: `machine`, `host`, `node`, `home`, `slurm` (Slurm was found), `cluster` and `runs_in` (`slurm_jobs` or `directly`: what is used), `partitions` (name, `default`, `max_hours`, `cpus`, `memory_gb`), `scratch`, `julia` (null until Julia has been started there once), `message`. `state` is `connecting` when the 45 s ran out: call again with the same host; until a call has connected the machine is saved but `list_machines` shows it as `not yet connected` and `use_machine` refuses it.

`slurm`: `true` runs Julia in Slurm jobs (an error if Slurm isn't there), `false` runs it directly on the machine even if Slurm is there. Left out, a new machine uses jobs when Slurm is there and a machine added before stays as saved. Changing a machine between the two is refused while Julia runs there.

## use_machine

| Argument | Meaning |
|----------|---------|
| `folder` | The session's folder on the machine; default its home folder |
| `partition`, `cpus`, `memory_gb`, `hours`, `gpus`, `account`, `extra_sbatch_flags` | Cluster only. What isn't given comes from the machine's saved defaults. What is submitted is saved as the new defaults. `gpus: 0` means no GPU and clears a saved default. Each `extra_sbatch_flags` entry starts with `-` and holds a flag and its value together (`"--constraint=a100"`); `--wrap` and line breaks are refused |

Result `state`:

| `state` | Meaning |
|---------|---------|
| `ready` | The runtime answers. `browser_url`, `node`, `folder`, `already_running` (it was running before this call), and for a cluster `job` (`id`, `node`, `ends_at`, `ends_in_minutes`) |
| `starting`, `queued` | Not up yet. `step`, and for a job `job` and `queue` (`state`, `reason`). Check with `pluto_session_status` |
| `needs_job` | Cluster, nothing running, no resources given. `defaults` has what would be submitted. Nothing was submitted and the session did not move (the project is unchanged). Ask the user, then call again with the values |

A call that fails (a bad argument or partition, a link that can't be reached) leaves the session, the project and the machine's saved defaults as they were.

`pluto_session_status` on a machine adds `machine`, and for a cluster `job`. When the runtime isn't up it answers from the connection's state: `state`, `step`, `queue`, `job`, `error`, `message`.

## stop_machine

Result `stopped` true, or false with a `message` telling you to ask the user: `other_sessions` (each `client`, `active_seconds_ago`, `notebook`) when another session was active lately; `state` `starting` or `queued` with `job` and `queue` when Julia is on its way or a job waits (stopping cancels it for any session waiting). It is an error, naming `force: true`, when it can't find out who else is active. `force: true` stops anyway, only after the user agreed.

## Local

`use_machine("local")` returns the session to the user's computer; the project then remembers nothing. `stop_machine("local")` stops the runtime on the user's computer, with the same check. The host tools (`list_folder`, `read_file`, `run_shell`) refuse there: use your own.
