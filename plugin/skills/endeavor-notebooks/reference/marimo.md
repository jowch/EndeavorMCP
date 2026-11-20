# marimo notebooks (Python)

Read this when the notebook is a marimo notebook (a `.py` file with `app = marimo.App()`). It covers what marimo adds to the general rules. Python notebooks run on macOS and Linux, not on Windows, and need uv: if a tool fails with `python_not_found`, tell the user to install uv and try again. They don't need Julia.

## What a cell may hold

- **Any number of lines.** A cell is a short Python script: one step a reader would look at on its own. The value of its last line is its output, shown as marimo shows it (a table for a DataFrame, a plot for a figure). A last line that assigns or imports shows nothing. `print()` and warnings go to the cell's console, which `read_cell` gives in `output` before the value. A table's output gives its first 10 rows and says how many there are: to see others, show them (`df.tail(5)`, `df.iloc[10:20]`).
- **Write the cell's body only.** In the file, marimo wraps each cell in a function (`@app.cell`, `def _(...)`, `return`). Don't write those lines: give the code a cell runs, and marimo writes the rest.
- **One definition per name.** Every name a cell assigns or imports at the top level is a notebook variable, and only one cell may define it. A second cell that assigns `df` fails with `error.kind` `multiple_definitions`, in both cells, and `names` says which. Changing a value from another cell (`df["col"] = v`, `items.append(x)`) isn't seen: marimo tracks names, not mutation, so cells that read it don't rerun. Build a value fully in the cell that defines it, or give the result a new name (`df2 = ...`). Before you add a top-level name, check with `find_symbol_definitions` that no other cell defines it.
- **Private names.** A name that starts with an underscore (`_tmp`, `_i`) belongs to its cell: other cells can't see it, and several cells can each use `_i`. Use them for a cell's temporary values. A cell that uses another cell's `_name` fails with a `NameError` that says so: drop the underscore in both cells.
- **Packages.** Import a package in a cell as usual (`import pandas as pd`). The first time a run imports one the notebook doesn't have, Endeavor installs it into the notebook's own environment and runs the cell again, so the run takes longer. A package new to the notebook waits for the user's OK first, as in Julia: the cells stay `queued`, `packages.step` is `asking` with `packages.new`, and `packages.download` names the PyPI packages. Do what the result's message and `approve_install`'s description say; don't wait for it with `read_cell`. Check the names first: a misspelled import asks for a package of that name. A name a letter away from a widely used package's has `close_to` naming that package, and the message says so. That's only a hint: real packages can sit close to common ones. If you meant the other package, fix the import, run again, and tell the user you changed it. If you meant this one, ask as usual and mention the close name. The notebook's header then lists it at the exact version installed. Versions are capped at the notebook's package date, the `exclude-newer` line in the header, so the notebook installs the same versions next year. Don't call `pip install`, and don't edit the header. No tool moves the date yet: if the user needs a newer version, say so.
- **Prose** is a cell holding only `mo.md("""...""")`, which needs `import marimo as mo` in one cell. Make that the first cell of a new notebook. Show computed values with an f-string: `mo.md(f"The mean is {mean_tonnes:.1f} t")`. The cell reads the variables it names, so it reruns when they change. Write numbers in the text this way rather than typing them. Fold prose cells.
- **Widgets.** `mo.ui.slider(...)` and the other `mo.ui` controls show on the notebook's page, where the user can move them. You can't set them yet (`set_input` fails with `not_supported`).
- **Files.** A cell runs in the notebook's folder, so a relative path (`pd.read_csv("data.csv")`) is relative to the `.py` file.

## Runs

- A run takes the cells that depend on the cells you ran, as in Julia, and runs a cell's stale or unrun ancestors first. A run from marimo's own page is lazy: it marks the cells below `stale` and doesn't run them. Run a stale cell when the user needs it current.
- After the notebook opens or restarts, every cell is `not_run` and has no output until it runs. Run the cells you need, or `run_all_cells`.
- A cell that stops because a cell it depends on failed has an error whose `msg` starts "Ancestor raised", and `cells` names the cell that failed. Fix that cell first.

## Safe preview

A Python notebook in safe preview can't be changed at all: edits fail with `execution_blocked`, as runs do, because its packages install as soon as it is opened for editing. Tell the user, and call `allow_execution` only if they ask you to work in it.

## The user's page

The user watches the notebook in marimo's own page in their browser (the `browser_url`). Your edits and outputs show there as you make them. If the user is typing in a cell when you edit it, your edit replaces their unsaved typing, as in the other engines. Read a cell again before you edit it, and if `stale_read` says the user changed it, see what they did first. Python notebooks can't show in the notebook view beside the chat yet: give the user the browser link.

## Errors

- `read_cell` gives `error.kind`: `runtime` (with `msg`, Python's exception, and `traceback`), `multiple_definitions`, `cycle` (cells that depend on each other) or `parse`.
- `validate_cell` checks code without changing the notebook. It doesn't check names other cells define, so it can't catch a duplicate definition.

## A usual layout

```python
import marimo as mo                        # first cell

mo.md("""# Harvest by year""")             # folded

import pandas as pd
import matplotlib.pyplot as plt

harvest = pd.read_csv("harvest.csv")

chosen = "wheat"                           # the user changes this

_rows = harvest[harvest.crop == chosen]    # one plot, built in one cell
_fig, _ax = plt.subplots()
_ax.plot(_rows.year, _rows.tonnes)
_ax.set_title(chosen)
_fig
```

Here `_rows` and `_fig` are private to the last cell, so another cell can use those names too.
