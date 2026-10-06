# Machine tools

| Tool | Purpose | Notes |
|------|---------|-------|
| `list_machines` | The added machines and their state, `"local"`, which one this session uses, and `Host` names in `~/.ssh/config` not added yet | Starts no connection; safe anytime |
| `add_machine(host, name?, julia?)` | Connect to an ssh alias or `user@host`, report and save what was found | Waits up to 45 s. Existing machine: updates it. Failure: nothing saved |
| `use_machine(machine, folder?, …)` | Put this session on a machine, or back on `"local"` | Cluster resources below. Remembered by the project |
| `stop_machine(machine, force?)` | Stop the runtime there for every client | Refuses and names who when another session was active in the last 15 minutes |

`machine` is a name from `list_machines`, or `"local"` for the user's own computer. A tool that fails says why in `message`.

## add_machine

Result: `machine`, `host`, `node`, `home`, `slurm`, `partitions` (name, `default`, `max_hours`, `cpus`, `memory_gb`), `scratch`, `julia` (null until Julia has been started there once), `message`. `state` is `connecting` when the 45 s ran out: call again.

## use_machine

| Argument | Meaning |
|----------|---------|
| `folder` | The session's folder on the machine; default its home folder |
| `partition`, `cpus`, `memory_gb`, `hours`, `gpus`, `account`, `extra_sbatch_flags` | Cluster only. What isn't given comes from the machine's saved defaults. What is submitted is saved as the new defaults |

Result `state`:

| `state` | Meaning |
|---------|---------|
| `ready` | The runtime answers. `browser_url`, `node`, `folder`, `already_running` (it was running before this call), and for a cluster `job` (`id`, `node`, `ends_at`, `ends_in_minutes`) |
| `starting`, `queued` | Not up yet. `step`, and for a job `job` and `queue` (`state`, `reason`). Check with `pluto_session_status` |
| `needs_job` | Cluster, nothing running, no resources given. `defaults` has what would be submitted. Ask the user, then call again with the values |

`pluto_session_status` on a machine adds `machine`, and for a cluster `job`. When the runtime isn't up it answers from the connection's state: `state`, `step`, `queue`, `job`, `error`, `message`.

## stop_machine

Result `stopped` true, or false with `other_sessions` (each `client`, `active_seconds_ago`, `notebook`) and a `message` telling you to ask the user.

## Local

`use_machine("local")` returns the session to the user's computer; the project then remembers nothing. `stop_machine("local")` stops the runtime on the user's computer, with the same check. The host tools (`list_folder`, `read_file`, `run_shell`) refuse there: use your own.
