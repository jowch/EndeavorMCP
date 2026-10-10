# Error codes and warnings

Read this when a notebook tool returns a code the skill's main text doesn't explain. A failed call returns `{"error": code, "message": ...}`. A call that worked can carry `warnings`, each written `code::text`; `allow_execution` puts them under `run_warnings`.

## Errors

| `error` | Meaning | Do |
|---|---|---|
| `invalid_argument` | The call named an argument the tool doesn't have, or left out a required one such as `notebook_id` or `path`; the message lists the tool's arguments | Call again with the arguments it names |
| `read_required`, `stale_read` | You haven't read the cell, or it changed since you did | Read it, then retry. `edit_cells` refuses the whole batch if one cell fails this |
| `placement_required` | `add_cell` without `after_cell_id` in a notebook that has cells | Pass the cell to add after |
| `not_staged` | `submit_changes` was given a `cell_ids` entry that isn't in `pending_run` | Leave `cell_ids` out, or use `execute_cell` for a cell you didn't edit |
| `one_notebook` | The call is for a notebook other than this session's | Work in this session's notebook; read the other one as a file |
| `run_conflict` | Cells you ran depend on cells another session changed since you read them; nothing ran | Read the cells it names, then run again |
| `notebook_not_found`, `cell_not_found` | The id is wrong, or the notebook was closed or the cell deleted | `list_notebooks`, or read the notebook again |
| `file_exists` | `new_notebook` on a path that exists | `open_notebook` it, or pick another name |
| `file_not_found` | `open_notebook` on a path with no file | Check the path; a relative path starts in the session's folder |
| `invalid_path` | `open_notebook` had a null path or one that isn't a string; `new_notebook` had a path that doesn't end in `.jl` or `.R`, or whose folder doesn't exist; or the server was not told the project folder and the path is relative (or `new_notebook` had none) | Fix the path; if the server was not told the project folder, give an absolute path |
| `execution_not_gated` | `allow_execution` on a notebook that isn't in safe preview | Nothing to do; it can already run |
| `invalid_keep` | `keep_notebook_alive` without `keep` as true or false | Pass `keep` |
| `no_image` | The output has no image form, the cell errored, or the notebook isn't running code and the output isn't already an image | `read_cell` |
| `image_too_large` | The image is over about 4 MB | Make the figure smaller (fewer points, lower resolution), then look again |
| `process_exited` | The notebook's process stopped while running a cell. The file is saved; outputs are gone until cells run again | Tell the user before running the same cell again |
| `host_tools` | `list_folder`, `read_file` or `run_shell` in a session that isn't on a server | Use your own file and shell tools |
| `not_found` | `list_folder`, `read_file` or `run_shell` named a folder or file that isn't there | Check the path |
| `not_a_file`, `not_a_folder` | `read_file` was given a folder, or `list_folder` a file | Use the other tool |
| `unsupported` | `run_shell` on a Windows server | Don't use it there |
| `unsupported` | An R notebook (`.R`) on Windows, in the Endeavor app, or in a runtime without R notebooks | Tell the user R notebooks don't run there yet; offer a Julia notebook |
| `r_installing` | The first R notebook on this computer or machine: Endeavor is installing its own R (on the user's own Mac, after they agreed) or Ember, which takes a few minutes | Tell the user, then make the same call again in a minute |
| `r_not_found` | No R was found: `Rscript` isn't on the PATH, or the R set for this machine isn't there. On the user's own Mac, the message offers Endeavor's own R | If it offers Endeavor's own R, ask the user, as the message says; don't pass `install: true` without their yes. Otherwise tell the user how to get R, as the message says |
| `r_failed` | R started but Ember didn't: it couldn't install, or it stopped | Tell the user what the message says; the runtime's log has the rest |
| `risky_source` | The notebook came from a remote source, so only the user can allow it to run | Ask the user to run it from the notebook |
| `julia_starting` | Julia is starting, for the first Julia notebook opened or made, or ahead of it when the session will use Julia notebooks. The first start can take several minutes | Tell the user, then make the same call again in a minute |
| `julia_not_found` | A Julia notebook was opened or made and no Julia was found. Endeavor may download one only if the user agrees | Ask the user, as the message says; don't pass `install: true` without their yes |
| `julia_failed` | Julia couldn't start: the Julia or shell line set for it doesn't work, or it stopped while starting | Tell the user what the message says. On a machine, they can set Julia's path with `add_machine`. Calling again tries once more |
| `julia_failed`, "made no progress" | Julia's start made no progress for 30 minutes, so Endeavor stopped it | Tell the user what the message says, and ask them before calling again: trying again starts Julia from the beginning |
| `pluto_not_running` | Julia's notebook server isn't up yet | Tell the user; `session_status` shows its state |

`not_approved`, `waiting_for_user`, `cancelled`, `no_app`, `older_runtime` and `plan_mode` come from the Endeavor app: see [app.md](app.md).

## Warnings

| Warning | Meaning |
|---|---|
| `async_execution` | The run was started and not waited for. The cells are no longer in `pending_run`; read them for results |
| `execution_blocked` | Nothing ran: the notebook is in safe preview, or its process is stopped. The cells stay in `pending_run` |
| `execution_timeout` | A waited run was still going after 45 seconds and the call returned. The run continues; `execution.still_running` names its cells that are running, and they are no longer in `pending_run`. Read them later and do not run them again |
| `also_ran` | The run also ran the named cells: ones your cells depend on that had never run |
| `already_ran` | The user ran your staged cells before your run reached them, so they were not run a second time. The outputs are from that run |
| `ember_previous` | From opening or making an R notebook: Ember's newest build didn't work here, so R notebooks use the one before it. Mention it to the user if R notebooks act up |
| `run_conflict` | From `add_cell` or `edit_cell` with `run_after`: the edit was made and staged, not run. Read the named cells, then `submit_changes` |

A cell that failed is not a tool error: `read_cell` shows `errored` and an `error` object whose `kind` depends on the engine.
