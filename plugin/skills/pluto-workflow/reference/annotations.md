# Annotation mode

The user can put the notebook pane into annotation mode, click one or more cells, and attach a comment. No browser tool or DOM resolution is involved on the agent's side — the app resolves the click to cell UUIDs itself and hands you plain resource links.

## What the prompt looks like

```
[Endeavor] The user annotated notebook cells in annotation mode...

pluto://notebook/{notebook_id}/cell/{cell_id_1}
pluto://notebook/{notebook_id}/cell/{cell_id_2}

Comment on the 2 cell(s) above: <the user's comment text>
```

- The preface tells you annotation mode was used; the `notebook_id` embedded in the resource links is the notebook in question.
- One or more `pluto://notebook/{notebook_id}/cell/{cell_id}` resource links identify the annotated cells.
- The trailing line is the user's comment, addressed to those specific cells.

## What to do with the links

`pluto://notebook/{id}/cell/{id}` links are **not fetchable** — they're a join key, not a URL. For each linked cell:

1. Call `read_cell(notebook_id, cell_id)` to get its current code, output, and any error.
2. Address the user's comment in terms of that cell's actual, current content — not whatever the pane was showing when they clicked. The pane's rendering can be stale by the time you respond; `read_cell` is the source of truth.
3. If the comment implies a change, follow the normal [edit loop](edit-loop.md) on those cells.

## Multiple cells

When several cells are linked, read all of them before responding — the comment may be about the relationship between them (e.g. "this plot doesn't reflect that slider"), not just one cell in isolation.

## Errored cells

If an annotated cell has an error, `read_cell` returns the same `error` object (`kind`, `hint`, `boundaries`, `fixes`) described in [errors.md](errors.md) — read it rather than inferring the problem from the user's comment or the pane's rendering.
