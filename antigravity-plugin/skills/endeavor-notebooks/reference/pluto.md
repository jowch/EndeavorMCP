# Pluto notebooks (Julia)

Read this when the notebook is a Pluto notebook (a `.jl` file). It covers what Pluto adds to the general rules.

## What a cell may hold

- **One expression per cell.** Two statements on separate lines fail with `error.kind` `pluto_multi_expression`. Wrap them in `begin ... end`, or `let ... end` when the temporaries should not become notebook variables. Keep the statements of one step together in one block rather than one line per cell, and start a new cell for each step a reader would look at on its own.
- **One definition per variable.** Every top-level name is a notebook variable, and two cells that assign the same name both fail, each with `error.kind` `runtime` and a `msg` starting "Multiple definitions for". Before you add a top-level name, check with `find_symbol_definitions` that no other cell defines it. To fix a duplicate, rename, merge the cells, or delete one; no tool disables a cell. For the same reason, don't change a value from another cell (`push!`, `plot!` on a plot made elsewhere): Pluto tracks assignments, not mutation, so the result depends on run order. Build a value fully in the cell that defines it.
- **Packages.** Put every `using` in one cell near the top. Pluto installs missing packages into the notebook's own environment when that cell first runs, which can take minutes: run that cell without `wait_for_completion` and read it again later. Don't call `Pkg.add` or `Pkg.activate`; activating an environment turns Pluto's package handling off for the notebook. Do that only if the user wants their own project environment.
- **Prose** is a cell whose code is `md"..."`. Fold it.
- **Widgets.** `@bind name Slider(1:10)` (widgets come from `PlutoUI`) must be the value of its cell: alone, inside `md"... $(@bind name Slider(1:10))"`, or last in a block. Other cells use `name` and rerun when the user moves the widget. The tools can't set a widget's value.

## Errors

- `read_cell` gives `error.kind`: `pluto_multi_expression` (with `hint`, and `boundaries`, the positions where the statements split) or `runtime` (with `msg`, the Julia error).
- `validate_cell` checks code without changing the notebook. Its `errors[].type` is `pluto_multi_expression` or `syntax_error`. It does not check names other cells define, so it can't catch a duplicate definition.

## A usual layout

Five cells, one per block:

```julia
md"# Harvest by year"                      # folded

begin                                       # all packages
    using CSV, DataFrames, Plots, PlutoUI
end

harvest = CSV.read("harvest.csv", DataFrame)

@bind crop Select(unique(harvest.crop))

begin                                       # one plot, built in one cell
    rows = harvest[harvest.crop .== crop, :]
    plot(rows.year, rows.tonnes; label=crop, xlabel="year", ylabel="tonnes")
end
```

Here `rows` is a notebook variable. If another cell also needs a name like it, use `let` instead of `begin`.
