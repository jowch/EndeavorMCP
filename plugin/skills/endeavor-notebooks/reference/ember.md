# Ember notebooks (R)

Read this when the notebook is an Ember notebook (a `.R` file). It covers what Ember adds to the general rules. R notebooks run on macOS and Linux, not on Windows, and not yet in the Endeavor app. Endeavor's runtime needs Julia for them too.

## What a cell may hold

- **Any number of lines.** A cell is a short R script: one step a reader would look at on its own. Its last value is shown as its output, as R prints it at the console, after anything the cell printed and its warnings.
- **One definition per variable.** Every name a cell assigns at the top level is a notebook variable, and only one cell may define it. Changing a variable in another cell defines it again: `df$col <- v`, `names(df) <- ...` and `df <- df |> filter(...)` in a later cell all fail with `error.kind` `multiple_definitions`, in both cells. Build a value fully in the cell that defines it, or give the result a new name (`df2 <- ...`). Before you add a top-level name, check with `find_symbol_definitions` that no other cell defines it.
- **Private names.** A name that starts with a dot (`.tmp`, `.i`) belongs to its cell: other cells can't see it, so several cells can each use `.i` as a loop variable. Use dot names for a cell's temporary values. A cell that uses another cell's dot name fails with `private_name`: drop the dot in both cells.
- **Packages.** Attach each package with `library()` in one cell; the same package attached in two cells fails with `package_conflict`. `pkg::fn` works in any cell. Ember installs the packages the code names, at versions from the notebook's date, the first time a cell that needs them runs. That can take minutes, and the cells that need them stay `queued` meanwhile: run without `wait_for_completion` and read a queued cell again later. Don't call `install.packages()`. A cell that fails with `missing_package` needs a package the code doesn't name; tell the user its `fixes`, since the tools can't add it.
- **Settings are definitions too.** `options()`, `Sys.setenv()`, `setwd()`, `Sys.setlocale()`, `ggplot2::theme_set()` and `attach()` set something for every cell, so each setting may be set in only one cell (`setting_conflict` otherwise), and that cell runs first. For a change meant for one piece of code, use `withr::with_options()` and the other `withr::with_*` functions, or pass the argument directly (`print(x, digits = 3)`). `set.seed()` is not a setting: call it in the cell that draws the random numbers.
- **Prose** is a cell of only `#'` lines (`#' # Harvest by year`). Text and code in one cell fail with `mixed_text`: put them in separate cells. Fold prose cells.
- **No widgets.** Ember has no `@bind`; use a variable the user can edit.
- A line can't start with `# %%` or `# ///`: Ember marks cells with those in the file.

## Runs

- Running a cell also runs the cells that depend on it. A cell whose result was made before a cell it depends on last ran shows `stale` in `read_cell`. Run it to bring it up to date.
- After `restart_notebook`, or when a notebook opens without running, every cell shows `not_run` and keeps no output until it runs. Run the cells you need, or `run_all_cells`.

## Errors

- `read_cell` gives `error.kind`: `runtime` (with `msg`, R's error), or one of the kinds above, `cycle` (cells that depend on each other) or `parse`. Many come with `fixes`, which say what to change, and `names`, the variables involved. Follow the fix in every cell it names.
- `validate_cell` checks code without changing the notebook. Its `errors[].type` is `syntax_error` or `marker_line`. It does not check names other cells define, so it can't catch a duplicate definition.

## A usual layout

Five cells, one per step:

```r
#' # Harvest by year                      (folded)

library(dplyr)                             # each package in one cell
library(ggplot2)

harvest <- read.csv("harvest.csv")

chosen <- "wheat"                          # the user changes this

.rows <- filter(harvest, crop == chosen)   # one plot, built in one cell
ggplot(.rows, aes(year, tonnes)) + geom_line() + labs(title = chosen)
```

Here `.rows` is private to the last cell, so another cell can use the name `.rows` for its own rows.
