# Install Ember for R notebooks from Ember's r-universe repository, or update
# it when a newer build is there. The core runs it each time it starts R for an
# R notebook, before R's adapter:
#
#   Rscript --vanilla runtime/r/install.R <folder> <repository>
#
# <folder> holds one library per Ember build, named by the first 12 characters
# of the SHA256 the repository lists for the file (r-universe's PACKAGES has no
# commit; the installed package's DESCRIPTION has it, as RemoteSha); `deps`, a library
# with the packages Ember needs that R lacks or can't load; and `ember.dcf`,
# which names the build in use (Current), the one before it (Previous) and the
# builds that failed to install or load here (Failed). R's adapter runs with
# R_LIBS=<folder>/<Current>:<folder>/deps.
#
# It exits with 0 when Current is ready: updated, already the newest, or an
# Ember is installed and the newest couldn't be reached or downloaded. With 3
# when the newest build failed to install or load and Current is the one before it.
# With 1 when there's no Ember to use. CRAN is ENDEAVOR_CRAN when set.

args <- commandArgs(TRUE)
folder <- args[[1]]
universe <- args[[2]]
cran <- Sys.getenv("ENDEAVOR_CRAN", "https://cloud.r-project.org")
dir.create(folder, recursive = TRUE, showWarnings = FALSE)
state_file <- file.path(folder, "ember.dcf")
deps <- file.path(folder, "deps")

read_state <- function() {
  state <- list(Current = "", Previous = "", Failed = "")
  read <- if (file.exists(state_file)) tryCatch(read.dcf(state_file), error = function(e) NULL)
  for (key in intersect(colnames(read), names(state))) if (!is.na(read[1, key])) state[[key]] <- read[1, key]
  state
}
write_state <- function(state) {
  tmp <- paste0(state_file, ".", Sys.getpid())
  write.dcf(matrix(unlist(state), nrow = 1, dimnames = list(NULL, names(state))), tmp)
  invisible(file.rename(tmp, state_file))
}
installed <- function(build) nzchar(build) && dir.exists(file.path(folder, build, "ember"))
state <- read_state()
failed <- strsplit(state$Failed, " ", fixed = TRUE)[[1]]

# The newest build: a binary where r-universe has one for this platform and R,
# else the source package. NULL when ember isn't listed, NA when the repository
# didn't answer.
options(timeout = 20)
newest <- function(type) tryCatch({
  listed <- suppressWarnings(available.packages(repos = universe, type = type, fields = "SHA256"))
  if ("ember" %in% rownames(listed)) listed["ember", ]
}, error = function(e) {
  message(conditionMessage(e))
  NA
})
type <- .Platform$pkgType
build <- if (type != "source") newest(type)
if (is.null(build)) {
  type <- "source"
  build <- newest(type)
}
if (!is.character(build)) {
  if (installed(state$Current)) {
    message("Couldn't reach Ember's repository (", universe, "); R notebooks use the installed Ember, ", state$Current, ".")
    quit(status = 0)
  }
  message("Couldn't reach Ember's repository (", universe, ") to install Ember for R notebooks.")
  quit(status = 1)
}
sha <- if (is.na(build[["SHA256"]])) build[["Version"]] else build[["SHA256"]]
name <- substr(sha, 1, 12)
if (name == state$Current && installed(name)) quit(status = 0)
if (name %in% failed && installed(state$Current)) {
  message("Ember ", name, " didn't install or load here before; R notebooks use Ember ", state$Current, ".")
  quit(status = 0)
}

part <- file.path(folder, paste0(name, ".part.", Sys.getpid()))
# Why not. `blame` when it's this build's fault: it isn't tried again until
# there's a newer one, and the core tells the agent. Otherwise (no network, or a
# package Ember needs didn't install) the next start tries again.
fail <- function(why, blame = TRUE) {
  message(why)
  unlink(part, recursive = TRUE)
  state <- read_state()
  if (blame) {
    state$Failed <- paste(unique(c(strsplit(state$Failed, " ", fixed = TRUE)[[1]], name)), collapse = " ")
    write_state(state)
  }
  if (installed(state$Current)) {
    message("R notebooks keep using Ember ", state$Current, ".")
    quit(status = if (blame) 3 else 0)
  }
  quit(status = 1)
}

# Ember's dependencies, all the way down, that this R can't load: missing ones,
# and ones in a library that were built for another R. They go into `deps`,
# which comes first, so a broken copy elsewhere is shadowed.
options(timeout = max(600, getOption("timeout")), Ncpus = max(1L, parallel::detectCores() - 1L))
dir.create(deps, showWarnings = FALSE)
.libPaths(c(deps, .libPaths()))
names_in <- function(field) if (is.na(field)) character() else trimws(sub("\\(.*", "", strsplit(field, ",", fixed = TRUE)[[1]]))
direct <- unique(unlist(lapply(c("Depends", "Imports", "LinkingTo"), function(field) names_in(build[[field]]))))
on_cran <- tryCatch(available.packages(repos = cran), error = function(e) fail(paste("Couldn't reach CRAN:", conditionMessage(e)), blame = FALSE))
tree <- unique(c(direct, unlist(tools::package_dependencies(direct, db = on_cran, which = c("Depends", "Imports", "LinkingTo"), recursive = TRUE))))
tree <- setdiff(tree, c("R", rownames(installed.packages(priority = "base"))))
loads <- function(pkg) suppressWarnings(requireNamespace(pkg, quietly = TRUE))
missing <- tree[!vapply(tree, loads, logical(1))]
if (length(missing)) {
  message("Installing ", paste(missing, collapse = ", "), " for R notebooks")
  install.packages(missing, lib = deps, repos = cran)
  still <- missing[!vapply(missing, loads, logical(1))]
  if (length(still)) fail(paste("Couldn't install", paste(still, collapse = ", "), "for R notebooks; R's messages are above."), blame = FALSE)
}

message(if (installed(state$Current)) paste("Updating Ember for R notebooks from", state$Current, "to", name) else paste("Installing Ember", name, "for R notebooks"))
unlink(part, recursive = TRUE)
dir.create(part)
# Downloaded first, so a download that fails isn't counted against the build.
downloads <- tempfile("ember")
dir.create(downloads)
file <- tryCatch(download.packages("ember", downloads, repos = universe, type = type, quiet = TRUE)[1, 2], error = function(e) {
  message(conditionMessage(e))
  NA
})
if (is.na(file)) fail(paste("Couldn't download Ember", name, "from", universe), blame = FALSE)
install.packages(file, lib = part, repos = NULL, type = type)
loaded <- tryCatch({
  loadNamespace("ember", lib.loc = c(part, .libPaths()))
  TRUE
}, error = function(e) {
  message(conditionMessage(e))
  FALSE
})
if (!loaded || !dir.exists(file.path(part, "ember"))) fail(paste("Ember", name, "didn't install or load; R's messages are above."))
# Another start may have installed it first.
if (!file.rename(part, file.path(folder, name))) unlink(part, recursive = TRUE)
message("Installed Ember ", name, " (commit ", packageDescription("ember", lib.loc = file.path(folder, name))$RemoteSha, ") for R notebooks")

state <- read_state()
if (state$Current != name) {
  # The build that stops being Previous is dated from now, for the cleanup below.
  if (nzchar(state$Previous) && state$Previous != name) invisible(Sys.setFileTime(file.path(folder, state$Previous), Sys.time()))
  state$Previous <- if (installed(state$Current)) state$Current else ""
  state$Current <- name
  write_state(state)
}

# Builds that are neither Current nor Previous, a week after they stopped being
# either: a runtime started before then may still have one loaded. And
# unfinished installs a day old.
others <- setdiff(list.files(folder), c("deps", "ember.dcf", state$Current, state$Previous))
age <- difftime(Sys.time(), file.mtime(file.path(folder, others)), units = "days")
unlink(file.path(folder, others[!grepl(".", others, fixed = TRUE) & age > 7]), recursive = TRUE)
unlink(file.path(folder, others[grepl(".part.", others, fixed = TRUE) & age > 1]), recursive = TRUE)
