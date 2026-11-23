# The Pluto adapter's side of the engine interface (docs/runtime-core.md). The
# core (`endeavor core`) calls `POST /adapter` with {"method", "params"}
# and reads the reply's "result" or "error". The methods (see `adapter_call`):
# status, open, new, and for a notebook_id snapshot, graph, shutdown, apply,
# run, interrupt, restart, allow_execution, move, render_png and validate. What happens in
# Pluto reaches the core as notifications on `GET /notifications`, one
# `data: {"method", "params"}` line each: notebook_opened, notebook_shut_down
# (it left the session), file_saved, execution_done, cell_state (every cell's
# code and run state: Pluto's hook doesn't say which cell changed),
# topology_changed, run_finished (an unwaited run ended: its `run` id and the
# cells it finished), and process_exited (its own process ended by itself; the
# cells that were running).
#
# Each notification carries `seq`, a counter that `apply` also advances; its
# reply and `snapshot`'s carry it too. A snapshot or notification numbered
# below an `apply`'s `seq` may show the notebook from before that change.

const _NOTIFY_LOCK = ReentrantLock()
const _NOTIFY_SUBSCRIBERS = Set{Channel{String}}()
const _SEQ = Ref(0)
# Each notebook's dependency graph as last announced.
const _TOPOLOGY_SENT = Dict{UUID,Any}()
# Cells the tools edited or added and that haven't run since, per notebook, and
# the topology with their new code (`current_topology`). Pluto's own topology
# stays as of the last run: a run compares it with the new one to know what the
# edits changed, as a run from Pluto's page does. Analysing a staged edit into
# `nb.topology` left that comparison empty, so a run after it didn't run the
# other cell of a conflict it fixed, nor drop a name it removed (L1 on Haiku, 11-20).
const _STAGED = Dict{UUID,Any}()

function notify!(method::AbstractString, params::AbstractDict)::Nothing
    lock(_NOTIFY_LOCK) do
        json = JSON.json(Dict("method" => method, "params" => params, "seq" => (_SEQ[] += 1)))
        foreach(ch -> put!(ch, json), _NOTIFY_SUBSCRIBERS)
    end
    return nothing
end

_seq() = lock(() -> _SEQ[], _NOTIFY_LOCK)
_next_seq!() = lock(() -> _SEQ[] += 1, _NOTIFY_LOCK)

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
        topology = current_topology(nb)
        get(_TOPOLOGY_SENT, nb.notebook_id, nothing) === topology && return false
        _TOPOLOGY_SENT[nb.notebook_id] = topology
        true
    end
    changed && _notify_notebook!("topology_changed", nb)
    return nothing
end

function forget_topology!(notebook_id::UUID)
    lock(() -> (delete!(_TOPOLOGY_SENT, notebook_id); delete!(_STAGED, notebook_id)), _NOTIFY_LOCK)
    return nothing
end

# Stage cells whose code the tools changed, for `current_topology`.
function _stage!(nb, cells)
    lock(_NOTIFY_LOCK) do
        staged = get!(() -> (cells = Set{Pluto.Cell}(), base = nothing, topology = nothing), _STAGED, nb.notebook_id)
        _STAGED[nb.notebook_id] = (cells = union!(staged.cells, cells), base = nothing, topology = nothing)
    end
    return nothing
end

# The notebook's topology with the staged edits analysed: Pluto's own (as of
# the last run) when none is left. A staged cell drops out once a run (ours or
# the page's) has analysed its current code, or once it is deleted.
function current_topology(nb)
    lock(_NOTIFY_LOCK) do
        staged = get(_STAGED, nb.notebook_id, nothing)
        staged === nothing && return nb.topology
        base = nb.topology
        left = filter(c -> haskey(nb.cells_dict, c.cell_id) && base.codes[c].code != c.code, staged.cells)
        if isempty(left)
            delete!(_STAGED, nb.notebook_id)
            return base
        end
        staged.base === base && staged.topology !== nothing && left == staged.cells && return staged.topology
        topology = Pluto.updated_topology(base, nb, collect(left))
        _STAGED[nb.notebook_id] = (cells = left, base = base, topology = topology)
        return topology
    end
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

# Whether a run is over: its Pluto task has ended, or its notebook's own
# process is gone or the notebook has left the session, which no task ending
# would say.
function _run_over(session, nb, run)
    istaskdone(run) || nb.process_status === Pluto.ProcessStatus.no_process ||
        get(session.notebooks, nb.notebook_id, nothing) !== nb
end

# Whether `over()` became true within `timeout` seconds.
_wait_over(over, timeout) = timedwait(over, timeout; pollint=0.05) === :ok

_unfinished(cell) = cell.running || cell.queued

# Pluto marks the cells it runs queued itself; a run task that failed leaves
# the cells marked by hand in `run_cells!` queued.
function _release_cells!(nb, cells)
    foreach(c -> c.queued = false, cells)
    notify_state!(nb)
    return nothing
end

# What happens when a run ends that no caller is waiting for: a failed task
# releases the cells, and `run_finished` says which run (`id`, the `run` that
# `run_cells!` answered) ended and which of its cells finished. A cell can end
# the run unfinished, queued again by a later run (a restart's). Waits as long
# as the run takes, which only the notebook's process ending or leaving the
# session cuts short.
function _watch_run!(session, nb, cells, run, id)
    _wait_over(() -> _run_over(session, nb, run), Inf)
    istaskfailed(run) && _release_cells!(nb, cells)
    notify!("run_finished", Dict{String,Any}("notebook_id" => string(nb.notebook_id), "run" => id, "cells" => [string(c.cell_id) for c in cells if !_unfinished(c)]))
    return nothing
end

# Watch a run that goes on (`_watch_run!`); its id goes in the reply as `run`.
function _watch!(result, session, nb, cells, run)
    id = string(uuid4())
    result["run"] = id
    @async _watch_run!(session, nb, cells, run, id)
    return nothing
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

# `inputs` are the cell's controls as `read_cell` lists them, and `moved` the notes on a cell
# whose result used a control moved away from its default (Controls.jl).
function _snapshot_cell(cell; inputs = nothing, moved = String[])
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
        # A running cell's logs are its current run's so far, not its last run's.
        "notes"    => cell.running ? String[] : vcat(_cell_notes(cell), moved),
    )
    inputs === nothing || (d["inputs"] = [c["line"] for c in inputs])
    err = _cell_output_error(cell)
    err !== nothing && (d["error"] = err)
    return d
end

# Pluto's package step as it goes, or nothing when none is under way. Pluto
# adds, installs and precompiles a notebook's packages inside its run task,
# before any cell runs, with the cells queued; the first time a package is used
# that takes minutes. `step` is the part under way, `packages` the ones it is
# busy with (none until Pluto says), `seconds` how long Pluto's package work
# has taken so far, and `last_line` the last line of its package log, with any
# user name or token in a URL taken out (a private registry's URL may carry
# one). It reads Pluto's internals, so if a Pluto release changes them it
# reports nothing rather than failing the snapshot.
const _PKG_STEPS = Dict(
    :analysis => "checking", :registry_update => "updating the package registry",
    :waiting_for_others => "waiting for another notebook's packages",
    :resolve => "resolving", :remove => "removing", :add => "adding",
    :instantiate1 => "installing", :instantiate2 => "installing", :instantiate => "installing",
    :precompile => "precompiling", :write_project_toml => "checking",
)

_business_going(b) = b.started_at !== nothing && b.finished_at === nothing

function package_step(nb)
    try
        _package_step(nb)
    catch
        nothing
    end
end

# A URL's `user:token@` taken out of a line of text.
_without_userinfo(line) = replace(line, r"(\w+://)[^/\s@]+@" => s"\1")

function _package_step(nb)
    pkg = lock(() -> get(nb.status_tree.subtasks, :pkg, nothing), nb.status_tree.lock)
    (pkg === nothing || !_business_going(pkg)) && return nothing
    going = lock(() -> [b for b in values(pkg.subtasks) if _business_going(b)], pkg.lock)
    sort!(going; by = b -> b.started_at)
    step = isempty(going) ? "checking" : get(_PKG_STEPS, last(going).name, string(last(going).name))
    log = replace(get(nb.nbpkg_terminal_outputs, "nbpkg_sync", ""), r"\e\[[0-9;]*[A-Za-z]" => "", '\r' => '\n')
    lines = filter(l -> !isempty(l) && !all(==('='), l), strip.(split(log, '\n')))
    Dict{String,Any}(
        "step"      => step,
        # Pluto lists its own log, `nbpkg_sync`, with the packages.
        "packages"  => filter(!=("nbpkg_sync"), nb.nbpkg_busy_packages),
        "seconds"   => round(Int, time() - pkg.started_at),
        "last_line" => isempty(lines) ? nothing : first(_without_userinfo(String(last(lines))), 300),
    )
end

# Asking before a new package (#397): a run whose cells would make Pluto add a
# package the notebook doesn't have yet (not in its Project.toml, not a
# standard library) waits for the user's OK instead of running, when the
# caller says new packages ask (`ask_new`). Its cells stay queued, and the
# snapshot's `packages` says `asking`, with the packages, as an R notebook's
# large download does; `approve_install` is the OK, and the cells run. A yes
# holds for those packages in this notebook while it is open. A notebook that
# manages its own environment (`Pkg.activate`) adds nothing, so asks nothing.
const _ASKING = Dict{UUID,Any}()        # notebook => (cells = the held cells, packages)
const _APPROVED = Dict{UUID,Set{String}}()  # notebook => packages the user said yes to
const _ASKING_LOCK = ReentrantLock()
# Cells that waited for an OK no longer needed (an edit dropped the package, or it came another
# way): released, they run with the notebook's next run, which is what was asked of them (#523).
# Each with its last run's time then: one that has run since (the user's run, or as a dependent)
# isn't run again.
const _LEFT = Dict{UUID,Vector{Tuple{Pluto.Cell,Float64}}}()

function new_packages(nb)
    ctx = nb.nbpkg_ctx
    ctx === nothing && return String[]
    have = Set(String.(keys(Pluto.PkgCompat.project(ctx).dependencies)))
    approved = lock(() -> get(_APPROVED, nb.notebook_id, Set{String}()), _ASKING_LOCK)
    topology = Pluto.updated_topology(nb.topology, nb, nb.cells)
    uses = String.(collect(Pluto.external_package_names(topology)))
    sort!(filter(p -> p ∉ have && p ∉ approved && !Pluto.PkgCompat.is_stdlib(p) && Pluto.PkgCompat.package_exists(p), uses))
end

_asking(nb) = lock(() -> get(_ASKING, nb.notebook_id, nothing), _ASKING_LOCK)

# `keep`: the cells still want to run, with the next run (`_LEFT`). A Stop's are dropped.
function _forget_asking!(nb; keep::Bool=false)
    held = lock(_ASKING_LOCK) do
        a = pop!(_ASKING, nb.notebook_id, nothing)
        held = a === nothing ? Pluto.Cell[] : a.cells
        keep ? append!(get!(Vector{Tuple{Pluto.Cell,Float64}}, _LEFT, nb.notebook_id), [(c, c.output.last_run_timestamp) for c in held]) : delete!(_LEFT, nb.notebook_id)
        held
    end
    isempty(held) || _release_cells!(nb, held)
    return nothing
end

forget_packages!(notebook_id::UUID) = lock(() -> (delete!(_ASKING, notebook_id); delete!(_APPROVED, notebook_id); delete!(_LEFT, notebook_id)), _ASKING_LOCK)

# The user's yes to the packages a notebook asks about: they install, and the
# held cells run. `packages`, when given, are the ones the user was shown: the
# yes covers only those (#418). A package added since asks again, and the held
# cells wait for that too; a yes that names none of what waits approves nothing.
function approve_install!(session, nb, timeout::Real; packages=nothing)
    seen = packages === nothing ? nothing : Set(String[string(p) for p in packages])
    a, ok = lock(_ASKING_LOCK) do
        a = get(_ASKING, nb.notebook_id, nothing)
        a === nothing && return (nothing, String[])
        ok = seen === nothing ? a.packages : filter(in(seen), a.packages)
        isempty(ok) && return (a, ok)
        delete!(_ASKING, nb.notebook_id)
        union!(get!(Set{String}, _APPROVED, nb.notebook_id), ok)
        (a, ok)
    end
    a === nothing && throw(ArgumentError("nothing_to_approve::Nothing in this notebook waits for the user's OK"))
    cells = [string(c.cell_id) for c in a.cells]
    isempty(ok) && return Dict{String,Any}("approved" => false, "packages" => String[], "asking" => a.packages, "cells" => cells)
    # Still asks about a package that is new since the question.
    ran = run_cells!(session, nb, a.cells; wait=false, timeout, ask_new=true)
    return merge!(Dict{String,Any}("approved" => true, "packages" => ok, "cells" => cells), ran)
end

function _asking_step(nb)
    a = _asking(nb)
    a === nothing && return nothing
    # The packages were added another way, or the code changed so that nothing new is used:
    # nothing waits for an OK any more. A snapshot releases the held cells then, on purpose, so an
    # edit unblocks without another call; it costs a topology pass per snapshot while one asks.
    if isempty(new_packages(nb))
        _forget_asking!(nb; keep=true)
        return nothing
    end
    Dict{String,Any}(
        "step" => "asking", "new" => true, "packages" => a.packages, "seconds" => 0, "last_line" => nothing,
        "download" => Dict{String,Any}("bytes" => nothing, "packages" => [Dict{String,Any}("package" => p) for p in a.packages]),
    )
end

function snapshot(nb; controls = Dict{String,Any}())
    moved = moved_input_notes(nb, controls)
    cell(id) = _snapshot_cell(nb.cells_dict[id]; inputs = get(controls, string(id), nothing), moved = get(moved, string(id), String[]))
    Dict{String,Any}(
        "notebook_id"       => string(nb.notebook_id),
        "path"              => nb.path,
        "cell_order"        => [string(id) for id in nb.cell_order],
        "process_status"    => string(nb.process_status),
        "execution_allowed" => Pluto.will_run_code(nb),
        "safe_preview"      => nb.process_status === Pluto.ProcessStatus.waiting_for_permission,
        "exited"            => exited_cells(nb),
        "packages"          => something(_asking_step(nb), Some(package_step(nb))),
        "cells"          => [cell(id) for id in nb.cell_order],
    )
end

# Pluto's dependency data with the staged edits (`current_topology`),
# cells in the order Pluto analysed them. `fresh` analyses the notebook as it is
# now without keeping the result; `refresh` first updates the dependency cache
# Pluto's page shows; `edges` adds each cell's direct upstream and downstream
# cells; `packages` the packages each loads (`using`/`import`), sorted.
function graph(nb; fresh::Bool=false, refresh::Bool=false, edges::Bool=false, packages::Bool=false)
    refresh && Pluto.update_dependency_cache!(nb)
    topology = fresh ? Pluto.updated_topology(nb.topology, nb, nb.cells) : current_topology(nb)
    order = fresh || topology !== nb.topology ? Pluto.PlutoDependencyExplorer.topological_order(topology) : Pluto.topological_order(nb)
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
# assigns a new vector. Cells whose code changed are staged (`_stage!`); the file is
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
    isempty(changed) || _stage!(nb, changed)
    Pluto.save_notebook(session, nb)
    _notify_browser(session, nb)
    return Dict{String,Any}("inserted" => inserted, "seq" => _next_seq!())
end

# A run's cells and the staged cells (`_stage!`) downstream of them, which the
# run reaches and runs anyway. Pluto analyses only a run's own cells, so a staged
# cell left out would be missing from the run (a new cell) or run with its old
# names still defined (an edited one).
function _with_staged_downstream(nb, cells)
    topology = current_topology(nb)
    staged = lock(() -> (s = get(_STAGED, nb.notebook_id, nothing); s === nothing ? Pluto.Cell[] : collect(s.cells)), _NOTIFY_LOCK)
    isempty(staged) && return cells
    # Reached through the staged edits, or through what a staged cell used before them.
    reached = Set{Pluto.Cell}()
    for t in (topology, nb.topology)
        order = Pluto.PlutoDependencyExplorer.topological_order(t, collect(cells))
        union!(reached, order.runnable, keys(order.errable))
    end
    return vcat(collect(cells), [c for c in staged if c in reached && !(c in cells)])
end

# Run cells; none is Pluto's reactive cleanup after a delete. Not accepted when
# the notebook won't run code (safe preview, a stopped process). With `wait`,
# the reply says which cells finished within `timeout` seconds, which cells of
# the notebook were still running or queued then, and `exited` the cells that
# were running if its process ended by itself meanwhile. A run that goes on,
# waited for or not, is watched (`_watch_run!`): the reply's `run` names it,
# and a `run_finished` notification with that `run` says when it ends.
function run_cells!(session, nb, cells; wait::Bool, timeout::Real, ask_new::Bool=false)
    accepted = Pluto.will_run_code(nb)
    # Cells released from a wait for an OK no longer needed run with this run (#523).
    if accepted
        left = lock(() -> pop!(_LEFT, nb.notebook_id, Tuple{Pluto.Cell,Float64}[]), _ASKING_LOCK)
        due = [c for (c, ran) in left if haskey(nb.cells_dict, c.cell_id) && c.output.last_run_timestamp == ran && !_unfinished(c)]
        cells = unique!(vcat(collect(cells), due))
    end
    # A notebook that asks already holds these cells too, and asks again with
    # what is new now (an edit may have dropped or added a package).
    if accepted && (ask_new || _asking(nb) !== nothing)
        held = lock(() -> (a = get(_ASKING, nb.notebook_id, nothing); a === nothing ? Pluto.Cell[] : a.cells), _ASKING_LOCK)
        # Less any held cell deleted since.
        cells = unique!(filter(c -> haskey(nb.cells_dict, c.cell_id), vcat(held, collect(cells))))
        new = isempty(cells) ? String[] : new_packages(nb)
        if !isempty(new)
            foreach(c -> c.queued = true, cells)
            lock(() -> _ASKING[nb.notebook_id] = (cells = cells, packages = new), _ASKING_LOCK)
            notify_state!(nb)
            _notify_browser(session, nb)
            return Dict{String,Any}("accepted" => true, "asking" => new, "process_status" => string(nb.process_status),
                                    "completed" => String[], "timed_out" => [string(c.cell_id) for c in cells])
        end
        lock(() -> delete!(_ASKING, nb.notebook_id), _ASKING_LOCK)
    end
    # Pluto marks cells queued only inside its run task, after package sync;
    # mark them now, as its own run button does, so a caller that looks right
    # away sees them waiting.
    roots = _with_staged_downstream(nb, cells)
    accepted && foreach(c -> c.queued = true, roots)
    run = Pluto.update_save_run!(session, nb, roots; run_async=true, save=true)
    result = Dict{String,Any}("accepted" => accepted, "process_status" => string(nb.process_status))
    if accepted && wait
        over = _wait_over(() -> _run_over(session, nb, run), timeout)
        result["process_status"] = string(nb.process_status)
        exited = exited_cells(nb)
        if exited !== nothing
            result["exited"] = exited
        elseif over && istaskfailed(run)
            _release_cells!(nb, roots)
            fetch(run)
        end
        result["completed"] = [string(c.cell_id) for c in cells if !_unfinished(c)]
        result["timed_out"] = [string(c.cell_id) for c in (over ? cells : nb.cells) if _unfinished(c)]
        over || _watch!(result, session, nb, cells, run)
    elseif accepted && !isempty(cells)
        _watch!(result, session, nb, cells, run)
    end
    _notify_browser(session, nb)
    return result
end

# A notebook's process is this Julia started again (Malt runs `Base.julia_cmd()`, from `Sys.BINDIR`).
# juliaup removes a version it updates past even while that version runs, and then no notebook can
# start a process until Julia restarts: Malt's spawn fails with ENOENT. What would start one asks
# first, so it says why and what to do, and nothing that runs now is stopped for a start that can't
# happen. The path is a Ref so the tests can point it somewhere empty.
const _JULIA_EXE = Ref{String}("")
_julia_exe() = isempty(_JULIA_EXE[]) ? joinpath(Sys.BINDIR, Base.julia_exename()) : _JULIA_EXE[]

function _require_julia()
    isfile(_julia_exe()) && return nothing
    throw(ArgumentError(
        "julia_removed::Julia $VERSION, which this Endeavor runs on, was removed from $(dirname(dirname(_julia_exe()))) " *
        "(juliaup removes a version when it updates past it), so no notebook can start. " *
        "Restart Julia to use the Julia installed now. Notebooks running now keep running until then, " *
        "though loading a package they haven't loaded yet may fail",
    ))
end

# Pluto's "Run notebook code" for a notebook in safe preview: restart its
# process, then run every cell (`run`) or leave it to start on the next run.
function allow_execution!(session, nb, run::Bool, timeout::Real; ask_new::Bool=false)
    ps = nb.process_status
    ps === Pluto.ProcessStatus.ready &&
        return Dict{String,Any}("already_allowed" => true, "ran" => false, "process_status" => string(ps))
    ps !== Pluto.ProcessStatus.waiting_for_permission &&
        throw(ArgumentError("execution_not_gated::Notebook is not in safe preview (process_status=$ps)"))
    haskey(nb.metadata, "risky_file_source") && throw(ArgumentError(
        "risky_source::Cannot allow execution for risky remote sources via MCP; ask the user to run it from the notebook itself",
    ))
    _require_julia()
    return _leave_preview!(session, nb, run, timeout; ask_new)
end

# Out of safe preview: a new process, then every cell runs (`run`), through the
# new-package gate when `ask_new` (#397), or it starts on the next run.
function _leave_preview!(session, nb, run::Bool, timeout::Real; ask_new::Bool=false)
    nb.process_status = Pluto.ProcessStatus.waiting_to_restart
    session.options.evaluation.run_notebook_on_load && Pluto._report_business_cells_planned!(nb)
    _notify_browser(session, nb)
    Pluto.SessionActions.shutdown(session, nb; keep_in_session=true, async=true, verbose=false)
    if run
        nb.process_status = Pluto.ProcessStatus.starting
        _notify_browser(session, nb)
        ran = run_cells!(session, nb, collect(nb.cells); wait=false, timeout, ask_new)
    else
        nb.process_status = Pluto.ProcessStatus.ready
        _notify_browser(session, nb)
    end
    result = Dict{String,Any}("already_allowed" => false, "ran" => run, "process_status" => string(nb.process_status))
    # The run it started, named as `run` names one, so the core can tell when it ends.
    run && haskey(ran, "run") && (result["run"] = ran["run"])
    run && haskey(ran, "asking") && (result["asking"] = ran["asking"])
    return result
end

# Pluto's own Restart: a new process, then every cell runs. Refused in safe
# preview, where allow_execution starts the notebook.
function restart!(session, nb, timeout::Real; ask_new::Bool=false)
    ps = nb.process_status
    ps === Pluto.ProcessStatus.waiting_for_permission &&
        throw(ArgumentError("execution_blocked::The notebook is in safe preview; Run notebook starts it"))
    ps === Pluto.ProcessStatus.waiting_to_restart && return Dict{String,Any}("restarted" => false)
    _require_julia()
    nb.process_status = Pluto.ProcessStatus.waiting_to_restart
    session.options.evaluation.run_notebook_on_load && Pluto._report_business_cells_planned!(nb)
    _notify_browser(session, nb)
    # A process that ended while it started leaves its failed start behind, and the shutdown
    # and the new run would rethrow its error (#194). So would an interrupt asked of that start.
    _forget_failed_start!(nb)
    nb.wants_to_interrupt = false
    Pluto.SessionActions.shutdown(session, nb; keep_in_session=true, async=true, verbose=false)
    nb.process_status = Pluto.ProcessStatus.starting
    _notify_browser(session, nb)
    ran = run_cells!(session, nb, collect(nb.cells); wait=false, timeout, ask_new)
    result = Dict{String,Any}("restarted" => true)
    haskey(ran, "asking") && (result["asking"] = ran["asking"])
    return result
end

# Drop the notebook's start from Pluto's workspaces if it failed (its process ended before Pluto
# connected): every later fetch of it rethrows that error, so nothing could start a new one.
function _forget_failed_start!(nb)
    WM = Pluto.WorkspaceManager
    WM.withtoken(WM.get_workspace_token) do
        task = get(WM.active_workspaces, nb.notebook_id, nothing)
        task !== nothing && istaskfailed(task) && delete!(WM.active_workspaces, nb.notebook_id)
    end
    return nothing
end

# Pluto's own Restart, from the notebook's page, goes to its `restart_process` handler and not
# through `restart!`, so it forgets a failed start the same way first (#194). Once per Julia.
const _PLUTO_RESTART = Ref{Any}(nothing)
function _wrap_page_restart!()
    _PLUTO_RESTART[] === nothing || return nothing
    original = _PLUTO_RESTART[] = Pluto.responses[:restart_process]
    Pluto.responses[:restart_process] = function (request; kwargs...)
        nb = request.notebook
        # A restart that can't start a process would stop the one running: the page's Restart
        # does nothing then, as the adapter's `restart` refuses with `julia_removed`.
        if nb !== nothing && !isfile(_julia_exe())
            @warn "Restart: Julia $VERSION was removed from $(dirname(dirname(_julia_exe()))), so the notebook can't restart until Julia restarts; it keeps the process it has"
            return nothing
        end
        if nb !== nothing && nb.process_status !== Pluto.ProcessStatus.waiting_to_restart
            _forget_failed_start!(nb)
            nb.wants_to_interrupt = false
        end
        original(request; kwargs...)
    end
    return nothing
end

# Pluto's own Stop, from the notebook's page, goes to its `interrupt_all` handler and not through
# `interrupt!`, so it ended a waiting cell's process, and that read as a crash (#139). It is
# `interrupt!` now, which does what that handler does, but stops the cell in its own task first.
# Only on the Pluto releases `_stop_hook_ok` knows. Once per Julia.
const _PLUTO_STOP = Ref{Any}(nothing)
function _wrap_page_stop!()
    (_PLUTO_STOP[] === nothing && _stop_hook_ok()) || return nothing
    original = _PLUTO_STOP[] = Pluto.responses[:interrupt_all]
    Pluto.responses[:interrupt_all] = function (request; kwargs...)
        nb = request.notebook
        nb === nothing && return original(request; kwargs...)
        interrupt!(request.session, nb)
        return nothing
    end
    return nothing
end

# Move the notebook's file to `path` (checked by the core), as Pluto's own file box does.
function move!(session, nb, path::AbstractString)
    Pluto.SessionActions.move(session, nb, path)
    return Dict{String,Any}("path" => nb.path)
end

# Whether `path` is a notebook open in `session`, as Pluto's open tells.
function _open_already(session, path::AbstractString)
    tamed = Pluto.tamepath(path)
    isfile(tamed) || return false
    return any(nb -> isfile(nb.path) && realpath(nb.path) == realpath(tamed), values(session.notebooks))
end

# Registries in the user's own depot, behind ours (a LocalRegistry, say). Pkg updates only the
# first depot's (`Pkg.Registry.update`'s `depots = [depots1()]`, which Pluto's package manager uses),
# which is ours, so a version registered from another computer was never seen: "no versions left"
# (#367). Before a notebook opens, at most once per `cooldown` seconds, the ones we don't also have
# are updated where they are; one we have too (General) is ours to update, not the user's copy.
const _USER_REGISTRIES_UPDATED = Ref(-Inf)

function update_user_registries(; cooldown::Real = 3600, depots = DEPOT_PATH)
    time() - _USER_REGISTRIES_UPDATED[] < cooldown && return 0
    _USER_REGISTRIES_UPDATED[] = time()
    length(depots) < 2 && return 0
    ours = Set(r.uuid for r in Pkg.Registry.reachable_registries(; depots = [depots[1]]))
    updated = 0
    for depot in depots[2:end]
        isdir(joinpath(depot, "registries")) || continue
        theirs = [r for r in Pkg.Registry.reachable_registries(; depots = [depot]) if !(r.uuid in ours)]
        isempty(theirs) && continue
        try
            Pkg.Registry.update([Pkg.Registry.RegistrySpec(; uuid = r.uuid) for r in theirs]; depots = [depot], io = stderr)
            updated += length(theirs)
        catch e
            # Offline, or a registry that can't be reached: the copy there is used as it is.
            @warn "Couldn't update the registries in $depot" exception = (e, catch_backtrace())
        end
    end
    return updated
end

function open_notebook(session, path::AbstractString, run::Bool; ask_new::Bool=false, timeout::Real=60)
    # One already open says so (below), as it would with its Julia there.
    run && !_open_already(session, path) && _require_julia()
    try
        update_user_registries()
    catch e
        @warn "Couldn't update the user's registries" exception = (e, catch_backtrace())
    end
    gate = run && ask_new
    nb = try
        # Pluto's own run on open adds every package the file uses: where new ones ask (#397),
        # it opens as a safe preview and leaves it through the gate. A safe preview's load is over
        # before the open answers (it runs nothing, so it is quick): one still loading when it
        # leaves the preview, here or by an allow_execution sent at once, would find the notebook
        # allowed to run and add them after all.
        allowed = run && !gate
        Pluto.SessionActions.open(session, path; run_async = allowed, execution_allowed = allowed)
    catch e
        # Printing this exception walks the whole Notebook and never finishes.
        e isa Pluto.SessionActions.NotebookIsRunningException || rethrow()
        throw(ArgumentError("notebook_already_open::'$path' is already open as notebook_id $(e.notebook.notebook_id); use that id"))
    end
    opened = Dict{String,Any}("notebook_id" => string(nb.notebook_id), "path" => nb.path)
    if gate
        ran = _leave_preview!(session, nb, true, timeout; ask_new)
        haskey(ran, "run") && (opened["run"] = ran["run"])
        haskey(ran, "asking") && (opened["asking"] = ran["asking"])
    end
    opened["process_status"] = string(nb.process_status)
    return opened
end

# A new notebook at `path`, else named as Pluto names them in `folder`, else in
# Pluto's own folder. Pluto writes the file (never a hand-written header), then
# it opens and runs like any other; its cells come back so the caller can edit them.
function new_notebook(session, path, folder)
    _require_julia()  # before its file is made
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
    png, source = _cell_png(session, nb, cell)
    rendered = Dict{String,Any}("png" => png === nothing ? nothing : base64encode(png), "mime" => string(cell.output.mime))
    png !== nothing && source != "value" && (rendered["source"] = source)
    _interactive_html(cell.output) && (rendered["interactive"] = true)
    return rendered
end

render_text(session, nb, cell) = Dict{String,Any}("text" => _cell_text(session, nb, cell))

function adapter_call(session, method::AbstractString, params)
    session === nothing && throw(ArgumentError("pluto_not_running::Pluto is not running yet."))
    method == "status" && return session_status_dict()
    method == "open" && return open_notebook(session, params["path"], params["run"]; ask_new=get(params, "ask_new", false), timeout=get(params, "timeout", 60))
    method == "new" && return new_notebook(session, get(params, "path", nothing), get(params, "folder", nothing))
    id = get(params, "notebook_id", nothing)
    if method == "snapshot" && id === nothing
        seq = _seq()
        return Dict{String,Any}("notebooks" => [snapshot(nb; controls = controls(session, nb)) for nb in values(session.notebooks)], "seq" => seq)
    end
    nb = _get_notebook(session, string(id))
    method == "snapshot" && return (seq = _seq(); merge!(snapshot(nb; controls = controls(session, nb)), Dict{String,Any}("seq" => seq)))
    method == "graph" && return graph(nb; fresh=get(params, "fresh", false), refresh=get(params, "refresh", false),
                                      edges=get(params, "edges", false), packages=get(params, "packages", false))
    method == "shutdown" && return shutdown_notebook!(session, nb)
    method == "apply" && return apply!(session, nb, params["ops"])
    method == "run" && return run_cells!(session, nb, [_adapter_cell(nb, c) for c in params["cells"]]; wait=params["wait"], timeout=params["timeout"], ask_new=get(params, "ask_new", false))
    method == "approve_install" && return approve_install!(session, nb, get(params, "timeout", 30); packages=get(params, "packages", nothing))
    method == "interrupt" && return interrupt!(session, nb)
    method == "restart" && return restart!(session, nb, params["timeout"]; ask_new=get(params, "ask_new", false))
    method == "allow_execution" && return allow_execution!(session, nb, params["run"], params["timeout"]; ask_new=get(params, "ask_new", false))
    method == "move" && return move!(session, nb, params["path"])
    method == "render_text" && return render_text(session, nb, _adapter_cell(nb, params["cell_id"]))
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
