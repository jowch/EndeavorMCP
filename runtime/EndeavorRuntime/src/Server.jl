# ---------------------------------------------------------------------------
# HTTP bridge
# ---------------------------------------------------------------------------

# `Authorization: Bearer <token>` matches the configured token, compared in
# constant time. Only the core (`endeavor core`) calls this port, and it
# checks each request's Origin and Host first; but loopback isn't private on a
# shared machine: any local user can reach the port, so the token is what keeps
# them out.
function _authorized(http::HTTP.Stream)
    token = _BRIDGE_TOKEN[]
    isempty(token) && return true
    given = HTTP.header(http.message, "Authorization", "")
    expected = "Bearer " * token
    length(given) == length(expected) || return false
    diff = 0x00
    for (a, b) in zip(codeunits(given), codeunits(expected))
        diff |= a ⊻ b
    end
    return diff == 0x00
end

function _run_http_mcp_server(pluto_session, port::Int; listenany::Bool=false)
    function handler(http::HTTP.Stream)
        method = http.message.method
        target = http.message.target
        if !(method == "GET" && (target == "/health" || startswith(target, "/health?"))) && !_authorized(http)
            read(http)
            HTTP.setstatus(http, 401)
            HTTP.setheader(http, "Content-Type" => "application/json")
            HTTP.startwrite(http)
            write(http, """{"error":"unauthorized"}""")
            return
        end

        if method == "GET" && startswith(target, "/notifications")
            _handle_notifications(http)

        elseif method == "POST" && startswith(target, "/adapter")
            active = standalone_session()
            _handle_adapter(http, active !== nothing ? active : pluto_session)

        elseif method == "POST" && startswith(target, "/call")
            # The app's calls the core passes on (it answers the rest).
            # Must read the body before responding (HTTP.jl stream contract).
            msg = try
                JSON.parse(String(read(http)), Dict{String,Any})
            catch
                HTTP.setstatus(http, 400)
                HTTP.startwrite(http)
                write(http, """{"error":"Invalid JSON"}""")
                return
            end
            active = standalone_session()
            sess   = active !== nothing ? active : pluto_session
            id     = get(msg, "id", nothing)
            method_name = string(get(msg, "method", ""))
            resp = if method_name == "endeavor/set_folder"
                set_folder!(sess, string(get(get(msg, "params", Dict{String,Any}()), "path", "")))
                Dict("jsonrpc" => "2.0", "id" => id, "result" => Dict{String,Any}())
            elseif method_name == "endeavor/shutdown"
                # Pluto has already saved every notebook file. Exit after this reply is out.
                @info "Shutting down at the app's request"
                @async (sleep(0.2); _SHUTDOWN[]())
                Dict("jsonrpc" => "2.0", "id" => id, "result" => Dict{String,Any}())
            else
                Dict("jsonrpc" => "2.0", "id" => id, "error" => Dict("code" => -32601, "message" => "Method not found: $method_name"))
            end
            HTTP.setstatus(http, 200)
            HTTP.setheader(http, "Content-Type" => "application/json")
            HTTP.startwrite(http)
            write(http, JSON.json(resp))

        elseif method == "GET" && (target == "/health" || startswith(target, "/health?"))
            HTTP.setstatus(http, 200)
            HTTP.startwrite(http)
            write(http, "ok")

        else
            HTTP.setstatus(http, 404)
            HTTP.startwrite(http)
        end
    end

    return HTTP.serve!(
        handler, "127.0.0.1", port;
        stream=true, verbose=false, listenany=listenany,
    )
end
