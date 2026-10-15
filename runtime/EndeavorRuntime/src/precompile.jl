# A cold first open spent about 25 s compiling Pluto's open-and-run path and ours
# (#16). This workload runs that path once while the package precompiles, so the
# code it compiles is cached with the package: a new notebook's first run in its
# own worker process, an edit and a run, and the reads an agent makes, through
# the same `adapter_call` the bridge uses. A first open still starts a worker
# process (about 7 s) and compiles what the workload can't reach.
#
# It must leave nothing running, or the precompile waits forever: the session's
# timer and the worker's output pipes are closed by hand. A workload that fails
# only costs the speedup, so its error is a warning, not a failed precompile.
using PrecompileTools: @setup_workload, @compile_workload

@setup_workload begin
    folder = mktempdir()
    @compile_workload begin
        session = Pluto.ServerSession(; options = Pluto.Configuration.from_flat_kwargs(
            launch_browser = false,
            enable_ai_editor_features = false,
            on_event = _handle_pluto_event,
        ))
        worker = nothing
        try
            call(method, params) = adapter_call(session, method, params)
            # Not `open`: it first renders text cells in this process, which can't run
            # while precompiling. The notebook is added as `open` adds it, and its first
            # run starts its worker process.
            nb = Pluto.emptynotebook(joinpath(folder, "workload.jl"))
            Pluto.save_notebook(nb, nb.path)
            session.notebooks[nb.notebook_id] = nb
            id = string(nb.notebook_id)
            cells = [string(c.cell_id) for c in nb.cells]
            call("run", Dict{String,Any}("notebook_id" => id, "cells" => cells, "wait" => true, "timeout" => 120.0))
            worker = fetch(Pluto.WorkspaceManager.active_workspaces[nb.notebook_id]).worker
            set = Dict{String,Any}("op" => "set_code", "cell_id" => first(cells), "code" => "x = [1, 2, 3] .* 2")
            call("apply", Dict{String,Any}("notebook_id" => id, "ops" => [set]))
            call("run", Dict{String,Any}("notebook_id" => id, "cells" => cells[1:1], "wait" => true, "timeout" => 120.0))
            call("snapshot", Dict{String,Any}())
            call("snapshot", Dict{String,Any}("notebook_id" => id))
            call("graph", Dict{String,Any}("notebook_id" => id, "edges" => true, "packages" => true))
            call("render_text", Dict{String,Any}("notebook_id" => id, "cell_id" => first(cells)))
            call("shutdown", Dict{String,Any}("notebook_id" => id))
        catch e
            @warn "EndeavorRuntime's precompile workload failed, so a first open will compile more" exception = e
        finally
            # Malt leaves an ended worker's output pipes open.
            if worker isa Pluto.Malt.Worker
                Pluto.Malt.stop(worker)
                close(worker.stdout)
                close(worker.stderr)
            end
            close_notification_streams!()
            finalize(session)
        end
    end
    rm(folder; recursive = true, force = true)
end
