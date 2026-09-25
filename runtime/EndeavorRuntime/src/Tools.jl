const TOOL_TIMEOUT_SECONDS = 60.0

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

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

function _get_cell(notebook, cell_id_str)
    cid = try
        UUID(cell_id_str)
    catch
        throw(ArgumentError("invalid_cell_id::Invalid cell ID: '$cell_id_str'"))
    end
    cell = get(notebook.cells_dict, cid, nothing)
    cell === nothing && throw(KeyError("cell_not_found::No cell with id '$cell_id_str' in notebook"))
    return cell
end

function _cell_to_dict(cell; notebook_id=nothing)
    d = Dict{String,Any}(
        "cell_id" => string(cell.cell_id),
        "code"    => cell.code,
        "output"  => _serialize_output(cell),
        "errored" => cell.errored,
        "running" => cell.running,
        "queued"  => cell.queued,
        "code_folded" => cell.code_folded,
    )
    err = _cell_output_error(cell)
    err !== nothing && (d["error"] = err)
    if notebook_id !== nothing
        d["stale"] = is_stale(notebook_id, cell.cell_id)
    end
    return d
end

function _wait_for_cell(cell; timeout=TOOL_TIMEOUT_SECONDS)
    t = time()
    while cell.running || cell.queued
        time() - t > timeout && return false
        sleep(0.05)
    end
    return true
end

function _wait_cells!(cells; timeout=TOOL_TIMEOUT_SECONDS)
    completed = UUID[]
    timed_out = UUID[]
    for cell in cells
        if _wait_for_cell(cell; timeout=timeout)
            push!(completed, cell.cell_id)
        else
            push!(timed_out, cell.cell_id)
        end
    end
    return completed, timed_out
end

function _notify_browser(session, notebook)
    try
        Pluto.send_notebook_changes!(Pluto.ClientRequest(; session, notebook))
    catch
        # Best-effort: no connected clients is fine
    end
end

function _require_bool(value, name::AbstractString)
    value isa Bool || throw(ArgumentError("invalid_argument::$name must be a boolean"))
    return value
end

# Assign a new cell_order vector instead of mutating in place. Pluto's Firebasey
# diff caches cell_order by reference; in-place push!/insert!/deleteat! updates
# the cached snapshot too, so no cell_order patch reaches connected browsers.
function _set_cell_order!(notebook, new_order::Vector{UUID})
    notebook.cell_order = new_order
    return notebook
end

function _insert_cell_after!(notebook, after_id::UUID, new_id::UUID)
    target_idx = findfirst(==(after_id), notebook.cell_order)
    target_idx === nothing &&
        throw(KeyError("cell_not_found::Cell '$after_id' not found in notebook"))
    _set_cell_order!(notebook, [
        notebook.cell_order[1:target_idx]...,
        new_id,
        notebook.cell_order[target_idx+1:end]...,
    ])
end

function _append_cell!(notebook, new_id::UUID)
    _set_cell_order!(notebook, [notebook.cell_order..., new_id])
end

function _remove_cell_from_order!(notebook, cell_id::UUID)
    _set_cell_order!(notebook, filter(!=(cell_id), notebook.cell_order))
end

function _move_cell_in_order!(notebook, cell_id::UUID, after_cell_id)
    order = collect(notebook.cell_order)
    old_idx = findfirst(==(cell_id), order)
    old_idx === nothing && throw(KeyError("cell_not_found::Cell not found in cell_order"))
    deleteat!(order, old_idx)

    if after_cell_id == ""
        insert!(order, 1, cell_id)
    else
        target_id = try
            UUID(after_cell_id)
        catch
            throw(ArgumentError("invalid_cell_id::Invalid cell ID: '$after_cell_id'"))
        end
        new_idx = findfirst(==(target_id), order)
        new_idx === nothing &&
            throw(KeyError("cell_not_found::Target cell '$after_cell_id' not found"))
        insert!(order, new_idx + 1, cell_id)
    end

    _set_cell_order!(notebook, order)
end

function _stage_cell!(session, nb, cell)
    _stage_cells!(session, nb, [cell])
end

function _stage_cells!(session, nb, cells)
    for cell in cells
        mark_pending!(nb.notebook_id, cell.cell_id)
    end
    nb.topology = Pluto.updated_topology(nb.topology, nb, cells)
    Pluto.save_notebook(session, nb)
    _notify_browser(session, nb)
end

"""
Resolve wait_for_completion for the dispatch thread. Standalone sessions always
honor the caller's requested value.
"""
function _effective_wait(wait_for_completion::Bool)
    return wait_for_completion, String[]
end

# Pluto flips `cell.queued` only inside its (possibly async) run task, after
# package sync, so a waiter polling `running || queued` can see "idle" before the
# run has started and clear pending_run for cells that never ran. Pre-mark like
# Pluto's own run_multiple_cells handler. Returns false when the notebook will
# not run code at all (safe preview); pending_run must then stay set.
function _queue_cells!(nb, cells)
    Pluto.will_run_code(nb) || return false
    foreach(c -> c.queued = true, cells)
    return true
end

function _blocked_warning(nb)
    msg = "execution_blocked::notebook is not running code (process_status=$(nb.process_status)); pending_run kept"
    nb.process_status === Pluto.ProcessStatus.waiting_for_permission ?
        msg * "; call allow_execution to exit safe preview" : msg
end

function _run_cells!(session, nb, cells; wait_for_completion=true)
    prune_orphan_pending!(nb)
    warnings = String[]
    wait_for, force_warnings = _effective_wait(wait_for_completion)
    append!(warnings, force_warnings)
    will_run = _queue_cells!(nb, cells)
    Pluto.update_save_run!(session, nb, cells; run_async=!wait_for, save=true)
    if !will_run
        push!(warnings, _blocked_warning(nb))
    elseif wait_for
        completed, timed_out = _wait_cells!(cells)
        for cid in timed_out
            push!(warnings, "execution_timeout::Cell $cid did not finish within $(TOOL_TIMEOUT_SECONDS)s")
        end
        !isempty(completed) && clear_pending!(nb.notebook_id, completed)
    else
        @async begin
            completed, = _wait_cells!(cells)
            !isempty(completed) && clear_pending!(nb.notebook_id, completed)
        end
        push!(warnings, "async_execution::cells running; pending_run clears when execution finishes")
    end
    _notify_browser(session, nb)
    return warnings
end


# ---------------------------------------------------------------------------
# Tool implementations
# ---------------------------------------------------------------------------

function tool_list_notebooks(session, _args)
    [
        Dict{String,Any}(
            "notebook_id" => string(nb.notebook_id),
            "path"        => nb.path,
            "cell_count"  => length(nb.cell_order),
            # Run state without read receipts, so a host can check it (e.g. at the end
            # of an agent turn) without weakening read-before-edit.
            "pending_run"       => [string(id) for id in pending_run_ids(nb.notebook_id)],
            "running"           => [string(id) for id in nb.cell_order if (c = nb.cells_dict[id]; c.running || c.queued)],
            "execution_allowed" => Pluto.will_run_code(nb),
        )
        for nb in values(session.notebooks)
    ]
end

function tool_view_cell_output(session, args)
    nb   = _get_notebook(session, args["notebook_id"])
    cell = _get_cell(nb, args["cell_id"])
    cell.errored &&
        throw(ArgumentError("no_image::Cell $(cell.cell_id) errored; read_cell shows the error"))
    png = _cell_png(session, nb, cell)
    png === nothing && throw(ArgumentError("no_image::Cell $(cell.cell_id): its output " *
        "($(cell.output.mime)) has no PNG rendering; read_cell shows it as text"))
    length(png) > MAX_IMAGE_BYTES &&
        throw(ArgumentError("image_too_large::Cell $(cell.cell_id) renders to $(length(png)) bytes (max $MAX_IMAGE_BYTES)"))
    return CellImage(Dict{String,Any}(
        "cell_id"   => string(cell.cell_id),
        "shown_as"  => string(cell.output.mime),
        "png_bytes" => length(png),
    ), png)
end

function tool_read_cell(session, args)
    nb   = _get_notebook(session, args["notebook_id"])
    cell = _get_cell(nb, args["cell_id"])
    record_read!(nb.notebook_id, cell.cell_id, cell.code)
    _cell_to_dict(cell; notebook_id=nb.notebook_id)
end

function tool_edit_cell(session, args)
    nb        = _get_notebook(session, args["notebook_id"])
    cell      = _get_cell(nb, args["cell_id"])
    code      = args["code"]
    run_after = get(args, "run_after", false)

    require_fresh_read!(nb.notebook_id, cell)

    cell.code = code
    note_agent_edit!(nb.notebook_id, cell)
    record_read!(nb.notebook_id, cell.cell_id, code)

    if run_after
        # Non-blocking: blocking wait on stdio-bound sessions can starve MCP (#3).
        warnings = _run_cells!(session, nb, [cell]; wait_for_completion=false)
    else
        _stage_cell!(session, nb, cell)
        warnings = String[]
    end

    receipt = _mutation_receipt(session, nb;
        applied=true,
        mutation=Dict{String,Any}("type" => "edit_cell", "cell_id" => string(cell.cell_id)),
        cell_ids_run=run_after ? [cell.cell_id] : UUID[],
        warnings=warnings,
    )
    merge!(receipt, _cell_to_dict(cell; notebook_id=nb.notebook_id))
    return receipt
end

function tool_edit_cells(session, args)
    nb    = _get_notebook(session, args["notebook_id"])
    edits = args["cells"]

    edited_ids = UUID[]
    staged_cells = Pluto.Cell[]
    for edit in edits
        cell = _get_cell(nb, edit["cell_id"])
        require_fresh_read!(nb.notebook_id, cell)
    end
    for edit in edits
        cell = _get_cell(nb, edit["cell_id"])
        cell.code = edit["code"]
        note_agent_edit!(nb.notebook_id, cell)
        record_read!(nb.notebook_id, cell.cell_id, edit["code"])
        push!(edited_ids, cell.cell_id)
        push!(staged_cells, cell)
    end
    _stage_cells!(session, nb, staged_cells)

    return _mutation_receipt(session, nb;
        applied=true,
        mutation=Dict{String,Any}(
            "type"     => "edit_cells",
            "cell_ids" => [string(id) for id in edited_ids],
        ),
        cell_ids_run=UUID[],
    )
end

function tool_add_cell(session, args)
    nb            = _get_notebook(session, args["notebook_id"])
    code          = get(args, "code", "")
    after_cell_id = get(args, "after_cell_id", nothing)
    run_after     = get(args, "run_after", false)
    folded        = _require_bool(get(args, "folded", false), "folded")

    if !isempty(nb.cell_order) && (after_cell_id === nothing || after_cell_id == "")
        throw(ArgumentError(
            "placement_required::after_cell_id is required when the notebook is not empty"
        ))
    end

    if !isempty(nb.cell_order)
        anchor = _get_cell(nb, after_cell_id)
        require_fresh_read!(nb.notebook_id, anchor)
    end

    new_cell = Pluto.Cell(; code=string(code), code_folded=folded)
    nb.cells_dict[new_cell.cell_id] = new_cell
    note_agent_edit!(nb.notebook_id, new_cell)
    record_read!(nb.notebook_id, new_cell.cell_id, string(code))

    if after_cell_id === nothing || after_cell_id == ""
        _append_cell!(nb, new_cell.cell_id)
    else
        target_id = try
            UUID(after_cell_id)
        catch
            throw(ArgumentError("invalid_cell_id::Invalid cell ID: '$after_cell_id'"))
        end
        _insert_cell_after!(nb, target_id, new_cell.cell_id)
    end

    if run_after
        # Non-blocking: blocking wait on stdio-bound sessions can starve MCP (#3).
        warnings = _run_cells!(session, nb, [new_cell]; wait_for_completion=false)
    else
        _stage_cell!(session, nb, new_cell)
        warnings = String[]
    end

    receipt = _mutation_receipt(session, nb;
        applied=true,
        mutation=Dict{String,Any}("type" => "add_cell", "cell_id" => string(new_cell.cell_id)),
        cell_ids_run=run_after ? [new_cell.cell_id] : UUID[],
        warnings=warnings,
    )
    merge!(receipt, _cell_to_dict(new_cell; notebook_id=nb.notebook_id))
    return receipt
end

function tool_delete_cell(session, args)
    nb   = _get_notebook(session, args["notebook_id"])
    cell = _get_cell(nb, args["cell_id"])

    cell_id_str = string(cell.cell_id)

    _remove_cell_from_order!(nb, cell.cell_id)
    delete!(nb.cells_dict, cell.cell_id)
    clear_pending!(nb.notebook_id, [cell.cell_id])
    clear_read_receipt!(nb.notebook_id, cell.cell_id)

    # Non-blocking: run_async=false blocked the MCP stdio thread on reactive
    # cleanup / nbpkg (Styx #3 residual footgun after wait-default flip).
    Pluto.update_save_run!(session, nb, Pluto.Cell[]; run_async=true, save=true)
    _notify_browser(session, nb)

    return _mutation_receipt(session, nb;
        applied=true,
        mutation=Dict{String,Any}("type" => "delete_cell", "cell_id" => cell_id_str),
        cell_ids_run=UUID[],
        warnings=String["async_execution::cell deletion cleanup queued"],
        execution_status="completed",
    )
end

function tool_execute_cell(session, args)
    nb       = _get_notebook(session, args["notebook_id"])
    cell     = _get_cell(nb, args["cell_id"])
    wait_for = get(args, "wait_for_completion", false)

    warnings = _run_cells!(session, nb, [cell]; wait_for_completion=wait_for)

    return _mutation_receipt(session, nb;
        applied=true,
        mutation=Dict{String,Any}("type" => "execute_cell", "cell_id" => string(cell.cell_id)),
        cell_ids_run=[cell.cell_id],
        warnings=warnings,
    )
end

function tool_submit_changes(session, args)
    nb       = _get_notebook(session, args["notebook_id"])
    wait_for = get(args, "wait_for_completion", false)
    prune_orphan_pending!(nb)

    target_ids = if haskey(args, "cell_ids")
        ids = [try
            UUID(cid)
        catch
            throw(ArgumentError("invalid_cell_id::Invalid cell ID: '$cid'"))
        end for cid in args["cell_ids"]]
        if !get(args, "force", false)
            pending = Set(pending_run_ids(nb.notebook_id))
            for cid in ids
                cid in pending || throw(ArgumentError(
                    "not_staged::Cell $cid is not in pending_run; stage first or pass force=true"
                ))
            end
        end
        ids
    else
        pending_run_ids(nb.notebook_id)
    end

    if isempty(target_ids)
        return _mutation_receipt(session, nb;
            applied=true,
            mutation=Dict{String,Any}("type" => "submit_changes"),
            cell_ids_run=UUID[],
            execution_status="completed",
        )
    end

    cells = [_get_cell(nb, string(cid)) for cid in target_ids]
    warnings = _run_cells!(session, nb, cells; wait_for_completion=wait_for)

    return _mutation_receipt(session, nb;
        applied=true,
        mutation=Dict{String,Any}("type" => "submit_changes"),
        cell_ids_run=target_ids,
        warnings=warnings,
    )
end

function tool_run_all_cells(session, args)
    nb       = _get_notebook(session, args["notebook_id"])
    wait_for = get(args, "wait_for_completion", false)

    cells = collect(nb.cells)
    cell_ids = [c.cell_id for c in cells]
    # Same queue/run/wait/clear path as submit_changes; every cell is a target,
    # so per-cell clear_pending! covers the whole notebook once it completes.
    warnings = _run_cells!(session, nb, cells; wait_for_completion=wait_for)

    return _mutation_receipt(session, nb;
        applied=true,
        mutation=Dict{String,Any}("type" => "run_all_cells"),
        cell_ids_run=cell_ids,
        warnings=warnings,
    )
end

# Fold = hide the code editor, show only the output (Pluto's eye toggle).
# Metadata only: persisted in the file's cell-order markers, no execution.
function tool_fold_cell(session, args)
    nb     = _get_notebook(session, args["notebook_id"])
    cell   = _get_cell(nb, args["cell_id"])
    folded = _require_bool(args["folded"], "folded")

    cell.code_folded = folded
    Pluto.save_notebook(session, nb)
    _notify_browser(session, nb)

    return _mutation_receipt(session, nb;
        applied=true,
        mutation=Dict{String,Any}(
            "type"    => "fold_cell",
            "cell_id" => string(cell.cell_id),
            "folded"  => folded,
        ),
        cell_ids_run=UUID[],
        execution_status="completed",
    )
end

function tool_move_cell(session, args)
    nb            = _get_notebook(session, args["notebook_id"])
    cell          = _get_cell(nb, args["cell_id"])
    after_cell_id = args["after_cell_id"]

    old_idx = findfirst(==(cell.cell_id), nb.cell_order)
    _move_cell_in_order!(nb, cell.cell_id, after_cell_id)
    new_idx = findfirst(==(cell.cell_id), nb.cell_order)

    Pluto.save_notebook(session, nb)
    _notify_browser(session, nb)

    return _mutation_receipt(session, nb;
        applied=true,
        mutation=Dict{String,Any}(
            "type"      => "move_cell",
            "cell_id"   => string(cell.cell_id),
            "old_index" => old_idx,
            "new_index" => new_idx,
        ),
        cell_ids_run=UUID[],
        execution_status="completed",
    )
end

# ---------------------------------------------------------------------------
# Dispatch table
# ---------------------------------------------------------------------------

function call_tool(session, name, arguments)
    if name == "list_notebooks"
        tool_list_notebooks(session, arguments)
    elseif name == "view_cell_output"
        tool_view_cell_output(session, arguments)
    elseif name == "read_cell"
        tool_read_cell(session, arguments)
    elseif name == "edit_cell"
        tool_edit_cell(session, arguments)
    elseif name == "edit_cells"
        tool_edit_cells(session, arguments)
    elseif name == "add_cell"
        tool_add_cell(session, arguments)
    elseif name == "delete_cell"
        tool_delete_cell(session, arguments)
    elseif name == "execute_cell"
        tool_execute_cell(session, arguments)
    elseif name == "submit_changes"
        tool_submit_changes(session, arguments)
    elseif name == "run_all_cells"
        tool_run_all_cells(session, arguments)
    elseif name == "move_cell"
        tool_move_cell(session, arguments)
    elseif name == "fold_cell"
        tool_fold_cell(session, arguments)
    elseif name == "read_notebook_code"
        tool_read_notebook_code(session, arguments)
    elseif name == "get_cell_order"
        tool_get_cell_order(session, arguments)
    elseif name == "get_execution_order"
        tool_get_execution_order(session, arguments)
    elseif name == "get_cell_dependencies"
        tool_get_cell_dependencies(session, arguments)
    elseif name == "get_cell_dependents"
        tool_get_cell_dependents(session, arguments)
    elseif name == "find_symbol_definitions"
        tool_find_symbol_definitions(session, arguments)
    elseif name == "find_symbol_references"
        tool_find_symbol_references(session, arguments)
    elseif name == "validate_cell"
        tool_validate_cell(session, arguments)
    elseif name == "search_code"
        tool_search_code(session, arguments)
    else
        throw(ArgumentError("unknown_tool::Unknown tool: '$name'"))
    end
end
