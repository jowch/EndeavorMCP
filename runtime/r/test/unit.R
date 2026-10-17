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
wanted <- c("%||%", "package_step", "structure_error", "input_line", "json_value", "json_choice")
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
state <- function(status = "ready", progress = NULL, log = character()) {
  list(packages = list(target = list(status = status, progress = progress, log = log)))
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
p <- A$package_step(rec, snap(cell(TRUE, "dplyr")), state("checking"))
check("checking", identical(p$step, "checking"))
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
check("a number for a slider is left as it is",
      identical(A$json_value(slider, 7L), 7L))

cat(sprintf("\n%d passed, %d failed\n", passed, failed))
if (failed > 0) quit(status = 1)
