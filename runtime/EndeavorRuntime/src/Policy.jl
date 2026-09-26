# Per-session run policy, set by the app ("plan" | "ask" | "auto"). Each agent
# session's MCP connection carries `X-Endeavor-Session: <key>`, so tool calls
# know whose they are. In "plan" the session's notebook writes and runs are
# refused. "ask" and "auto" pass through for now: runs are still gated by the
# app's Claude hook (moving that here is the rest of runtime step 5).

const _POLICY_LOCK = ReentrantLock()
const _POLICIES = Dict{String,String}()

# Tools that change the notebook or run code.
const _WRITE_TOOLS = Set([
    "edit_cell", "edit_cells", "add_cell", "delete_cell", "move_cell", "fold_cell", "new_notebook",
    "execute_cell", "submit_changes", "run_all_cells", "allow_execution",
])

set_policy!(owner::AbstractString, policy::AbstractString) = lock(() -> (_POLICIES[String(owner)] = String(policy)), _POLICY_LOCK)
policy_of(owner::AbstractString) = lock(() -> get(_POLICIES, owner, "ask"), _POLICY_LOCK)

function policy_refusal(owner::AbstractString, tool::AbstractString)
    (tool in _WRITE_TOOLS && policy_of(owner) == "plan") || return nothing
    return ArgumentError("plan_mode::Plan mode is read-only: `$tool` would change or run the notebook. " *
                         "Finish the plan; the user switches modes to carry it out.")
end

# One notebook per agent session: owner => the notebook path it works on. The
# app binds a session started from an existing notebook; otherwise the first
# notebook the session opens or creates binds it. Other notebooks stay readable.
const _BINDINGS = Dict{String,String}()

function _canonical_path(path::AbstractString)
    p = abspath(expanduser(String(path)))
    ispath(p) && return realpath(p)
    isdir(dirname(p)) && return joinpath(realpath(dirname(p)), basename(p))
    return normpath(p)
end

# An empty path clears the binding.
function bind_notebook!(owner::AbstractString, path::AbstractString)
    lock(_POLICY_LOCK) do
        isempty(path) ? delete!(_BINDINGS, String(owner)) : (_BINDINGS[String(owner)] = _canonical_path(path))
    end
    return nothing
end

bound_notebook(owner::AbstractString) = lock(() -> get(_BINDINGS, owner, nothing), _POLICY_LOCK)

# After a successful open/new: bind the owner if it isn't bound yet.
function note_notebook_opened!(owner::AbstractString, path::AbstractString)
    isempty(owner) && return nothing
    lock(_POLICY_LOCK) do
        haskey(_BINDINGS, owner) || (_BINDINGS[String(owner)] = _canonical_path(path))
    end
    return nothing
end

function _one_notebook_error(bound::AbstractString, what::AbstractString)
    ArgumentError("one_notebook::This session works on one notebook, $(bound), so it can't $what. " *
                  "You can still read other notebooks as plain .jl files. " *
                  "To work on another notebook, suggest the user start a new session with it.")
end

function notebook_refusal(session, owner::AbstractString, tool::AbstractString, arguments)
    isempty(owner) && return nothing
    bound = bound_notebook(owner)
    bound === nothing && return nothing
    if tool == "open_notebook" || tool == "new_notebook"
        requested = get(arguments, "path", nothing)
        requested isa AbstractString && _canonical_path(requested) == bound && return nothing
        what = requested isa AbstractString ? "$(tool == "open_notebook" ? "open" : "create") $(requested)" :
                                              "create another notebook"
        return _one_notebook_error(bound, what)
    end
    tool in _WRITE_TOOLS || return nothing
    sess = session === nothing ? standalone_session() : session
    sess === nothing && return nothing
    nid = tryparse(UUID, string(get(arguments, "notebook_id", "")))
    nid === nothing && return nothing
    nb = get(sess.notebooks, nid, nothing)
    (nb === nothing || _canonical_path(nb.path) == bound) && return nothing
    return _one_notebook_error(bound, "change or run $(nb.path)")
end
