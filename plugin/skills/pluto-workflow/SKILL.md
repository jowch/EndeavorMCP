---
name: pluto-workflow
description: >-
  Use when editing Pluto notebook cells via the notebook tools, staging changes before
  submit_changes, handling read_required/stale_read/execution_blocked
  responses, exiting safe preview, acting on an annotation-mode comment on
  notebook cells, or writing a plan while in Plan mode.
---

# Pluto workflow (cell editing)

Live reactive session — not a `.jl` file to patch. See [pluto-mental-model.md](reference/pluto-mental-model.md) for Pluto semantics.

## Plan mode

Write the plan as a short title, then a numbered list of steps, one plain
sentence each. Put any context, caveats or checks after the numbered list,
not folded into a step's line. Keep the whole plan short.

## Which notebook, and where the context comes from

The notebook tools run either in the Endeavor app or without it. You are working without the app when the Endeavor server's instructions say the notebooks run without the Endeavor app, or when tool results carry a `browser_url`. Otherwise you are in the app. Where this skill says "in the app" or "without the app", follow only the part for your setting.

In the app, Endeavor already shows the live notebook in a pane next to the chat — there's no landing page, no browser tool, and nothing to navigate. The notebook you're acting on comes from the prompt itself:

- **Viewing context.** A prompt may open with "[Endeavor] The user is viewing Pluto notebook {id} in the notebook pane…" — that `notebook_id` is "the notebook". Each session edits and runs only its own notebook; other notebooks can be read as plain `.jl` files (see **pluto-session**).
- **Annotation mode.** A prompt may instead open with "[Endeavor] The user annotated notebook cells in annotation mode…", followed by one or more `notebook://pluto/{notebook_id}/cell/{cell_id}` resource links and "Comment on the N cell(s) above: …". These links are **not fetchable** — call `read_cell(notebook_id, cell_id)` on each before responding. See [annotations.md](reference/annotations.md).

If neither is present and you don't already know the notebook_id, use **pluto-session** first.

Without the app, the user watches the notebook on Pluto's own page in a web browser, and prompts carry no notebook context. The notebook is the one the user names or the one you are already working in; otherwise use **pluto-session**.

## Edit loop

```
read_cell / read_notebook_code → edit_cell / edit_cells / add_cell (run_after=false)
  → submit_changes(wait_for_completion=false)
  → read_cell (verify; poll if still running/queued)
```

The notebook tools enforce read-before-edit themselves: editing a cell you haven't read yet, or that changed since your last read, returns `read_required` / `stale_read` — re-read with `read_cell` and retry, rather than assuming your edit was wrong. `submit_changes` responses track `pending_run`.

**Safe preview:** notebooks open with code loaded but not executed (`execution_blocked` shows up in mutation responses). Keep editing normally — staging and `submit_changes` still work, they just don't run yet. The user exits it themselves by clicking "Run notebook code" at the top of the notebook; call `allow_execution` yourself only when the user explicitly asks you to run the notebook. See [safe-preview.md](reference/safe-preview.md).

**In the app, running code needs the user's approval.** `execute_cell`, `submit_changes`, `run_all_cells`, `allow_execution`, `delete_cell` (it re-runs dependents), and `add_cell` / `edit_cell` with `run_after=true` each show the user an approval prompt (they may choose to stop being asked). The call waits until the user answers. Staging edits doesn't ask, except in the user's Manual mode, where every change to the notebook (edits, new cells, moves, folds, deletes, a new notebook) waits for approval the same way. Batch your edits and run once rather than asking repeatedly. A denied run fails with the error `not_approved`. A denied `add_cell` / `edit_cell` with `run_after=true` still makes the edit: the cell is staged and not run, and the result has a `not_approved` warning. In Manual a denied change is not made at all, and the call fails with `not_approved`. If a run or change is denied, don't retry or look for another way to do it: say what you'd do and why, and continue with what you can do without it.

**Without the app, the notebook tools don't ask before a run.** Your own agent's permission prompts, if it has them, are how the user approves a call. Still batch your edits and run once.

**Reading rich output:** for tables, arrays, dicts, HTML and Markdown, `read_cell`'s `output` only names the type; its `output_text` holds the value as Julia prints it as text, numbers included. After a run, each changed cell in `outputs.changed` has the first 2 KB of it too. Read that instead of editing the cell to print the value.

**Checking visual output:** `read_cell` only describes plots (e.g. `[image/svg+xml output, … bytes; call view_cell_output to see it]`). After making or changing a plot, call `view_cell_output(notebook_id, cell_id)` to actually look at it (axes, labels, whether the data looks right) before telling the user it's done. It needs the notebook to be running (not safe preview).

**Cell structure / parse errors:** → **pluto-semantics** — [cell-structure.md](../pluto-semantics/reference/cell-structure.md).

## REQUIRED chain

- No notebook_id yet, or need to open/create one → **pluto-session**
- Cell layout, `@bind`, `pluto_multi_expression` → **pluto-semantics**

## Common mistakes

| Mistake | Fix |
|---------|-----|
| Edit without `read_cell` first | Read first — the notebook tools enforce this and returns `read_required` |
| Ignore a `stale_read` response | `read_cell` again, then retry the edit |
| In the app, tell the user to open a browser or click through a landing page | There isn't one — the pane already reflects the notebook from context. (Without the app, do give the user `browser_url`.) |
| End the turn with staged edits | `submit_changes(wait_for_completion=false)` first |
| Claim outputs/widgets are live while still in safe preview | Only true once the user clicks "Run notebook code", or you call `allow_execution` because they asked |
| Markdown cell added with its code showing | `add_cell(..., folded=true)`; `fold_cell(folded=true)` for existing prose cells |
| Treat a `notebook://pluto/{id}/cell/{id}` link as a URL to fetch | It's a join key — `read_cell(notebook_id, cell_id)` instead |
| Say a plot "looks right" from its code or `read_cell` alone | `view_cell_output` and look at it |

## Additional resources

- **Pluto mental model:** [reference/pluto-mental-model.md](reference/pluto-mental-model.md)
- **Safe preview:** [reference/safe-preview.md](reference/safe-preview.md)
- **Annotation mode:** [reference/annotations.md](reference/annotations.md)
- **Full edit pipeline:** [reference/edit-loop.md](reference/edit-loop.md)
- **Error fields + kinds:** [reference/errors.md](reference/errors.md)
- **Notebook cell structure:** [pluto-semantics/reference/cell-structure.md](../pluto-semantics/reference/cell-structure.md)
