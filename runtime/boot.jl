# Started detached by endeavor-remote:
#   julia --project=runtime runtime/boot.jl <pluto_port> <mcp_port>
# with ENDEAVOR_TOKEN (the bridge's bearer token), ENDEAVOR_STATE (where to write
# runtime.json) and ENDEAVOR_LAUNCHER in the environment. Once Pluto and the bridge
# are up it writes runtime.json, which is how the helper learns the runtime is
# ready; it serves until the bridge's `endeavor/shutdown` or a signal ends it.
import Pkg
Pkg.instantiate(; io=stderr)
using EndeavorRuntime

pluto_port, mcp_port = parse.(Int, ARGS[1:2])
# Taken from the environment (not argv, which `ps` shows to every user) and dropped
# so notebook worker processes don't inherit them.
token = get(ENV, "ENDEAVOR_TOKEN", "")
isempty(token) && error("ENDEAVOR_TOKEN is not set; endeavor-remote starts this script with one.")
state = get(ENV, "ENDEAVOR_STATE", "")
isempty(state) && error("ENDEAVOR_STATE is not set; endeavor-remote starts this script with one.")
launcher = get(ENV, "ENDEAVOR_LAUNCHER", "process")
foreach(k -> delete!(ENV, k), ("ENDEAVOR_TOKEN", "ENDEAVOR_STATE", "ENDEAVOR_LAUNCHER"))

EndeavorRuntime.configure_standalone!(; pluto_port, mcp_port, token)
EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false)
secret = EndeavorRuntime.standalone_session().secret

# The helper checks the bridge answers before using it; wait here too, so the state
# file never names a runtime that isn't serving yet.
for _ in 1:600
    EndeavorRuntime._STANDALONE_HTTP_SERVER[] !== nothing && break
    sleep(0.05)
end

# Made private before the token goes in, then moved into place whole, so a reader
# never sees half a file and nobody else can read the token.
tmp = state * ".tmp"
open(tmp, "w") do io
    chmod(tmp, 0o600)
    EndeavorRuntime.JSON.print(io, Dict(
        "launcher" => launcher, "node" => gethostname(), "pid" => getpid(),
        "pluto_port" => pluto_port, "mcp_port" => mcp_port,
        "token" => token, "pluto_secret" => secret,
    ))
end
mv(tmp, state; force=true)
atexit(() -> rm(state; force=true))
@info "Runtime ready" pluto_port mcp_port

wait(Condition())
