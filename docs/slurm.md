# The Slurm launcher

`slurm.rs`. On a cluster's login node, `connect --launcher slurm` submits a
batch job whose script is `endeavor node-start`. That starts the runtime on
the compute node, which writes `runtime.json` (with the job id) to the state
folder, shared with the login node. While the job waits in the queue the app
hears its state. Once the runtime is up, the login helper starts
`endeavor relay` on the node and passes the app's streams through its stdin
and stdout. The runtime stays on the node's loopback.

## Submitting

- `job.json` records a submitted job until its runtime is up, so a reconnect
  from any login node goes on waiting for it instead of submitting another.
- The start lock is held while the helper reads `job.json`, decides and
  submits, so two helpers never submit two jobs. It is let go before the wait
  in the queue: a helper that comes in meanwhile finds `job.json` and waits
  for the same job. A helper waiting for the job removes `job.json` once the
  runtime is up.
- With `attach_only` nothing is submitted.
- A job that starts Julia only when a notebook needs it may never need one (R
  notebooks don't), so the user isn't asked about Julia when it is submitted
  (#80). The job's core looks for Julia on the node when it needs it, and
  never downloads it there, where there is often no internet. When it isn't
  found, the user's yes downloads it on the login node (`login_julia`), and
  the path is handed to the core (`endeavor/use_julia`). That download runs
  in a thread, one at a time, and each question meanwhile answers how far it
  is.
- The job's log shows Pluto's secret URL. A cluster's nodes run Linux, so a
  job never runs `node-start` on Windows.

## Waiting and relaying

- The helper asks `squeue` about a waiting job, and tells the app its state
  and Slurm's reason. Once it runs, the reason slot carries the node it got.
- The relay starts through `srun --overlap` inside the job, so no SSH between
  cluster nodes is needed, or else `ssh` to the node, for clusters whose Slurm
  is too old for `--overlap`. srun gets `--unbuffered`: without it, it holds
  the relay's output until a newline, and frames are binary.
- The relay on the node has 60 s to say it is attached (`RELAY_TIMEOUT`). A
  relay that ends while the job may still run is started again.
- To stop the relay, SIGTERM goes first: srun then ends its step on the node.

## Stopping and ending

- A stop asks the runtime to shut down through the relay, then cancels the
  job, forced or not, whoever submitted it. The caller holds the start lock.
  The app leaving leaves a queued job queued, for the next connect.
  A cluster's job is left either way by `--own-with-client`.
- The job ends with Julia, since its script execs it.
- A job's records are dropped once it ended or was cancelled, unless they have
  moved on to another job: helpers that shared one call this late, after one
  may have submitted the next. The check and the removal are made under the
  start lock, if it's free within a few seconds; otherwise the records stay,
  and a later look drops a `job.json` whose job is gone.
- Why a job ended comes from `sacct`, else `squeue` while it still lists
  finished jobs, else Slurm's words in the log. `sacct` can lag a job's end by
  a moment, and a job on its way out still reads RUNNING or COMPLETING for a
  few seconds, so that is waited for up to 10 s, and the log, where Slurm
  writes when it signals the job, is read on every try. An exit by signal, or
  one nobody saw, may be Slurm ending the job, which the job's state says and
  the status doesn't. "exited" is what a runtime nobody waited on reports, so
  the app is told nothing more for it.
