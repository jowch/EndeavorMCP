# A cold first open spends about 25 s compiling Pluto's open-and-run path and
# EndeavorRuntime's (#16). This package's precompile workload runs that path
# once, so the code it compiles is cached with this package: a new notebook's
# first run in its own worker process, an edit and a run, and the reads an agent
# makes, through the same `adapter_call` the bridge uses. A first open still
# starts a worker process and compiles what the workload can't reach.
#
# It is a package of its own so that setup doesn't wait for it: on Windows the
# workload adds about 30 s to setup's precompile. Setup precompiles
# EndeavorRuntime alone; once the runtime has been quiet for a while it
# precompiles this one in a low-priority process of its own
# (`EndeavorRuntime.warm_up_later`), and the next start loads it if it's ready.
#
# A workload that fails only costs the speedup, so its error is a warning, not a
# failed precompile. Whatever happens it must leave nothing running, or the
# precompile waits forever: the notebook is shut down, any worker process still
# running ended, the pipes it opened closed, the session's timer closed, and
# Pluto's throttle timers left to run out.
module EndeavorRuntimeWarm

using EndeavorRuntime
using Pluto
using PrecompileTools: @setup_workload, @compile_workload

# The pipes being read now: Base keeps each one it reads from in `uvhandles`.
_read_pipes() = Base.@lock Base.preserve_handle_lock Set{Any}(h for h in keys(Base.uvhandles) if h isa Base.PipeEndpoint)

# Shut the workload's notebook down and close everything it opened (see above).
function _end_workload!(session, nb, procs_before, pipes_before)
    try
        haskey(session.notebooks, nb.notebook_id) && Pluto.SessionActions.shutdown(session, nb; async = false, verbose = false)
    catch e
        @warn "Couldn't shut down the precompile workload's notebook" exception = e
    end
    # A worker process still running, one that never connected say, is ended.
    for proc in Base.@lock(Pluto.Malt.procs_lock, setdiff(Pluto.Malt.__iNtErNaL_running_procs, procs_before))
        process_running(proc) && kill(proc)
        timedwait(() -> !process_running(proc), 10.0) === :ok || kill(proc, Base.SIGKILL)
    end
    # Malt leaves a worker's output pipes open after it ends, and those of one that never
    # connected have no owner left to close them.
    foreach(close, setdiff(_read_pipes(), pipes_before))
    # One not being read is closed when it is collected.
    GC.gc()
    EndeavorRuntime.close_notification_streams!()
    finalize(session)
    # Pluto's throttled updates keep a timer for a moment after their last call.
    sleep(2.0)
end

@setup_workload begin
    folder = mktempdir()
    @compile_workload begin
        session = Pluto.ServerSession(; options = Pluto.Configuration.from_flat_kwargs(
            launch_browser = false,
            enable_ai_editor_features = false,
            on_event = EndeavorRuntime._handle_pluto_event,
        ))
        # Not `open`: it first renders text cells in this process, which can't run
        # while precompiling. The notebook is added as `open` adds it, and its first
        # run starts its worker process. Pluto's package manager can't run while
        # precompiling either: a cell that names `Pkg.activate` (and never calls it)
        # turns it off for this notebook, as it does for any notebook.
        nb = Pluto.Notebook([Pluto.Cell("x = 1 + 1"), Pluto.Cell("false && Pkg.activate()")], joinpath(folder, "workload.jl"))
        procs = Base.@lock Pluto.Malt.procs_lock copy(Pluto.Malt.__iNtErNaL_running_procs)
        pipes = _read_pipes()
        try
            Pluto.save_notebook(nb, nb.path)
            session.notebooks[nb.notebook_id] = nb
            call(method, params) = EndeavorRuntime.adapter_call(session, method, params)
            id = string(nb.notebook_id)
            cells = [string(c.cell_id) for c in nb.cells]
            ran = call("run", Dict{String,Any}("notebook_id" => id, "cells" => cells, "wait" => true, "timeout" => 120.0))
            isempty(ran["timed_out"]) || error("the first run took over 120 s")
            set = Dict{String,Any}("op" => "set_code", "cell_id" => first(cells), "code" => "x = [1, 2, 3] .* 2")
            call("apply", Dict{String,Any}("notebook_id" => id, "ops" => [set]))
            call("run", Dict{String,Any}("notebook_id" => id, "cells" => cells[1:1], "wait" => true, "timeout" => 120.0))
            call("snapshot", Dict{String,Any}())
            call("snapshot", Dict{String,Any}("notebook_id" => id))
            call("graph", Dict{String,Any}("notebook_id" => id, "edges" => true, "packages" => true))
            call("render_text", Dict{String,Any}("notebook_id" => id, "cell_id" => first(cells)))
        catch e
            @warn "EndeavorRuntimeWarm's precompile workload failed, so a first open will compile more" exception = e
        finally
            _end_workload!(session, nb, procs, pipes)
        end
    end
    rm(folder; recursive = true, force = true)
end

end
