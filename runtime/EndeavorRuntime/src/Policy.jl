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
