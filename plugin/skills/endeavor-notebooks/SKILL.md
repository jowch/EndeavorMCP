---
name: endeavor-notebooks
description: >-
  Use for any work in a live notebook through Endeavor's notebook tools
  (`new_notebook`, `open_notebook`, `read_cell`, `edit_cell`,
  `submit_changes`, ...): creating or opening a notebook; adding, editing,
  deleting, folding or running cells; reading results and checking plots;
  widgets and data analysis in a Julia (Pluto) or R (Ember) notebook; working in a notebook
  another agent has open; or a notebook tool refusing a call. For a `.jl` or
  `.R` file the user asks to open, show or look at, try `open_notebook` before
  reading the file. Read it before the first
  notebook tool call of a session. Not for Jupyter `.ipynb` files.
---

# Working in a live notebook

These tools act on a notebook that is running in Endeavor's runtime. The user watches the same notebook and can edit it while you work. It is not a file to patch: the runtime owns the notebook's file and rewrites it, so create notebooks with `new_notebook` and change them only through the tools. You can read any notebook's file to reuse its code.

The notebook is reactive. Running a cell also runs every cell that depends on it, and cells run in dependency order, not page order. `read_notebook_code` shows the order they run in.

## Before the first cell

- **Engine.** The file's ending names the language: `.jl` is a Julia notebook (Pluto), `.R` an R notebook (Ember). Each engine limits what one cell may hold, and code that would be fine in a script fails. Read the engine's reference before you write a cell. Julia: [reference/pluto.md](reference/pluto.md). R: [reference/ember.md](reference/ember.md).
- **Host.** You are in the Endeavor app unless the server's instructions say the notebooks run without it. In the app, read [reference/app.md](reference/app.md): prompts name the notebook, and the user approves runs. Without the app, skip it. Either way, when `new_notebook` or `open_notebook` returns `browser_url`, give it to the user so they can watch. If the result has `opened_in_browser` true, the notebook should already be open in their browser: say so, and give the address in case it isn't.
- **Paths.** If a tool says the server was not told the project folder, give absolute paths: your working folder plus the file name.
- **On a server.** When the notebook runs on a server, its files are there. Use `list_folder`, `read_file` and `run_shell` for them; your own file tools see the user's computer, and may find a copy of the project there and show no error.

## One notebook per session

A session works in one notebook: the first it creates or opens. `list_notebooks` marks it `this_session`.

- No notebook yet and the user wants notebook work: call `new_notebook` with a short descriptive file name, ending in `.jl` for Julia or `.R` for R. Make an R notebook when the user asks for R or their project is in R; otherwise Julia. R notebooks don't run on Windows yet: there, tell the user and offer Julia. Don't ask the user to make one. Its empty first cell is in `cell_ids` and counts as read, so edit that cell first.
- The user names an existing notebook: `open_notebook`. If it is unclear which file they mean, ask; don't search for one to open. If the notebook is already open, the session joins it as it is and nothing runs (`already_open`). That is also how a second agent shares your notebook: give it the path.
- After that, any call that opens, creates, changes or runs another notebook fails with `one_notebook`. A new step of the analysis is a new section in this notebook. If the user wants to work in another notebook, they start a new session.

## Read, stage, run, check

1. **Read.** An edit is accepted only for a cell you have read as it is now. `read_required` and `stale_read` mean read it and try again; `stale_read` means someone changed it since, so see what they did before you overwrite it. `read_notebook_code` reads every cell it returns, but leaves prose cells out unless `include_markdown=true`. `add_cell` needs a read of the cell it goes after. A cell you just added or edited counts as read.
2. **Stage.** `edit_cell`, `edit_cells` and `add_cell` change code without running it. Staged cells are listed in `pending_run`, and `read_cell` shows them `stale`: the output is from the old code. Each `add_cell` puts its cell directly after `after_cell_id`, so two cells added after the same cell end up in reverse order. To add several in order, add them one at a time, each after the `cell_id` the previous call returned.
3. **Run once.** `submit_changes` runs everything staged, with dependents. An R notebook can be in Ember's lazy mode instead (`on_cell_change` is `lazy` in `list_notebooks` and `open_notebook`): there a run marks the cells that depend on it stale and doesn't run them, and its result warns `lazy_mode` and lists the notebook's stale cells in `stale_cells`. Running a stale cell runs its stale ancestors first, so run the ones the user needs current. Run once per batch of edits, not per edit, so expensive cells downstream run once. Don't end a turn with cells staged unless the user asked for that, declined the run, or the notebook is in safe preview. If cells stay staged, tell the user which ones haven't run, since the notebook shows output that doesn't match their code.
4. **Check.**
   - Pass `wait_for_completion=true` when you will read the result next. Without it the run returns at once: do other work, then read the cells until `running` and `queued` are false.
   - A waited run waits at most 45 seconds from the start of the call. If the run is still going then, the call returns normally with the cells still running in `execution.still_running` and a `message` (`execution.status` is `running`, or `errored` if a cell already failed). The run continues and its cells are no longer in `pending_run`: read them later with `read_cell`, which waits up to 45 seconds for a cell your run is still running (if the cell is still running then, its `message` says how long the read waited), and do not run them again. `submit_changes` with nothing staged and `wait_for_completion=true` waits the same way for the rest of your run, and its `affected_cells` and `execution.status` are those of the cells that ran meanwhile (`errored` if one failed).
   - An edit with `run_after=true` isn't waited for. While its cell is still `running` or `queued`, the result gives the cell's output and error from before the edit as `previous_output`, `previous_errored` and `previous_error`, not `output`, `errored` and `error`, and `outputs.changed` leaves it out. Read the cell for the new output. A run not waited for names the dependents that will rerun after it in `execution.will_run`; read them too once they finish.
   - A waited run that ended lists, in `affected_cells` and `outputs.changed`, the cells you ran, the upstream cells it ran first, and the dependents that reran with them. `outputs.changed` leaves out cells with no output. `execution.status` is `errored` if any of these failed, a dependent included. A dependent still running when the wait ended is in `execution.still_running` instead.
   - The first time a notebook uses a package (`using DataFrames` in Julia, `library(dplyr)` in R), it is installed before the cells that need it run, and they stay `queued`. That can take several minutes. Tell the user it is installing, then call `read_cell` on a queued cell to wait. Don't run the cells again meanwhile. Results have `packages` and a `message` saying so, and while packages install each `read_cell` waits up to 45 seconds. In Julia every cell waits; in R, the cells that may need the packages wait and the others run. If the message says it is taking longer than usual, tell the user and follow the message: keep waiting only while the last line of the package log changes.
   - Look at `errored`, `error` and the output before you report.
5. **Stop.** `interrupt_notebook` stops the notebook's run, as the notebook's Stop button does: the running cells end interrupted and the queued ones don't run. Call it when the user asks you to stop a run, or when a run you started is taking too long or is wrong. Don't call it for a run the user or another session started unless the user asked. The result lists the cells that `stopped`, which now show the interrupt as their error, and the queued cells `dropped`, which keep their old output. Run them again once you've fixed what was wrong. A cell in `still_running` is in code that ignores the interrupt. Tell the user; only a restart ends it.
6. **Restart.** `restart_notebook` starts the notebook's process again, as the Restart on its page does, and every value is gone. Call it when the user asks for a fresh start ("start R fresh", "restart the kernel"), or for a cell in `still_running` once the user agrees. A Julia notebook then runs every cell, including one that was stuck: the result names those in `running_again`. To end a stuck Julia cell, interrupt, edit the cell so it finishes (or tell the user it needs changing), then restart. An R notebook's cells are all `not_run`; run what you need, or `run_all_cells`.

`delete_cell`, `move_cell` and `fold_cell` are not staged. Deleting reruns the cell's dependents and can't be undone.

## Outputs

- For tables and other rich values, `output` only names the type and `output_text` has the value as text. Read that; don't edit the cell to print it. In a run's result, `output` is named `output_summary`, and `output_text` is cut at 2000 bytes; `read_cell` gives up to 16 KB.
- For a plot, call `view_cell_output` and look at it before you say it is right. Code that runs can still draw the wrong thing.
- Fold cells that hold only prose (`add_cell`, `edit_cell` or `edit_cells` with `folded=true`, or `fold_cell`), so the reader sees the rendered text and not its source.

## Safe preview

A notebook opened from a file runs nothing until the user allows it, because its code may not be theirs (`execution_allowed` is false). Edits and `submit_changes` still stage, and the result warns `execution_blocked`: nothing ran. The warning names `allow_execution`, but call that only when the user asked you to run the notebook. Otherwise say the outputs are not current, and that they can run it from the notebook or ask you to.

## Other sessions in the notebook

Several agents can work in one notebook. When a write is refused with `stale_read`, or a run with `run_conflict`, those cells changed since this session read them: the user changed them, or another session did, or this conversation did before it was resumed: read them again, then retry. A notebook with `this_session` false is another session's: read it, but don't change it. If the user asks you to work in it and this session has no notebook yet, `open_notebook` with its path joins it; if this session already has one, that fails with `one_notebook`, so tell the user it needs a new session.

For any other error code or warning, see [reference/errors.md](reference/errors.md).

## Feedback

If the user wants to report a problem with Endeavor or say what got in their way, give them this link, with your own agent's name at the end (`claude`, `codex` or `antigravity`): https://jowch.github.io/Endeavor/feedback/?source=plugin&agent=claude. It offers a short form that needs no account, or a GitHub issue. Don't fill in or send either yourself.
