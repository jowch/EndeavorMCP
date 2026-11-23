module EndeavorRuntimeWarm

using EndeavorRuntime
using Pluto
using PrecompileTools: @setup_workload, @compile_workload

_read_pipes() = Base.@lock Base.preserve_handle_lock Set{Any}(h for h in keys(Base.uvhandles) if h isa Base.PipeEndpoint)

function _end_workload!(session, nb, procs_before, pipes_before)
    try
        haskey(session.notebooks, nb.notebook_id) && Pluto.SessionActions.shutdown(session, nb; async = false, verbose = false)
    catch e
        @warn "Couldn't shut down the precompile workload's notebook" exception = e
    end
    for proc in Base.@lock(Pluto.Malt.procs_lock, setdiff(Pluto.Malt.__iNtErNaL_running_procs, procs_before))
        process_running(proc) && kill(proc)
        timedwait(() -> !process_running(proc), 10.0) === :ok || kill(proc, Base.SIGKILL)
    end
    foreach(close, setdiff(_read_pipes(), pipes_before))
    GC.gc()
    EndeavorRuntime.close_notification_streams!()
    finalize(session)
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
