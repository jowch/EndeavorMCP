const _CELL_MARKER_PREFIX = "# ╔═╡ "

const _PLUTO_PROJECT_TOML_CELL_ID = UUID("00000000-0000-0000-0000-000000000001")
const _PLUTO_MANIFEST_TOML_CELL_ID = UUID("00000000-0000-0000-0000-000000000002")

function _is_manifest_cell(cell_id::UUID)
    return cell_id == _PLUTO_PROJECT_TOML_CELL_ID || cell_id == _PLUTO_MANIFEST_TOML_CELL_ID
end

function _is_fake_bind_shim(cell::Pluto.Cell)
    code = cell.code
    if occursin(Pluto.PlutoRunner.fake_bind, code)
        return true
    end
    stripped = lstrip(code)
    return startswith(stripped, "macro bind")
end

function _is_markdown_cell(cell::Pluto.Cell)
    return startswith(lstrip(cell.code), "md\"")
end

function _should_exclude_from_projection(cell::Pluto.Cell; include_markdown::Bool)
    _is_manifest_cell(cell.cell_id) && return true
    _is_fake_bind_shim(cell) && return true
    Pluto.must_be_commented_in_file(cell) && return true
    _is_markdown_cell(cell) && !include_markdown && return true
    return false
end

function _cell_projection_body(cell::Pluto.Cell; include_markdown::Bool)
    if _is_markdown_cell(cell)
        include_markdown || return nothing
        return "# md:\n" * cell.code
    end
    if isempty(strip(cell.code))
        return "# (empty)"
    end
    return cell.code
end

function _format_cell_block(cell::Pluto.Cell; include_markdown::Bool)
    body = _cell_projection_body(cell; include_markdown=include_markdown)
    body === nothing && return nothing
    return _CELL_MARKER_PREFIX * string(cell.cell_id) * "\n" * body
end

function _cells_in_order(nb::Pluto.Notebook, order::AbstractString)
    if order == "visual"
        return [get(nb.cells_dict, cid, nothing) for cid in nb.cell_order]
    elseif order == "execution"
        topo = Pluto.topological_order(nb)
        return collect(topo.runnable)
    else
        throw(ArgumentError("invalid_order::order must be 'execution' or 'visual', got '$order'"))
    end
end

function _visual_order_cell_ids(nb::Pluto.Notebook)
    return [string(cid) for cid in nb.cell_order]
end

function _execution_order_cell_ids(nb::Pluto.Notebook)
    topo = Pluto.topological_order(nb)
    return [string(cell.cell_id) for cell in topo.runnable]
end

function _project_notebook_code(nb::Pluto.Notebook; order::AbstractString, include_markdown::Bool)
    blocks = String[]
    cell_ids = String[]
    for cell in _cells_in_order(nb, order)
        cell === nothing && continue
        _should_exclude_from_projection(cell; include_markdown=include_markdown) && continue
        block = _format_cell_block(cell; include_markdown=include_markdown)
        block === nothing && continue
        push!(blocks, block)
        push!(cell_ids, string(cell.cell_id))
    end
    code = join(blocks, "\n\n")
    return cell_ids, code
end

function _stale_cell_ids(notebook_id::UUID, nb::Pluto.Notebook)
    return [
        string(cid) for cid in keys(nb.cells_dict)
        if is_stale(notebook_id, cid)
    ]
end

function tool_read_notebook_code(session, args)
    nb               = _get_notebook(session, args["notebook_id"])
    order            = get(args, "order", "execution")
    include_markdown = get(args, "include_markdown", false)

    cell_ids, code = _project_notebook_code(nb; order=order, include_markdown=include_markdown)

    for cid_str in cell_ids
        cid = UUID(cid_str)
        cell = get(nb.cells_dict, cid, nothing)
        cell === nothing && continue
        record_read!(nb.notebook_id, cid, cell.code)
    end

    return Dict{String,Any}(
        "notebook_id"     => string(nb.notebook_id),
        "path"            => nb.path,
        "order"           => order,
        "cell_ids"        => cell_ids,
        "stale_cell_ids"  => _stale_cell_ids(nb.notebook_id, nb),
        "pending_run"     => [string(id) for id in pending_run_ids(nb.notebook_id)],
        "code"            => code,
    )
end

function tool_get_cell_order(session, args)
    nb = _get_notebook(session, args["notebook_id"])
    return Dict{String,Any}(
        "notebook_id" => string(nb.notebook_id),
        "cell_ids"    => _visual_order_cell_ids(nb),
    )
end

function tool_get_execution_order(session, args)
    nb = _get_notebook(session, args["notebook_id"])
    return Dict{String,Any}(
        "notebook_id" => string(nb.notebook_id),
        "cell_ids"    => _execution_order_cell_ids(nb),
    )
end
