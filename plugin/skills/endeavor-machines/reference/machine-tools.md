# Machine tools

| Tool | Purpose | Notes |
|------|---------|-------|
| `list_machines` | The added machines, `"local"`, which one this session uses, and `Host` names in `~/.ssh/config` not added yet | Starts no connection; safe anytime. A machine has a state only while this session is connected to it; otherwise it says `not connected`, which doesn't say whether Julia runs there |
| `add_machine(host, name?, julia?, slurm?, install?)` | Connect to an ssh alias or `user@host`, report and save what was found | Waits up to 45 s. Existing machine: updates it. Failure: nothing saved. Installs the helper only with `install: true`, after the user agreed; it never downloads Julia |
| `use_machine(machine, folder?, …)` | Put this session on a machine, or back on `"local"` | Cluster resources below. Remembered by the project |
| `stop_machine(machine, force?, install?)` | Stop the runtime there for every client | Refuses and says how many when another session made a tool call in the last 15 minutes. Needs this plugin's helper on the machine: if only an older one is there it returns `needs_install` and stops nothing |

`machine` is a name from `list_machines`, or `"local"` for the user's own computer. A tool that fails says why in `message`.

## Installing

`install: true` is the user's yes to what a `needs_install` result named. Without it nothing is installed or downloaded on the machine. The result has `install`: `what` `helper` (with `os`, `arch`, `folder`, `size_mb` (null when not known), `update` true when an older helper is there, and `running`: `{process}` or `{slurm_job}` when a runtime is running there, `{process_recorded}` or `{slurm_job_recorded}` when one is recorded and alive but Endeavor couldn't check what it is, else null) or `julia` (`detail`: what would be downloaded and where). The helper and Julia are two questions: `install: true` on one call covers what that call needs and is not kept, so a yes to the helper (even through `add_machine`) never covers Julia. `add_machine` saves nothing until it has connected (call it again with the same arguments and `install: true`) and only ever installs the helper; `use_machine` and `stop_machine` leave the session and the runtime where they were.

## add_machine

Result: `machine`, `host`, `node`, `home`, `slurm` (Slurm was found), `cluster` and `runs_in` (`slurm_jobs` or `directly`: what is used), `partitions` (name, `default`, `max_hours`, `cpus`, `memory_gb`), `scratch`, `julia` (null until Julia has been started there once), `message`. `state` is `connecting` when the 45 s ran out: call again with the same host; a new machine is saved only once a call has connected (`saved` is false until then), and an existing one keeps its saved settings until the new ones have connected.

`slurm`: `true` runs Julia in Slurm jobs (an error if Slurm isn't there), `false` runs it directly on the machine even if Slurm is there. Left out, a new machine uses jobs when Slurm is there and a machine added before stays as saved. Changing a machine between the two is refused while Julia runs there.

## use_machine

| Argument | Meaning |
|----------|---------|
| `folder` | The session's folder on the machine; default its home folder |
| `partition`, `cpus`, `memory_gb`, `hours`, `gpus`, `account`, `extra_sbatch_flags` | Cluster only. What isn't given comes from the machine's saved defaults. What is submitted is saved as the new defaults. `gpus: 0` means no GPU and clears a saved default. Each `extra_sbatch_flags` entry starts with `-` and holds a flag and its value together (`"--constraint=a100"`); `--wrap` and line breaks are refused |

Result `state`:

| `state` | Meaning |
|---------|---------|
| `ready` | The runtime answers. `browser_url` (works while this session is connected), `node`, `remote_port` (the runtime's own port on the machine), `folder`, `already_running` (it was running before this call), and for a cluster `job` (`id`, `node`, `ends_at`, `ends_in_minutes`) |
| `starting`, `queued` | Not up yet. `step`, and for a job `job` and `queue` (`state`, `reason`). Check with `pluto_session_status` |
| `needs_install` | The machine lacks the helper (or an update of it), or Julia. `install` says what. Ask the user, then call again with `install: true`; the session did not move |
| `needs_job` | Cluster, nothing running, no resources given. `defaults` has what would be submitted. Nothing was submitted and the session did not move (the project is unchanged). Ask the user, then call again with the values |

A call that fails (a bad argument or partition, a machine that can't be reached) leaves the session, the project and the machine's saved defaults as they were.

`pluto_session_status` on a machine adds `machine`, and for a cluster `job`. When the runtime isn't up it answers from the connection's state: `state`, `step`, `queue`, `job`, `error`, `message`.

## stop_machine

Result `stopped` true, or false with a `message` telling you to ask the user: `active_sessions` and `active_seconds_ago` (how many other sessions made a tool call in the last 15 minutes, and how long ago the latest did) when there were any; `state` `starting` or `queued` with `job` and `queue` when Julia is on its way or a job waits (stopping cancels it for any session waiting). It is an error, naming `force: true`, when it can't find out who else is active. `force: true` stops anyway, only after the user agreed.

## Local

`use_machine("local")` returns the session to the user's computer; the project then remembers nothing. `stop_machine("local")` stops the runtime on the user's computer, with the same check. The host tools (`list_folder`, `read_file`, `run_shell`) refuse there: use your own.
