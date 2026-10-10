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
wanted <- c("%||%", "package_step", "structure_error")
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

cat(sprintf("\n%d passed, %d failed\n", passed, failed))
if (failed > 0) quit(status = 1)
