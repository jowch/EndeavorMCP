# MCP error fields

## Session / notebook errors

| Error | Meaning | Action |
|-------|---------|--------|
| `pluto_not_running` | Endeavor's Pluto session isn't up | Unusual in normal use — Endeavor starts Pluto with the app, or with `endeavor serve` / `endeavor mcp` without it; tell the user something's wrong rather than calling lifecycle tools yourself |
| `notebook_not_found` | Unknown or stale `notebook_id` | Confirm the notebook/path with the user; `list_notebooks` to see what's open |
| `one_notebook` | This session already works on another notebook; the call tried to open, create, edit, or run a different one | Keep working in the session's notebook. Read the other notebook as a plain `.jl` file if you need its code. If the user wants to work on it, suggest they start a new session with it |

If one of these fires before you've established any `notebook_id`, go to **pluto-session** first.

## Read-before-edit guard

| Error | Meaning | Action |
|-------|---------|--------|
| `read_required` | You haven't `read_cell` / `read_notebook_code`'d this cell yet | Read it, then retry the edit |
| `stale_read` | The cell changed since your last read (the user edited it live in the notebook, or another call touched it) | `read_cell` again, then retry |

`edit_cells` is all-or-nothing on this guard across the whole batch.

## Another session on the same notebook

Several Endeavor sessions can work on one notebook. Your reads count only for you: a cell another session edited needs a fresh read before you edit it (`stale_read`).

| Signal | Meaning | Action |
|--------|---------|--------|
| `other_session` warning (on a write or run result) | Another session changed the listed cells in the last two minutes | Read those cells before relying on their code or outputs |
| `run_conflict` error (from `execute_cell`, `submit_changes`, `run_all_cells`) | A cell you're running depends on cells another session changed since you last read them; nothing ran | Read the named cells, then run again |
| `run_conflict` warning (from `edit_cell` / `add_cell` with `run_after`) | Same conflict; your edit was applied and staged, not run | Read the named cells, then `submit_changes` |

Cells that don't depend on the other session's changes run normally.

## Cell error fields (from `read_cell` / mutation results)

| Field | Use |
|-------|-----|
| `error.kind` | e.g. `pluto_multi_expression` |
| `error.hint` | Default fix text |
| `error.boundaries` | Byte positions for splits |
| `error.fixes` | `wrap_begin_end` first, then `split_cells` |
| `pending_run` | Cells staged and awaiting execution |
| `execution_blocked` warning | Notebook is in safe preview — the edit is staged, it just hasn't run yet |
| `already_ran` warning (from `execute_cell` / `submit_changes`, in the app) | While your run waited for approval, the user's own run reached the cells you changed, so they already ran with your code; they weren't run again. The receipt's outputs are from that run |
| `not_approved` warning (from `edit_cell` / `add_cell` with `run_after`, in the app) | The user chose not to run it yet. Your edit was made and staged, not run. Don't run it again on your own |

## Error kinds

| `error.kind` | Default action |
|--------------|-----------------|
| `pluto_multi_expression` | `edit_cell` with `begin`/`end`, then `submit_changes(wait_for_completion=false)` |
| `runtime` | Read `error.msg`, fix the code, re-submit |

## Refused runs and changes

These come from the Endeavor app asking the user before a run. Without the app, nothing in the notebook tools asks, so they don't occur.

| `error` | Meaning | Action |
|---------|---------|--------|
| `not_approved` | The user denied this run, or (in Manual) this change; a denied change isn't made | Don't retry or do it another way. Say what you'd do and why |
| `cancelled` | The call was cancelled before the user answered | Ask the user before trying again |
| `no_app` | Endeavor isn't open to ask the user | Tell the user; try again once Endeavor is open |
| `older_runtime` | This notebook's Julia is from an older Endeavor and can't ask the user before a run | Don't run code. Tell the user to restart Julia, or to switch to Auto |

## Common mistakes

| Mistake | Fix |
|---------|-----|
| Edit without `read_cell` | `read_cell` first |
| Ignore `read_required` / `stale_read` | Re-read, then retry |
| Patch the `.jl` file on disk directly | Notebook tools only — never hand-edit the notebook file |
| Leave safe preview on when outputs are needed | User clicks "Run notebook code" at the top of the notebook, or `allow_execution` when they ask you to |
| Claim `submit_changes` ran cells while `execution_blocked` is set | Wait for safe preview to be exited, then poll `read_cell` for outputs |
