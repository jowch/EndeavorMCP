# A notebook's controls (`@bind name Slider(...)` and the like), as `read_cell` shows them under
# `inputs`, and a note on the cells whose results used a control moved away from its default.
# A control's value lives only in the notebook's process and isn't saved: a rerun of the file
# uses the default. An agent that reads a result made at a moved value reports numbers the
# notebook won't reproduce unless it is told.

# The code that describes controls, run in the notebook's process, where the widgets are. It is
# also evaluated into this module, so the tests can describe widgets directly.
const _CONTROLS_CODE = quote
    # A value as the agent reads it, cut to `max` bytes.
    function __endeavor_shown(x; max::Int = 80)
        text = try
            repr(x; context = :compact => true)
        catch
            "(a $(nameof(typeof(x))))"
        end
        text = replace(text, r"\s*\n\s*" => " ")
        sizeof(text) <= max ? text : text[1:prevind(text, max - 2)] * " …"
    end

    # A widget's own text form, when its package gives it one (Masque's describes the picture,
    # its layers and its value), else nothing.
    function __endeavor_own_text(el)
        # Base's own methods print any value's fields, which says nothing a widget's kind doesn't.
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

    # The label a choice has on the page, for widgets that list `value => label` pairs (PlutoUI's
    # Select, Radio, MultiSelect), when it differs from the value.
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

    # What a control allows, after `transform_value`: the page's own values are indices for a
    # Slider and "puiselect-3" for a Select, which would mislead. Empty when it doesn't say.
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

    # One control: `name`, its `line` for `inputs`, and whether it is `moved` from its default,
    # with the value and the default as text. A widget that gives no default (a home-made
    # `html"<input>"`) has none and is never moved: its value starts as `missing`.
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

    # Every control the given cells made on their last run, by cell, in the notebook's module
    # `mod`. A cell with none is left out.
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

# What the runtime last learned about each notebook's controls: `key` says what it was learned
# for (the notebook's process, its control cells' last runs, the page's values), `info` is what
# `__endeavor_controls` answered, and `task` a query still on its way.
mutable struct _Controls
    key::Any
    info::Dict{String,Any}
    task::Union{Task,Nothing}
    task_key::Any
end

const _CONTROLS = Dict{UUID,_Controls}()
const _CONTROLS_LOCK = ReentrantLock()

# How long a snapshot waits for the notebook's process to describe its controls. A process busy
# with code that never yields doesn't answer: the snapshot then shows what was known before.
const _CONTROLS_WAIT = 3.0

_has_control(cell) = occursin("@bind", cell.code)

forget_controls!(notebook_id::UUID) = lock(() -> delete!(_CONTROLS, notebook_id), _CONTROLS_LOCK)

# A finished query's answer becomes what is known. A failed one keeps the last answer, and counts as
# asked for its key, so it isn't asked again until something changes.
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

"""
    controls(session, nb) -> Dict{String,Any}

Each of the notebook's cells that made controls on its last run, by cell id, with each control as
`__endeavor_control` describes it. Asks the notebook's process only when something that could
change the answer changed, and only while no cell runs or waits to: a busy process may not answer
for a long time. Until it answers, the last answer stands. Empty for a notebook with no `@bind`,
and with no process (safe preview). Reads Pluto's internals, so a Pluto release that changes
them gives none rather than failing the snapshot.
"""
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
    key = (objectid(workspace), [(c.cell_id, c.output.last_run_timestamp) for c in with], bonds)
    busy = any(c -> c.running || c.queued, cells)
    # Only the call that starts a query waits for it. A query that outlasts the wait is left to
    # finish, and every call until then answers with what was known before, at once.
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

"""
    moved_input_notes(nb, controls) -> Dict{String,Vector{String}}

For each cell whose upstream includes a control moved away from its default, a note naming it,
by cell id. The control's own cell has its default in its `inputs` line instead. Empty when every
control is at its default, the usual case.
"""
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
