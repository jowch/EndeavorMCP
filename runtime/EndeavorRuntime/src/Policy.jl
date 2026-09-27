# Each agent session's MCP connection carries `X-Endeavor-Session: <key>`, so
# tool calls know whose they are (the owner). The core (`endeavor-remote core`)
# keeps each session's run policy and its one notebook, and refuses what they
# don't allow before a call gets here.

const _POLICY_LOCK = ReentrantLock()

# Tools that change the notebook or run code (the core's list, less its host tools).
const _WRITE_TOOLS = Set([
    "edit_cell", "edit_cells", "add_cell", "delete_cell", "move_cell", "fold_cell", "new_notebook",
    "execute_cell", "submit_changes", "run_all_cells", "allow_execution",
])

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
