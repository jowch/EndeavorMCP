# Started by the app: `julia --project=runtime runtime/boot.jl <pluto_port> <mcp_port>`.
# Prints one `READY <pluto_url> <mcp_url>` line on stdout, then serves until stdin closes.
import Pkg
Pkg.instantiate(; io=stderr)
using PlutoMCP

pluto_port, mcp_port = parse.(Int, ARGS[1:2])
PlutoMCP.configure_standalone!(; pluto_port, mcp_port)
PlutoMCP.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false)
secret = PlutoMCP.standalone_session().secret

println("READY http://127.0.0.1:$pluto_port/?secret=$secret http://127.0.0.1:$mcp_port/sse")
flush(stdout)

# ponytail: the app holds our stdin; EOF means it quit or crashed, so no PID bookkeeping.
read(stdin)
exit()
