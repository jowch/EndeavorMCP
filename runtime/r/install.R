# Install Ember at the commit the core pins, into a library of Endeavor's own,
# with the packages it needs that R doesn't already have. The core runs it as
#
#   Rscript --vanilla runtime/r/install.R <library> <commit>
#
# the first time an R notebook is opened. It installs into <library>.part.<pid>
# and renames that to <library> once Ember loads from it, so a library by that
# name is complete. CRAN is ENDEAVOR_CRAN when set.

args <- commandArgs(TRUE)
library_dir <- args[[1]]
commit <- args[[2]]
part <- paste0(library_dir, ".part.", Sys.getpid())
unlink(part, recursive = TRUE)
dir.create(part, recursive = TRUE, showWarnings = FALSE)
.libPaths(c(part, .libPaths()))
repos <- c(CRAN = Sys.getenv("ENDEAVOR_CRAN", "https://cloud.r-project.org"))
options(Ncpus = max(1L, parallel::detectCores() - 1L))

have <- function(pkg) requireNamespace(pkg, quietly = TRUE)
fail <- function(what) {
  unlink(part, recursive = TRUE)
  message("Couldn't install ", what, " for R notebooks; R's messages are above.")
  quit(status = 1)
}

needed <- c("commonmark", "httpuv", "jsonlite", "later", "processx", "promises", "RcppMsgPack", "renv")
missing <- needed[!vapply(needed, have, logical(1))]
if (length(missing)) {
  message("Installing ", paste(missing, collapse = ", "), " for R notebooks")
  install.packages(missing, lib = part, repos = repos)
  if (!all(vapply(missing, have, logical(1)))) fail(paste(missing[!vapply(missing, have, logical(1))], collapse = ", "))
}

message("Installing Ember ", substr(commit, 1, 12), " for R notebooks")
tarball <- tempfile(fileext = ".tar.gz")
ok <- tryCatch({
  download.file(sprintf("https://codeload.github.com/jowch/Ember/tar.gz/%s", commit), tarball, mode = "wb", quiet = TRUE)
  TRUE
}, error = function(e) { message(conditionMessage(e)); FALSE })
if (!ok) fail("Ember")
install.packages(tarball, lib = part, repos = NULL, type = "source")
if (!requireNamespace("ember", lib.loc = part, quietly = TRUE)) fail("Ember")

if (!file.rename(part, library_dir)) {
  # Another start installed it first.
  unlink(part, recursive = TRUE)
  if (!dir.exists(library_dir)) fail("Ember")
}
