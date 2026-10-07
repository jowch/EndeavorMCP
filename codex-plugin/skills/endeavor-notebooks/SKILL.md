---
name: endeavor-notebooks
description: >-
  Use for any work in a live notebook through Endeavor's notebook tools
  (`new_notebook`, `open_notebook`, `read_cell`, `edit_cell`,
  `submit_changes`, ...): creating or opening a notebook; adding, editing,
  deleting, folding or running cells; reading results and checking plots;
  widgets and data analysis in a Pluto (Julia) notebook; working in a notebook
  another agent has open; or a notebook tool refusing a call. Read it before
  the first notebook tool call of a session. Not for Jupyter `.ipynb` files.
---

# Working in a live notebook

These tools act on a notebook that is running in Endeavor's runtime. The user watches the same notebook and can edit it while you work. It is not a file to patch: the runtime owns the notebook's file and rewrites it, so create notebooks with `new_notebook` and change them only through the tools. You can read any notebook's file to reuse its code.

The notebook is reactive. Running a cell also runs every cell that depends on it, and cells run in dependency order, not page order. `read_notebook_code` shows the order they run in.

## Before the first cell

- **Engine.** Each engine limits what one cell may hold, and code that would be fine in a script fails. Read the engine's reference before you write a cell. Pluto (Julia, `.jl`): [reference/pluto.md](reference/pluto.md).
- **Host.** You are in the Endeavor app unless the server's instructions say the notebooks run without it. In the app, read [reference/app.md](reference/app.md): prompts name the notebook, and the user approves runs. Without the app, skip it; `new_notebook` and `open_notebook` return `browser_url`, which you give to the user so they can watch.
- **On a server.** When the notebook runs on a server, its files are there. Use `list_folder`, `read_file` and `run_shell` for them; your own file tools see the user's computer, and may find a copy of the project there and show no error.

## One notebook per session

A session works in one notebook: the first it creates or opens. `list_notebooks` marks it `this_session`.

- No notebook yet and the user wants notebook work: call `new_notebook` with a short descriptive file name. Don't ask the user to make one. Its empty first cell is in `cell_ids` and counts as read, so edit that cell first.
- The user names an existing notebook: `open_notebook`. If it is unclear which file they mean, ask; don't search for one to open. If the notebook is already open, the session joins it as it is and nothing runs (`already_open`). That is also how a second agent shares your notebook: give it the path.
- After that, any call that opens, creates, changes or runs another notebook fails with `one_notebook`. A new step of the analysis is a new section in this notebook. If the user wants to work in another notebook, they start a new session.

## Read, stage, run, check

1. **Read.** An edit is accepted only for a cell you have read as it is now. `read_required` and `stale_read` mean read it and try again; `stale_read` means someone changed it since, so see what they did before you overwrite it. `read_notebook_code` reads every cell it returns, but leaves prose cells out unless `include_markdown=true`. `add_cell` needs a read of the cell it goes after. A cell you just added or edited counts as read.
2. **Stage.** `edit_cell`, `edit_cells` and `add_cell` change code without running it. Staged cells are listed in `pending_run`, and `read_cell` shows them `stale`: the output is from the old code.
3. **Run once.** `submit_changes` runs everything staged, with dependents. Run once per batch of edits, not per edit, so expensive cells downstream run once. Don't end a turn with cells staged unless the user asked for that, since the notebook would show output that doesn't match its code.
4. **Check.** A run returns at once. Read the cells again until `running` and `queued` are false. `wait_for_completion=true` blocks until the whole run ends, with no time limit, so use it only for a run you know takes a few seconds. Its result lists, under `outputs.changed`, the cells you ran and any upstream cells that had never run, leaving out cells with no output. It does not cover dependents, and `completed` only means those cells did not error. Read the dependents you care about. Then look at `errored`, `error` and the output before you report.

`delete_cell`, `move_cell` and `fold_cell` are not staged. Deleting reruns the cell's dependents and can't be undone.

## Outputs

- For tables and other rich values, `output` only names the type and `output_text` has the value as text. Read that; don't edit the cell to print it.
- For a plot, call `view_cell_output` and look at it before you say it is right. Code that runs can still draw the wrong thing.
- Fold cells that hold only prose (`add_cell` with `folded=true`, or `fold_cell`), so the reader sees the rendered text and not its source.

## Safe preview

A notebook opened from a file runs nothing until the user allows it, because its code may not be theirs (`execution_allowed` is false). Edits and `submit_changes` still stage, and the result warns `execution_blocked`: nothing ran. The warning names `allow_execution`, but call that only when the user asked you to run the notebook. Otherwise say the outputs are not current, and that they can run it from the notebook or ask you to.

## Other sessions in the notebook

Several agents can work in one notebook. When a write is refused with `stale_read`, or a run with `run_conflict`, another session changed those cells: read them again, then retry. Leave a notebook with `this_session` false alone unless the user asks you to work in it.

For any other error code or warning, see [reference/errors.md](reference/errors.md).
