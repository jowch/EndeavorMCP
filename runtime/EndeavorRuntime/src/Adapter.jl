# The Pluto adapter's side of the engine interface (docs/runtime-core.md). The
# core (`endeavor-remote core`) calls `POST /adapter` with
#   {"method": "snapshot" | "graph" | "shutdown", "params": {"notebook_id": ...}}
# and reads the reply's "result" or "error". What happens in Pluto reaches it as
# notifications on `GET /notifications`, one `data: {"method", "params"}` line
# each: notebook_opened, notebook_shut_down (it left the session), file_saved,
# execution_done, cell_state (every cell's code and run state: Pluto's hook
# doesn't say which cell changed) and topology_changed.

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
        # Staging moves to the core with the edit tools; until then it's read here.
        "pending_run"       => [string(id) for id in pending_run_ids(nb.notebook_id, nb)],
        "cells"             => [_snapshot_cell(nb.cells_dict[id]) for id in nb.cell_order],
    )
end

# Pluto's dependency data as of the last run or staged edit: no reanalysis.
function graph(nb)
    node(c) = nb.topology.nodes[c]   # a default dict: an unanalysed cell is an empty node
    names(syms) = sort!(string.(collect(syms)))
    Dict{String,Any}(
        "notebook_id" => string(nb.notebook_id),
        "cells" => [Dict{String,Any}(
            "cell_id"     => string(id),
            "definitions" => names(node(nb.cells_dict[id]).definitions),
            "functions"   => names(node(nb.cells_dict[id]).funcdefs_without_signatures),
            "references"  => names(node(nb.cells_dict[id]).references),
        ) for id in nb.cell_order],
        "order" => [string(c.cell_id) for c in Pluto.topological_order(nb).runnable],
    )
end

"Shut down a notebook: its process ends and it leaves the session."
function shutdown_notebook!(session, nb)
    safe_preview = nb.process_status === Pluto.ProcessStatus.waiting_for_permission
    Pluto.SessionActions.shutdown(session, nb; async = false, verbose = false)
    return Dict{String,Any}("safe_preview" => safe_preview)
end

function adapter_call(session, method::AbstractString, params)
    session === nothing && throw(ArgumentError("pluto_not_running::Pluto is not running yet."))
    id = get(params, "notebook_id", nothing)
    if method == "snapshot" && id === nothing
        return Dict{String,Any}("notebooks" => [snapshot(nb) for nb in values(session.notebooks)])
    end
    nb = _get_notebook(session, string(id))
    method == "snapshot" && return snapshot(nb)
    method == "graph" && return graph(nb)
    method == "shutdown" && return shutdown_notebook!(session, nb)
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
