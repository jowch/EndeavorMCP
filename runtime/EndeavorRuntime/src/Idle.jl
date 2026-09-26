# Open notebooks stop after a stretch with no activity: no tool call on them, no
# save or run through Pluto, no cells running. The app sets the limit
# (`endeavor/set_idle_limit`, 0 = never); `keep_notebook_alive` exempts one.

const IDLE_CHECK_SECONDS = 300

const _IDLE_LOCK = ReentrantLock()
const _IDLE_LIMIT_HOURS = Ref(48.0)
const _IDLE_CLOCK = Ref{Function}(time)
const _LAST_ACTIVE = Dict{UUID,Float64}()
const _KEPT_ALIVE = Set{UUID}()
# Notebooks the idle check stopped, by canonical path, until they open again:
# the app reads these from the event stream to say why a notebook stopped.
const _IDLE_STOPPED = Dict{String,Dict{String,Any}}()

set_idle_limit!(hours::Real) = (_IDLE_LIMIT_HOURS[] = Float64(hours); nothing)

note_activity!(notebook_id::UUID) = lock(() -> (_LAST_ACTIVE[notebook_id] = _IDLE_CLOCK[](); nothing), _IDLE_LOCK)

function note_activity!(args)
    nid = tryparse(UUID, string(get(args, "notebook_id", "")))
    nid === nothing || note_activity!(nid)
    return nothing
end

function note_opened!(notebook)
    note_activity!(notebook.notebook_id)
    lock(() -> delete!(_IDLE_STOPPED, _canonical_path(notebook.path)), _IDLE_LOCK)
    return nothing
end

function note_shut_down!(notebook_id::UUID)
    lock(_IDLE_LOCK) do
        delete!(_LAST_ACTIVE, notebook_id)
        delete!(_KEPT_ALIVE, notebook_id)
    end
    return nothing
end

idle_stopped() = lock(() -> collect(values(_IDLE_STOPPED)), _IDLE_LOCK)

"""
    stop_idle_notebooks!(session)

Stop every notebook idle longer than the limit, except kept-alive ones and ones
with cells running (which count as activity). Returns the stopped paths.
"""
function stop_idle_notebooks!(session)
    hours = _IDLE_LIMIT_HOURS[]
    (session === nothing || hours <= 0) && return String[]
    now = _IDLE_CLOCK[]()
    idle = lock(_IDLE_LOCK) do
        filter(collect(values(session.notebooks))) do nb
            nb.notebook_id in _KEPT_ALIVE && return false
            if any(c -> c.running || c.queued, nb.cells)
                _LAST_ACTIVE[nb.notebook_id] = now
                return false
            end
            now - get!(_LAST_ACTIVE, nb.notebook_id, now) >= hours * 3600
        end
    end
    for nb in idle
        # Recorded first, so the publish inside stop_notebook! carries it.
        lock(_IDLE_LOCK) do
            _IDLE_STOPPED[_canonical_path(nb.path)] = Dict{String,Any}(
                "path"         => nb.path,
                "hours"        => round(Int, hours),
                "safe_preview" => nb.process_status === Pluto.ProcessStatus.waiting_for_permission,
            )
        end
        stop_notebook!(session, nb.path)
        @info "Stopped $(nb.path) after $(round(Int, hours)) hours idle"
    end
    return [nb.path for nb in idle]
end

function start_idle_checks!(session)
    @async while standalone_session() === session
        sleep(IDLE_CHECK_SECONDS)
        standalone_session() === session || break
        try
            stop_idle_notebooks!(session)
        catch e
            @warn "Idle check failed" exception = (e, catch_backtrace())
        end
    end
    return nothing
end

function tool_keep_notebook_alive(session, args)
    nb = _get_notebook(session, get(args, "notebook_id", ""))
    keep = get(args, "keep", nothing)
    keep isa Bool || throw(ArgumentError("invalid_keep::keep must be true or false"))
    lock(_IDLE_LOCK) do
        keep ? push!(_KEPT_ALIVE, nb.notebook_id) : delete!(_KEPT_ALIVE, nb.notebook_id)
        _LAST_ACTIVE[nb.notebook_id] = _IDLE_CLOCK[]()
    end
    return Dict{String,Any}("notebook_id" => string(nb.notebook_id), "kept_alive" => keep)
end
