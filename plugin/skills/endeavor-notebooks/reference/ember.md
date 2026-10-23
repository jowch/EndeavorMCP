# Ember notebooks (R)

Read this when the notebook is an Ember notebook (a `.R` file). It covers what Ember adds to the general rules. R notebooks run on macOS and Linux, not on Windows. They don't need Julia.

## What a cell may hold

- **Any number of lines.** A cell is a short R script: one step a reader would look at on its own. The value of its last line is its output, as R prints it at the console; a last line that assigns (`x <- ...`) shows the assigned value, and `library()` or `invisible()` shows nothing. Earlier lines that show a value, `print()`, `cat()`, messages and warnings go to the cell's console, in order. `read_cell` gives both in one `output`: the console first, then the output.
- **One definition per variable.** Every name a cell assigns at the top level is a notebook variable, and only one cell may define it. Changing a variable in another cell defines it again: `df$col <- v`, `names(df) <- ...` and `df <- df |> filter(...)` in a later cell all fail with `error.kind` `multiple_definitions`, in both cells. Build a value fully in the cell that defines it, or give the result a new name (`df2 <- ...`). Before you add a top-level name, check with `find_symbol_definitions` that no other cell defines it.
- **Private names.** A name that starts with a dot (`.tmp`, `.i`) belongs to its cell: other cells can't see it, so several cells can each use `.i` as a loop variable. Use dot names for a cell's temporary values. A cell that uses another cell's dot name fails with `private_name`: drop the dot in both cells.
- **Packages.** Attach each package with `library()` in one cell; the same package attached in two cells fails with `package_conflict`. `pkg::fn` works in any cell. Ember installs the packages the code names, at versions from the notebook's date, the first time a cell that needs them runs. That can take minutes, and the cells that may need them stay `queued` meanwhile, with `packages` and a `message` in the results. Tell the user it is installing, then call `read_cell` on a queued cell: while packages install, each read waits up to 45 seconds. Follow the message. A large Bioconductor data download (over 500 MB by default) doesn't start by itself: `packages.step` is `asking` and `packages.download` lists the packages and sizes. Tell the user what it would download and how much, and call `approve_install` only after they say yes; don't wait for it with `read_cell`. Don't call `install.packages()`. If a package fails to install because it needs a compiler (the error names a missing compiler, `make` or `clang`), tell the user plainly that the package has to be built and this computer can't build it; don't try again. A cell that fails with `missing_package` needs a package the code doesn't name, such as svglite for `ggsave("plot.svg")`. Do what its `fixes` says: add `requireNamespace("svglite")` as the first line of that cell and run the cell. Ember installs the package first.
- **Packages from elsewhere.** A package that isn't on CRAN or Bioconductor needs an install call in a cell, as in plain R: `remotes::install_github("user/repo")` (or `devtools::`, with `@ref` or `ref =` for a branch, tag or commit), `pak::pak("user/repo")`, or `install.packages("pkg", repos = "https://owner.r-universe.dev")`. Then `library(pkg)` as usual. Write the call on a line of its own. Ember reads it before running, writes the source into the file's header and locks the commit; the call doesn't run, and the cell gets a `from_lock` note. A call inside `if` or another call isn't skipped and runs when reached, so don't wrap it in `if (!requireNamespace(...))` or `suppressMessages()`. Two calls naming different sources for one package fail with `source_conflict`: keep one. Only literal strings count, and a subdirectory (`"user/repo/pkg"`, `subdir =`) isn't supported.
- **The package date.** Package versions are the ones current on the notebook's date, `package_date` in `list_notebooks` and `open_notebook`. Inside the notebook, `available.packages()` and `old.packages()` read that date's index, so they can't tell you whether newer versions exist. To update the packages, or move the date, call `preview_package_update` (no `date` means today, the newest), tell the user what would change, then `apply_package_update` with the preview's `date`. To update one package, or pin it to a version, call `preview_package_update` with `package` (and `version`; none means CRAN's newest) instead of a date: Ember finds the date that puts that package there and moves the others as little as it can. A date moves every package, never one alone, so tell the user what else changes. Don't edit the date in the file, and don't look versions up on the web.
- **Settings are definitions too.** `options()`, `Sys.setenv()`, `setwd()`, `Sys.setlocale()`, `ggplot2::theme_set()` and `attach()` set something for every cell, so each setting may be set in only one cell (`setting_conflict` otherwise), and that cell runs first. For a change meant for one piece of code, use `withr::with_options()` and the other `withr::with_*` functions, or pass the argument directly (`print(x, digits = 3)`). `set.seed()` is not a setting: call it in the cell that draws the random numbers.
- **Graphics parameters end with the cell.** Each cell draws on its own device, so `par()`, `layout()` and `mfrow` last only for the cell that sets them; they are not settings. To share them, keep them in a variable (`fig_par <- list(mar = c(4, 4, 1, 1))`) and call `par(fig_par)` at the top of each figure cell.
- **Files.** A cell runs in the notebook's folder, so a relative path (`read.csv("data.csv")`, `write.csv(x, "results.csv")`) is relative to the `.R` file. Write files the notebook reads again there, not under `tempdir()`, which a restart replaces.
- **Prose** is a cell of only `#'` lines (`#' # Harvest by year`). Text and code in one cell fail with `mixed_text`: put them in separate cells. Fold prose cells.
- **Values in prose.** A prose line can show a computed value with `` `r expr` ``, as in R Markdown: `` #' The mean is `r round(mean(df$mpg), 1)` mpg. `` Each span is one expression on one line. The span reads other cells' variables like code does, so it reruns when they change. Write numbers in the text this way rather than typing them or building the sentence with `cat()` in a code cell.
- **No widgets.** Ember has no `@bind`; use a variable the user can edit.
- A line can't start with `# %%` or `# ///`: Ember marks cells with those in the file.

## Runs

- Running a cell also runs the cells that depend on it. A cell whose result was made before a cell it depends on last ran shows `stale` in `read_cell`. Run it to bring it up to date.
- A cell that `source()`s a file depends on that file too. When the file changes, the cell and the cells that depend on it rerun, or show `stale` in lazy mode (`on_cell_change` is `lazy`). After you edit a helper file, read the cells; don't run them again to pick the change up. A file newly added to a folder that a cell sources in a loop is picked up only when that cell runs again.
- After the notebook restarts, or when it opens without running, every cell shows `not_run` and keeps no output until it runs. Run the cells you need, or `run_all_cells`.
- **Prompts.** Code that asks at the console, such as `readline()`, `menu()`, `askYesNo()` or a package's "Do you want to download...?", waits for an answer, as R does at a console. Nothing runs until it gets one. A run's result and `read_cell` then show `waiting_for_input`: the cell, what it asked, and `menu()`'s `choices`; what it printed before is in the cell's output. Answer with `answer_prompt`, passing its `prompt_id` (for `menu()`, a choice's number or its text), or stop the run with `interrupt_notebook`. If the answer is the user's to give (a download, an overwrite, a choice of theirs), ask the user first and pass on their answer. Don't write code that prompts unless the user asked for it.
- Ember can put a note on a cell's last run. A run's `warnings` give each one as `kind::Cell <id>: text`, and `read_cell` lists them in `notes`:
  - `changed_in_place`: the cell changed, by reference, a value another cell defines, such as a data.table changed with `:=` or `set()`, an environment or an R6 object. The cells that read it don't rerun, so their results and a rerun of this cell can be wrong. Make the change in the cell that defines the value.
  - `settings_found`: the run set a global setting, such as `options()`, so the cell became a settings cell and the cells after it reran.
  - `load_note`: a package the cell loaded reset a setting that a settings cell sets. Move that setting into this cell, after the package loads.
  - `from_lock`: the cell's install calls didn't run; the notebook's lock installs those packages when a cell uses them. Nothing to fix. For a newer version, use `preview_package_update`.

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
