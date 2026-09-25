# ---------------------------------------------------------------------------
# URL query-string parser (avoids HTTP.URIs API uncertainty)
# ---------------------------------------------------------------------------

function _query_params(target::String)
    d   = Dict{String,String}()
    idx = findfirst('?', target)
    idx === nothing && return d
    for pair in split(target[idx+1:end], '&')
        kv = split(pair, '='; limit=2)
        length(kv) == 2 && (d[kv[1]] = kv[2])
    end
    d
end

# ---------------------------------------------------------------------------
# SSE session state
# ---------------------------------------------------------------------------

const _SSE_SESSIONS      = Dict{String,Channel{String}}()
const _SSE_SESSIONS_LOCK = ReentrantLock()

# ---------------------------------------------------------------------------
# Internal HTTP/SSE endpoint handlers
# ---------------------------------------------------------------------------

function _handle_sse(http::HTTP.Stream)
    sid = string(uuid4())
    ch  = Channel{String}(64)

    lock(_SSE_SESSIONS_LOCK) do
        _SSE_SESSIONS[sid] = ch
    end

    HTTP.setheader(http, "Content-Type"  => "text/event-stream")
    HTTP.setheader(http, "Cache-Control" => "no-cache")
    HTTP.setheader(http, "Connection"    => "keep-alive")
    HTTP.startwrite(http)

    # Tell the client where to POST messages
    write(http, "event: endpoint\ndata: /message?sessionId=$sid\n\n")
    flush(http)

    # Background keepalive so proxies don't close idle connections
    keepalive = @async while isopen(ch)
        sleep(15)
        try
            write(http, ": keepalive\n\n")
            flush(http)
        catch
            break
        end
    end

    try
        for msg_json in ch
            write(http, "event: message\ndata: $msg_json\n\n")
            flush(http)
        end
    catch
        # Client disconnected
    finally
        lock(_SSE_SESSIONS_LOCK) do
            delete!(_SSE_SESSIONS, sid)
        end
        isopen(ch) && close(ch)
        try schedule(keepalive, InterruptException(); error=true) catch end
    end
end

function _handle_post(http::HTTP.Stream, pluto_session)
    params = _query_params(http.message.target)
    sid    = get(params, "sessionId", "")

    ch = lock(_SSE_SESSIONS_LOCK) do
        get(_SSE_SESSIONS, sid, nothing)
    end

    if ch === nothing
        HTTP.setstatus(http, 404)
        HTTP.startwrite(http)
        write(http, """{"error":"Session not found"}""")
        return
    end

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
    resp = _dispatch_mcp(pluto_session, msg; owner)
    isopen(ch) && resp !== nothing && put!(ch, JSON.json(resp))

    HTTP.setstatus(http, 202)
    HTTP.startwrite(http)
end

# ---------------------------------------------------------------------------
# HTTP/SSE MCP server
# ---------------------------------------------------------------------------

# `Host` as clients send it: `127.0.0.1:2346`, `localhost`, `[::1]:2346`.
function _loopback_host(host::AbstractString)
    name = if startswith(host, '[')
        i = findfirst(']', host)
        i === nothing ? "" : host[1:i]
    else
        first(split(host, ':'; limit=2))
    end
    return name == "127.0.0.1" || name == "localhost" || name == "[::1]"
end

function _refuse(http::HTTP.Stream, code::AbstractString)
    read(http)
    HTTP.setstatus(http, 403)
    HTTP.setheader(http, "Content-Type" => "application/json")
    HTTP.startwrite(http)
    write(http, """{"error":"$code"}""")
end

# `Authorization: Bearer <token>` matches the configured token, compared in
# constant time. Loopback isn't private on a shared machine: any local user can
# reach the port, so the token is what keeps them out.
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
        # Loopback control bridge, never a web API: no CORS, and any request that
        # carries an Origin header came from a browser page (cross-site fetches and
        # preflights always send one; MCP clients never do), so refuse it outright.
        # A DNS-rebinding page sends a same-origin GET with no Origin but a foreign
        # Host, so Host must name loopback too.
        if HTTP.header(http.message, "Origin", nothing) !== nothing
            _refuse(http, "browser_origin_refused")
            return
        elseif !_loopback_host(HTTP.header(http.message, "Host", ""))
            _refuse(http, "host_not_loopback")
            return
        end

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

        elseif method == "GET" && startswith(target, "/sse")
            _handle_sse(http)

        elseif method == "POST" && startswith(target, "/message")
            _handle_post(http, pluto_session)

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
            # App-only (not reachable through the agent's /message path).
            resp = if get(msg, "method", "") == "endeavor/set_policy"
                p = get(msg, "params", Dict{String,Any}())
                set_policy!(string(get(p, "owner", "")), string(get(p, "policy", "ask")))
                Dict("jsonrpc" => "2.0", "id" => get(msg, "id", nothing), "result" => Dict{String,Any}())
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
