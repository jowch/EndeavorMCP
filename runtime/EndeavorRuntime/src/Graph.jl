# ---------------------------------------------------------------------------
# Graph & validation tools (Phase 2 — debugging, not default workflow)
# ---------------------------------------------------------------------------

function _ensure_topology!(nb::Pluto.Notebook)
    Pluto.update_dependency_cache!(nb)
    return nb.topology
end

function _parse_symbol_arg(args, key::String)
    sym_str = args[key]
    try
        return Symbol(sym_str)
    catch
        throw(ArgumentError("invalid_symbol::Invalid symbol: '$sym_str'"))
    end
end

function _line_hint_for_symbol(code::String, sym::Symbol)
    sym_str = string(sym)
    for (i, line) in enumerate(split(code, '\n'; keepempty=true))
        occursin(sym_str, line) && return i
    end
    return nothing
end

function _unique_cell_ids(cells)
    seen = Set{UUID}()
    ids = String[]
    for cell in cells
        cid = cell.cell_id
        cid in seen && continue
        push!(seen, cid)
        push!(ids, string(cid))
    end
    return ids
end

function _collect_upstream_cells(topology, cell::Pluto.Cell)
    upstream = Pluto.MoreAnalysis.upstream_recursive(topology, [cell])
    delete!(upstream, cell)
    return collect(upstream)
end

function _collect_downstream_cells(topology, cell::Pluto.Cell)
    downstream = Pluto.MoreAnalysis.downstream_recursive(topology, [cell])
    delete!(downstream, cell)
    return collect(downstream)
end

function _scan_symbol_in_topology(nb::Pluto.Notebook, sym::Symbol, field::Symbol)
    results = Dict{String,Any}[]
    for cell in Pluto.PlutoDependencyExplorer.all_cells(nb.topology)
        node = nb.topology.nodes[cell]
        sym_set = getproperty(node, field)
        sym ∈ sym_set || continue
        hint = _line_hint_for_symbol(cell.code, sym)
        push!(results, Dict{String,Any}(
            "cell_id"   => string(cell.cell_id),
            "line_hint" => hint,
        ))
    end
    return results
end

function _snippet_around_match(code::String, query::String; context::Int=40)
    idx = findfirst(query, code)
    idx === nothing && return nothing
    start = max(1, first(idx) - context)
    stop = min(ncodeunits(code), last(idx) + context)
    snippet = strip(code[start:stop])
    return isempty(snippet) ? nothing : snippet
end

function _validation_error(type::String, message::String)
    Dict{String,Any}("type" => type, "message" => message)
end

function _parse_validation_errors(nb::Pluto.Notebook, cell::Pluto.Cell, code::String)
    errors = Dict{String,Any}[]

    if !Pluto.is_single_expression(code)
        push!(errors, _validation_error(
            "pluto_multi_expression",
            "Cell must contain a single expression",
        ))
    end

    temp_cell = Pluto.Cell(; cell_id=cell.cell_id, code=code)
    expr = Pluto.parse_custom(nb, temp_cell)

    if Meta.isexpr(expr, :toplevel, 2) &&
       Meta.isexpr(expr.args[2], :call, 2) &&
       expr.args[2].args[1] == :(PlutoRunner.throw_syntax_error)
        push!(errors, _validation_error("syntax_error", string(expr.args[2].args[2])))
    end

    return errors
end

function tool_get_cell_dependencies(session, args)
    nb   = _get_notebook(session, args["notebook_id"])
    cell = _get_cell(nb, args["cell_id"])
    topo = _ensure_topology!(nb)

    upstream_cells = _collect_upstream_cells(topo, cell)
    symbols = [string(sym) for sym in sort(collect(topo.nodes[cell].references); by=string)]

    return Dict{String,Any}(
        "upstream" => _unique_cell_ids(upstream_cells),
        "symbols"  => symbols,
    )
end

function tool_get_cell_dependents(session, args)
    nb   = _get_notebook(session, args["notebook_id"])
    cell = _get_cell(nb, args["cell_id"])
    topo = _ensure_topology!(nb)

    downstream_cells = _collect_downstream_cells(topo, cell)

    return Dict{String,Any}(
        "downstream" => _unique_cell_ids(downstream_cells),
    )
end

function tool_find_symbol_definitions(session, args)
    nb  = _get_notebook(session, args["notebook_id"])
    sym = _parse_symbol_arg(args, "symbol")
    _ensure_topology!(nb)
    return _scan_symbol_in_topology(nb, sym, :definitions)
end

function tool_find_symbol_references(session, args)
    nb  = _get_notebook(session, args["notebook_id"])
    sym = _parse_symbol_arg(args, "symbol")
    _ensure_topology!(nb)
    return _scan_symbol_in_topology(nb, sym, :references)
end

function tool_validate_cell(session, args)
    nb   = _get_notebook(session, args["notebook_id"])
    cell = _get_cell(nb, args["cell_id"])
    code = args["code"]

    errors = _parse_validation_errors(nb, cell, code)

    return Dict{String,Any}(
        "valid"  => isempty(errors),
        "errors" => errors,
    )
end

function tool_search_code(session, args)
    nb    = _get_notebook(session, args["notebook_id"])
    query = args["query"]

    results = Dict{String,Any}[]
    seen = Set{UUID}()
    for cell in values(nb.cells_dict)
        cell.cell_id in seen && continue
        push!(seen, cell.cell_id)
        snippet = _snippet_around_match(cell.code, query)
        snippet === nothing && continue
        push!(results, Dict{String,Any}(
            "cell_id" => string(cell.cell_id),
            "snippet" => snippet,
        ))
    end
    return results
end
