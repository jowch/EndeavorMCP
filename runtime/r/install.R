# Install Ember for R notebooks from Ember's r-universe repository, or update
# it when a newer build is there. The core runs it each time it starts R for an
# R notebook, before R's adapter:
#
#   Rscript --vanilla runtime/r/install.R <folder> <repository> [<mac-arm64>]
#
# <mac-arm64> is where Ember's CI publishes its Apple Silicon Mac builds, one
# repository per R version (<mac-arm64>/macos-arm64-r4.6): r-universe has none
# (Ember #66). On an Apple Silicon Mac they come first, then r-universe.
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

# The newest build, from the first of these places that lists one: Ember's own
# Apple Silicon build, a binary where r-universe has one for this platform and
# R, else the source package. Its row of the index, which the download uses
# too, so it gets the build chosen here even if the index changes meanwhile.
# NULL when ember isn't listed (or a repository didn't answer, which R only
# warns about), NA when R couldn't ask.
options(timeout = 20)
newest <- function(where) tryCatch({
  listed <- suppressWarnings(available.packages(contriburl = where$contriburl, type = where$type, fields = "SHA256"))
  if ("ember" %in% rownames(listed)) listed["ember", , drop = FALSE]
}, error = function(e) {
  message(conditionMessage(e))
  NA
})
binary <- .Platform$pkgType
places <- list(list(contriburl = contrib.url(universe, "source"), type = "source"))
if (binary != "source") places <- c(list(list(contriburl = contrib.url(universe, binary), type = binary)), places)
if (length(args) > 2 && nzchar(args[[3]]) && binary != "source" && startsWith(R.version$platform, "aarch64-apple-darwin")) {
  version <- paste(R.version$major, sub("\\..*", "", R.version$minor), sep = ".")
  places <- c(list(list(contriburl = paste0(args[[3]], "/macos-arm64-r", version), type = binary)), places)
  # Without the developer tools, r-universe's source can't be built here. When
  # Ember's own build can't be reached and an Ember is installed, trying it
  # would only fail and say the newest Ember didn't work, so keep the installed one.
  tools_here <- suppressWarnings(system2("xcode-select", "-p", stdout = FALSE, stderr = FALSE)) == 0
  if (installed(state$Current) && !tools_here) places <- Filter(function(where) where$type != "source", places)
}
for (where in places) {
  listed <- newest(where)
  if (!is.null(listed)) break
}
type <- where$type
build <- if (is.matrix(listed)) listed[1, ]
if (is.null(build)) {
  if (installed(state$Current)) {
    message("Couldn't reach Ember's repositories, or they list no Ember for this R; R notebooks use the installed Ember, ", state$Current, ".")
    quit(status = 0)
  }
  message("Couldn't reach Ember's repositories (", universe, ") to install Ember for R notebooks.")
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
file <- tryCatch(download.packages("ember", downloads, available = listed, contriburl = where$contriburl, type = type, quiet = TRUE)[1, 2], error = function(e) {
  message(conditionMessage(e))
  NA
})
if (is.na(file)) fail(paste("Couldn't download Ember", name, "from", where$contriburl), blame = FALSE)
# The file is the build the index lists, not one cut short or changed on the
# way. tools::sha256sum came with R 4.5; before it, coreutils' sha256sum.
sha256_of <- function(file) {
  if (exists("sha256sum", asNamespace("tools"))) return(unname(tools::sha256sum(file)))
  if (!nzchar(Sys.which("sha256sum"))) return(NA)
  sub(" .*", "", system2("sha256sum", shQuote(file), stdout = TRUE)[[1]])
}
got <- if (!is.na(build[["SHA256"]])) sha256_of(file) else NA
if (!is.na(got) && got != build[["SHA256"]]) fail(paste0("The download of Ember ", name, " from ", where$contriburl, " isn't the file its index lists (SHA256 ", got, ")"), blame = FALSE)
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
