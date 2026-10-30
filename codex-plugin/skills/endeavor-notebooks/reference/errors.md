# Error codes and warnings

Read this when a notebook tool returns a code the skill's main text doesn't explain. A failed call returns `{"error": code, "message": ...}`. A call that worked can carry `warnings`, each written `code::text`; `allow_execution` puts them under `run_warnings`.

## Still starting

These come back the same way, but nothing failed and the call is not marked an error: something is still starting. Tell the user, then make the same call again right away: each call waits up to 30 seconds, so calling again is how you wait. Keep calling until the message says otherwise. Don't look for a fix or try another way meanwhile.

| `error` | Meaning | Do |
|---|---|---|
| `julia_starting` | Julia is starting, for the first Julia notebook opened or made, or ahead of it when the session will use Julia notebooks. The first start can take 10 minutes; the message says how long so far | Make the same call again |
| `r_installing` | The first R notebook on this computer or machine: Endeavor is installing its own R (on the user's own Mac, after they agreed) or Ember, which takes a few minutes | Make the same call again |
| `no_preview` | `apply_package_update` without a ready `preview_package_update` for that date, or the notebook's packages changed since | Preview that date again, tell the user what changes, then apply |
| `not_updatable` | `preview_package_update` with a `package` that isn't in the notebook's packages, or doesn't come from CRAN | Only a CRAN package in the notebook has release dates to move to. Tell the user; a package from elsewhere changes with its install call |
| `nothing_to_approve` | `approve_install` when no install is waiting for an OK, or in safe preview, before the notebook may run | Nothing to approve: `list_notebooks` shows what the notebook's packages are doing |
| `not_r_notebook` | A package-date tool or `approve_install` on a Julia notebook | Julia notebooks keep their package versions in their Pkg environment |
| `notebook_opening` | `open_notebook`, `new_notebook` or `allow_execution` took longer than one call waits: often the first notebook in a new Julia process, or a notebook leaving safe preview whose packages need installing. It goes on | Make the same call again, with the same arguments, to get its result |

On a machine, a call made while Endeavor starts there, or while its Slurm job is queued, says so in plain text; it is not an error either. See the machines skill.

## Errors

| `error` | Meaning | Do |
|---|---|---|
| `invalid_argument` | The call named an argument the tool doesn't have, or left out a required one such as `notebook_id` or `path`; the message lists the tool's arguments | Call again with the arguments it names |
| `read_required`, `stale_read` | You haven't read the cell, or it changed since you did | Read it, then retry. `edit_cells` refuses the whole batch if one cell fails this |
| `placement_required` | `add_cell` without `after_cell_id` in a notebook that has cells | Pass the cell to add after |
| `not_staged` | `submit_changes` was given a `cell_ids` entry that isn't in `pending_run` | Leave `cell_ids` out, or use `execute_cell` for a cell you didn't edit |
| `one_notebook` | The call is for a notebook other than this session's | Work in this session's notebook; read the other one as a file |
| `run_conflict` | Cells you ran depend on cells changed since this session read them (by the user, by another session, or before this conversation was resumed); nothing ran | Read the cells it names, then run again |
| `notebook_not_found`, `cell_not_found` | The id is wrong, or the notebook was closed or the cell deleted | `list_notebooks`, or read the notebook again |
| `file_exists` | `new_notebook` on a path that exists | `open_notebook` it, or pick another name |
| `file_not_found` | `open_notebook` on a path with no file | Check the path; a relative path starts in the session's folder |
| `not_a_notebook` | `open_notebook` on a file that isn't a Pluto or Ember notebook, such as a script | Read it as a file |
| `invalid_path` | `open_notebook` had a null path or one that isn't a string; `new_notebook` had a path that doesn't end in `.jl` or `.R`, or whose folder doesn't exist; or the server was not told the project folder and the path is relative (or `new_notebook` had none) | Fix the path; if the server was not told the project folder, give an absolute path |
| `execution_not_gated` | `allow_execution` on a notebook that isn't in safe preview | Nothing to do; it can already run |
| `invalid_keep` | `keep_notebook_alive` without `keep` as true or false | Pass `keep` |
| `no_image` | The output has no image form, the cell errored, or the notebook isn't running code and the output isn't already an image or HTML that holds one | `read_cell` |
| `image_too_large` | The image is over about 4 MB | Make the figure smaller (fewer points, lower resolution), then look again |
| `process_exited` | The notebook's process stopped while running a cell. The file is saved; outputs are gone until cells run again | Tell the user before running the same cell again |
| `invalid_value` | `set_input` with a value the input doesn't allow; the message says what it allows. Nothing changed | Pick a value it allows; for a dropdown, send the choice itself, not its label |
| `input_refused` | `set_input` named no bound input, or the input's cell hasn't run since it was edited, or R is busy setting another value (one an earlier call left `pending`) | `read_cell` the input's cell: its `inputs` show the names, and whether each is bound. Run the cell first if it's stale |
| `notebook_closed` | R ended, and the R notebooks open in it closed with it. The message says why. The file is saved; what its cells made is gone until they run again | Tell the user R ended and why. `open_notebook` opens it again and starts R; ask the user before running its cells again |
| `host_tools` | `list_folder`, `read_file` or `run_shell` in a session that isn't on a server | Use your own file and shell tools |
| `not_found` | `list_folder`, `read_file` or `run_shell` named a folder or file that isn't there | Check the path |
| `not_a_file`, `not_a_folder` | `read_file` was given a folder, or `list_folder` a file | Use the other tool |
| `unsupported` | `run_shell` on a Windows server | Don't use it there |
| `unsupported` | An R notebook (`.R`) on Windows, or in a runtime without R notebooks | Tell the user R notebooks don't run there yet; offer a Julia notebook |
| `r_not_found` | No R was found: `Rscript` isn't on the PATH, or the R set for this machine isn't there. On the user's own Mac, the message offers Endeavor's own R | If it offers Endeavor's own R, do what the message says: through `use_machine`, ask the user and don't pass `install: true` without their yes; in the Endeavor app, tell the user where to install it, as the message says. Otherwise tell the user how to get R, as the message says |
| `r_failed` | R started but Ember didn't: it couldn't install, or it stopped | Tell the user what the message says; the runtime's log has the rest |
| `risky_source` | The notebook came from a remote source, so only the user can allow it to run | Ask the user to run it from the notebook |
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
| `also_ran` | The run also ran the named cells: ones your cells depend on that had never run, or (in R) that were edited or stale since they last ran. A waited run lists them in `affected_cells` and `outputs.changed` |
| `lazy_mode` | The R notebook is in Ember's lazy mode: the cells that depend on what ran were marked stale, not rerun. `stale_cells` names every cell that is stale now; run the ones that should be current |
| `changed_in_place`, `settings_found`, `load_note`, `untracked_read`, `untracked_file` | Ember's notes on a cell, from its last run or (`untracked_read`) its code, in R: see reference/ember.md. `changed_in_place` means the cells that read the named value didn't rerun, so their results may be wrong. Make the change in the cell that defines the value. `untracked_read` means the cell doesn't rerun when the variables it reads through `get()` change. `untracked_file` means it doesn't rerun when the file the named reader read changes: pass that reader a file path |
| `logged_error`, `logged_warning`, `logged_info`, `printed` | What a Julia cell logged with `@error`, `@warn` or `@info`, or printed with `println`: see reference/pluto.md. A warning or error there can mean the result is wrong even though the cell didn't fail. For a cell that only ran along, the reply leaves out `printed` and `logged_info`; `read_cell` has them |
| `already_ran` | The user ran your staged cells before your run reached them, so they were not run a second time. The outputs are from that run |
| `ember_previous` | From opening or making an R notebook: Ember's newest build didn't work here, so R notebooks use the one before it. Mention it to the user if R notebooks act up |
| `run_conflict` | From `add_cell` or `edit_cell` with `run_after`: the edit was made and staged, not run. Read the named cells, then `submit_changes` |

A cell that failed is not a tool error: `read_cell` shows `errored` and an `error` object whose `kind` depends on the engine.
