# D15 — deferred Pluto session lifecycle (standalone connect + lifecycle MCP tools).

const LIFECYCLE_TOOLS = Set([
    "pluto_session_status",
    "open_notebook",
    "new_notebook",
    "allow_execution",
])

is_lifecycle_tool(name::AbstractString) = name in LIFECYCLE_TOOLS

const _STANDALONE_SESSION = Ref{Any}(nothing)
const _STANDALONE_HTTP_TASK = Ref{Union{Nothing,Task}}(nothing)
const _STANDALONE_HTTP_SERVER = Ref{Any}(nothing)
const _STANDALONE_PLUTO_SERVER = Ref{Any}(nothing)
const _STANDALONE_PLUTO_TASK = Ref{Union{Nothing,Task}}(nothing)
const _STANDALONE_PLUTO_PORT = Ref{Union{Nothing,Int}}(1234)
const _STANDALONE_MCP_PORT = Ref{Union{Nothing,Int}}(2346)
const _STANDALONE_PLUTO_PORT_HINT = Ref(1234)
const _STANDALONE_MCP_PORT_HINT = Ref(2346)
const _STANDALONE_REQUIRE_SECRET = Ref(true)
const _HTTP_BRIDGE_RUNNER = Ref{Function}(
    (session, port; kwargs...) -> error("HTTP bridge not registered"),
)

function register_http_bridge!(f::Function)
    _HTTP_BRIDGE_RUNNER[] = f
end

function configure_standalone!(;
    pluto_port=1234,
    mcp_port=2346,
    pluto_port_hint=nothing,
    mcp_port_hint=nothing,
    require_secret_for_access=true,
)
    hint_pluto = pluto_port_hint === nothing ? Int(pluto_port) : Int(pluto_port_hint)
    hint_mcp = mcp_port_hint === nothing ? Int(mcp_port) : Int(mcp_port_hint)
    _STANDALONE_PLUTO_PORT_HINT[] = hint_pluto
    _STANDALONE_MCP_PORT_HINT[] = hint_mcp
    _STANDALONE_PLUTO_PORT[] = Int(pluto_port)
    _STANDALONE_MCP_PORT[] = Int(mcp_port)
    _STANDALONE_REQUIRE_SECRET[] = require_secret_for_access
end

function standalone_session()
    _STANDALONE_SESSION[]
end

function pluto_running_standalone()
    _STANDALONE_SESSION[] !== nothing
end

function _notebook_summaries(session)
    [
        Dict{String,Any}(
            "notebook_id" => string(nb.notebook_id),
            "path"        => nb.path,
            "cell_count"  => length(nb.cell_order),
        )
        for nb in values(session.notebooks)
    ]
end

function session_status_dict()
    sess = _STANDALONE_SESSION[]
    pluto_port = _STANDALONE_PLUTO_PORT[]
    mcp_port = _STANDALONE_MCP_PORT[]
    pluto = sess === nothing ? "stopped" : "running"
    Dict{String,Any}(
        "pluto"      => pluto,
        "pluto_port" => pluto_port,
        "mcp_port"   => mcp_port,
        "notebooks"  => sess === nothing ? [] : _notebook_summaries(sess),
        "managed"    => false,
        "session_id" => nothing,
        "mcp_url"    => mcp_port === nothing ? nothing : "http://127.0.0.1:$mcp_port",
        "pluto_url"  =>
            (pluto == "running" && pluto_port !== nothing) ?
            "http://127.0.0.1:$pluto_port" : nothing,
    )
end

function _handle_pluto_event(event)::Nothing
    if event isa Pluto.ServerStartEvent
        _STANDALONE_PLUTO_PORT[] = Int(event.port)
    end
    nothing
end

function _init_pluto_session!(; pluto_port, launch_browser, require_secret_for_access, notebook)
    opts = Pluto.Configuration.from_flat_kwargs(
        port                      = pluto_port,
        launch_browser            = launch_browser,
        require_secret_for_access = require_secret_for_access,
        on_event                  = _handle_pluto_event,
    )
    sess = Pluto.ServerSession(; options = opts)
    if notebook !== nothing
        Pluto.SessionActions.open(sess, String(notebook); run_async = true)
    end
    _STANDALONE_PLUTO_TASK[] = @async begin
        try
            server = Pluto.run!(sess)
            _STANDALONE_PLUTO_SERVER[] = server
            wait(server)
        catch e
            isa(e, InterruptException) || @error "Pluto server error" exception=(e, catch_backtrace())
        finally
            _STANDALONE_PLUTO_SERVER[] = nothing
        end
    end
    deadline = time() + 30.0
    while time() < deadline
        _STANDALONE_PLUTO_SERVER[] !== nothing && break
        sleep(0.05)
    end
    _STANDALONE_PLUTO_SERVER[] === nothing &&
        error("Pluto failed to start within 30s (port=$pluto_port)")
    sess
end

"""
    start_pluto_stack!(; pluto_port, mcp_port, require_secret_for_access, launch_browser, notebook, http_async)

Start Pluto.run! and, when no HTTP bridge is already up, the MCP HTTP bridge.
Idempotent when Pluto is already running.
"""
function start_pluto_stack!(;
    pluto_port::Union{Int,Nothing} = nothing,
    mcp_port::Union{Int,Nothing} = nothing,
    require_secret_for_access::Bool = _STANDALONE_REQUIRE_SECRET[],
    launch_browser::Bool = false,
    notebook = nothing,
    http_async::Bool = true,
)
    if _STANDALONE_SESSION[] !== nothing
        return session_status_dict()
    end

    resolved_pluto = pluto_port === nothing ? _STANDALONE_PLUTO_PORT_HINT[] : pluto_port
    resolved_mcp = mcp_port === nothing ? _STANDALONE_MCP_PORT_HINT[] : mcp_port
    _STANDALONE_PLUTO_PORT[] = resolved_pluto
    _STANDALONE_MCP_PORT[] = resolved_mcp
    _STANDALONE_REQUIRE_SECRET[] = require_secret_for_access

    sess = _init_pluto_session!(;
        pluto_port = resolved_pluto,
        launch_browser,
        require_secret_for_access,
        notebook,
    )
    _STANDALONE_SESSION[] = sess

    if http_async && _STANDALONE_HTTP_SERVER[] === nothing
        _STANDALONE_HTTP_TASK[] = @async begin
            try
                http_server = _HTTP_BRIDGE_RUNNER[](sess, resolved_mcp; listenany=false)
                _STANDALONE_HTTP_SERVER[] = http_server
                wait(http_server)
            catch e
                isa(e, InterruptException) || rethrow()
            finally
                _STANDALONE_HTTP_SERVER[] = nothing
            end
        end
    end

    return session_status_dict()
end

function _close_standalone_http!()
    http_server = _STANDALONE_HTTP_SERVER[]
    if http_server !== nothing
        try
            close(http_server)
        catch
        end
        _STANDALONE_HTTP_SERVER[] = nothing
    end
    t = _STANDALONE_HTTP_TASK[]
    if t !== nothing && t !== current_task()
        try
            schedule(t, InterruptException(); error=true)
        catch
        end
        try
            wait(t)
        catch
        end
    end
    _STANDALONE_HTTP_TASK[] = nothing
end

function _close_standalone_pluto!()
    pluto_server = _STANDALONE_PLUTO_SERVER[]
    if pluto_server !== nothing
        try
            close(pluto_server)
        catch
        end
        _STANDALONE_PLUTO_SERVER[] = nothing
    end
    t = _STANDALONE_PLUTO_TASK[]
    if t !== nothing && t !== current_task()
        try
            wait(t)
        catch
        end
    end
    _STANDALONE_PLUTO_TASK[] = nothing
end

"""
    stop_pluto_stack!(; close_control_bridge=true)

Shut down Pluto notebooks and the Pluto server, and (by default) the HTTP/SSE bridge.
"""
function stop_pluto_stack!(; close_control_bridge::Bool = true)
    sess = _STANDALONE_SESSION[]
    if sess !== nothing
        for nb in collect(values(sess.notebooks))
            try
                Pluto.SessionActions.shutdown(sess, nb; async = false, verbose = false)
            catch
            end
        end
    end
    _STANDALONE_SESSION[] = nothing
    _close_standalone_pluto!()
    close_control_bridge && _close_standalone_http!()
    reset_staging_state!()
    return session_status_dict()
end

function require_standalone_session!()
    sess = _STANDALONE_SESSION[]
    sess === nothing &&
        throw(ArgumentError("pluto_not_running::Pluto is not running yet."))
    return sess
end

function _lifecycle_get_notebook!(session, notebook_id_str)
    nid = try
        UUID(notebook_id_str)
    catch
        throw(ArgumentError("invalid_notebook_id::Invalid notebook ID: '$notebook_id_str'"))
    end
    nb = get(session.notebooks, nid, nothing)
    nb === nothing &&
        throw(KeyError("notebook_not_found::No notebook with id '$notebook_id_str' in the current session"))
    return nb
end

function _lifecycle_notify_browser(session, notebook)
    try
        Pluto.send_notebook_changes!(Pluto.ClientRequest(; session, notebook))
    catch
    end
end

"""
    allow_notebook_execution!(session, notebook; run_async=true, run_cells=true)

Programmatic equivalent of Glass **Run notebook code** for safe-preview notebooks
(local paths only). Mirrors Pluto `restart_process` when `run_cells=true`.

When `run_cells=false`, exits safe preview without queuing a full notebook run
(workspace starts lazily on the next `submit_changes` / `update_save_run!`).
"""
function allow_notebook_execution!(session, notebook; run_async::Bool=true, run_cells::Bool=true)
    ps = notebook.process_status
    if ps === Pluto.ProcessStatus.ready
        return Dict{String,Any}(
            "notebook_id"       => string(notebook.notebook_id),
            "execution_allowed" => true,
            "already_allowed"   => true,
            "ran"               => false,
            "process_status"    => string(ps),
        )
    end
    if ps !== Pluto.ProcessStatus.waiting_for_permission
        throw(ArgumentError(
            "execution_not_gated::Notebook is not in safe preview (process_status=$ps)",
        ))
    end
    if haskey(notebook.metadata, "risky_file_source")
        throw(ArgumentError(
            "risky_source::Cannot allow execution for risky remote sources via MCP; use Glass UI",
        ))
    end

    notebook.process_status = Pluto.ProcessStatus.waiting_to_restart
    session.options.evaluation.run_notebook_on_load &&
        Pluto._report_business_cells_planned!(notebook)
    _lifecycle_notify_browser(session, notebook)

    Pluto.SessionActions.shutdown(session, notebook; keep_in_session=true, async=true, verbose=false)

    if run_cells
        notebook.process_status = Pluto.ProcessStatus.starting
        _lifecycle_notify_browser(session, notebook)
        # Run through _run_cells! so edits staged during safe preview (pending_run)
        # clear once their cells complete. Non-blocking by default: sync_nbpkg +
        # reactive run stay off the MCP thread.
        _run_cells!(session, notebook, collect(notebook.cells); wait_for_completion=!run_async)
        _lifecycle_notify_browser(session, notebook)
        ran = true
    else
        # Exit the gate without a full run; next mutation starts the workspace.
        notebook.process_status = Pluto.ProcessStatus.ready
        _lifecycle_notify_browser(session, notebook)
        ran = false
    end

    Dict{String,Any}(
        "notebook_id"       => string(notebook.notebook_id),
        "execution_allowed" => true,
        "already_allowed"   => false,
        "ran"               => ran,
        "process_status"    => string(notebook.process_status),
    )
end

# ---------------------------------------------------------------------------
# Lifecycle tool implementations
# ---------------------------------------------------------------------------

function tool_pluto_session_status(_args)
    session_status_dict()
end

function tool_open_notebook(args)
    sess = require_standalone_session!()
    path = get(args, "path", nothing)
    path === nothing && throw(ArgumentError("invalid_path::path is required"))
    path = String(path)
    ispath(path) || throw(ArgumentError("file_not_found::No file at '$path'"))
    run_nb = get(args, "run_notebook", false)

    # SessionActions.open already queues update_save_run! when execution_allowed;
    # do not call tool_run_all_cells again (double-run starved MCP / raced executetoken).
    nb = Pluto.SessionActions.open(sess, path; run_async = true, execution_allowed = run_nb)

    result = Dict{String,Any}(
        "notebook_id"         => string(nb.notebook_id),
        "path"                => nb.path,
        "execution_allowed"   => run_nb,
        "ran"                 => run_nb,
        "process_status"      => string(nb.process_status),
    )
    if run_nb
        result["warnings"] = String[
            "async_execution::open queued non-blocking notebook run; poll read_cell for completion",
        ]
    end
    return result
end

function tool_new_notebook(args)
    require_standalone_session!()
    requested = get(args, "path", nothing)
    nb = if requested === nothing
        # Pluto's own naming in its new-notebooks directory, like "Create a new notebook".
        Pluto.emptynotebook()
    else
        path = abspath(expanduser(String(requested)))
        endswith(path, ".jl") ||
            throw(ArgumentError("invalid_path::Notebook path must end in .jl: '$path'"))
        ispath(path) &&
            throw(ArgumentError("file_exists::'$path' already exists; use open_notebook to load it"))
        isdir(dirname(path)) ||
            throw(ArgumentError("invalid_path::Directory does not exist: '$(dirname(path))'"))
        Pluto.emptynotebook(path)
    end
    # Pluto serializes the file (never a hand-written header), then the normal open
    # path loads it: same safe preview as open_notebook.
    Pluto.save_notebook(nb, nb.path)
    result = tool_open_notebook(Dict{String,Any}("path" => nb.path))
    result["created"] = true
    return result
end

function tool_allow_execution(args)
    sess = require_standalone_session!()
    notebook_id = get(args, "notebook_id", nothing)
    notebook_id === nothing &&
        throw(ArgumentError("invalid_notebook_id::notebook_id is required"))
    nb = _lifecycle_get_notebook!(sess, String(notebook_id))
    run_cells = get(args, "run_notebook", true)
    # Single path: allow_notebook_execution! already queues the run when requested.
    # A follow-up tool_run_all_cells was a double-run footgun on the stdio thread.
    result = allow_notebook_execution!(sess, nb; run_async=true, run_cells=run_cells)
    if get(result, "ran", false)
        result["run_warnings"] = String[
            "async_execution::allow_execution queued non-blocking notebook run; poll read_cell for completion",
        ]
    end
    result["process_status"] = string(nb.process_status)
    return result
end

function call_lifecycle_tool(name::AbstractString, arguments)
    if name == "pluto_session_status"
        tool_pluto_session_status(arguments)
    elseif name == "open_notebook"
        tool_open_notebook(arguments)
    elseif name == "new_notebook"
        tool_new_notebook(arguments)
    elseif name == "allow_execution"
        tool_allow_execution(arguments)
    else
        throw(ArgumentError("unknown_tool::Unknown lifecycle tool: '$name'"))
    end
end

function call_tool_with_session(session, name::AbstractString, arguments)
    if is_lifecycle_tool(name)
        return call_lifecycle_tool(name, arguments)
    end
    sess = session === nothing ? standalone_session() : session
    sess === nothing &&
        throw(ArgumentError("pluto_not_running::Pluto is not running yet."))
    return call_tool(sess, name, arguments)
end

"""Test helper: bind an in-memory session as the standalone Pluto session."""
function bind_standalone_session!(sess)
    _STANDALONE_SESSION[] = sess
end
