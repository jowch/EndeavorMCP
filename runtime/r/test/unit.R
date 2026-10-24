# Checks of the adapter's pure functions, with no Ember and no server: the
# functions are read out of adapter.R and run on hand-made values. Run from
# anywhere:
#
#   Rscript --vanilla runtime/r/test/unit.R
#
# e2e_r runs it.

args <- commandArgs(trailingOnly = FALSE)
here <- dirname(normalizePath(sub("^--file=", "", args[startsWith(args, "--file=")])))
adapter <- normalizePath(file.path(here, "..", "adapter.R"))

# Only these definitions are evaluated; the rest of the file (libraries, the
# server) never runs.
wanted <- c("%||%", "package_step", "download_step", "structure_error", "EXTRA_FIX", "snapshot_cell", "output_text", "text_output", "INLINE_SPAN", "cap_text", "TEXT_MAX", "cell_notes", "date_proposal", "update_proposal", "input_line", "json_value", "json_choice")
A <- new.env()
for (e in parse(adapter, keep.source = FALSE)) {
  if (is.call(e) && identical(e[[1]], as.name("<-")) && is.name(e[[2]]) && as.character(e[[2]]) %in% wanted) eval(e, A)
}
stopifnot(all(wanted %in% ls(A, all.names = TRUE)))

passed <- 0L
failed <- 0L
check <- function(what, ok) {
  if (isTRUE(ok)) {
    passed <<- passed + 1L
    cat("ok   ", what, "\n")
  } else {
    failed <<- failed + 1L
    cat("FAIL ", what, "\n")
  }
}

cell <- function(queued = FALSE, waiting_for = character()) list(queued = queued, waiting_for = waiting_for)
snap <- function(...) list(cells = list(...))
state <- function(status = "ready", progress = NULL, log = character(), download = NULL, no_binary = NULL) {
  list(packages = list(target = list(status = status, progress = progress, log = log, download = download, no_binary = no_binary)))
}
to_json <- function(x) as.character(jsonlite::toJSON(x, auto_unbox = TRUE, null = "null"))

# ---- package_step ----
rec <- new.env()
check("nothing waiting, not installing: NULL",
      is.null(A$package_step(rec, snap(cell(), cell()), state())))
check("a cell that names a missing package but isn't queued: NULL",
      is.null(A$package_step(rec, snap(cell(waiting_for = "dplyr")), state())))

rec <- new.env()
p <- tryCatch(A$package_step(rec, snap(cell(), cell()), state("installing")), error = function(e) e)
check("installing with no queued cell waiting: a step with no packages, not an error",
      is.list(p) && !inherits(p, "error") && identical(p$step, "installing") && length(p$packages) == 0)
if (requireNamespace("jsonlite", quietly = TRUE)) {
  check("  and its packages are an empty array", grepl('"packages":[]', to_json(p), fixed = TRUE))
}

rec <- new.env()
p <- A$package_step(rec, snap(cell(TRUE, c("tidyr", "dplyr")), cell(TRUE, "dplyr"), cell()), state())
check("queued cells waiting while resolving: step resolving, their packages sorted, 0 seconds",
      identical(p$step, "resolving") && identical(unclass(p$packages), c("dplyr", "tidyr")) && p$seconds == 0 && is.null(p$last_line))
rec$packages_since <- Sys.time() - 75
p <- A$package_step(rec, snap(cell(TRUE, "dplyr")), state("installing", list(done = 3, total = 12, current = "vctrs")))
check("seconds count from the first sighting", p$seconds >= 75 && p$seconds < 80)
check("installing several: step says how far", identical(p$step, "installing, 3 of 12"))
check("no log yet: last line is the package being installed", identical(p$last_line, "installing vctrs"))
p <- A$package_step(rec, snap(cell(TRUE, "dplyr")), state("installing", log = c("", "fetching https://user:tok@example.org/x.tar.gz", "  ")))
check("last line of the log, with a URL's user and token taken out",
      identical(p$last_line, "fetching https://example.org/x.tar.gz"))
check("  and nothing builds from source", is.null(p$source_builds))
p <- A$package_step(rec, snap(cell(TRUE, "dplyr")), state("installing", no_binary = c(cli = "3.6.2", rlang = "1.1.4")))
check("installing with packages that have no binary: source_builds names them with their versions",
      identical(unclass(p$source_builds), c("cli 3.6.2", "rlang 1.1.4")))
p <- A$package_step(rec, snap(cell(TRUE, "dplyr")), state("checking"))
check("checking", identical(p$step, "checking"))
big <- data.frame(package = c("BSgenome.Hsapiens.UCSC.hg38", "GO.db"), version = c("1.4.5", "3.22.0"),
                  bytes = c(870e6, NA), stringsAsFactors = FALSE)
p <- A$package_step(rec, snap(cell(TRUE, "GO.db")), state("asking", download = big))
check("asking: step asking, with the download's total and each package (an unknown size NULL)",
      identical(p$step, "asking") && p$download$bytes == 870e6 && length(p$download$packages) == 2 &&
        identical(p$download$packages[[1]]$package, "BSgenome.Hsapiens.UCSC.hg38") && is.null(p$download$packages[[2]]$bytes))
p <- A$package_step(new.env(), snap(cell()), state("asking", download = big))
check("asking with no queued cell: still a step, so the agent hears it", identical(p$step, "asking"))
if (requireNamespace("jsonlite", quietly = TRUE)) {
  check("  and its download's packages are an array", grepl('"packages":[{"package"', to_json(p), fixed = TRUE))
}
check("the work ending clears the first sighting",
      is.null(A$package_step(rec, snap(cell()), state())) && is.null(rec$packages_since))

# ---- structure_error ----
d <- A$structure_error(list(kind = "missing_package", message = "there is no package called 'svglite'",
                            names = "svglite", fixes = "Add svglite to [extra_packages]"))
check("a package the code doesn't name: the fix names it in the cell, not the header",
      identical(unclass(d$fixes), "Add `requireNamespace(\"svglite\")` as the first line of this cell and run it"))
d <- A$structure_error(list(kind = "missing_package", message = "there is no package called 'cli'; its install may have failed",
                            names = "cli", fixes = character()))
check("a missing package with no header fix: no fixes", is.null(d$fixes))
d <- A$structure_error(list(kind = "error", message = "boom", fixes = "Do this"))
check("another error keeps its fixes and reads as runtime", identical(d$kind, "runtime") && identical(unclass(d$fixes), "Do this"))
d <- A$structure_error(list(kind = "error", message = "The textdata package is required to download the AFINN lexicon.", fixes = "Add textdata to [extra_packages]"))
check("a run error naming a package (Ember #184): the fix the tools can do (trial 20)",
      identical(unclass(d$fixes), "Add `requireNamespace(\"textdata\")` as the first line of this cell and run it"))

# ---- cell_notes ----
check("no notes: none", length(A$cell_notes(list())) == 0)
n <- A$cell_notes(list(from_lock = c("mypkg", "", "other")))
check("install calls that didn't run: one note naming their packages, and the agent's way to a newer version",
      length(n) == 1 && startsWith(n, "from_lock::Install calls don't run in Ember: it installs mypkg, other from the notebook's lock") &&
        grepl("call preview_package_update", n, fixed = TRUE))
check("an install call that names no package: the note says packages",
      grepl("it installs packages from", A$cell_notes(list(from_lock = "")), fixed = TRUE))
n <- A$cell_notes(list(untracked_files = c("readRDS", "read_csv")))
check("readers given no file Ember can watch: one note naming them, worded as Ember's page words it",
      length(n) == 1 && startsWith(n, "untracked_file::Ember can't tell which file this cell read through readRDS(), read_csv(), so it doesn't rerun when the file changes.") &&
        grepl("Pass a file path or a file() connection", n, fixed = TRUE))
check("no reader left unresolved: no note", length(A$cell_notes(list(untracked_files = character()))) == 0)

# ---- snapshot_cell ----
view <- function(...) modifyList(list(id = "c", code = "x <- 1", kind = "code", status = "success", errors = list(), runtime = 0.1, last_run = 100, disabled = FALSE, disabled_by = NA_character_), list(...))
check("a cell that runs: not off", isFALSE(A$snapshot_cell(view())$off))
check("a disabled cell: off", isTRUE(A$snapshot_cell(view(disabled = TRUE))$off))
check("a cell downstream of a disabled one: off, though it ran before", isTRUE(A$snapshot_cell(view(disabled_by = "d"))$off))
check("an Ember without disabled_by: not off", isFALSE(A$snapshot_cell(view(disabled_by = NULL))$off))

# A cell rerun after an error, now waiting at a prompt: Ember keeps the last run's error on it
# while it runs. The agent sees what this run printed, not the old error (trial 20).
asking <- A$snapshot_cell(view(running = TRUE, status = "error",
                               errors = list(list(kind = "error", message = "textdata is required")),
                               output = list(mime = "text/plain", text = "old value"),
                               console = list(list(kind = "stdout", text = "License: CC BY. Do you accept?\n\n1: Yes\n2: No\n\n")),
                               prompt = list(id = 2L, text = "Selection: ", choices = c("Yes", "No"))))
check("a cell waiting at a prompt after an error: not errored, no old error", isFALSE(asking$errored) && is.null(asking[["error"]]))
check("  its output is what this run printed, the question's text", identical(asking$output, "License: CC BY. Do you accept?\n\n1: Yes\n2: No"))
check("  and no notes from the last run", length(A$snapshot_cell(view(running = TRUE, from_lock = "mypkg", prompt = list(id = 1L, text = "? ")))$notes) == 0)
check("  and the prompt", identical(asking$prompt$id, 2L) && identical(unclass(asking$prompt$choices), c("Yes", "No")))
check("a running cell not at a prompt keeps the last run's error, as before",
      isTRUE(A$snapshot_cell(view(running = TRUE, status = "error", errors = list(list(kind = "error", message = "boom"))))$errored))

# A text cell's output is its prose with the inline values put in, as the page shows it (#270).
prose <- "#' ## Results\n#'\n#' The slope is `r round(b, 3)` over `r n` plates.\n#' Write `` `r x` `` to show a value."
inline <- function(...) list(mime = "application/vnd.ember.inline", data = list(values = list(...)), text = "")
text_view <- function(...) view(kind = "markdown", code = prose, ...)
# Ember counts a span inside double backticks too, so the page shows a value there.
check("a text cell that ran: its prose with each value in place of its span",
      identical(A$snapshot_cell(text_view(output = inline("0.409", "72", "1")))$output,
                "## Results\n\nThe slope is 0.409 over 72 plates.\nWrite `` 1 `` to show a value."))
check("  and it is still a markdown cell", isTRUE(A$snapshot_cell(text_view(output = inline("0.409", "72", "1")))$markdown))
check("a text cell not run yet: its prose with the spans as written",
      identical(A$snapshot_cell(text_view())$output,
                "## Results\n\nThe slope is `r round(b, 3)` over `r n` plates.\nWrite `` `r x` `` to show a value."))
check("a text cell edited since its values were made: the spans as written, not old values",
      grepl("`r round(b, 3)`", A$snapshot_cell(text_view(output = inline("0.409", "72", "1"), code_differs = TRUE))$output, fixed = TRUE))
check("a text cell with no spans: its prose",
      identical(A$snapshot_cell(view(kind = "markdown", code = "#' Just words."))$output, "Just words."))

# ---- update_proposal ----
ready <- function(name, version) list(status = "ready", date = "2026-04-23", update = list(name = name, version = version))
newest <- list(name = "ggplot2", version = NULL)
check("the newest asked for: Ember's resolved version still matches (#267)",
      identical(A$update_proposal(ready("ggplot2", "4.0.3"), newest)$date, "2026-04-23"))
check("a pin asked for: only that version matches",
      !is.null(A$update_proposal(ready("dplyr", "1.1.2"), list(name = "dplyr", version = "1.1.2"))) &&
        is.null(A$update_proposal(ready("dplyr", "1.1.4"), list(name = "dplyr", version = "1.1.2"))))
check("another package's, a date's, or nothing asked: NULL",
      is.null(A$update_proposal(ready("dplyr", "1.1.2"), newest)) &&
        is.null(A$update_proposal(list(status = "ready", date = "2026-10-01"), newest)) &&
        is.null(A$update_proposal(ready("ggplot2", "4.0.3"), NULL)))

# ---- date_proposal ----
# A preview of a day not published yet moved back to the newest that is (#247).
moved <- list(date = "2026-10-22", asked = "2026-10-23", status = "ready")
check("a preview moved back is found by the day asked for", identical(A$date_proposal(moved, "2026-10-23"), moved))
check("  and by the day it moved to", identical(A$date_proposal(moved, "2026-10-22"), moved))
check("  but not by another day", is.null(A$date_proposal(moved, "2026-10-21")))
check("a proposal with no `asked` (an older Ember) is found by its date",
      identical(A$date_proposal(list(date = "2026-10-22"), "2026-10-22"), list(date = "2026-10-22")))
check("no proposal: NULL", is.null(A$date_proposal(NULL, "2026-10-22")))

# ---- input_line ----
input <- function(spec, value = spec$value, default = spec$value, name = "n", bound = TRUE, reason = NULL) {
  list(name = name, bound = bound, reason = reason, spec = spec, default = default, value = value)
}
slider <- list(type = "slider", value = 100, min = 10, max = 500, step = 10, label = NULL)
check("a slider says its range, step and value",
      identical(A$input_line(input(slider)), "n: slider 10 to 500, step 10, value 100"))
check("a value moved from its default says the default",
      identical(A$input_line(input(slider, value = 250)), "n: slider 10 to 500, step 10, value 250 (default 100)"))
check("a number with one bound and no step",
      identical(A$input_line(input(list(type = "number", value = 1.5, min = NA, max = 2, step = NA))), "n: number at most 2, value 1.5"))
check("a checkbox and a text box",
      identical(A$input_line(input(list(type = "checkbox", value = TRUE))), "n: checkbox, value TRUE") &&
      identical(A$input_line(input(list(type = "text", value = "Results"))), "n: text, value \"Results\""))
select <- list(type = "select", value = "pearson", choices = c(Pearson = "pearson", Spearman = "spearman"),
               labels = c("Pearson", "Spearman"), multiple = FALSE)
check("a dropdown names its labels and the values to send",
      identical(A$input_line(input(select)), "n: select of Pearson = \"pearson\", Spearman = \"spearman\", value \"pearson\""))
radio <- list(type = "radio", value = "a", choices = c("a", "b"), labels = c("a", "b"))
check("a choice whose label is the value is shown once",
      identical(A$input_line(input(radio)), "n: radio of \"a\", \"b\", value \"a\""))
check("an input read in another cell isn't bound, and says how to fix it",
      grepl("Not bound: read `n` in another cell.", A$input_line(input(slider, bound = FALSE, reason = "read_elsewhere")), fixed = TRUE))
check("an input not assigned at the top of a cell has no name",
      startsWith(A$input_line(input(slider, name = NULL, bound = FALSE, reason = "not_assigned")), "slider 10 to 500"))

# ---- json_value ----
check("JSON's 2 is the choice 2L",
      identical(A$json_value(list(type = "select", choices = 1:3), 2L), 2L) &&
      identical(A$json_value(list(type = "radio", choices = c(1.5, 2)), 2L), 2))
check("a string choice is itself, and one not offered is left for R to refuse",
      identical(A$json_value(select, "spearman"), "spearman") && identical(A$json_value(select, "kendall"), "kendall"))
check("a JSON array for a multiple dropdown is a vector of its choices",
      identical(A$json_value(list(type = "select", choices = c("a", "b", "c"), multiple = TRUE), list("c", "a")), c("c", "a")))
check("a JSON array for a single dropdown is one choice that is a vector",
      identical(A$json_value(list(type = "select", choices = list(a = c(2, 3), b = "x"), multiple = FALSE), list(2L, 3L)), c(2, 3)))
check("a number for a slider is left as it is",
      identical(A$json_value(slider, 7L), 7L))

cat(sprintf("\n%d passed, %d failed\n", passed, failed))
if (failed > 0) quit(status = 1)
