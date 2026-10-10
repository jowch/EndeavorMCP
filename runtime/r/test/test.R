# The Ember adapter end to end: starts runtime/r/adapter.R as the core does,
# then drives it over HTTP. Run from anywhere:
#
#   Rscript runtime/r/test/test.R
#
# Needs ember, processx and curl. Uses a temp folder, cache and data dir, so
# it never touches the user's own; every notebook sticks to base R, so nothing
# is installed.

suppressPackageStartupMessages({
  library(processx)
  library(curl)
  library(jsonlite)
})

args <- commandArgs(trailingOnly = FALSE)
here <- dirname(normalizePath(sub("^--file=", "", args[startsWith(args, "--file=")])))
adapter <- normalizePath(file.path(here, "..", "adapter.R"))

dir <- tempfile("ember-adapter-test-")
dir.create(dir)
dir <- normalizePath(dir)
state_file <- file.path(dir, "state.json")
token <- paste(sample(c(letters, 0:9), 40, replace = TRUE), collapse = "")

proc <- process$new(
  file.path(R.home("bin"), "Rscript"), c("--vanilla", adapter),
  env = c("current", ENDEAVOR_TOKEN = token, ENDEAVOR_R_STATE = state_file,
          R_USER_CACHE_DIR = file.path(dir, "cache"), R_USER_DATA_DIR = file.path(dir, "data"),
          R_LIBS = paste(.libPaths(), collapse = .Platform$path.sep)),
  wd = dir, stdout = file.path(dir, "adapter.log"), stderr = "2>&1", cleanup = TRUE)
on.exit({ if (proc$is_alive()) proc$kill(); unlink(dir, recursive = TRUE) }, add = TRUE)

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

deadline <- Sys.time() + 60
while (!file.exists(state_file) && Sys.time() < deadline && proc$is_alive()) Sys.sleep(0.1)
if (!file.exists(state_file)) {
  cat(readLines(file.path(dir, "adapter.log")), sep = "\n")
  stop("the adapter never wrote its state file")
}
st <- fromJSON(state_file)
check("state file has pid, bridge_port, ember_port, ember_secret",
      all(c("pid", "bridge_port", "ember_port", "ember_secret") %in% names(st)) && st$pid == proc$get_pid())
base <- sprintf("http://127.0.0.1:%d", st$bridge_port)

http <- function(method, path, body = NULL, auth = TRUE, timeout = 90) {
  h <- new_handle(timeout = timeout)
  headers <- list("Content-Type" = "application/json")
  if (auth) headers$Authorization <- paste("Bearer", token)
  do.call(handle_setheaders, c(list(h), headers))
  if (method == "POST") handle_setopt(h, postfields = body)
  r <- curl_fetch_memory(paste0(base, path), handle = h)
  list(status = r$status_code, body = rawToChar(r$content))
}

call <- function(method, params = setNames(list(), character())) {
  r <- http("POST", "/adapter", toJSON(list(method = method, params = params), auto_unbox = TRUE, null = "null"))
  stopifnot(r$status == 200)
  fromJSON(r$body, simplifyVector = FALSE)
}
result <- function(method, params = setNames(list(), character())) {
  reply <- call(method, params)
  if (!is.null(reply$error)) stop(method, ": ", reply$error)
  reply$result
}
notes <- function(after, timeout = 30) {
  r <- http("GET", sprintf("/notifications?after=%d", after), timeout = timeout)
  stopifnot(r$status == 200)
  fromJSON(r$body, simplifyVector = FALSE)
}
# Notifications after `after` until `done(all so far)` holds or 20 s pass.
collect <- function(after, done) {
  got <- list()
  end <- Sys.time() + 20
  while (Sys.time() < end) {
    batch <- notes(after, timeout = 30)
    got <- c(got, batch)
    if (length(batch) > 0) after <- max(vapply(batch, function(n) n$seq, numeric(1)))
    if (done(got)) break
  }
  got
}
cell_of <- function(snap, id) Find(function(c) identical(c$cell_id, id), snap$cells)
methods_of <- function(ns) vapply(ns, function(n) n$method, character(1))

# ---- Auth ----

check("401 without the token", http("POST", "/adapter", '{"method":"status"}', auth = FALSE)$status == 401)
check("401 for notifications without the token", http("GET", "/notifications", auth = FALSE)$status == 401)
check("400 for a body that isn't JSON", http("POST", "/adapter", "{nope")$status == 400)

# ---- status, new, open ----

s <- result("status")
check("status: running, no notebooks, seq", identical(s$ember, "running") && length(s$notebooks) == 0 && is.numeric(s$seq))
seq0 <- s$seq

made <- result("new", list(path = file.path(dir, "fresh.R")))
check("new: an id, its path, one empty cell", grepl("^[0-9a-f-]{36}$", made$notebook_id) &&
      identical(made$path, file.path(dir, "fresh.R")) && length(made$cells) == 1 && identical(made$cells[[1]]$code, ""))
check("new: the file exists", file.exists(file.path(dir, "fresh.R")))
named <- result("new", list(folder = dir))
check("new in a folder: notebook.R", identical(basename(named$path), "notebook.R"))
check("new over an existing file: file_exists", startsWith(call("new", list(path = made$path))$error, "ArgumentError: file_exists::"))

# An existing Ember notebook, written as Ember writes them.
existing <- file.path(dir, "existing.R")
writeLines(c(
  "### An Ember notebook ###", "# /// environment", '# ember_version = "0.0.0.9000"',
  '# r_version = "4.3.3"', '# snapshot = "2026-01-01"', '# on_cell_change = "lazy"', "# ///", "",
  "# %% id=11111111-1111-4111-8111-111111111111", "x <- 1:10", "",
  "# %% id=22222222-2222-4222-8222-222222222222", "y <- sum(x)", "",
  "# %% id=33333333-3333-4333-8333-333333333333", "y * 2", "",
  "# /// cell order", "# 11111111-1111-4111-8111-111111111111",
  "# 22222222-2222-4222-8222-222222222222", "# 33333333-3333-4333-8333-333333333333", "# ///"), existing)
opened <- result("open", list(path = existing, run = FALSE))
nid <- opened$notebook_id
check("open: in safe preview", identical(opened$process_status, "waiting_for_permission"))
again <- call("open", list(path = existing, run = FALSE))
check("open twice: notebook_already_open", startsWith(again$error, "ArgumentError: notebook_already_open::"))
opened_notes <- collect(seq0, function(ns) sum(methods_of(ns) == "notebook_opened") >= 3)
check("notebook_opened for each, with its path",
      sum(methods_of(opened_notes) == "notebook_opened") == 3 &&
      any(vapply(opened_notes, function(n) identical(n$params$path, existing), logical(1))))

# ---- snapshot ----

all <- result("snapshot")
check("snapshot of all: three notebooks and seq", length(all$notebooks) == 3 && is.numeric(all$seq))
snap <- result("snapshot", list(notebook_id = nid))
ids <- unlist(snap$cell_order)
check("snapshot of one: cell order", identical(ids, c("11111111-1111-4111-8111-111111111111",
      "22222222-2222-4222-8222-222222222222", "33333333-3333-4333-8333-333333333333")))
c1 <- snap$cells[[1]]
check("snapshot cell fields", all(c("cell_id", "code", "folded", "running", "queued", "errored", "last_run",
      "runtime", "output", "hidden", "markdown") %in% names(c1)) && identical(c1$code, "x <- 1:10"))
check("snapshot: safe preview, not allowed", isTRUE(snap$safe_preview) && identical(snap$execution_allowed, FALSE) && is.numeric(snap$seq))
check("snapshot of an unknown notebook: notebook_not_found KeyError",
      startsWith(call("snapshot", list(notebook_id = "00000000-0000-4000-8000-000000000000"))$error, "KeyError: key \"notebook_not_found::"))
check("an invalid notebook id: invalid_notebook_id",
      startsWith(call("graph", list(notebook_id = "nope"))$error, "ArgumentError: invalid_notebook_id::"))

# ---- run before allowing, allow_execution, run with wait ----

blocked <- result("run", list(notebook_id = nid, cells = list(ids[3]), wait = TRUE, timeout = 30))
check("run in safe preview: not accepted", identical(blocked$accepted, FALSE) && identical(blocked$process_status, "waiting_for_permission"))
allowed <- result("allow_execution", list(notebook_id = nid, run = FALSE, timeout = 30))
check("allow_execution: allowed, nothing ran", identical(allowed$already_allowed, FALSE) && identical(allowed$ran, FALSE))
check("allow_execution again: already allowed", isTRUE(result("allow_execution", list(notebook_id = nid, run = FALSE, timeout = 30))$already_allowed))

ran <- result("run", list(notebook_id = nid, cells = list(ids[3]), wait = TRUE, timeout = 60))
check("run with wait: completed, nothing timed out", isTRUE(ran$accepted) && identical(unlist(ran$completed), ids[3]) && length(ran$timed_out) == 0)
snap <- result("snapshot", list(notebook_id = nid))
c3 <- snap$cells[[3]]
check("the cell ran (and its ancestors): output [1] 110", grepl("110", c3$output) && c3$last_run > 0 && c3$runtime > 0 &&
      snap$cells[[2]]$last_run > 0)
check("last_run is when the run ended, runtime in ns", abs(c3$last_run - as.numeric(Sys.time())) < 120 && c3$runtime < 120e9)
check("process ready", identical(snap$process_status, "ready") && isTRUE(snap$execution_allowed))

# ---- graph ----

g <- result("graph", list(notebook_id = nid, edges = TRUE, packages = TRUE))
node <- function(g, id) Find(function(n) identical(n$cell_id, id), g$cells)
check("graph: definitions, references, edges", identical(unlist(node(g, ids[2])$definitions), "y") &&
      "x" %in% unlist(node(g, ids[2])$references) && identical(unlist(node(g, ids[2])$upstream), ids[1]) &&
      identical(unlist(node(g, ids[2])$downstream), ids[3]))
check("graph: order and errable", identical(unlist(g$order), ids) && length(g$errable) == 0)

# ---- apply: set code, insert, delete, move ----

before <- result("status")$seq
applied <- result("apply", list(notebook_id = nid, ops = list(
  list(op = "set_code", cell_id = ids[3], code = "y * 3", expected = "y * 2"),
  list(op = "insert", index = 3, code = "f <- function(a) a + 1\nf(y)", folded = FALSE),
  list(op = "insert", index = 0, code = "z <- 1", folded = TRUE))))
check("apply: two inserted cells and a seq", length(applied$inserted) == 2 && applied$seq > before)
new_id <- applied$inserted[[1]]
top_id <- applied$inserted[[2]]
snap <- result("snapshot", list(notebook_id = nid))
order <- unlist(snap$cell_order)
check("apply: inserted at 0-based indexes", identical(order, c(top_id, ids, new_id)))
check("apply: code set, fold kept", identical(snap$cells[[4]]$code, "y * 3") && isTRUE(snap$cells[[1]]$folded))
check("snapshot seq is at least apply's", snap$seq >= applied$seq)
stale <- call("apply", list(notebook_id = nid, ops = list(list(op = "set_code", cell_id = ids[3], code = "1", expected = "y * 2"))))
check("apply with a stale expected: stale_read", startsWith(stale$error, "ArgumentError: stale_read::"))
unknown <- call("apply", list(notebook_id = nid, ops = list(list(op = "delete", cell_id = "99999999-9999-4999-8999-999999999999"))))
check("apply on an unknown cell: cell_not_found KeyError", startsWith(unknown$error, "KeyError: key \"cell_not_found::"))
moved <- result("apply", list(notebook_id = nid, ops = list(list(op = "move", cell_id = top_id, index = 4))))
deleted <- result("apply", list(notebook_id = nid, ops = list(list(op = "delete", cell_id = top_id))))
snap <- result("snapshot", list(notebook_id = nid))
check("apply: move then delete", identical(unlist(snap$cell_order), c(ids, new_id)) && deleted$seq > moved$seq)
fx <- node(result("graph", list(notebook_id = nid)), new_id)
check("graph: a function is under functions", identical(unlist(fx$functions), "f"))

ns <- collect(before, function(ns) "topology_changed" %in% methods_of(ns) && "file_saved" %in% methods_of(ns))
changed <- unlist(lapply(Filter(function(n) n$method == "cell_state", ns), function(n) vapply(n$params$cells, function(c) c$cell_id, "")))
check("cell_state after the edit names the changed cells, with their code",
      all(c(ids[3], new_id) %in% changed) && !(ids[1] %in% changed))
check("cell_state from the edit is numbered below apply's seq",
      all(vapply(Filter(function(n) n$method == "cell_state" && length(n$params$cells) > 0 &&
                          any(vapply(n$params$cells, function(c) identical(c$cell_id, ids[3]), logical(1))), ns),
                 function(n) n$seq < applied$seq, logical(1))))
check("topology_changed and file_saved", all(c("topology_changed", "file_saved") %in% methods_of(ns)))

# ---- run without waiting: execution_done, run_finished ----

after_edit <- result("status")$seq
going <- result("run", list(notebook_id = nid, cells = list(new_id), wait = FALSE, timeout = 30))
check("run without wait: accepted, no completed", isTRUE(going$accepted) && is.null(going$completed))
ns <- collect(after_edit, function(ns) all(c("execution_done", "run_finished") %in% methods_of(ns)))
check("execution_done after the run", "execution_done" %in% methods_of(ns))
fin <- Find(function(n) n$method == "run_finished", ns)
check("run_finished names the cell", !is.null(fin) && identical(unlist(fin$params$cells), new_id) && identical(fin$params$notebook_id, nid))
check("seq increases", all(diff(vapply(ns, function(n) n$seq, numeric(1))) > 0))
snap <- result("snapshot", list(notebook_id = nid))
check("the new cell's output", grepl("56", cell_of(snap, new_id)$output))

# ---- errors, render_text, render_png, validate ----

err <- result("apply", list(notebook_id = nid, ops = list(list(op = "insert", index = 4, code = "stop(\"boom\")", folded = FALSE),
                                                          list(op = "insert", index = 5, code = "plot(1:3)", folded = FALSE))))
r <- result("run", list(notebook_id = nid, cells = err$inserted, wait = TRUE, timeout = 60))
snap <- result("snapshot", list(notebook_id = nid))
ec <- cell_of(snap, err$inserted[[1]])
check("an error: errored, structured, message as output", isTRUE(ec$errored) && identical(ec$error$kind, "runtime") &&
      identical(ec$error$msg, "boom") && identical(ec$output, "boom"))
pc <- cell_of(snap, err$inserted[[2]])
check("a plot: placeholder output", grepl("^\\[image/png output, [0-9]+ bytes; call view_cell_output", pc$output))
png <- result("render_png", list(notebook_id = nid, cell_id = err$inserted[[2]]))
check("render_png: base64 PNG", identical(png$mime, "image/png") && identical(as.integer(base64_dec(png$png)[2:4]), c(0x50L, 0x4eL, 0x47L)))
check("render_png of a text output: no png", is.null(result("render_png", list(notebook_id = nid, cell_id = ids[3]))$png))
check("render_text: null (the snapshot's output is already text)", is.null(result("render_text", list(notebook_id = nid, cell_id = ids[3]))$text))
v <- result("validate", list(notebook_id = nid, cell_id = ids[3], code = "x <- ("))
check("validate: a syntax error", length(v$errors) == 1 && identical(v$errors[[1]]$type, "syntax_error"))
check("validate: fine code", length(result("validate", list(notebook_id = nid, cell_id = ids[3], code = "x + 1"))$errors) == 0)
check("validate: an unknown cell", startsWith(call("validate", list(notebook_id = nid, cell_id = "x", code = "1"))$error, "KeyError: key \"cell_not_found::"))
check("unknown method", startsWith(call("frobnicate", list(notebook_id = nid))$error, "ArgumentError: unknown_method::"))
warned <- result("apply", list(notebook_id = nid, ops = list(list(op = "insert", index = 6, code = "warning(\"careful\")\n7", folded = FALSE))))
r <- result("run", list(notebook_id = nid, cells = warned$inserted, wait = TRUE, timeout = 60))
wc <- cell_of(result("snapshot", list(notebook_id = nid)), warned$inserted[[1]])
check("a warning reads as one, before the value", grepl("^Warning: careful\n\\[1\\] 7$", wc$output))
r <- result("run", list(notebook_id = nid, cells = list(ids[3]), wait = TRUE, timeout = 60))
r <- result("run", list(notebook_id = nid, cells = list(ids[2]), wait = TRUE, timeout = 60))
snap <- result("snapshot", list(notebook_id = nid))
# existing.R is lazy, so the stale result stays stale (in autorun Ember would rerun it on its own).
check("a result made before its ancestor last ran: stale", isTRUE(cell_of(snap, ids[3])$stale) && identical(cell_of(snap, ids[2])$stale, FALSE))

# ---- a long run: timeout, interrupt ----

slow <- result("apply", list(notebook_id = nid, ops = list(list(op = "insert", index = 6, code = "Sys.sleep(30)", folded = FALSE))))
t0 <- Sys.time()
r <- result("run", list(notebook_id = nid, cells = slow$inserted, wait = TRUE, timeout = 1))
check("run with a short timeout: timed out, returns", length(r$completed) == 0 && identical(unlist(r$timed_out), slow$inserted[[1]]) &&
      as.numeric(Sys.time() - t0, units = "secs") < 10)
check("interrupt: something was running", isTRUE(result("interrupt", list(notebook_id = nid))$interrupted))
ns <- collect(result("status")$seq - 5, function(ns) "run_finished" %in% methods_of(ns))
check("run_finished once the interrupted run ends", "run_finished" %in% methods_of(ns))

# ---- the notebook's R ending by itself ----

killer <- result("apply", list(notebook_id = nid, ops = list(list(op = "insert", index = 0, code = "tools::pskill(Sys.getpid())", folded = FALSE))))
before <- result("status")$seq
r <- result("run", list(notebook_id = nid, cells = killer$inserted, wait = TRUE, timeout = 60))
check("run that kills its R: exited names the cell", identical(unlist(r$exited), killer$inserted[[1]]) && identical(r$process_status, "no_process"))
ns <- collect(before, function(ns) "process_exited" %in% methods_of(ns))
pe <- Find(function(n) n$method == "process_exited", ns)
check("process_exited with the running cell", !is.null(pe) && identical(unlist(pe$params$running), killer$inserted[[1]]))
snap <- result("snapshot", list(notebook_id = nid))
check("snapshot: exited, no_process", identical(unlist(snap$exited), killer$inserted[[1]]) && identical(snap$process_status, "no_process"))
invisible(result("apply", list(notebook_id = nid, ops = list(list(op = "delete", cell_id = killer$inserted[[1]])))))
r <- result("run", list(notebook_id = nid, cells = list(ids[3]), wait = TRUE, timeout = 60))
check("a run after that starts a new R", identical(unlist(r$completed), ids[3]) && is.null(result("snapshot", list(notebook_id = nid))$exited))

# ---- move, restart, shutdown ----

target <- file.path(dir, "moved.R")
check("move", identical(result("move", list(notebook_id = nid, path = target))$path, target) && file.exists(target) && !file.exists(existing))
check("restart", isTRUE(result("restart", list(notebook_id = nid, timeout = 30))$restarted))
snap <- result("snapshot", list(notebook_id = nid))
check("after a restart every code cell is not run", all(vapply(snap$cells, function(c) isTRUE(c$not_run) || isTRUE(c$markdown), logical(1))))
s <- result("status")
before <- s$seq
check("status lists the notebooks", length(s$notebooks) == 3)
down <- result("shutdown", list(notebook_id = nid))
check("shutdown: was not in safe preview", identical(down$safe_preview, FALSE))
ns <- collect(before, function(ns) "notebook_shut_down" %in% methods_of(ns))
check("notebook_shut_down", any(vapply(ns, function(n) n$method == "notebook_shut_down" && identical(n$params$notebook_id, nid), logical(1))))
check("gone after shutdown", startsWith(call("snapshot", list(notebook_id = nid))$error, "KeyError: key \"notebook_not_found::"))
check("status: two left", length(result("status")$notebooks) == 2)

# ---- the long poll waits ----

now <- result("status")$seq
t0 <- Sys.time()
h <- new_handle(timeout = 40)
handle_setheaders(h, Authorization = paste("Bearer", token))
pool <- new_pool()
got <- NULL
curl_fetch_multi(sprintf("%s/notifications?after=%d", base, now), handle = h, pool = pool,
                 done = function(r) got <<- fromJSON(rawToChar(r$content), simplifyVector = FALSE))
invisible(multi_run(timeout = 0.5, pool = pool))
check("the long poll is held while nothing happens", is.null(got))
invisible(result("apply", list(notebook_id = named$notebook_id, ops = list(list(op = "set_code", cell_id = named$cells[[1]]$cell_id, code = "1 + 1")))))
invisible(multi_run(timeout = 10, pool = pool))
check("and answers when something does", length(got) > 0 && all(vapply(got, function(n) n$seq > now, logical(1))))

cat(sprintf("\n%d passed, %d failed\n", passed, failed))
if (failed > 0) {
  cat("\n--- adapter log ---\n")
  cat(readLines(file.path(dir, "adapter.log")), sep = "\n")
  quit(status = 1)
}
