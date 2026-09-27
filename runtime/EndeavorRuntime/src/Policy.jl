# Each agent session's MCP connection carries `X-Endeavor-Session: <key>`, so
# tool calls know whose they are (the owner). The core (`endeavor-remote core`)
# keeps each session's run policy and refuses plan mode's writes and runs before
# a call gets here.

const _POLICY_LOCK = ReentrantLock()

# Tools that change the notebook or run code (the core's list, less its host tools).
const _WRITE_TOOLS = Set([
    "edit_cell", "edit_cells", "add_cell", "delete_cell", "move_cell", "fold_cell", "new_notebook",
    "execute_cell", "submit_changes", "run_all_cells", "allow_execution",
])

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

# owner => the session's working folder on this machine, where new_notebook puts
# an unnamed notebook. The app sets it (the core keeps a copy for run_shell).
const _FOLDERS = Dict{String,String}()

function set_session_folder!(owner::AbstractString, folder::AbstractString)
    lock(_POLICY_LOCK) do
        isempty(folder) ? delete!(_FOLDERS, String(owner)) : (_FOLDERS[String(owner)] = String(folder))
    end
    return nothing
end

session_folder(owner::AbstractString) = lock(() -> get(_FOLDERS, owner, nothing), _POLICY_LOCK)

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
