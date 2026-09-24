---
name: pluto-workflow
description: >-
  Use when editing Pluto notebook cells via PlutoMCP, staging changes before
  submit_changes, handling read_required/stale_read/execution_blocked
  responses, exiting safe preview, or acting on an annotation-mode comment on
  notebook cells.
---

# Pluto workflow (cell editing)

Live reactive session — not a `.jl` file to patch. See [pluto-mental-model.md](reference/pluto-mental-model.md) for Pluto semantics.

## Which notebook, and where the context comes from

Endeavor already shows the live notebook in a pane next to the chat — there's no landing page, no browser tool, and nothing to navigate. The notebook you're acting on comes from the prompt itself:

- **Viewing context.** A prompt may open with "[Endeavor] The user is viewing Pluto notebook {id} in the notebook pane…" — that `notebook_id` is "the notebook" unless the user names another one.
- **Annotation mode.** A prompt may instead open with "[Endeavor] The user annotated notebook cells in annotation mode…", followed by one or more `pluto://notebook/{notebook_id}/cell/{cell_id}` resource links and "Comment on the N cell(s) above: …". These links are **not fetchable** — call `read_cell(notebook_id, cell_id)` on each before responding. See [annotations.md](reference/annotations.md).

If neither is present and you don't already know the notebook_id, use **pluto-session** first.

## Edit loop

```
read_cell / read_notebook_code → edit_cell / edit_cells / add_cell (run_after=false)
  → submit_changes(wait_for_completion=false)
  → read_cell (verify; poll if still running/queued)
```

PlutoMCP enforces read-before-edit itself: editing a cell you haven't read yet, or that changed since your last read, returns `read_required` / `stale_read` — re-read with `read_cell` and retry, rather than assuming your edit was wrong. `submit_changes` responses track `pending_run`.

**Safe preview:** notebooks open with code loaded but not executed (`execution_blocked` shows up in mutation responses). Keep editing normally — staging and `submit_changes` still work, they just don't run yet. The user exits it themselves by clicking "Run notebook code" in the pane; call `allow_execution` yourself only when the user explicitly asks you to run the notebook. See [safe-preview.md](reference/safe-preview.md).

**Checking visual output:** `read_cell` only describes plots and other non-text outputs (e.g. `[image/svg+xml output, … bytes; call view_cell_output to see it]`). After making or changing a plot, call `view_cell_output(notebook_id, cell_id)` to actually look at it (axes, labels, whether the data looks right) before telling the user it's done. It needs the notebook to be running (not safe preview).

**Cell structure / parse errors:** → **pluto-semantics** — [cell-structure.md](../pluto-semantics/reference/cell-structure.md).

## REQUIRED chain

- No notebook_id yet, or need to open/create one → **pluto-session**
- Cell layout, `@bind`, `pluto_multi_expression` → **pluto-semantics**

## Common mistakes

| Mistake | Fix |
|---------|-----|
| Edit without `read_cell` first | Read first — PlutoMCP enforces this and returns `read_required` |
| Ignore a `stale_read` response | `read_cell` again, then retry the edit |
| Tell the user to open a browser or click through a landing page | There isn't one — the pane already reflects the notebook from context |
| End the turn with staged edits | `submit_changes(wait_for_completion=false)` first |
| Claim outputs/widgets are live while still in safe preview | Only true once the user clicks "Run notebook code", or you call `allow_execution` because they asked |
| Markdown cell added with its code showing | `add_cell(..., folded=true)`; `fold_cell(folded=true)` for existing prose cells |
| Treat a `pluto://notebook/{id}/cell/{id}` link as a URL to fetch | It's a join key — `read_cell(notebook_id, cell_id)` instead |
| Say a plot "looks right" from its code or `read_cell` alone | `view_cell_output` and look at it |

## Additional resources

- **Pluto mental model:** [reference/pluto-mental-model.md](reference/pluto-mental-model.md)
- **Safe preview:** [reference/safe-preview.md](reference/safe-preview.md)
- **Annotation mode:** [reference/annotations.md](reference/annotations.md)
- **Full edit pipeline:** [reference/edit-loop.md](reference/edit-loop.md)
- **Error fields + kinds:** [reference/errors.md](reference/errors.md)
- **Notebook cell structure:** [pluto-semantics/reference/cell-structure.md](../pluto-semantics/reference/cell-structure.md)
