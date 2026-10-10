# In the Endeavor app

Read this only when you work inside the Endeavor app. Nothing here applies to an agent that reaches the notebooks from its own client.

The user sees the notebook in a pane next to the chat. `new_notebook` and `open_notebook` switch the pane to that notebook, so there is no address to give and nothing to tell the user to open, unless a result carries `browser_url`: then give it to the user.

## Which notebook and cells a prompt means

- A prompt that starts "[Endeavor] The user is viewing notebook {id} in the notebook pane" names the notebook to work in, unless the user names another.
- A prompt that starts "[Endeavor] The user annotated notebook cells" carries `notebook://pluto/{notebook_id}/cell/{cell_id}` links (`notebook://ember/…` for an R notebook) and then the user's comment on those cells. The links can't be fetched; they give the ids. Call `read_cell` on every linked cell before you answer, since the comment may be about how the cells relate, and `view_cell_output` if it is about how an output looks. Answer from what the cells hold now, not from what the pane showed when the user clicked.

## Approval

The user chooses when the app asks them first, and a call that asks waits up to 45 seconds for their answer (20 for opening or creating a notebook).

- **Auto:** nothing asks; calls run as you make them.
- **Ask to run:** calls that run code ask: `execute_cell`, `submit_changes`, `run_all_cells`, `allow_execution`, `delete_cell`, `run_shell`, `open_notebook` with `run_notebook=true`, and `add_cell` or `edit_cell` with `run_after=true`. Stage your edits and run once, so the user is asked once.
- **Manual:** every change to the notebook asks too, including edits, moves, folds and `new_notebook`.
- **Plan mode:** calls that change or run anything (including `open_notebook` with `run_notebook=true`) fail with `plan_mode`. Read what you need and finish the plan; the user switches modes to carry it out. Write the plan as a short title and numbered one-sentence steps, with caveats after the list.

When the user says no, the call fails with `not_approved`. Don't retry it or reach the same result another way: say what you wanted to do and why, and go on with what doesn't need it. Two cases differ:

- A declined `add_cell` or `edit_cell` with `run_after=true` still makes the edit. It is staged, and the result carries a `not_approved` warning. Don't run it yourself.
- In Manual, a declined change is not made at all.

Other refusals:

| `error` | Meaning | Do |
|---|---|---|
| `waiting_for_user` | The user hasn't answered yet; nothing was changed or run, and the request is still on their screen | To keep waiting, make the same call again with the same arguments, and don't try another way meanwhile: a call that changes or runs something takes the request down. If you stop waiting, tell the user the request is waiting for their answer, and make the same call again when they write back |
| `cancelled` | The call was cancelled before the user answered | Ask before trying again |
| `no_app` | The app isn't open to ask the user | Tell the user; try again when it is |
| `older_runtime` | This runtime predates approval and can't ask | Don't run code. Tell the user to restart the runtime, or to switch to Auto |
