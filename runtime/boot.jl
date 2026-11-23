
const log_flusher = Timer(_ -> (flush(stdout); flush(stderr)), 0.25; interval=0.25)

include(joinpath(@__DIR__, "cache_guard.jl"))
endeavor_drop_unreadable_caches()

import Pkg
Pkg.instantiate(; io=stderr, allow_autoprecomp=false)
Pkg.precompile(["EndeavorRuntime"]; io=stderr)
using EndeavorRuntime
const warm = Base.isprecompiled(EndeavorRuntime.WARM)
if warm
    try
        Base.require(EndeavorRuntime.WARM)
    catch e
        @warn "Couldn't load EndeavorRuntimeWarm; notebooks open without it" exception = e
    end
end

pluto_port, mcp_port = parse.(Int, ARGS[1:2])
token = get(ENV, "ENDEAVOR_TOKEN", "")
isempty(token) && error("ENDEAVOR_TOKEN is not set; endeavor starts this script with one.")
state = get(ENV, "ENDEAVOR_STATE", "")
isempty(state) && error("ENDEAVOR_STATE is not set; endeavor starts this script with one.")
launcher = get(ENV, "ENDEAVOR_LAUNCHER", "process")
job = launcher == "slurm" ? get(ENV, "SLURM_JOB_ID", "") : ""
foreach(k -> delete!(ENV, k), ("ENDEAVOR_TOKEN", "ENDEAVOR_STATE", "ENDEAVOR_LAUNCHER"))

EndeavorRuntime.configure_standalone!(; pluto_port, mcp_port, token)
EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false)
secret = EndeavorRuntime.standalone_session().secret

tmp = state * ".tmp"
open(tmp, "w") do io
    chmod(tmp, 0o600)
    EndeavorRuntime.JSON.print(io, Dict(
        "launcher" => launcher, "node" => gethostname(), "pid" => getpid(),
        "pluto_port" => pluto_port, "mcp_port" => mcp_port,
        "pluto_secret" => secret, "job" => job,
    ))
end
mv(tmp, state; force=true)
atexit(() -> rm(state; force=true))
@info "Runtime ready" pluto_port mcp_port
warm || launcher == "slurm" || EndeavorRuntime.warm_up_later()

wait(Condition())
