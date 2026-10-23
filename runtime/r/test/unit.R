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
wanted <- c("%||%", "package_step", "download_step", "structure_error", "snapshot_cell", "output_text", "text_output", "INLINE_SPAN", "cap_text", "TEXT_MAX", "cell_notes", "date_proposal", "update_proposal", "update_pending", "update_blocked")
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

# ---- cell_notes ----
check("no notes: none", length(A$cell_notes(list())) == 0)
n <- A$cell_notes(list(from_lock = c("mypkg", "", "other")))
check("install calls that didn't run: one note naming their packages, and the agent's way to a newer version",
      length(n) == 1 && startsWith(n, "from_lock::Install calls don't run in Ember: it installs mypkg, other from the notebook's lock") &&
        grepl("call preview_package_update", n, fixed = TRUE))
check("an install call that names no package: the note says packages",
      grepl("it installs packages from", A$cell_notes(list(from_lock = "")), fixed = TRUE))

# ---- snapshot_cell ----
view <- function(...) modifyList(list(id = "c", code = "x <- 1", kind = "code", status = "success", errors = list(), runtime = 0.1, last_run = 100, disabled = FALSE, disabled_by = NA_character_), list(...))
check("a cell that runs: not off", isFALSE(A$snapshot_cell(view())$off))
check("a disabled cell: off", isTRUE(A$snapshot_cell(view(disabled = TRUE))$off))
check("a cell downstream of a disabled one: off, though it ran before", isTRUE(A$snapshot_cell(view(disabled_by = "d"))$off))
check("an Ember without disabled_by: not off", isFALSE(A$snapshot_cell(view(disabled_by = NULL))$off))

# A text cell's output is its prose with the inline values put in, as the page shows it (#270).
prose <- "#' ## Results\n#'\n#' The slope is `r round(b, 3)` over `r n` plates.\n#' Write `` `r x` `` to show a value."
inline <- function(...) list(mime = "application/vnd.ember.inline", data = list(values = list(...)), text = "")
text_view <- function(...) view(kind = "markdown", code = prose, ...)
check("a text cell that ran: its prose with each value in place of its span",
      identical(A$snapshot_cell(text_view(output = inline("0.409", "72")))$output,
                "## Results\n\nThe slope is 0.409 over 72 plates.\nWrite `` `r x` `` to show a value."))
check("  and it is still a markdown cell", isTRUE(A$snapshot_cell(text_view(output = inline("0.409", "72")))$markdown))
check("a text cell not run yet: its prose with the spans as written",
      identical(A$snapshot_cell(text_view())$output,
                "## Results\n\nThe slope is `r round(b, 3)` over `r n` plates.\nWrite `` `r x` `` to show a value."))
check("a text cell edited since its values were made: the spans as written, not old values",
      grepl("`r round(b, 3)`", A$snapshot_cell(text_view(output = inline("0.409", "72"), code_differs = TRUE))$output, fixed = TRUE))
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

# ---- update_pending ----
opened <- list(packages = list(wanted_cache = "jsonlite", resolved_for = character()), file = list(lock = NULL))
resolved <- list(packages = list(wanted_cache = "jsonlite", resolved_for = "jsonlite"),
                 file = list(lock = list(entries = data.frame(name = "jsonlite", stringsAsFactors = FALSE))))
check("a package the code names, before the lock has it: pending", A$update_pending(opened, "jsonlite"))
check("once resolved: not pending", !A$update_pending(resolved, "jsonlite"))
offline <- opened
offline$packages$indexes <- list(`cran/2026-06-01` = list(status = "failed", message = "could not fetch the index for cran/2026-06-01"))
check("an index that failed: not pending, and blocked with its message (#267)",
      !A$update_pending(offline, "jsonlite") && grepl("could not fetch the index", A$update_blocked(offline, "jsonlite"), fixed = TRUE))
fetching <- opened
fetching$packages$indexes <- list(`cran/2026-06-01` = list(status = "fetching"))
check("an index still coming: pending, not blocked", A$update_pending(fetching, "jsonlite") && is.null(A$update_blocked(fetching, "jsonlite")))
check("a locked package isn't blocked by a failed index", is.null(A$update_blocked(within(offline, file <- resolved$file), "jsonlite")))
check("a package the code doesn't name, or an Ember that doesn't say: not pending",
      !A$update_pending(opened, "ggplot2") && !A$update_pending(list(packages = list(), file = list()), "jsonlite"))

# ---- date_proposal ----
# A preview of a day not published yet moved back to the newest that is (#247).
moved <- list(date = "2026-10-22", asked = "2026-10-23", status = "ready")
check("a preview moved back is found by the day asked for", identical(A$date_proposal(moved, "2026-10-23"), moved))
check("  and by the day it moved to", identical(A$date_proposal(moved, "2026-10-22"), moved))
check("  but not by another day", is.null(A$date_proposal(moved, "2026-10-21")))
check("a proposal with no `asked` (an older Ember) is found by its date",
      identical(A$date_proposal(list(date = "2026-10-22"), "2026-10-22"), list(date = "2026-10-22")))
check("no proposal: NULL", is.null(A$date_proposal(NULL, "2026-10-22")))

cat(sprintf("\n%d passed, %d failed\n", passed, failed))
if (failed > 0) quit(status = 1)
