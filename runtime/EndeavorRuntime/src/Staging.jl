# Per-session staging state: cells the agent edited that haven't run since.
#
# Each edit records its time; a cell stops being pending once Pluto's
# `last_run_timestamp` for it is newer, however it ran (our run tools, Pluto's
# own run button, a reactive re-run). Submitting code in Pluto's UI runs it at
# once, so only our tools leave a cell edited-but-unrun.

const _STAGING_LOCK = ReentrantLock()
const _pending_run = Dict{UUID, Dict{UUID, Float64}}()   # notebook => cell => edit time
const _read_receipts = Dict{UUID, Dict{UUID, String}}()

function _with_staging_lock(f)
    lock(_STAGING_LOCK) do
        return f()
    end
end

function _pending_for(notebook_id::UUID)
    get!(_pending_run, notebook_id) do
        Dict{UUID, Float64}()
    end
end

function mark_pending!(notebook_id::UUID, cell_id::UUID)
    _with_staging_lock() do
        _pending_for(notebook_id)[cell_id] = time()
    end
    return nothing
end

function clear_pending!(notebook_id::UUID, cell_ids)
    _with_staging_lock() do
        pending = get(_pending_run, notebook_id, nothing)
        pending === nothing && return nothing
        for cid in cell_ids
            delete!(pending, cid)
        end
        isempty(pending) && delete!(_pending_run, notebook_id)
    end
    return nothing
end

function clear_all_pending!(notebook_id::UUID)
    _with_staging_lock() do
        delete!(_pending_run, notebook_id)
    end
    return nothing
end

# Drop pending ids for cells no longer in the notebook. Pluto's file hot-reload
# (update_from_file) and the browser delete remove cells straight from
# cells_dict, bypassing delete_cell's clear_pending!, and a ghost id would
# otherwise make submit_changes throw cell_not_found on every call.
function prune_orphan_pending!(nb::Pluto.Notebook)
    _with_staging_lock() do
        pending = get(_pending_run, nb.notebook_id, nothing)
        pending === nothing && return nothing
        filter!(((cid, _),) -> haskey(nb.cells_dict, cid), pending)
        isempty(pending) && delete!(_pending_run, nb.notebook_id)
    end
    return nothing
end

function clear_notebook_staging!(notebook_id::UUID)
    _with_staging_lock() do
        delete!(_pending_run, notebook_id)
        delete!(_read_receipts, notebook_id)
    end
    return nothing
end

function reset_staging_state!()
    _with_staging_lock() do
        empty!(_pending_run)
        empty!(_read_receipts)
    end
    return nothing
end

function _open_notebook(notebook_id::UUID)
    sess = standalone_session()
    sess === nothing ? nothing : get(sess.notebooks, notebook_id, nothing)
end

# Ran since `t`: a run that *started* after the edit and has finished. Pluto stamps
# `last_run_timestamp` when a run ends, so a run already under way at the edit
# (old code) ends after it; subtract the run's duration to get its start.
function _ran_since(nb, cell_id, t)
    c = get(nb.cells_dict, cell_id, nothing)
    (c === nothing || c.running || c.queued) && return false
    started = c.output.last_run_timestamp - something(c.runtime, 0) / 1e9
    return c.output.last_run_timestamp > 0 && started >= t
end

"Cells edited through our tools that haven't run since (a run from anywhere counts)."
function pending_run_ids(notebook_id::UUID, nb = _open_notebook(notebook_id))
    _with_staging_lock() do
        pending = get(_pending_run, notebook_id, nothing)
        pending === nothing && return UUID[]
        nb !== nothing && filter!(((cid, t),) -> !_ran_since(nb, cid, t), pending)
        isempty(pending) && delete!(_pending_run, notebook_id)
        return collect(keys(pending))
    end
end

is_stale(notebook_id::UUID, cell_id::UUID) = cell_id in pending_run_ids(notebook_id)

function _read_receipts_for(notebook_id::UUID)
    get!(_read_receipts, notebook_id) do
        Dict{UUID, String}()
    end
end

function record_read!(notebook_id::UUID, cell_id::UUID, code::String)
    _with_staging_lock() do
        _read_receipts_for(notebook_id)[cell_id] = code
    end
    return nothing
end

function clear_read_receipt!(notebook_id::UUID, cell_id::UUID)
    _with_staging_lock() do
        receipts = get(_read_receipts, notebook_id, nothing)
        receipts === nothing && return nothing
        delete!(receipts, cell_id)
        isempty(receipts) && delete!(_read_receipts, notebook_id)
    end
    return nothing
end

function require_fresh_read!(notebook_id::UUID, cell::Pluto.Cell)
    _with_staging_lock() do
        receipts = get(_read_receipts, notebook_id, nothing)
        if receipts === nothing || !haskey(receipts, cell.cell_id)
            throw(ArgumentError(
                "read_required::Call read_cell or read_notebook_code before editing cell $(cell.cell_id)"
            ))
        end
        if receipts[cell.cell_id] != cell.code
            throw(ArgumentError(
                "stale_read::Cell $(cell.cell_id) changed since last read; call read_cell again"
            ))
        end
    end
    return nothing
end

function _receipt_output_summary(cell)
    _serialize_output(cell)
end

function _execution_status(nb, cell_ids_run; warnings=String[])
    any(x -> startswith(x, "execution_blocked::"), warnings) && return "blocked"
    any(x -> startswith(x, "execution_timeout::"), warnings) && return "timeout"
    any(x -> startswith(x, "async_execution::"), warnings) && return "running"
    if isempty(cell_ids_run)
        return "staged"
    end
    any_errored = false
    any_running = false
    for cid in cell_ids_run
        cell = get(nb.cells_dict, cid, nothing)
        cell === nothing && continue
        cell.errored && (any_errored = true)
        (cell.running || cell.queued) && (any_running = true)
    end
    any_errored && return "errored"
    any_running && return "running"
    return "completed"
end

function _mutation_receipt(session, nb; applied, mutation, cell_ids_run=UUID[], warnings=String[], execution_status=nothing)
    status = execution_status === nothing ?
        _execution_status(nb, cell_ids_run; warnings) :
        execution_status

    # A blocked run touched no cell; existing outputs are not this run's result.
    outputs_changed = Dict{String,Any}[]
    for cid in (status == "blocked" ? UUID[] : cell_ids_run)
        cell = get(nb.cells_dict, cid, nothing)
        cell === nothing && continue
        summary = _receipt_output_summary(cell)
        isempty(summary) && continue
        entry = Dict{String,Any}(
            "cell_id"        => string(cid),
            "output_summary" => summary,
        )
        err = _cell_output_error(cell)
        err !== nothing && (entry["error"] = err)
        push!(outputs_changed, entry)
    end

    Dict{String,Any}(
        "applied"          => applied,
        "mutation"         => mutation,
        "cell_order"       => [string(id) for id in nb.cell_order],
        "execution_order"  => [string(c.cell_id) for c in collect(Pluto.topological_order(nb))],
        "affected_cells"   => [string(id) for id in cell_ids_run],
        "execution"        => Dict{String,Any}("status" => status),
        "outputs"          => Dict{String,Any}("changed" => outputs_changed),
        "pending_run"      => [string(id) for id in pending_run_ids(nb.notebook_id)],
        "warnings"         => warnings,
    )
end
