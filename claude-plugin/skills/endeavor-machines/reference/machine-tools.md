# Machine tools

| Tool | Purpose |
|------|---------|
| `list_machines` | The added machines, `"local"`, which one this session uses, and `Host` names in `~/.ssh/config` not added yet. Starts no connection; safe anytime |
| `add_machine(host, name?, julia?, slurm?, install?)` | Connect to an ssh alias or `user@host`, report and save what was found |
| `use_machine(machine, folder?, …)` | Put this session on a machine, or back on `"local"`. Cluster resources below |
| `stop_machine(machine, force?, install?)` | Stop Endeavor there for every client |

`machine` is a name from `list_machines`, or `"local"` for the user's own computer. A tool that fails says why in `message`.

## list_machines

Each machine has `name`, `host`, `cluster`, `this_session`, and a `state`: `not connected` when this session has no connection to it (this says nothing about whether Endeavor runs there), else `connecting`, `connected`, `starting`, `queued`, `ready`, `failed` (with `error`) or `needs_install`. `local` has `running`, `not running` or `stopped from this session`.

## Installing

`install: true` is the user's yes to what a `needs_install` result named. Without it nothing is installed or downloaded on the machine. `needs_install` results have `state` `needs_install`, `message`, and `install`:

- `items`: what would be installed, in order. Each has `kind` (`helper`: Endeavor's program on the machine; `runtime`: Julia), `name` (with version), `size_mb` and `place` (the folder). Each may be null when not known.
- Only when a `helper` item is there: `os` and `arch`; `update` (true when an older version is installed, which stays beside the new one); `running`, which is `{process}` or `{slurm_job}` when Endeavor runs there, `{process_recorded}` or `{slurm_job_recorded}` when one is recorded but Endeavor couldn't check what it is, else null. Installing doesn't touch what runs there.

Endeavor's program and Julia are two questions: `install: true` covers what that one call needs and is not kept, so a yes to `add_machine` never covers Julia. `use_machine` and `stop_machine` leave the session and Julia where they were. A `needs_install` from `add_machine` also has `host` and `saved` (true only for a machine added before): nothing new is saved until a call has connected.

## add_machine

Waits up to 45 seconds. Result: `machine`, `host`, `state` `connected`, `saved`, `updated` (the machine was added before), `node`, `home`, `os`, `arch`, `slurm` (Slurm was found), `cluster` and `runs_in` (`slurm_jobs` or `directly`: what is used), `partitions` (name, `default`, `max_hours`, `cpus`, `memory_gb`), `scratch`, `found` (`name`, `version`, `path` of the Julia found; empty until Julia has been started there once), `message`.

`state` `connecting` means the 45 seconds ran out: call again with the same host. A new machine is saved only once a call has connected (`saved` is false until then), and an existing one keeps its saved settings until the new ones have connected. If it can't connect, nothing is saved and the call is an error.

`slurm`: `true` runs notebooks in Slurm jobs (an error if Slurm isn't there), `false` runs it directly on the machine even if Slurm is there (on a cluster that is the shared login node, so only for the user's own workstation or once the user confirms it isn't a shared cluster). Left out, a new machine uses jobs when Slurm is there and a machine added before stays as saved. Changing a machine between the two is refused while Endeavor runs there; `stop_machine` first, with the user's agreement.

## use_machine

| Argument | Meaning |
|----------|---------|
| `folder` | The session's folder on the machine: where new notebooks go, where a notebook tool's relative path starts, and where `run_shell` runs by default. Default: the folder the project remembers for it, else its home folder. A folder that doesn't exist is made when the folder it goes in exists, and `message` says it was made; when that one is missing too, the call fails before any job is asked for. Not used for `"local"` |
| `partition`, `cpus`, `memory_gb`, `hours`, `gpus`, `account`, `extra_sbatch_flags` | Cluster only (an error on a plain server or `"local"`). What isn't given comes from the machine's saved defaults; what is submitted is saved as the new defaults. `gpus: 0` means no GPU and clears a saved default. Each `extra_sbatch_flags` entry starts with `-` and holds a flag and its value together (`"--constraint=a100"`); `--wrap` and line breaks are refused. If a job is already queued or running, the resources are not used |

Result `state`:

| `state` | Meaning |
|---------|---------|
| `ready` | Endeavor answers there; Julia starts when a Julia notebook needs it, and R notebooks don't need it. `browser_url` (works while this session is connected) and `folder`. On a machine also `node`, `remote_port` (Endeavor's own port on the machine: on a plain server `message` gives an ssh command the user can run to reach it after the session ends; on a cluster it is on the job's node, behind the login node, and there is no such command), `already_running` (it was running before this call), and for a cluster `job` (`id`, `node`, `ends_at`, `ends_in_minutes`) |
| `starting`, `queued` | Not up yet. `step`, and for a job `job` and `queue` (`state`, `reason`, `reason_text`; once the job runs, `node` in place of `reason`). Wait by calling a notebook tool again (each call waits up to 45 seconds); a queued job can wait minutes or hours, so after a few tries let the user say when to check again. `session_status` shows the step at once |
| `needs_install` | The machine lacks Endeavor's program (or an update of it), or Julia. `install` says what. Ask the user, then call again with `install: true`; the session did not move |
| `needs_job` | Cluster, nothing running, no resources given. `defaults` has what would be submitted and `partitions` what the cluster offers. Nothing was submitted and the session did not move (the project is unchanged). Ask the user, then call again with the values |

A call that fails (a bad argument or partition, a machine that can't be reached) leaves the session, the project and the machine's saved defaults as they were.

`session_status` on a machine adds `machine`, and for a cluster `job`. When Endeavor isn't up there it answers from the connection's state: `state`, `step`, `queue`, `job`, `error`, `message`.

## stop_machine

Result `stopped` true, or false with a `message`. Without `force` it stops nothing and says to ask the user when:

- another session working in an open notebook made a tool call in the last 15 minutes: `active_sessions` and `active_seconds_ago` (how many, and how long ago the latest did);
- Endeavor is starting or a job is queued: `state` `starting` or `queued` with `job` and `queue`. Stopping cancels it for any session waiting for it, which can't be seen; this holds on this computer too.

It is an error, naming `force: true`, when it can't find out who else is active. `force: true` stops anyway, only after the user agreed. If nothing is running, `stopped` is false and the message says so. If Endeavor's program on the machine is missing or older than this plugin's, the result is `needs_install` with `stopped` false and nothing is stopped.

## Local

`use_machine("local")` returns the session to the user's computer; the project then remembers nothing. `stop_machine("local")` stops Endeavor on the user's computer, with the same check. The host tools (`list_folder`, `read_file`, `run_shell`) refuse there: use your own.
