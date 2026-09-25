# Started by the app: `julia --project=runtime runtime/boot.jl <pluto_port> <mcp_port>`.
# Prints one `READY <pluto_url> <mcp_url>` line on stdout, then serves until stdin closes.
# Stdin lines are commands from the app: `folder <path>` makes <path> the folder Pluto's
# "Save notebook" box suggests (new notebooks start unsaved in Pluto's scratch folder).
import Pkg
Pkg.instantiate(; io=stderr)
using EndeavorRuntime

pluto_port, mcp_port = parse.(Int, ARGS[1:2])
# The app passes the bridge's bearer token in the environment (not argv, which
# `ps` shows to every user); drop it so notebook worker processes don't inherit it.
token = get(ENV, "ENDEAVOR_TOKEN", "")
isempty(token) && error("ENDEAVOR_TOKEN is not set; the app starts this script with one.")
delete!(ENV, "ENDEAVOR_TOKEN")
EndeavorRuntime.configure_standalone!(; pluto_port, mcp_port, token)
EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false)
secret = EndeavorRuntime.standalone_session().secret

println("READY http://127.0.0.1:$pluto_port/?secret=$secret http://127.0.0.1:$mcp_port/sse")
flush(stdout)

# ponytail: the app holds our stdin; EOF means it quit or crashed, so no PID bookkeeping.
options = EndeavorRuntime.standalone_session().options.server
for line in eachline(stdin)
    startswith(line, "folder ") && (options.notebook_path_suggestion = joinpath(line[8:end], ""))
end
exit()
