# Seeded from PlutoMCP.jl (https://github.com/mthelm85/PlutoMCP.jl, MIT) via the jowch fork at 918e75d.
module EndeavorRuntime

using Base64
using JSON
using UUIDs
using HTTP
using Pluto
import Pkg

include("Saving.jl")
include("Output.jl")
include("Controls.jl")
include("Adapter.jl")
include("Lifecycle.jl")
include("Server.jl")

register_http_bridge!(_run_http_mcp_server)

end
