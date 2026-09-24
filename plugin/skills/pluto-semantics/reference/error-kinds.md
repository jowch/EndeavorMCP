# Parse and runtime error kinds

## `pluto_multi_expression`

**Symptom:** `extra token after end of expression` + `Boundaries: [...]`

**Default fix (repair):** `edit_cell` with `begin`/`end` wrap in same cell → `submit_changes(wait_for_completion=false)`

**Alternative:** split at reactive boundaries using `error.boundaries` — see [cell-structure.md](cell-structure.md)

**Authoring:** avoid the error by following structure patterns (`imports_cell`, `widget_cell`, `compute_cell`) in [cell-structure.md](cell-structure.md).

**`error.fixes` order:** `wrap_begin_end` first, `split_cells` second

## `runtime`

Read `error.msg`, fix code, `submit_changes(wait_for_completion=false)`, re-read.

## Other Pluto errors agents see

| Error | Cause | Fix |
|-------|-------|-----|
| Multiple definitions | Same global in two cells | Disable one or merge |
| Cyclic reference | Dependency cycle | Break cycle |
| `@bind` widget missing | `@bind` not returning expression | Move to end or embed in `md`/`html` |

## Errors surfaced through annotation-mode comments

The user can select an errored cell in annotation mode and comment on it (e.g. "why is this red?"). The prompt gives you `pluto://notebook/{id}/cell/{id}` links, not the error text itself — call `read_cell(notebook_id, cell_id)` and read the `error` object (`kind`, `hint`, `boundaries`, `fixes`) from the response rather than guessing from the comment alone. See **pluto-workflow** → [annotations.md](../../pluto-workflow/reference/annotations.md).
