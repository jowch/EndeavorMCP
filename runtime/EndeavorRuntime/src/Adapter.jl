# The Pluto adapter's side of the engine interface (docs/runtime-core.md). The
# core (`endeavor-remote core`) calls `POST /adapter` with {"method", "params"}
# and reads the reply's "result" or "error". The methods (see `adapter_call`):
# status, open, new, and for a notebook_id snapshot, graph, shutdown, apply,
# run, interrupt, restart, allow_execution, move, render_png and validate. What happens in
# Pluto reaches the core as notifications on `GET /notifications`, one
# `data: {"method", "params"}` line each: notebook_opened, notebook_shut_down
# (it left the session), file_saved, execution_done, cell_state (every cell's
# code and run state: Pluto's hook doesn't say which cell changed),
# topology_changed, and run_finished (the cells an unwaited run finished).

const _NOTIFY_LOCK = ReentrantLock()
const _NOTIFY_SUBSCRIBERS = Set{Channel{String}}()
# Each notebook's dependency graph as last announced.
const _TOPOLOGY_SENT = Dict{UUID,Any}()

function notify!(method::AbstractString, params::AbstractDict)::Nothing
    json = JSON.json(Dict("method" => method, "params" => params))
    lock(() -> foreach(ch -> put!(ch, json), _NOTIFY_SUBSCRIBERS), _NOTIFY_LOCK)
    return nothing
end

_notify_notebook!(method, nb) = notify!(method, Dict{String,Any}("notebook_id" => string(nb.notebook_id)))

function notify_state!(nb)::Nothing
    notify!("cell_state", Dict{String,Any}(
        "notebook_id" => string(nb.notebook_id),
        "cells" => [Dict{String,Any}(
            "cell_id" => string(c.cell_id), "code" => c.code,
            "running" => c.running, "queued" => c.queued, "errored" => c.errored,
        ) for c in nb.cells],
    ))
    changed = lock(_NOTIFY_LOCK) do
        get(_TOPOLOGY_SENT, nb.notebook_id, nothing) === nb.topology && return false
        _TOPOLOGY_SENT[nb.notebook_id] = nb.topology
        true
    end
    changed && _notify_notebook!("topology_changed", nb)
    return nothing
end

function forget_topology!(notebook_id::UUID)
    lock(() -> delete!(_TOPOLOGY_SENT, notebook_id), _NOTIFY_LOCK)
    return nothing
end

"End every open notification stream (e.g. when the bridge stops)."
function close_notification_streams!()::Nothing
    lock(_NOTIFY_LOCK) do
        foreach(close, _NOTIFY_SUBSCRIBERS)
        empty!(_NOTIFY_SUBSCRIBERS)
        empty!(_TOPOLOGY_SENT)
    end
    return nothing
end

function _handle_notifications(http::HTTP.Stream)
    read(http)
    ch = Channel{String}(Inf)
    try
        lock(() -> push!(_NOTIFY_SUBSCRIBERS, ch), _NOTIFY_LOCK)
        HTTP.setstatus(http, 200)
        HTTP.setheader(http, "Content-Type" => "text/event-stream")
        HTTP.setheader(http, "Cache-Control" => "no-cache")
        HTTP.startwrite(http)
        for json in ch
            write(http, "data: $json\n\n")
        end
    catch e
        e isa Base.IOError || e isa EOFError || e isa InvalidStateException || rethrow()
    finally
        lock(() -> delete!(_NOTIFY_SUBSCRIBERS, ch), _NOTIFY_LOCK)
        close(ch)
    end
end

function _get_notebook(session, notebook_id_str)
    nid = try
        UUID(notebook_id_str)
    catch
        throw(ArgumentError("invalid_notebook_id::Invalid notebook ID: '$notebook_id_str'"))
    end
    nb = get(session.notebooks, nid, nothing)
    nb === nothing && throw(KeyError("notebook_not_found::No notebook with id '$notebook_id_str' in the current session"))
    return nb
end

function _notify_browser(session, notebook)
    try
        Pluto.send_notebook_changes!(Pluto.ClientRequest(; session, notebook))
    catch
        # No page open is fine.
    end
end

# Each cell in turn until it has finished, for up to `timeout` seconds each:
# the ids that finished and those that didn't.
function _wait_cells!(cells; timeout)
    completed = UUID[]
    timed_out = UUID[]
    for cell in cells
        t = time()
        while (cell.running || cell.queued) && time() - t <= timeout
            sleep(0.05)
        end
        push!(cell.running || cell.queued ? timed_out : completed, cell.cell_id)
    end
    return completed, timed_out
end

const _PLUTO_PROJECT_TOML_CELL_ID = UUID("00000000-0000-0000-0000-000000000001")
const _PLUTO_MANIFEST_TOML_CELL_ID = UUID("00000000-0000-0000-0000-000000000002")

_is_manifest_cell(cell_id::UUID) = cell_id == _PLUTO_PROJECT_TOML_CELL_ID || cell_id == _PLUTO_MANIFEST_TOML_CELL_ID
_is_fake_bind_shim(cell) = occursin(Pluto.PlutoRunner.fake_bind, cell.code) || startswith(lstrip(cell.code), "macro bind")
_is_markdown_cell(cell) = startswith(lstrip(cell.code), "md\"")

function _parse_validation_errors(nb::Pluto.Notebook, cell::Pluto.Cell, code::String)
    errors = Dict{String,Any}[]
    if !Pluto.is_single_expression(code)
        push!(errors, Dict{String,Any}("type" => "pluto_multi_expression", "message" => "Cell must contain a single expression"))
    end
    expr = Pluto.parse_custom(nb, Pluto.Cell(; cell_id=cell.cell_id, code=code))
    if Meta.isexpr(expr, :toplevel, 2) && Meta.isexpr(expr.args[2], :call, 2) && expr.args[2].args[1] == :(PlutoRunner.throw_syntax_error)
        push!(errors, Dict{String,Any}("type" => "syntax_error", "message" => string(expr.args[2].args[2])))
    end
    return errors
end

function _snapshot_cell(cell)
    d = Dict{String,Any}(
        "cell_id"  => string(cell.cell_id),
        "code"     => cell.code,
        "folded"   => cell.code_folded,
        "running"  => cell.running,
        "queued"   => cell.queued,
        "errored"  => cell.errored,
        "last_run" => cell.output.last_run_timestamp,
        "runtime"  => cell.runtime,
        "output"   => _serialize_output(cell),
        # Boilerplate the tools hide: Pluto's package cells, the `@bind` shim,
        # and cells Pluto comments out in the file.
        "hidden"   => _is_manifest_cell(cell.cell_id) || _is_fake_bind_shim(cell) || Pluto.must_be_commented_in_file(cell),
        "markdown" => _is_markdown_cell(cell),
    )
    err = _cell_output_error(cell)
    err !== nothing && (d["error"] = err)
    return d
end

function snapshot(nb)
    Dict{String,Any}(
        "notebook_id"       => string(nb.notebook_id),
        "path"              => nb.path,
        "cell_order"        => [string(id) for id in nb.cell_order],
        "process_status"    => string(nb.process_status),
        "execution_allowed" => Pluto.will_run_code(nb),
        "safe_preview"      => nb.process_status === Pluto.ProcessStatus.waiting_for_permission,
        "cells"           => [_snapshot_cell(nb.cells_dict[id]) for id in nb.cell_order],
    )
end

# Pluto's dependency data as of the last run or staged edit (no reanalysis),
# cells in the order Pluto analysed them. `fresh` analyses the notebook as it is
# now without keeping the result; `refresh` first updates the dependency cache
# Pluto's page shows; `edges` adds each cell's direct upstream and downstream
# cells; `packages` the packages each loads (`using`/`import`), sorted.
function graph(nb; fresh::Bool=false, refresh::Bool=false, edges::Bool=false, packages::Bool=false)
    refresh && Pluto.update_dependency_cache!(nb)
    topology = fresh ? Pluto.updated_topology(nb.topology, nb, nb.cells) : nb.topology
    order = fresh ? Pluto.PlutoDependencyExplorer.topological_order(topology) : Pluto.topological_order(nb)
    names(syms) = sort!(string.(collect(syms)))
    ids(cells) = [string(c.cell_id) for c in cells]
    function cell(c)
        node = topology.nodes[c]
        d = Dict{String,Any}(
            "cell_id"     => string(c.cell_id),
            "definitions" => names(node.definitions),
            "functions"   => names(node.funcdefs_without_signatures),
            "references"  => names(node.references),
        )
        if edges
            d["upstream"] = ids(Pluto.PlutoDependencyExplorer.where_assigned(topology, node.references))
            d["downstream"] = ids(Pluto.PlutoDependencyExplorer.where_referenced(topology, c))
        end
        if packages
            # A cell never analysed has the default, empty analysis.
            usings = topology.codes[c].module_usings_imports
            d["packages"] = sort!(string.(collect(Pluto.ExpressionExplorer.external_package_names(usings))))
        end
        return d
    end
    Dict{String,Any}(
        "notebook_id" => string(nb.notebook_id),
        "cells"       => [cell(c) for c in Pluto.PlutoDependencyExplorer.all_cells(topology)],
        "order"       => ids(order.runnable),
        "errable"     => ids(keys(order.errable)),
    )
end

"Shut down a notebook: its process ends and it leaves the session."
function shutdown_notebook!(session, nb)
    safe_preview = nb.process_status === Pluto.ProcessStatus.waiting_for_permission
    Pluto.SessionActions.shutdown(session, nb; async = false, verbose = false)
    return Dict{String,Any}("safe_preview" => safe_preview)
end

_adapter_cell(nb, id) = nb.cells_dict[UUID(id)]

# Change the notebook. Each op sets a cell's code, inserts a new cell at an
# index of the new order, deletes a cell, moves one to an index, or folds one. A
# set_code with `expected` is refused, before any op applies, if the cell's code
# isn't that. Pluto's page diffs `cell_order` by reference, so each change
# assigns a new vector. Cells whose code changed are analysed; the file is
# saved and the page updated.
function apply!(session, nb, ops)
    for op in ops
        if op["op"] == "set_code" && haskey(op, "expected")
            cell = _adapter_cell(nb, op["cell_id"])
            cell.code == op["expected"] ||
                throw(ArgumentError("stale_read::Cell $(cell.cell_id) changed since last read; call read_cell again"))
        end
    end
    changed = Pluto.Cell[]
    inserted = String[]
    for op in ops
        kind = op["op"]
        if kind == "set_code"
            cell = _adapter_cell(nb, op["cell_id"])
            cell.code = op["code"]
            push!(changed, cell)
        elseif kind == "insert"
            cell = Pluto.Cell(; code=string(op["code"]), code_folded=op["folded"])
            nb.cells_dict[cell.cell_id] = cell
            order = collect(nb.cell_order)
            insert!(order, op["index"] + 1, cell.cell_id)
            nb.cell_order = order
            push!(changed, cell)
            push!(inserted, string(cell.cell_id))
        elseif kind == "delete"
            id = UUID(op["cell_id"])
            nb.cell_order = filter(!=(id), nb.cell_order)
            delete!(nb.cells_dict, id)
        elseif kind == "move"
            id = UUID(op["cell_id"])
            order = filter(!=(id), nb.cell_order)
            insert!(order, op["index"] + 1, id)
            nb.cell_order = order
        elseif kind == "fold"
            _adapter_cell(nb, op["cell_id"]).code_folded = op["folded"]
        else
            throw(ArgumentError("unknown_op::Unknown op: '$kind'"))
        end
    end
    isempty(changed) || (nb.topology = Pluto.updated_topology(nb.topology, nb, changed))
    Pluto.save_notebook(session, nb)
    _notify_browser(session, nb)
    return Dict{String,Any}("inserted" => inserted)
end

# Run cells; none is Pluto's reactive cleanup after a delete. Not accepted when
# the notebook won't run code (safe preview, a stopped process). With `wait`,
# the reply says which cells finished within `timeout` seconds each; without, a
# `run_finished` notification says so later.
function run_cells!(session, nb, cells; wait::Bool, timeout::Real)
    accepted = Pluto.will_run_code(nb)
    # Pluto marks cells queued only inside its (possibly async) run task, after
    # package sync; mark them now, as its own run button does, so a caller that
    # looks right away sees them waiting.
    accepted && foreach(c -> c.queued = true, cells)
    Pluto.update_save_run!(session, nb, cells; run_async=!wait, save=true)
    result = Dict{String,Any}("accepted" => accepted, "process_status" => string(nb.process_status))
    if accepted && wait
        completed, timed_out = _wait_cells!(cells; timeout)
        result["completed"] = [string(id) for id in completed]
        result["timed_out"] = [string(id) for id in timed_out]
    elseif accepted && !isempty(cells)
        @async begin
            completed, = _wait_cells!(cells; timeout)
            notify!("run_finished", Dict{String,Any}("notebook_id" => string(nb.notebook_id), "cells" => [string(id) for id in completed]))
        end
    end
    _notify_browser(session, nb)
    return result
end

# Pluto's "Run notebook code" for a notebook in safe preview: restart its
# process, then run every cell (`run`) or leave it to start on the next run.
function allow_execution!(session, nb, run::Bool, timeout::Real)
    ps = nb.process_status
    ps === Pluto.ProcessStatus.ready &&
        return Dict{String,Any}("already_allowed" => true, "ran" => false, "process_status" => string(ps))
    ps !== Pluto.ProcessStatus.waiting_for_permission &&
        throw(ArgumentError("execution_not_gated::Notebook is not in safe preview (process_status=$ps)"))
    haskey(nb.metadata, "risky_file_source") && throw(ArgumentError(
        "risky_source::Cannot allow execution for risky remote sources via MCP; ask the user to run it from the notebook pane",
    ))
    nb.process_status = Pluto.ProcessStatus.waiting_to_restart
    session.options.evaluation.run_notebook_on_load && Pluto._report_business_cells_planned!(nb)
    _notify_browser(session, nb)
    Pluto.SessionActions.shutdown(session, nb; keep_in_session=true, async=true, verbose=false)
    if run
        nb.process_status = Pluto.ProcessStatus.starting
        _notify_browser(session, nb)
        run_cells!(session, nb, collect(nb.cells); wait=false, timeout)
    else
        nb.process_status = Pluto.ProcessStatus.ready
        _notify_browser(session, nb)
    end
    return Dict{String,Any}("already_allowed" => false, "ran" => run, "process_status" => string(nb.process_status))
end

# Pluto's own Restart: a new process, then every cell runs. Refused in safe
# preview, where allow_execution starts the notebook.
function restart!(session, nb, timeout::Real)
    ps = nb.process_status
    ps === Pluto.ProcessStatus.waiting_for_permission &&
        throw(ArgumentError("execution_blocked::The notebook is in safe preview; Run notebook starts it"))
    ps === Pluto.ProcessStatus.waiting_to_restart && return Dict{String,Any}("restarted" => false)
    nb.process_status = Pluto.ProcessStatus.waiting_to_restart
    session.options.evaluation.run_notebook_on_load && Pluto._report_business_cells_planned!(nb)
    _notify_browser(session, nb)
    Pluto.SessionActions.shutdown(session, nb; keep_in_session=true, async=true, verbose=false)
    nb.process_status = Pluto.ProcessStatus.starting
    _notify_browser(session, nb)
    run_cells!(session, nb, collect(nb.cells); wait=false, timeout)
    return Dict{String,Any}("restarted" => true)
end

# Move the notebook's file to `path` (checked by the core), as Pluto's own file box does.
function move!(session, nb, path::AbstractString)
    Pluto.SessionActions.move(session, nb, path)
    return Dict{String,Any}("path" => nb.path)
end

function open_notebook(session, path::AbstractString, run::Bool)
    nb = try
        Pluto.SessionActions.open(session, path; run_async = true, execution_allowed = run)
    catch e
        # Printing this exception walks the whole Notebook and never finishes.
        e isa Pluto.SessionActions.NotebookIsRunningException || rethrow()
        throw(ArgumentError("notebook_already_open::'$path' is already open as notebook_id $(e.notebook.notebook_id); use that id"))
    end
    return Dict{String,Any}("notebook_id" => string(nb.notebook_id), "path" => nb.path, "process_status" => string(nb.process_status))
end

# A new notebook at `path`, else named as Pluto names them in `folder`, else in
# Pluto's own folder. Pluto writes the file (never a hand-written header), then
# it opens and runs like any other; its cells come back so the caller can edit them.
function new_notebook(session, path, folder)
    nb = if path !== nothing
        Pluto.emptynotebook(path)
    elseif folder !== nothing
        Pluto.emptynotebook(Pluto.numbered_until_new(joinpath(folder, Pluto.cutename()); create_file=false))
    else
        Pluto.emptynotebook()
    end
    Pluto.save_notebook(nb, nb.path)
    opened = open_notebook(session, nb.path, true)
    opened["cells"] = [Dict{String,Any}("cell_id" => string(c.cell_id), "code" => c.code) for c in nb.cells]
    return opened
end

function render_png(session, nb, cell)
    png = _cell_png(session, nb, cell)
    return Dict{String,Any}("png" => png === nothing ? nothing : base64encode(png), "mime" => string(cell.output.mime))
end

function adapter_call(session, method::AbstractString, params)
    session === nothing && throw(ArgumentError("pluto_not_running::Pluto is not running yet."))
    method == "status" && return session_status_dict()
    method == "open" && return open_notebook(session, params["path"], params["run"])
    method == "new" && return new_notebook(session, get(params, "path", nothing), get(params, "folder", nothing))
    id = get(params, "notebook_id", nothing)
    if method == "snapshot" && id === nothing
        return Dict{String,Any}("notebooks" => [snapshot(nb) for nb in values(session.notebooks)])
    end
    nb = _get_notebook(session, string(id))
    method == "snapshot" && return snapshot(nb)
    method == "graph" && return graph(nb; fresh=get(params, "fresh", false), refresh=get(params, "refresh", false),
                                      edges=get(params, "edges", false), packages=get(params, "packages", false))
    method == "shutdown" && return shutdown_notebook!(session, nb)
    method == "apply" && return apply!(session, nb, params["ops"])
    method == "run" && return run_cells!(session, nb, [_adapter_cell(nb, c) for c in params["cells"]]; wait=params["wait"], timeout=params["timeout"])
    method == "interrupt" && return Dict{String,Any}("interrupted" => Pluto.WorkspaceManager.interrupt_workspace((session, nb); verbose=false))
    method == "restart" && return restart!(session, nb, params["timeout"])
    method == "allow_execution" && return allow_execution!(session, nb, params["run"], params["timeout"])
    method == "move" && return move!(session, nb, params["path"])
    method == "render_png" && return render_png(session, nb, _adapter_cell(nb, params["cell_id"]))
    method == "validate" && return Dict{String,Any}("errors" => _parse_validation_errors(nb, _adapter_cell(nb, params["cell_id"]), params["code"]))
    throw(ArgumentError("unknown_method::Unknown adapter method: '$method'"))
end

function _handle_adapter(http::HTTP.Stream, session)
    msg = try
        JSON.parse(String(read(http)), Dict{String,Any})
    catch
        HTTP.setstatus(http, 400)
        HTTP.startwrite(http)
        write(http, """{"error":"Invalid JSON"}""")
        return
    end
    reply = try
        Dict("result" => adapter_call(session, string(get(msg, "method", "")), get(msg, "params", Dict{String,Any}())))
    catch e
        Dict("error" => sprint(showerror, e))
    end
    HTTP.setstatus(http, 200)
    HTTP.setheader(http, "Content-Type" => "application/json")
    HTTP.startwrite(http)
    write(http, JSON.json(reply))
end
