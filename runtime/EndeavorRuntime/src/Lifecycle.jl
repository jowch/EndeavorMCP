# The Pluto session: starting and stopping Pluto and the bridge, and what
# Pluto's events tell the core.

const _STANDALONE_SESSION = Ref{Any}(nothing)
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
            "packages"    => package_step(nb),
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
            lock(() -> (delete!(_WATCHED, nb.notebook_id); delete!(_EXITED, nb.notebook_id); delete!(_INTERRUPTED, nb.notebook_id)), _NOTIFY_LOCK)
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
# The process an interrupt was sent to (`interrupt!`), which may end it.
const _INTERRUPTED = Dict{UUID,Any}()

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

"""
Interrupt the notebook's run, as Pluto's Stop does. A cell that doesn't stop
(one long `sleep`) is ended with its process, Pluto pressing Ctrl+C until it
does; that isn't the process ending by itself (`worker_exited!`), and the next
run starts a new one. While the notebook's process is still starting, its run
hasn't started a cell: it is told to skip them all once the process is up, as
it skips the cells queued after one Stop interrupts (#179: before, nothing was
interrupted, and every cell ran).
"""
function interrupt!(session, nb)
    task = get(Pluto.WorkspaceManager.active_workspaces, nb.notebook_id, nothing)
    if (task === nothing || !istaskdone(task)) && Pluto.will_run_code(nb) && any(_unfinished, nb.cells)
        # Pluto's run clears this when it ends.
        nb.wants_to_interrupt = true
        return Dict{String,Any}("interrupted" => true, "pending" => true)
    end
    (task === nothing || !istaskdone(task) || istaskfailed(task)) && return Dict{String,Any}("interrupted" => false)
    workspace = fetch(task)
    # As Pluto's Stop button (`interrupt_all`): once, and only while a cell runs. The cells
    # queued after it are skipped.
    (nb.wants_to_interrupt || isready(workspace.dowork_token) || !any(c -> c.running, nb.cells)) &&
        return Dict{String,Any}("interrupted" => false)
    nb.wants_to_interrupt = true
    lock(() -> _INTERRUPTED[nb.notebook_id] = workspace.worker, _NOTIFY_LOCK)
    interrupted = Pluto.WorkspaceManager.interrupt_workspace((session, nb); verbose=false)
    # Its process lived through it: a later end of it is a crash again.
    Pluto.Malt.isrunning(workspace.worker) &&
        lock(() -> get(_INTERRUPTED, nb.notebook_id, nothing) === workspace.worker && delete!(_INTERRUPTED, nb.notebook_id), _NOTIFY_LOCK)
    Dict{String,Any}("interrupted" => interrupted)
end

"The cells that were running when the notebook's process ended by itself; nothing while it has one."
function exited_cells(nb)
    nb.process_status === Pluto.ProcessStatus.no_process || return nothing
    lock(() -> get(_EXITED, nb.notebook_id, nothing), _NOTIFY_LOCK)
end

function worker_exited!(session, nb, workspace)::Nothing
    # Pluto marks a process it stops (shutdown, restart) discarded first.
    workspace.discarded && return
    # Ended by an interrupt: as after Pluto's Stop, the next run starts a new process.
    lock(() -> pop!(_INTERRUPTED, nb.notebook_id, nothing), _NOTIFY_LOCK) === workspace.worker && return
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
    _wrap_page_restart!()
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
Returns once both are listening. If the bridge can't start, stops Pluto and
throws. Idempotent when Pluto is already running.
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
        _STANDALONE_HTTP_SERVER[] = try
            _HTTP_BRIDGE_RUNNER[](sess, resolved_mcp; listenany=false)
        catch e
            stop_pluto_stack!()
            error("Couldn't start the bridge on port $resolved_mcp: $(sprint(showerror, e))")
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

# EndeavorRuntimeWarm: the precompile workload that saves a notebook's first
# open about 10 s, kept out of setup because it adds about 30 s to setup's
# precompile on Windows. boot.jl loads it when it's precompiled; when it isn't,
# this precompiles it in a separate, low-priority process once nothing has been
# opening or running for `quiet` seconds, so it doesn't slow a notebook the user
# is waiting for. The next start loads it.
const WARM = Base.PkgId(UUID("49c484a2-cf3f-4fbb-b7be-c7f520ff48f2"), "EndeavorRuntimeWarm")

# A notebook is starting its process, or a cell is queued or running.
function _runtime_busy(session)::Bool
    session === nothing && return false
    any(values(session.notebooks)) do nb
        nb.process_status in (Pluto.ProcessStatus.starting, Pluto.ProcessStatus.waiting_to_restart) ||
            any(c -> c.running || c.queued, nb.cells)
    end
end

# The command that precompiles `pkg` in `project`, below normal priority: `nice`
# where there is one, and on Windows the process lowers its own priority class,
# which the precompile processes it starts inherit.
function _warm_command(pkg::Base.PkgId, project::AbstractString)
    lower = Sys.iswindows() ?
        "ccall((:SetPriorityClass, \"kernel32\"), stdcall, Cint, (Ptr{Cvoid}, UInt32), ccall((:GetCurrentProcess, \"kernel32\"), stdcall, Ptr{Cvoid}, ()), 0x00004000); " : ""
    script = lower * "import Pkg; Pkg.precompile([$(repr(pkg.name))])"
    julia = `$(Base.julia_cmd()) --startup-file=no --project=$project -e $script`
    nice = Sys.iswindows() ? nothing : Sys.which("nice")
    cmd = nice === nothing ? julia : `$nice -n 10 $julia`
    # The project and the standard library, whatever load path this process was given.
    addenv(cmd, "JULIA_LOAD_PATH" => join(["@", "@stdlib"], Sys.iswindows() ? ';' : ':'))
end

# A precompile writes its image to temporary files (`jl_…`) in the package's cache folder and
# renames them when it's done, so one that was ended leaves them behind, tens of MB each. An
# hour old means no precompile is still writing them.
function _sweep_warm_temps(pkg::Base.PkgId)
    dir = Base.compilecache_dir(pkg)
    isdir(dir) || return
    for name in readdir(dir)
        path = joinpath(dir, name)
        # One that can't be removed (held by antivirus on Windows, say) is left for next time.
        try
            startswith(name, "jl_") && isfile(path) && time() - mtime(path) > 3600 && rm(path; force=true)
        catch
        end
    end
end

"""
Precompile `pkg` in the background once the runtime has been quiet for `quiet` seconds. The task.
The quiet starts when the runtime is ready, so where the app starts Julia ahead the warm-up can
run while the user is still choosing what to do, and a notebook opened then shares the computer
with it.
"""
function warm_up_later(pkg::Base.PkgId=WARM; project::AbstractString=dirname(Base.active_project()), quiet::Real=30.0, every::Real=5.0)
    errormonitor(@async begin
        quiet_since = time()
        while time() - quiet_since < quiet
            sleep(every)
            _runtime_busy(standalone_session()) && (quiet_since = time())
        end
        _sweep_warm_temps(pkg)
        @info "Precompiling $(pkg.name) in the background, for faster notebook opens from the next start"
        out = IOBuffer()
        began = time()
        proc = run(pipeline(ignorestatus(_warm_command(pkg, project)); stdout=out, stderr=out); wait=false)
        # Not left running when the runtime ends.
        atexit(() -> process_running(proc) && kill(proc))
        wait(proc)
        took = round(time() - began; digits=1)
        said = String(take!(out))
        tail = join(last(split(said, '\n'; keepempty=false), 20), '\n')
        if success(proc)
            # A workload that failed is a warning, not a failed precompile.
            occursin("Warning:", said) ? @warn("Precompiled $(pkg.name) in $took s, with warnings", output = tail) :
                @info("Precompiled $(pkg.name) in $took s")
        else
            @warn "Couldn't precompile $(pkg.name) ($(proc.exitcode)), so notebooks keep opening without it; trying again at the next start" output = tail
        end
        nothing
    end)
end
