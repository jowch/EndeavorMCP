# Ember notebooks (R)

Read this when the notebook is an Ember notebook (a `.R` file). It covers what Ember adds to the general rules. R notebooks run on macOS and Linux, not on Windows. They don't need Julia.

## What a cell may hold

- **Any number of lines.** A cell is a short R script: one step a reader would look at on its own. The value of its last line is its output, as R prints it at the console; a last line that assigns (`x <- ...`) shows the assigned value, and `library()` or `invisible()` shows nothing. Earlier lines that show a value, `print()`, `cat()`, messages and warnings go to the cell's console, in order. `read_cell` gives both in one `output`: the console first, then the output.
- **One definition per variable.** Every name a cell assigns at the top level is a notebook variable, and only one cell may define it. Changing a variable in another cell defines it again: `df$col <- v`, `names(df) <- ...` and `df <- df |> filter(...)` in a later cell all fail with `error.kind` `multiple_definitions`, in both cells. Build a value fully in the cell that defines it, or give the result a new name (`df2 <- ...`). Before you add a top-level name, check with `find_symbol_definitions` that no other cell defines it.
- **Private names.** A name that starts with a dot (`.tmp`, `.i`) belongs to its cell: other cells can't see it, so several cells can each use `.i` as a loop variable. Use dot names for a cell's temporary values. A cell that uses another cell's dot name fails with `private_name`: drop the dot in both cells.
- **Packages.** Attach each package with `library()` in one cell; the same package attached in two cells fails with `package_conflict`. `pkg::fn` works in any cell. Ember installs the packages the code names, at versions from the notebook's date, the first time a cell that needs them runs. That can take minutes, and the cells that may need them stay `queued` meanwhile, with `packages` and a `message` in the results. Tell the user it is installing, then call `read_cell` on a queued cell: while packages install, each read waits up to 45 seconds. Follow the message. Don't call `install.packages()`. If a package fails to install because it needs a compiler (the error names a missing compiler, `make` or `clang`), tell the user plainly that the package has to be built and this computer can't build it; don't try again. A cell that fails with `missing_package` needs a package the code doesn't name, such as svglite for `ggsave("plot.svg")`. Do what its `fixes` says: add `requireNamespace("svglite")` as the first line of that cell and run the cell. Ember installs the package first.
- **The package date.** Package versions are the ones current on the notebook's date, `package_date` in `list_notebooks` and `open_notebook`. Inside the notebook, `available.packages()` and `old.packages()` read that date's index, so they can't tell you whether newer versions exist. To update the packages, or move the date, call `preview_package_update` (no `date` means today, the newest), tell the user what would change, then `apply_package_update` with the preview's `date`. Don't edit the date in the file.
- **Settings are definitions too.** `options()`, `Sys.setenv()`, `setwd()`, `Sys.setlocale()`, `ggplot2::theme_set()` and `attach()` set something for every cell, so each setting may be set in only one cell (`setting_conflict` otherwise), and that cell runs first. For a change meant for one piece of code, use `withr::with_options()` and the other `withr::with_*` functions, or pass the argument directly (`print(x, digits = 3)`). `set.seed()` is not a setting: call it in the cell that draws the random numbers.
- **Prose** is a cell of only `#'` lines (`#' # Harvest by year`). Text and code in one cell fail with `mixed_text`: put them in separate cells. Fold prose cells.
- **Inputs.** A slider, number box, checkbox, text box, dropdown or radio buttons comes from the emberui package: `n <- input_slider(10, 500, value = 100)` at the top of a cell, and other cells read `n` as a plain number. `read_cell` lists a cell's inputs under `inputs`, with what each allows and its value. `set_input` sets one, as the user moving the widget does: the cells that read it rerun (or turn stale in lazy mode), and its own cell doesn't. Nothing else in the input's cell may read or redefine `n`, or the input isn't bound; `inputs` says so. A value set this way is gone when the notebook closes; to keep one, edit the default in the code.
- A line can't start with `# %%` or `# ///`: Ember marks cells with those in the file.

## Runs

- Running a cell also runs the cells that depend on it. A cell whose result was made before a cell it depends on last ran shows `stale` in `read_cell`. Run it to bring it up to date.
- After the notebook restarts, or when it opens without running, every cell shows `not_run` and keeps no output until it runs. Run the cells you need, or `run_all_cells`.
- Ember can put a note on a cell's last run. A run's `warnings` give each one as `kind::Cell <id>: text`, and `read_cell` lists them in `notes`:
  - `changed_in_place`: the cell changed, by reference, a value another cell defines, such as a data.table changed with `:=` or `set()`, an environment or an R6 object. The cells that read it don't rerun, so their results and a rerun of this cell can be wrong. Make the change in the cell that defines the value.
  - `settings_found`: the run set a global setting, such as `options()`, so the cell became a settings cell and the cells after it reran.
  - `load_note`: a package the cell loaded reset a setting that a settings cell sets. Move that setting into this cell, after the package loads.

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
