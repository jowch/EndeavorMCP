# ---------------------------------------------------------------------------
# The agent's MCP messages, from the core (`endeavor-remote core`), which
# serves the agent's MCP connection and passes on what it doesn't answer, with
# the caller's X-Endeavor-Session and X-Endeavor-Host headers. The reply is the
# JSON-RPC response ("null" for a notification).
# ---------------------------------------------------------------------------

function _handle_dispatch(http::HTTP.Stream, pluto_session)
    body = String(read(http))
    msg  = try
        JSON.parse(body, Dict{String,Any})
    catch
        HTTP.setstatus(http, 400)
        HTTP.startwrite(http)
        write(http, """{"error":"Invalid JSON"}""")
        return
    end
    owner = HTTP.header(http.message, "X-Endeavor-Session", "")
    host  = HTTP.header(http.message, "X-Endeavor-Host", "")
    resp  = _dispatch_mcp(pluto_session, msg; owner, host)
    HTTP.setstatus(http, 200)
    HTTP.setheader(http, "Content-Type" => "application/json")
    HTTP.startwrite(http)
    write(http, JSON.json(resp))
end

# ---------------------------------------------------------------------------
# HTTP bridge
# ---------------------------------------------------------------------------

# `Authorization: Bearer <token>` matches the configured token, compared in
# constant time. Only the core (`endeavor-remote core`) calls this port, and it
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

        if method == "GET" && startswith(target, "/events")
            _handle_events(http)

        elseif method == "POST" && startswith(target, "/dispatch")
            _handle_dispatch(http, pluto_session)

        elseif method == "POST" && startswith(target, "/call")
            # Must read the body before responding (HTTP.jl stream contract).
            body = String(read(http))
            msg  = try
                JSON.parse(body, Dict{String,Any})
            catch
                HTTP.setstatus(http, 400)
                HTTP.startwrite(http)
                write(http, """{"error":"Invalid JSON"}""")
                return
            end
            active   = standalone_session()
            sess     = active !== nothing ? active : pluto_session
            # App-only (not reachable through the agent's MCP connection).
            resp = if get(msg, "method", "") == "endeavor/tool_called"
                # A tool call the core answered itself: activity on its notebook,
                # and the app hears the notebooks' state after it.
                note_activity!(get(get(msg, "params", Dict{String,Any}()), "arguments", Dict{String,Any}()))
                publish_notebooks!()
                Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "result" => Dict{String,Any}())
            elseif get(msg, "method", "") == "endeavor/set_notebook"
                p = get(msg, "params", Dict{String,Any}())
                owner, notebook = string(get(p, "owner", "")), string(something(get(p, "notebook", ""), ""))
                bind_notebook!(owner, notebook)
                @info "Session $owner notebook: $(isempty(notebook) ? "(none)" : notebook)"
                Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "result" => Dict{String,Any}())
            elseif get(msg, "method", "") == "endeavor/set_session_folder"
                p = get(msg, "params", Dict{String,Any}())
                owner, folder = string(get(p, "owner", "")), string(something(get(p, "folder", ""), ""))
                set_session_folder!(owner, folder)
                Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "result" => Dict{String,Any}())
            elseif get(msg, "method", "") == "endeavor/set_idle_limit"
                hours = get(get(msg, "params", Dict{String,Any}()), "hours", 48)
                set_idle_limit!(hours isa Real ? hours : 48)
                @info "Idle notebooks stop after: $(hours == 0 ? "never" : "$hours hours")"
                Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "result" => Dict{String,Any}())
            elseif get(msg, "method", "") == "endeavor/stop_notebook"
                p = get(msg, "params", Dict{String,Any}())
                result = stop_notebook!(sess, string(get(p, "path", "")))
                @info "Stopped notebook $(get(p, "path", "")): $(result["stopped"])"
                Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "result" => result)
            elseif get(msg, "method", "") == "endeavor/set_folder"
                path = string(get(get(msg, "params", Dict{String,Any}()), "path", ""))
                set_folder!(sess, path)
                Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "result" => Dict{String,Any}())
            elseif get(msg, "method", "") == "endeavor/shutdown"
                # Pluto has already saved every notebook file. Exit after this reply is out.
                @info "Shutting down at the app's request"
                @async (sleep(0.2); _SHUTDOWN[]())
                Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "result" => Dict{String,Any}())
            elseif get(msg, "method", "") in ("endeavor/restart_notebook", "endeavor/move_notebook", "endeavor/file_info", "endeavor/new_notebook")
                p = get(msg, "params", Dict{String,Any}())
                try
                    result = if msg["method"] == "endeavor/restart_notebook"
                        restart_notebook!(sess, string(get(p, "notebook_id", "")))
                    elseif msg["method"] == "endeavor/move_notebook"
                        move_notebook!(sess, string(get(p, "notebook_id", "")), string(get(p, "path", "")))
                    elseif msg["method"] == "endeavor/file_info"
                        file_info(string(get(p, "path", "")))
                    else
                        new_notebook_for!(string(get(p, "owner", "")))
                    end
                    Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "result" => result)
                catch e
                    Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "error" => Dict("code" => -32000, "message" => sprint(showerror, e)))
                end
            elseif get(msg, "method", "") == "endeavor/run_preview"
                p = get(msg, "params", Dict{String,Any}())
                try
                    result = run_preview(sess, string(get(p, "tool", "")), get(p, "arguments", Dict{String,Any}()))
                    Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "result" => result)
                catch e
                    Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "error" => Dict("code" => -32000, "message" => sprint(showerror, e)))
                end
            else
                _dispatch_mcp(sess, msg)
            end
            resp_json = resp !== nothing ? JSON.json(resp) : "{}"
            HTTP.setstatus(http, 200)
            HTTP.setheader(http, "Content-Type" => "application/json")
            HTTP.startwrite(http)
            write(http, resp_json)

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
