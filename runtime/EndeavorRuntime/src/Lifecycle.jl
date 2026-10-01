# The Pluto session: starting and stopping Pluto and the bridge, and what
# Pluto's events tell the core.

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
# Bearer token the bridge requires on every request; empty = no check (tests).
const _BRIDGE_TOKEN = Ref("")
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
    token::AbstractString="",
)
    _BRIDGE_TOKEN[] = String(token)
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
    elseif event isa Pluto.FileSaveEvent
        _notify_notebook!("file_saved", event.notebook)
    elseif event isa Pluto.StateChangeEvent
        sess = standalone_session()
        sess === nothing || watch_worker!(sess, event.notebook)
        notify_state!(event.notebook)
    elseif event isa Pluto.NotebookExecutionDoneEvent
        _notify_notebook!("execution_done", event.notebook)
    elseif event isa Pluto.OpenNotebookEvent
        notify!("notebook_opened", Dict{String,Any}("notebook_id" => string(event.notebook.notebook_id), "path" => event.notebook.path))
    elseif event isa Pluto.ShutdownNotebookEvent
        # Also fired when a notebook restarts in place (safe preview's "Run
        # notebook code"); it stays in the session then and keeps its state.
        sess = standalone_session()
        nb = event.notebook
        if sess === nothing || !haskey(sess.notebooks, nb.notebook_id)
            forget_topology!(nb.notebook_id)
            lock(() -> (delete!(_WATCHED, nb.notebook_id); delete!(_EXITED, nb.notebook_id)), _NOTIFY_LOCK)
            _notify_notebook!("notebook_shut_down", nb)
        else
            notify_state!(nb)
        end
    end
    nothing
end

# A notebook's own process (Pluto's Malt worker) ending by itself. Pluto only
# notices when a call into it fails, so a process killed while idle, or by the
# OS while it runs, would leave the notebook "ready" and its cells running.
const _WATCHED = Dict{UUID,Any}()
# The cells that were running when a notebook's process ended by itself, until
# it gets a new one.
const _EXITED = Dict{UUID,Vector{String}}()

"Watch the notebook's process, once it has one and if no one watches it yet."
function watch_worker!(session, nb)::Nothing
    task = get(Pluto.WorkspaceManager.active_workspaces, nb.notebook_id, nothing)
    (task === nothing || !istaskdone(task) || istaskfailed(task)) && return
    workspace = fetch(task)
    worker = workspace.worker
    worker isa Pluto.Malt.Worker || return
    new = lock(_NOTIFY_LOCK) do
        get(_WATCHED, nb.notebook_id, nothing) === worker && return false
        _WATCHED[nb.notebook_id] = worker
        delete!(_EXITED, nb.notebook_id)
        true
    end
    # Not the logger of whichever Pluto task saw it first (a package
    # operation's tees into its own log).
    new && @async Base.CoreLogging.with_logger(Base.CoreLogging.global_logger()) do
        wait(worker.proc)
        worker_exited!(session, nb, workspace)
    end
    return nothing
end

"The cells that were running when the notebook's process ended by itself; nothing while it has one."
function exited_cells(nb)
    nb.process_status === Pluto.ProcessStatus.no_process || return nothing
    lock(() -> get(_EXITED, nb.notebook_id, nothing), _NOTIFY_LOCK)
end

function worker_exited!(session, nb, workspace)::Nothing
    # Pluto marks a process it stops (shutdown, restart) discarded first.
    workspace.discarded && return
    get(session.notebooks, nb.notebook_id, nothing) === nb || return
    running = [string(id) for id in nb.cell_order if nb.cells_dict[id].running]
    lock(() -> _EXITED[nb.notebook_id] = running, _NOTIFY_LOCK)
    nb.process_status = Pluto.ProcessStatus.no_process
    for cell in nb.cells
        cell.running = false
        cell.queued = false
    end
    @warn "A notebook's Julia process ended by itself" nb.path running
    notify!("process_exited", Dict{String,Any}("notebook_id" => string(nb.notebook_id), "running" => running))
    _notify_browser(session, nb)
    notify_state!(nb)
    return nothing
end


function _init_pluto_session!(; pluto_port, launch_browser, require_secret_for_access, notebook)
    opts = Pluto.Configuration.from_flat_kwargs(
        port                      = pluto_port,
        launch_browser            = launch_browser,
        require_secret_for_access = require_secret_for_access,
        # Pluto's "Ask AI" and "Fix with AI"; the app adds its own agent's buttons.
        enable_ai_editor_features = false,
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
    # Closing the server waits for open connections; event streams never end on
    # their own, so end them first.
    close_notification_streams!()
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
    return session_status_dict()
end

# New notebooks start unsaved in Pluto's scratch folder; this is the folder its
# "Save notebook" box suggests instead (the page reads it on its next load).
function set_folder!(session, path::AbstractString)
    (session === nothing || isempty(path)) && return false
    session.options.server.notebook_path_suggestion = joinpath(path, "")
    return true
end

# How `endeavor/shutdown` ends the process; tests replace it.
const _SHUTDOWN = Ref{Function}(() -> exit(0))

"""Test helper: bind an in-memory session as the standalone Pluto session."""
function bind_standalone_session!(sess)
    _STANDALONE_SESSION[] = sess
end
