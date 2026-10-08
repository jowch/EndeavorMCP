# Started by `endeavor core` (which the helper starts, detached or as a
# Slurm job's script):
#   julia --project=runtime runtime/boot.jl <pluto_port> <mcp_port>
# with ENDEAVOR_TOKEN (the bridge's bearer token), ENDEAVOR_STATE (where to write
# its state) and ENDEAVOR_LAUNCHER in the environment. Both ports are private to
# the core, which serves the runtime's one port in front of them and adds Pluto's
# secret to what it passes on (docs/one-port.md). Once Pluto and the bridge are up
# it writes that state, which is how the core learns Julia is ready (the core then
# writes runtime.json for the helper); it serves until the bridge's
# `endeavor/shutdown` or a signal ends it.

# Our stdout and stderr are runtime.log, a file, which Julia buffers. The helper
# reads that file for boot progress and for a crash's last lines, so flush often.
const log_flusher = Timer(_ -> (flush(stdout); flush(stderr)), 0.25; interval=0.25)

import Pkg
Pkg.instantiate(; io=stderr)
using EndeavorRuntime

pluto_port, mcp_port = parse.(Int, ARGS[1:2])
# Taken from the environment (not argv, which `ps` shows to every user) and dropped
# so notebook worker processes don't inherit them.
token = get(ENV, "ENDEAVOR_TOKEN", "")
isempty(token) && error("ENDEAVOR_TOKEN is not set; endeavor starts this script with one.")
state = get(ENV, "ENDEAVOR_STATE", "")
isempty(state) && error("ENDEAVOR_STATE is not set; endeavor starts this script with one.")
launcher = get(ENV, "ENDEAVOR_LAUNCHER", "process")
# A Slurm job's id, so a reconnect can find the job with squeue.
job = launcher == "slurm" ? get(ENV, "SLURM_JOB_ID", "") : ""
foreach(k -> delete!(ENV, k), ("ENDEAVOR_TOKEN", "ENDEAVOR_STATE", "ENDEAVOR_LAUNCHER"))

EndeavorRuntime.configure_standalone!(; pluto_port, mcp_port, token)
EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false)
secret = EndeavorRuntime.standalone_session().secret

# Made private before the token goes in, then moved into place whole, so a reader
# never sees half a file and nobody else can read the token.
tmp = state * ".tmp"
open(tmp, "w") do io
    chmod(tmp, 0o600)
    EndeavorRuntime.JSON.print(io, Dict(
        "launcher" => launcher, "node" => gethostname(), "pid" => getpid(),
        "pluto_port" => pluto_port, "mcp_port" => mcp_port,
        "token" => token, "pluto_secret" => secret, "job" => job,
    ))
end
mv(tmp, state; force=true)
atexit(() -> rm(state; force=true))
@info "Runtime ready" pluto_port mcp_port

wait(Condition())
