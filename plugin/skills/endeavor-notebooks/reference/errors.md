# Error codes and warnings

Read this when a notebook tool returns a code the skill's main text doesn't explain. A failed call returns `{"error": code, "message": ...}`. A call that worked can carry `warnings`, each written `code::text`.

## Errors

| `error` | Meaning | Do |
|---|---|---|
| `read_required`, `stale_read` | You haven't read the cell, or it changed since you did | Read it, then retry. `edit_cells` refuses the whole batch if one cell fails this |
| `placement_required` | `add_cell` without `after_cell_id` in a notebook that has cells | Pass the cell to add after |
| `not_staged` | `submit_changes` was given a `cell_ids` entry that isn't in `pending_run` | Leave `cell_ids` out, or use `execute_cell` for a cell you didn't edit |
| `one_notebook` | The call is for a notebook other than this session's | Work in this session's notebook; read the other one as a file |
| `run_conflict` | Cells you ran depend on cells another session changed since you read them; nothing ran | Read the cells it names, then run again |
| `notebook_not_found`, `cell_not_found` | The id is wrong, or the notebook was closed or the cell deleted | `list_notebooks`, or read the notebook again |
| `file_exists` | `new_notebook` on a path that exists | `open_notebook` it, or pick another name |
| `no_image` | The output has no image form, the cell errored, or the notebook isn't running code | `read_cell` |
| `process_exited` | The notebook's process stopped while running a cell. The file is saved; outputs are gone until cells run again | Tell the user before running the same cell again |
| `host_tools` | `list_folder`, `read_file` or `run_shell` in a session that isn't on a server | Use your own file and shell tools |
| `risky_source` | The notebook came from a remote source, so only the user can allow it to run | Ask the user to run it from the notebook |
| `pluto_not_running` | The runtime isn't up | Tell the user; `pluto_session_status` shows its state |

`not_approved`, `cancelled`, `no_app`, `older_runtime` and `plan_mode` come from the Endeavor app: see [app.md](app.md).

## Warnings

| Warning | Meaning |
|---|---|
| `async_execution` | The run was started and not waited for. Read the cells for results |
| `execution_blocked` | Nothing ran: the notebook is in safe preview, or its process is stopped. The cells stay in `pending_run` |
| `execution_timeout` | A waited run passed 60 seconds. The cell is still running; read it later |
| `also_ran` | The run also ran the named cells: ones your cells depend on that had never run |
| `already_ran` | The user ran your staged cells before your run reached them, so they were not run a second time. The outputs are from that run |
| `other_session` | Another session changed the named cells in the last two minutes. Read them before relying on them |
| `run_conflict` | From `add_cell` or `edit_cell` with `run_after`: the edit was made and staged, not run. Read the named cells, then `submit_changes` |

A cell that failed is not a tool error: `read_cell` shows `errored` and an `error` object whose `kind` depends on the engine.
