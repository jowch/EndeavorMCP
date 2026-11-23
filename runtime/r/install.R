
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

options(timeout = 20)
reached <- FALSE
newest <- function(where) tryCatch({
  listed <- suppressWarnings(available.packages(contriburl = where$contriburl, type = where$type, fields = "SHA256"))
  if (nrow(listed)) reached <<- TRUE
  if ("ember" %in% rownames(listed)) listed["ember", , drop = FALSE]
}, error = function(e) {
  message(conditionMessage(e))
  NA
})
binary <- .Platform$pkgType
places <- list(list(contriburl = contrib.url(universe, "source"), type = "source"))
if (binary != "source") places <- c(list(list(contriburl = contrib.url(universe, binary), type = binary)), places)
if (binary != "source" && startsWith(R.version$platform, "aarch64-apple-darwin")) {
  tools_here <- suppressWarnings(system2("xcode-select", "-p", stdout = FALSE, stderr = FALSE)) == 0
  if (installed(state$Current) && !tools_here) places <- Filter(function(where) where$type != "source", places)
}
unreachable <- file.path(folder, "unreachable")
if (installed(state$Current) && file.exists(unreachable) && difftime(Sys.time(), file.mtime(unreachable), units = "hours") < 1) {
  message("Ember's repositories didn't answer within the last hour; R notebooks use the installed Ember, ", state$Current, ".")
  quit(status = 0)
}
for (where in places) {
  listed <- newest(where)
  if (!is.null(listed)) break
}
type <- where$type
build <- if (is.matrix(listed)) listed[1, ]
if (reached) unlink(unreachable) else if (installed(state$Current)) invisible(file.create(unreachable))
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

options(timeout = max(600, getOption("timeout")), Ncpus = max(1L, parallel::detectCores() - 1L))
dir.create(deps, showWarnings = FALSE)
.libPaths(c(deps, .libPaths()))
names_in <- function(field) if (is.na(field)) character() else trimws(sub("\\(.*", "", strsplit(field, ",", fixed = TRUE)[[1]]))
direct <- unique(unlist(lapply(c("Depends", "Imports", "LinkingTo"), function(field) names_in(build[[field]]))))
on_cran <- tryCatch(available.packages(repos = cran), error = function(e) fail(paste("Couldn't reach CRAN:", conditionMessage(e)), blame = FALSE))
tree <- unique(c(direct, unlist(tools::package_dependencies(direct, db = on_cran, which = c("Depends", "Imports", "LinkingTo"), recursive = TRUE))))
tree <- setdiff(tree, c("R", rownames(installed.packages(priority = "base"))))
wanted <- list()
needs <- function(field) {
  if (is.na(field)) return()
  for (dep in strsplit(field, ",", fixed = TRUE)[[1]]) {
    parts <- regmatches(dep, regexec("^\\s*([[:alnum:].]+)\\s*\\(\\s*>=?\\s*([^) ]+)\\s*\\)", dep))[[1]]
    if (length(parts) == 3 && parts[[2]] %in% tree) {
      known <- wanted[[parts[[2]]]]
      if (is.null(known) || package_version(parts[[3]]) > known) wanted[[parts[[2]]]] <<- package_version(parts[[3]])
    }
  }
}
for (field in c("Depends", "Imports", "LinkingTo")) {
  needs(build[[field]])
  for (pkg in intersect(tree, rownames(on_cran))) needs(on_cran[pkg, field])
}
too_old <- function(pkg) {
  if (is.null(wanted[[pkg]])) return(FALSE)
  found <- suppressWarnings(packageDescription(pkg, lib.loc = .libPaths(), fields = "Version"))
  !is.na(found) && package_version(found) < wanted[[pkg]]
}
loads <- function(pkg) !too_old(pkg) && suppressWarnings(requireNamespace(pkg, quietly = TRUE))
fails_fresh <- function(pkgs, lib) {
  script <- tempfile("ember-check", fileext = ".R")
  on.exit(unlink(script))
  writeLines(c(
    paste0(".libPaths(", paste(deparse(lib), collapse = ""), ")"),
    paste0("for (pkg in ", paste(deparse(pkgs), collapse = ""), ") tryCatch(suppressWarnings(suppressPackageStartupMessages(loadNamespace(pkg))), error = function(e) {"),
    "  message(pkg, \": \", conditionMessage(e))",
    "  cat(\"failed\", pkg, \"\\n\")",
    "})",
    "cat(\"checked\\n\")"), script)
  out <- suppressWarnings(system2(file.path(R.home("bin"), "Rscript"), c("--vanilla", shQuote(script)), stdout = TRUE))
  if (!("checked" %in% trimws(out))) return(pkgs)
  intersect(pkgs, sub("^failed ", "", trimws(grep("^failed ", out, value = TRUE))))
}
missing <- tree[!vapply(tree, loads, logical(1))]
if (length(missing)) {
  message("Installing ", paste(missing, collapse = ", "), " for R notebooks")
  install.packages(missing, lib = deps, repos = cran)
  still <- union(Filter(too_old, missing), fails_fresh(missing, .libPaths()))
  if (length(still)) fail(paste("Couldn't install", paste(still, collapse = ", "), "for R notebooks; R's messages are above."), blame = FALSE)
}

message(if (installed(state$Current)) paste("Updating Ember for R notebooks from", state$Current, "to", name) else paste("Installing Ember", name, "for R notebooks"))
unlink(part, recursive = TRUE)
dir.create(part)
downloads <- tempfile("ember")
dir.create(downloads)
file <- tryCatch(download.packages("ember", downloads, available = listed, contriburl = where$contriburl, type = type, quiet = TRUE)[1, 2], error = function(e) {
  message(conditionMessage(e))
  NA
})
if (is.na(file)) fail(paste("Couldn't download Ember", name, "from", where$contriburl), blame = FALSE)
sha256_of <- function(file) {
  if (exists("sha256sum", asNamespace("tools"))) return(unname(tools::sha256sum(file)))
  if (!nzchar(Sys.which("sha256sum"))) return(NA)
  sub(" .*", "", system2("sha256sum", shQuote(file), stdout = TRUE)[[1]])
}
got <- if (!is.na(build[["SHA256"]])) sha256_of(file) else NA
if (!is.na(got) && got != build[["SHA256"]]) fail(paste0("The download of Ember ", name, " from ", where$contriburl, " isn't the file its index lists (SHA256 ", got, ")"), blame = FALSE)
install.packages(file, lib = part, repos = NULL, type = type)
loaded <- length(fails_fresh("ember", c(part, .libPaths()))) == 0
if (!loaded || !dir.exists(file.path(part, "ember"))) fail(paste("Ember", name, "didn't install or load; R's messages are above."))
if (!file.rename(part, file.path(folder, name))) unlink(part, recursive = TRUE)
message("Installed Ember ", name, " (commit ", packageDescription("ember", lib.loc = file.path(folder, name))$RemoteSha, ") for R notebooks")

state <- read_state()
if (state$Current != name) {
  if (nzchar(state$Previous) && state$Previous != name) invisible(Sys.setFileTime(file.path(folder, state$Previous), Sys.time()))
  state$Previous <- if (installed(state$Current)) state$Current else ""
  state$Current <- name
  write_state(state)
}

others <- setdiff(list.files(folder), c("deps", "ember.dcf", state$Current, state$Previous))
age <- difftime(Sys.time(), file.mtime(file.path(folder, others)), units = "days")
unlink(file.path(folder, others[!grepl(".", others, fixed = TRUE) & age > 7]), recursive = TRUE)
unlink(file.path(folder, others[grepl(".part.", others, fixed = TRUE) & age > 1]), recursive = TRUE)
