
const _CONTROLS_CODE = quote
    function __endeavor_shown(x; max::Int = 80)
        text = try
            repr(x; context = :compact => true)
        catch
            "(a $(nameof(typeof(x))))"
        end
        text = replace(text, r"\s*\n\s*" => " ")
        sizeof(text) <= max ? text : text[1:prevind(text, max - 2)] * " …"
    end

    function __endeavor_own_text(el)
        own = try
            m = which(show, Tuple{IO, MIME"text/plain", typeof(el)})
            sig = Base.unwrap_unionall(m.sig)
            parentmodule(m.module) !== Base && m.module !== Base && m.module !== Core && sig.parameters[end] !== Any
        catch
            false
        end
        own || return nothing
        text = try
            strip(repr(MIME"text/plain"(), el))
        catch
            return nothing
        end
        isempty(text) && return nothing
        sizeof(text) <= 300 ? String(text) : text[1:prevind(text, 298)] * " …"
    end

    function __endeavor_label(el, value)
        options = try
            getfield(el, :options)
        catch
            return nothing
        end
        options isa AbstractVector || return nothing
        for o in options
            o isa Pair && isequal(o.first, value) || continue
            label = o.second
            label isa AbstractString && label != string(value) && return String(label)
            return nothing
        end
        return nothing
    end

    function __endeavor_evenly_spaced(values)
        length(values) >= 3 && all(v -> v isa Real && !(v isa Bool), values) || return nothing
        d = values[2] - values[1]
        iszero(d) && return nothing
        for i in 3:length(values)
            isapprox(values[i] - values[i-1], d; rtol = 1e-6) || return nothing
        end
        return d
    end

    function __endeavor_allows(el, default, possible, transform)
        (possible === :NotGiven || nameof(typeof(possible)) === :NotGiven) && return ""
        if nameof(typeof(possible)) === :InfinitePossibilities
            return default isa Number ? ", any number" : default isa AbstractString ? ", any text" : ""
        end
        n = try
            length(possible)
        catch
            return ""
        end
        n == 0 && return ""
        page = n <= 1000 ? collect(possible) : [first(possible), last(possible)]
        values = [transform(el, v) for v in page]
        all(v -> v isa Bool, values) && return ""
        if n > 1000
            what = values[1] isa Real ? "values" : "choices"
            return " $n $what, from $(__endeavor_shown(values[1])) to $(__endeavor_shown(values[end]))"
        end
        d = __endeavor_evenly_spaced(values)
        if d !== nothing
            range = try
                getfield(el, :values)
            catch
                nothing
            end
            step = range isa AbstractRange && length(range) == n ? Base.step(range) : d isa AbstractFloat ? round(d; sigdigits = 12) : d
            return " $(__endeavor_shown(values[1])) to $(__endeavor_shown(values[end])), step $(__endeavor_shown(step))"
        end
        if n <= 10
            choice(v) = (label = __endeavor_label(el, v); label === nothing ? __endeavor_shown(v) : "$label = $(__endeavor_shown(v))")
            return " of " * join(map(choice, values), ", ")
        end
        return " $n choices, from $(__endeavor_shown(values[1])) to $(__endeavor_shown(values[end]))"
    end

    function __endeavor_control(name, el, value, initial, possible, transform)
        default = try
            Core.applicable(Base.get, el) ? Base.get(el) : initial(el)
        catch
            missing
        end
        kind = string(nameof(typeof(el)))
        own = __endeavor_own_text(el)
        allows = own === nothing ? (try __endeavor_allows(el, default, possible, transform) catch; "" end) : ""
        shown = value === nothing ? "not set" : __endeavor_shown(something(value))
        moved = value !== nothing && !ismissing(default) && !isequal(something(value), default)
        line = "$name: $kind$allows, value $shown"
        moved && (line *= " (default $(__endeavor_shown(default)))")
        own === nothing || (line *= ". $own")
        Dict{String,Any}("name" => string(name), "line" => line, "moved" => moved, "value" => shown, "default" => __endeavor_shown(default))
    end

    function __endeavor_controls(mod::Symbol, cells)
        R = Main.PlutoRunner
        m = isdefined(Main, mod) ? getfield(Main, mod) : nothing
        out = Dict{String,Any}()
        for id in cells
            names = sort!(collect(R.get_bond_names(id)))
            isempty(names) && continue
            out[string(id)] = map(names) do name
                el = get(R.registered_bond_elements, name, nothing)
                value = m !== nothing && isdefined(m, name) ? Some(getfield(m, name)) : nothing
                possible = try R.possible_bond_values_ref[](el) catch; :NotGiven end
                __endeavor_control(name, el, value, R.initial_value_getter_ref[], possible, R.transform_value_ref[])
            end
        end
        out
    end
end

Core.eval(@__MODULE__, _CONTROLS_CODE)

mutable struct _Controls
    key::Any
    info::Dict{String,Any}
    task::Union{Task,Nothing}
    task_key::Any
end

const _CONTROLS = Dict{UUID,_Controls}()
const _CONTROLS_LOCK = ReentrantLock()

const _CONTROLS_WAIT = 3.0

_has_control(cell) = occursin("@bind", cell.code)

forget_controls!(notebook_id::UUID) = lock(() -> delete!(_CONTROLS, notebook_id), _CONTROLS_LOCK)

function _adopt!(entry::_Controls)
    task = entry.task
    (task === nothing || !istaskdone(task)) && return nothing
    istaskfailed(task) || (entry.info = fetch(task))
    entry.key = entry.task_key
    entry.task = nothing
    return nothing
end

function _controls_query(workspace, ids)
    code = _CONTROLS_CODE
    module_name = workspace.module_name
    Pluto.WorkspaceManager.Malt.remote_eval_fetch(workspace.worker, quote
        isdefined(Main, :__endeavor_controls) || Core.eval(Main, $(QuoteNode(code)))
        Base.invokelatest(Base.invokelatest(getglobal, Main, :__endeavor_controls), $(QuoteNode(module_name)), $ids)
    end)
end

function controls(session, nb)
    try
        _controls(session, nb)
    catch
        Dict{String,Any}()
    end
end

function _controls(session, nb)
    cells = [nb.cells_dict[id] for id in nb.cell_order if haskey(nb.cells_dict, id)]
    with = filter(_has_control, cells)
    if isempty(with) || nb.process_status != Pluto.ProcessStatus.ready
        forget_controls!(nb.notebook_id)
        return Dict{String,Any}()
    end
    workspace = Pluto.WorkspaceManager.get_workspace((session, nb); allow_creation = false)
    workspace === nothing && return Dict{String,Any}()
    bonds = try
        hash(sort!([(k, v.value) for (k, v) in nb.bonds]; by = first))
    catch
        UInt(0)
    end
    key = (objectid(workspace), [(c.cell_id, c.output.last_run_timestamp) for c in cells], bonds)
    busy = any(c -> c.running || c.queued, cells)
    task = lock(_CONTROLS_LOCK) do
        entry = get!(() -> _Controls(nothing, Dict{String,Any}(), nothing, nothing), _CONTROLS, nb.notebook_id)
        _adopt!(entry)
        (entry.key == key || busy || entry.task !== nothing) && return nothing
        ids = [c.cell_id for c in with]
        entry.task = @async _controls_query(workspace, ids)
        entry.task_key = key
        entry.task
    end
    if task !== nothing && timedwait(() -> istaskdone(task), _CONTROLS_WAIT; pollint = 0.02) === :ok
        lock(() -> _adopt!(_CONTROLS[nb.notebook_id]), _CONTROLS_LOCK)
    end
    return lock(() -> haskey(_CONTROLS, nb.notebook_id) ? _CONTROLS[nb.notebook_id].info : Dict{String,Any}(), _CONTROLS_LOCK)
end

function moved_input_notes(nb, controls)
    notes = Dict{String,Vector{String}}()
    isempty(controls) && return notes
    try
        for (cell_id, list) in controls, c in list
            c["moved"] === true || continue
            name = Symbol(c["name"])
            readers = Pluto.PlutoDependencyExplorer.where_referenced(nb.topology, Set([name]))
            isempty(readers) && continue
            order = Pluto.PlutoDependencyExplorer.topological_order(nb.topology, collect(readers))
            note = "uses_moved_input::This result used $(c["name"]) = $(c["value"]), set on the page. The file's default is $(c["default"]), so a rerun of the file gives other results."
            for reader in order.runnable
                id = string(reader.cell_id)
                id == cell_id && continue
                push!(get!(Vector{String}, notes, id), note)
            end
        end
    catch
    end
    return notes
end
