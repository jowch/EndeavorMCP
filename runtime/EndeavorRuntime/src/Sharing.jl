# Several agent sessions on one notebook. The owner of a tool call is its
# session's X-Endeavor-Session key ("" for the app and tests, which are exempt).
# Each cell remembers which owner last changed it and when. Another owner's
# writes and runs soon after get an `other_session` warning, and a run is
# refused while a cell upstream of it has changes its owner hasn't read.
# Same-cell edits are refused by the per-owner read receipts (Staging.jl).

const OTHER_SESSION_WINDOW_SECONDS = 120.0

const _CLOCK = Ref{Function}(time)
now_seconds() = _CLOCK[]()

# Orders reads against changes; two can share a clock reading.
const _SEQ = Ref(0)
_next_seq!() = (_SEQ[] += 1)

current_owner() = get(task_local_storage(), :endeavor_owner, "")::String
with_owner(f, owner::AbstractString) = task_local_storage(f, :endeavor_owner, String(owner))

struct CellChange
    owner::String
    time::Float64
    seq::Int
end

const _CHANGES = Dict{UUID, Dict{UUID, CellChange}}()

function note_cell_changed!(notebook_id::UUID, cell_id::UUID)
    _with_staging_lock() do
        changes = get!(() -> Dict{UUID, CellChange}(), _CHANGES, notebook_id)
        changes[cell_id] = CellChange(current_owner(), now_seconds(), _next_seq!())
    end
    return nothing
end

_other_owner(change::CellChange, owner) = !isempty(change.owner) && change.owner != owner

function other_session_warning(notebook_id::UUID, owner::AbstractString)
    isempty(owner) && return nothing
    now = now_seconds()
    recent = _with_staging_lock() do
        [(cid, ch) for (cid, ch) in get(_CHANGES, notebook_id, Dict{UUID, CellChange}())
         if _other_owner(ch, owner) && now - ch.time <= OTHER_SESSION_WINDOW_SECONDS]
    end
    isempty(recent) && return nothing
    sort!(recent; by = r -> r[2].seq)
    ago = round(Int, now - recent[end][2].time)
    cells = join((string(cid) for (cid, _) in recent), ", ")
    return "other_session::Another Endeavor session changed $cells in this notebook $ago s ago. " *
           "Read cells before relying on them."
end

"""
The cells another owner changed since the current owner last read them that
`targets` depend on (the targets included), in notebook order, as a
`run_conflict::` message; `nothing` when there are none.
"""
function run_conflict(nb::Pluto.Notebook, targets)
    owner = current_owner()
    isempty(owner) && return nothing
    unread = _with_staging_lock() do
        Set(cid for (cid, ch) in get(_CHANGES, nb.notebook_id, Dict{UUID, CellChange}())
            if _other_owner(ch, owner) && ch.seq > _read_seq(nb.notebook_id, owner, cid))
    end
    isempty(unread) && return nothing
    topology = Pluto.updated_topology(nb.topology, nb, nb.cells)
    targets = Pluto.Cell[targets...]
    upstream = union!(Pluto.MoreAnalysis.upstream_recursive(topology, targets), targets)
    conflicted = [c.cell_id for c in nb.cells if c.cell_id in unread && c in upstream]
    isempty(conflicted) && return nothing
    return "run_conflict::Another Endeavor session changed $(join(conflicted, ", ")) since you last read " *
           "them, and the cells you're running depend on them. Read them (read_cell or read_notebook_code), then run again."
end

function require_no_run_conflict!(nb::Pluto.Notebook, targets)
    conflict = run_conflict(nb, targets)
    conflict === nothing || throw(ArgumentError(conflict))
    return nothing
end
