# The Ember adapter: the R side of the engine interface (docs/runtime-core.md),
# as Adapter.jl is Pluto's. The core starts it as
#
#   Rscript --vanilla runtime/r/adapter.R
#
# with ENDEAVOR_TOKEN (the bearer token every request must carry),
# ENDEAVOR_R_STATE (where to write {pid, bridge_port, ember_port,
# ember_secret} once it listens) and R_LIBS (where ember is). It runs one
# Ember server holding every notebook it opens, and next to it a bridge on
# 127.0.0.1 that answers:
#
#   POST /adapter               {"method", "params"} -> {"result"} or {"error"}
#   GET  /notifications?after=n every notification numbered above n, as a JSON
#                               array; held up to 25 s while there are none
#
# The methods and their replies are Adapter.jl's (`adapter_call`): status,
# open, new, and for a notebook_id snapshot, graph, shutdown, apply, run,
# interrupt, restart, allow_execution, move, render_text, render_png and
# validate. Errors read as Julia shows them, which the core parses:
# `ArgumentError: kind::message`, or `KeyError: key "kind::message" not found`
# for a notebook or cell that isn't there; any other R error is
# `r_error::message`.
#
# Notifications are Adapter.jl's too: notebook_opened, notebook_shut_down,
# file_saved, execution_done, cell_state, topology_changed, run_finished and
# process_exited, each with `seq`, a counter that `apply` also advances; its
# reply, `snapshot`'s and `status`'s carry it. Ember's own per-notebook `seq`
# is not used. Everything shares Ember's one R thread, so notifications stay
# small: `cell_state` names only the cells whose code changed.

suppressPackageStartupMessages({
  library(ember)
  library(httpuv)
  library(later)
  library(promises)
  library(jsonlite)
})

`%||%` <- function(a, b) if (is.null(a)) b else a

TOKEN <- Sys.getenv("ENDEAVOR_TOKEN")
STATE_FILE <- Sys.getenv("ENDEAVOR_R_STATE")
# Ember's page secret, made by the core, which already knows it: it isn't written back. Not left in the
# environment, which the notebooks' R processes would inherit.
EMBER_SECRET <- Sys.getenv("ENDEAVOR_EMBER_SECRET")
Sys.unsetenv("ENDEAVOR_EMBER_SECRET")
NOTES_KEPT <- 1000L
POLL_HOLD <- 25
TEXT_MAX <- 16000L

A <- new.env()
A$server <- NULL         # the ember_server
A$bridge <- NULL         # the httpuv handle
A$bridge_port <- NULL
A$records <- new.env()   # notebook id -> record (environment: id, nb, codes, status)
A$seq <- 0
A$notes <- list()        # the last NOTES_KEPT notifications
A$waiters <- list()      # held /notifications requests: key -> function()
A$keys <- 0L

# ---- Errors ------------------------------------------------------------------

adapter_error <- function(text) {
  stop(structure(class = c("adapter_error", "error", "condition"),
                 list(message = text, call = NULL)))
}

argument_error <- function(kind, message) adapter_error(sprintf("ArgumentError: %s::%s", kind, message))

# Julia's KeyError, which wraps `kind::message` in the quoted key (mcp.rs, unwrap_key_error).
key_error <- function(kind, message) {
  key <- paste0(kind, "::", message)
  key <- gsub("\\", "\\\\", key, fixed = TRUE)
  key <- gsub("\"", "\\\"", key, fixed = TRUE)
  key <- gsub("$", "\\$", key, fixed = TRUE)
  adapter_error(sprintf("KeyError: key \"%s\" not found", key))
}

# ---- Notifications -----------------------------------------------------------

next_seq <- function() (A$seq <- A$seq + 1)

notify <- function(method, params) {
  A$notes[[length(A$notes) + 1L]] <- list(method = method, params = params, seq = next_seq())
  if (length(A$notes) > NOTES_KEPT) A$notes <- utils::tail(A$notes, NOTES_KEPT)
  for (w in A$waiters) w()
  invisible(NULL)
}

notes_after <- function(after) Filter(function(n) n$seq > after, A$notes)

# Every notification after `after`, at once, or a promise of those that come
# within POLL_HOLD seconds (an empty list if none does).
notifications <- function(after) {
  found <- notes_after(after)
  if (length(found) > 0) return(found)
  promise(function(resolve, reject) {
    key <- as.character(A$keys <- A$keys + 1L)
    done <- FALSE
    finish <- function(items) {
      if (done) return(invisible(NULL))
      done <<- TRUE
      A$waiters[[key]] <- NULL
      resolve(items)
    }
    A$waiters[[key]] <- function() {
      found <- notes_after(after)
      if (length(found) > 0) finish(found)
    }
    later::later(function() finish(list()), POLL_HOLD)
  })
}

# The cells among `ids` whose code isn't what the record last saw.
code_changes <- function(rec, state, ids) {
  changed <- list()
  for (id in ids) {
    cell <- state$cells[[id]]
    if (is.null(cell)) {
      rec$codes[[id]] <- NULL
    } else if (!identical(rec$codes[[id]], cell$code)) {
      rec$codes[[id]] <- cell$code
      changed[[length(changed) + 1L]] <- list(cell_id = id, code = cell$code)
    }
  }
  changed
}

# The cells that were running when the notebook's R ended by itself; NULL
# while it has one.
exited_cells <- function(state) {
  if (!identical(state$worker$status, "stopped") || is.null(state$worker$exit)) return(NULL)
  ids <- names(Filter(function(r) identical(r$error$kind, "worker_exited"), state$results))
  ids %||% character()
}

# A notebook's R ending by itself (or failing to start): Ember stops it and
# clears its results; the core hears which cell was running.
check_exit <- function(rec, state) {
  was <- rec$status
  rec$status <- state$worker$status
  if (identical(rec$status, "stopped") && !identical(was, "stopped") && !isTRUE(state$closed)) {
    notify("process_exited", list(notebook_id = rec$id, running = I(exited_cells(state) %||% character())))
  }
}

on_note <- function(rec, note) {
  id <- rec$id
  if (identical(note$kind, "notebook_shut_down")) return(forget(rec))
  state <- notebook_state(rec$nb)
  switch(note$kind,
    cell_state = notify("cell_state", list(notebook_id = id, cells = code_changes(rec, state, note$cells))),
    topology_changed = notify("topology_changed", list(notebook_id = id)),
    file_saved = notify("file_saved", list(notebook_id = id)),
    execution_done = notify("execution_done", list(notebook_id = id)),
    NULL)  # packages_changed, worker_usage: nothing the core follows
  check_exit(rec, state)
}

# ---- Notebooks ---------------------------------------------------------------

# Show `nb` in Ember's server, follow its events, and say it opened.
adopt <- function(nb) {
  state <- notebook_state(nb)
  rec <- new.env()
  rec$id <- state$id
  rec$nb <- nb
  rec$codes <- lapply(state$cells, function(c) c$code)
  rec$status <- state$worker$status
  host_notebook(A$server, nb)
  rec$unsubscribe <- on_notebook_event(nb, function(note) on_note(rec, note))
  assign(rec$id, rec, envir = A$records)
  notify("notebook_opened", list(notebook_id = rec$id, path = state$path))
  rec
}

forget <- function(rec) {
  if (!exists(rec$id, envir = A$records, inherits = FALSE)) return(invisible(NULL))
  rm(list = rec$id, envir = A$records)
  notify("notebook_shut_down", list(notebook_id = rec$id))
}

records <- function() mget(ls(A$records), envir = A$records)

get_record <- function(id) {
  id <- as.character(id %||% "")
  if (length(id) != 1 || !grepl("^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$", id)) {
    argument_error("invalid_notebook_id", sprintf("Invalid notebook ID: '%s'", id))
  }
  rec <- mget(id, envir = A$records, ifnotfound = list(NULL))[[1]]
  if (is.null(rec)) key_error("notebook_not_found", sprintf("No notebook with id '%s' in the current session", id))
  rec
}

check_cell <- function(rec, id) {
  id <- as.character(id %||% "")
  if (!(id %in% names(notebook_state(rec$nb)$cells))) {
    key_error("cell_not_found", sprintf("No cell with id '%s' in notebook", id))
  }
  id
}

# Pluto's process states, as near as Ember's come: safe preview is
# "waiting_for_permission"; a busy R is "ready", as a Pluto process running a
# cell is; an R that ended (or never started) is "no_process", though Ember
# starts a new one on the next run.
process_status <- function(state) {
  if (!isTRUE(state$allowed)) return("waiting_for_permission")
  switch(state$worker$status,
    starting = "starting",
    stopped = "no_process",
    "ready")
}

will_run <- function(state) isTRUE(state$allowed) && !isTRUE(state$read_only)

# ---- Snapshot ----------------------------------------------------------------

cap_text <- function(text) {
  if (nchar(text, "bytes") <= TEXT_MAX) return(text)
  paste0(substr(text, 1, TEXT_MAX), sprintf("\n... (cut at %d bytes of %d)", TEXT_MAX, nchar(text, "bytes")))
}

# Ember's error as the tools show it, in Pluto's shape ({kind, msg}); "error"
# (R's own) is Pluto's "runtime".
structure_error <- function(e) {
  d <- list(kind = if (identical(e$kind, "error")) "runtime" else e$kind, msg = e$message %||% "")
  if (length(e$fixes) > 0) d$fixes <- I(e$fixes)
  if (length(e$names) > 0) d$names <- I(e$names)
  if (length(e$cells) > 0) d$cells <- I(e$cells)
  if (!is.null(e$line)) d$line <- e$line
  if (length(e$traceback) > 0) d$traceback <- I(e$traceback)
  d
}

# The output as text: what the cell printed, then its value's print form.
# Plots aren't text; like Pluto's, their text points at view_cell_output.
output_text <- function(view) {
  # A warning reads as R prints one, so it isn't taken for the cell's own output.
  parts <- vapply(view$console, function(item) {
    text <- item$text %||% ""
    if (identical(item$kind, "warning") && !grepl("^Warning( message)?:", text)) paste0("Warning: ", text) else text
  }, character(1))
  out <- view$output
  if (!is.null(out)) {
    parts <- c(parts, if (out$mime %in% c("image/png", "image/svg+xml")) {
      size <- if (is.raw(out$data)) length(out$data) else nchar(paste(out$data, collapse = ""), "bytes")
      sprintf("[%s output, %d bytes; call view_cell_output to see it]", out$mime, size)
    } else {
      out$text %||% ""
    })
  }
  parts <- sub("\n+$", "", parts[nzchar(parts)])
  cap_text(paste(parts, collapse = "\n"))
}

snapshot_cell <- function(v) {
  errored <- length(v$errors) > 0 || isTRUE(v$status %in% c("error", "interrupted"))
  runtime <- v$runtime %||% 0
  d <- list(
    cell_id = v$id, code = v$code, folded = isTRUE(v$folded),
    running = isTRUE(v$running), queued = isTRUE(v$queued), errored = errored,
    # Pluto stamps a run when it ends; Ember records when it started.
    last_run = if (is.null(v$last_run)) 0 else as.numeric(v$last_run) + runtime,
    runtime = runtime * 1e9,
    output = "",
    # Ember has no boilerplate cells to hide.
    hidden = FALSE,
    markdown = !identical(v$kind, "code"),
    # Ember knows these itself: a result made before an ancestor last ran, and a
    # cell that hasn't run since the notebook started (a restart leaves all so).
    stale = isTRUE(v$stale),
    not_run = identical(v$status, "not_run") && identical(v$kind, "code"))
  if (errored) {
    e <- if (length(v$errors) > 0) v$errors[[1]] else list(kind = "interrupted", message = "The run was interrupted.")
    d$error <- structure_error(e)
    d$output <- d$error$msg
  } else {
    d$output <- output_text(v)
  }
  d
}

snapshot <- function(rec) {
  snap <- notebook_snapshot(rec$nb)
  state <- notebook_state(rec$nb)
  exited <- exited_cells(state)
  list(
    notebook_id = rec$id,
    path = snap$path,
    cell_order = I(snap$order),
    process_status = process_status(state),
    execution_allowed = will_run(state),
    safe_preview = !isTRUE(state$allowed),
    exited = if (is.null(exited)) NULL else I(exited),
    cells = unname(lapply(snap$cells, snapshot_cell)))
}

# ---- Graph -------------------------------------------------------------------

# Ember's graph always reflects the current code (staged edits included), so
# `fresh` and `refresh` change nothing. `functions` are the names a cell
# assigns a function to; `definitions` the rest, learned ones included.
graph <- function(rec, edges = FALSE, packages = FALSE) {
  g <- dependency_graph(rec$nb)
  blocked <- blocked_cells(g)
  cell <- function(id) {
    c <- g$cells[[id]]
    a <- g$analyses[[id]]
    defs <- a$definitions
    fns <- if (is.data.frame(defs) && nrow(defs) > 0) unique(defs$name[defs$kind == "function"]) else character()
    d <- list(cell_id = id,
              definitions = I(sort(setdiff(c$definitions, fns))),
              functions = I(sort(intersect(c$definitions, fns))),
              references = I(sort(c$references %||% character())))
    if (isTRUE(edges)) {
      d$upstream <- I(g$upstream[[id]] %||% character())
      d$downstream <- I(g$downstream[[id]] %||% character())
    }
    if (isTRUE(packages)) {
      pk <- if (is.data.frame(a$packages)) a$packages$name else character()
      d$packages <- I(sort(unique(c(pk, c$packages %||% character()))))
    }
    d
  }
  list(notebook_id = rec$id,
       cells = unname(lapply(g$order, cell)),
       order = I(setdiff(g$order, blocked)),
       errable = I(blocked))
}

# ---- Edits -------------------------------------------------------------------

uuid4 <- function() {
  hex <- function(n) paste(sample(c(0:9, letters[1:6]), n, replace = TRUE), collapse = "")
  paste(hex(8), hex(4), paste0("4", hex(3)), paste0(sample(c("8", "9", "a", "b"), 1), hex(3)), hex(12), sep = "-")
}

# Pluto's ops, whose indexes count from 0, as Ember's, which count from 1. A
# set_code with `expected` is refused, before any op applies, if the cell's
# code isn't that. Ember saves the file and updates its page itself. Ember
# trims trailing blank lines from code (and turns CRLF into LF).
apply_ops <- function(rec, ops) {
  cells <- notebook_state(rec$nb)$cells
  for (op in ops) {
    if (identical(op$op, "set_code") && !is.null(op$expected)) {
      id <- check_cell(rec, op$cell_id)
      if (!identical(cells[[id]]$code, op$expected)) {
        argument_error("stale_read", sprintf("Cell %s changed since last read; call read_cell again", id))
      }
    }
  }
  ember_ops <- list()
  inserted <- character()
  folds <- list()
  for (op in ops) {
    kind <- op$op %||% ""
    add <- function(...) ember_ops <<- c(ember_ops, list(...))
    if (kind == "set_code") {
      add(set_code(check_cell(rec, op$cell_id), as.character(op$code %||% "")))
    } else if (kind == "insert") {
      id <- uuid4()
      add(insert_cell(as.integer(op$index) + 1L, as.character(op$code %||% ""), id = id))
      if (!is.null(op$folded)) folds[[id]] <- isTRUE(op$folded)
      inserted <- c(inserted, id)
    } else if (kind == "delete") {
      add(delete_cell(check_cell(rec, op$cell_id)))
    } else if (kind == "move") {
      add(move_cell(check_cell(rec, op$cell_id), as.integer(op$index) + 1L))
    } else if (kind == "fold") {
      add(fold_cell(check_cell(rec, op$cell_id), isTRUE(op$folded)))
    } else {
      argument_error("unknown_op", sprintf("Unknown op: '%s'", kind))
    }
  }
  if (length(ember_ops) > 0) {
    tryCatch(do.call(edit_notebook, c(list(rec$nb), ember_ops)),
             ember_refused = function(e) argument_error("refused", conditionMessage(e)))
  }
  # Ember folds a new text cell and unfolds a new code cell, and can't name a
  # cell inserted in the same batch; a fold asked for otherwise is a second edit.
  cells <- notebook_state(rec$nb)$cells
  refold <- Filter(Negate(is.null), lapply(names(folds), function(id) {
    if (!is.null(cells[[id]]) && !identical(isTRUE(cells[[id]]$folded), folds[[id]])) fold_cell(id, folds[[id]])
  }))
  if (length(refold) > 0) do.call(edit_notebook, c(list(rec$nb), refold))
  list(inserted = I(inserted), seq = next_seq())
}

# ---- Runs --------------------------------------------------------------------

unfinished <- function(state, ids) {
  ids[ids %in% c(state$pending, state$worker$running$cell)]
}

registered <- function(rec) identical(mget(rec$id, envir = A$records, ifnotfound = list(NULL))[[1]], rec)

# A run is over when none of its cells is left, its R ended, or the notebook
# shut down.
run_over <- function(rec, ids) {
  if (!registered(rec)) return(TRUE)
  state <- notebook_state(rec$nb)
  identical(state$worker$status, "stopped") || length(unfinished(state, ids)) == 0
}

# Once a run nobody waits for ends, `run_finished` says which of its cells finished.
watch_run <- function(rec, cells, ids) {
  check <- function() {
    if (!run_over(rec, ids)) return(later::later(check, 0.2))
    done <- if (registered(rec)) setdiff(cells, unfinished(notebook_state(rec$nb), cells)) else cells
    notify("run_finished", list(notebook_id = rec$id, cells = I(done)))
  }
  later::later(check, 0.2)
}

# Run cells (Ember runs their unrun ancestors first). Not accepted in safe
# preview or for a read-only notebook: Ember would allow execution on a run,
# Pluto (and the user's approval) wants allow_execution for that. With
# `wait`, a promise of the reply once the run is over or `timeout` passes;
# none is Pluto's cleanup after a delete, which Ember doesn't need.
run <- function(rec, cells, wait, timeout) {
  cells <- vapply(cells, function(c) check_cell(rec, c), character(1), USE.NAMES = FALSE)
  state <- notebook_state(rec$nb)
  result <- list(accepted = will_run(state), process_status = process_status(state))
  if (!result$accepted || length(cells) == 0) {
    if (result$accepted && isTRUE(wait)) result[c("completed", "timed_out")] <- list(I(character()), I(character()))
    return(result)
  }
  r <- run_cells(rec$nb, cells, wait = FALSE)
  if (!isTRUE(r$accepted)) {
    result$accepted <- FALSE
    return(result)
  }
  ids <- union(cells, r$queued)
  if (!isTRUE(wait)) {
    watch_run(rec, cells, ids)
    return(result)
  }
  deadline <- Sys.time() + as.numeric(timeout %||% 60)
  promise(function(resolve, reject) {
    check <- function() {
      over <- run_over(rec, ids)
      if (!over && Sys.time() < deadline) return(later::later(check, 0.05))
      if (!registered(rec)) {
        result$completed <- I(cells)
        result$timed_out <- I(character())
        return(resolve(result))
      }
      state <- notebook_state(rec$nb)
      result$process_status <- process_status(state)
      exited <- exited_cells(state)
      if (!is.null(exited)) result$exited <- I(exited)
      result$completed <- I(setdiff(cells, unfinished(state, cells)))
      result$timed_out <- I(if (over) unfinished(state, cells) else unfinished(state, names(state$cells)))
      if (!over) watch_run(rec, cells, ids)
      resolve(result)
    }
    check()
  })
}

# Pluto's "Run notebook code" for a notebook in safe preview: allow
# execution, then run every cell (`run`) or leave it to start on the next run.
allow <- function(rec, run_all) {
  state <- notebook_state(rec$nb)
  if (isTRUE(state$read_only)) {
    argument_error("read_only", "This notebook was saved by a newer Ember and opens read-only; it can't run here")
  }
  if (isTRUE(state$allowed)) {
    return(list(already_allowed = TRUE, ran = FALSE, process_status = process_status(state)))
  }
  allow_execution(rec$nb)
  if (isTRUE(run_all)) run_everything(rec)
  list(already_allowed = FALSE, ran = isTRUE(run_all), process_status = process_status(notebook_state(rec$nb)))
}

run_everything <- function(rec) {
  r <- run_cells(rec$nb, NULL, wait = FALSE)
  if (isTRUE(r$accepted) && length(r$queued) > 0) watch_run(rec, r$queued, r$queued)
}

# Ember's restart: a new R, every cell left not run (Pluto's runs them all).
# Refused in safe preview, where allow_execution starts the notebook.
restart <- function(rec) {
  if (!isTRUE(notebook_state(rec$nb)$allowed)) {
    argument_error("execution_blocked", "The notebook is in safe preview; Run notebook starts it")
  }
  tryCatch(restart_notebook(rec$nb), ember_refused = function(e) argument_error("refused", conditionMessage(e)))
  list(restarted = TRUE)
}

interrupt <- function(rec) {
  state <- notebook_state(rec$nb)
  busy <- !is.null(state$worker$running) || length(state$pending) > 0
  interrupt_notebook(rec$nb)
  list(interrupted = busy)
}

# ---- Opening, moving, shutting down ------------------------------------------

open_path <- function(path, run_all) {
  path <- normalizePath(path, mustWork = FALSE)
  for (rec in records()) {
    if (identical(normalizePath(notebook_state(rec$nb)$path, mustWork = FALSE), path)) {
      argument_error("notebook_already_open", sprintf("'%s' is already open as notebook_id %s; use that id", path, rec$id))
    }
  }
  rec <- adopt(open_notebook(path))
  if (isTRUE(run_all)) {
    allow_execution(rec$nb)
    run_everything(rec)
  }
  state <- notebook_state(rec$nb)
  list(notebook_id = rec$id, path = state$path, process_status = process_status(state))
}

# A new notebook at `path`, else named "notebook.R" (then "notebook 2.R", ...)
# in `folder`, else in the working directory. Like Pluto's, it opens allowed
# to run, and its cells come back so the caller can edit them.
new_at <- function(path, folder) {
  if (is.null(path)) {
    folder <- folder %||% getwd()
    path <- file.path(folder, "notebook.R")
    n <- 1L
    while (file.exists(path)) path <- file.path(folder, sprintf("notebook %d.R", n <- n + 1L))
  }
  if (file.exists(path)) argument_error("file_exists", sprintf("'%s' already exists; use open_notebook to load it", path))
  rec <- adopt(new_notebook(normalizePath(path, mustWork = FALSE)))
  allow_execution(rec$nb)
  run_everything(rec)
  state <- notebook_state(rec$nb)
  list(notebook_id = rec$id, path = state$path, process_status = process_status(state),
       cells = unname(lapply(names(state$cells), function(id) list(cell_id = id, code = state$cells[[id]]$code))))
}

move <- function(rec, path) {
  moved <- tryCatch(move_notebook(rec$nb, path),
                    ember_refused = function(e) argument_error("invalid_path", conditionMessage(e)))
  list(path = moved)
}

shutdown <- function(rec) {
  safe_preview <- isTRUE(close_notebook(rec$nb))
  forget(rec)
  list(safe_preview = safe_preview)
}

# ---- Outputs -----------------------------------------------------------------

# The snapshot's `output` already holds every output's text form, so there is
# nothing more to render.
render_text <- function(rec, cell) list(text = NULL)

render_image <- function(rec, cell) {
  r <- render_png(rec$nb, cell)
  list(png = if (is.raw(r$png)) jsonlite::base64_enc(r$png) else NULL, mime = r$mime %||% "")
}

validate <- function(rec, cell, code) {
  errors <- list()
  parsed <- read_cell(code)$parse_error
  if (!is.null(parsed)) {
    errors[[length(errors) + 1L]] <- list(type = "syntax_error",
      message = sprintf("%s (line %s, column %s)", parsed$message, parsed$line %||% "?", parsed$column %||% "?"))
  }
  if (any(grepl("^# %%|^# ///", strsplit(code, "\n", fixed = TRUE)[[1]]))) {
    errors[[length(errors) + 1L]] <- list(type = "marker_line",
      message = "A line can't start with '# %%' or '# ///': Ember uses those to mark cells in the file")
  }
  list(errors = errors)
}

# ---- The calls ---------------------------------------------------------------

status <- function() {
  list(ember = "running",
       ember_port = A$server$port,
       ember_url = sprintf("http://127.0.0.1:%d", A$server$port),
       bridge_port = A$bridge_port,
       notebooks = unname(lapply(records(), function(rec) {
         state <- notebook_state(rec$nb)
         list(notebook_id = rec$id, path = state$path, cell_count = length(state$cells))
       })),
       seq = A$seq)
}

adapter_call <- function(method, params) {
  if (method == "status") return(status())
  if (method == "open") return(open_path(params$path, isTRUE(params$run)))
  if (method == "new") return(new_at(params$path, params$folder))
  if (method == "snapshot" && is.null(params$notebook_id)) {
    seq <- A$seq
    return(list(notebooks = unname(lapply(records(), snapshot)), seq = seq))
  }
  rec <- get_record(params$notebook_id)
  switch(method,
    snapshot = { seq <- A$seq; c(snapshot(rec), list(seq = seq)) },
    graph = graph(rec, edges = isTRUE(params$edges), packages = isTRUE(params$packages)),
    shutdown = shutdown(rec),
    apply = apply_ops(rec, params$ops),
    run = run(rec, unlist(params$cells) %||% character(), isTRUE(params$wait), params$timeout),
    interrupt = interrupt(rec),
    restart = restart(rec),
    allow_execution = allow(rec, isTRUE(params$run)),
    move = move(rec, params$path),
    render_text = render_text(rec, check_cell(rec, params$cell_id)),
    render_png = render_image(rec, check_cell(rec, params$cell_id)),
    validate = { check_cell(rec, params$cell_id); validate(rec, params$cell_id, as.character(params$code %||% "")) },
    argument_error("unknown_method", sprintf("Unknown adapter method: '%s'", method)))
}

error_text <- function(e) {
  if (inherits(e, "adapter_error")) conditionMessage(e) else paste0("r_error::", conditionMessage(e))
}

# The reply to one call: a value, or a promise of one; never an R error.
reply_to <- function(msg) {
  out <- tryCatch(adapter_call(as.character(msg$method %||% ""), msg$params %||% list()),
                  error = function(e) structure(list(error = error_text(e)), class = "adapter_reply"))
  if (inherits(out, "adapter_reply")) return(unclass(out))
  if (is.promising(out)) {
    return(promises::then(out, function(r) list(result = r), onRejected = function(e) list(error = error_text(e))))
  }
  list(result = out)
}

# ---- The bridge --------------------------------------------------------------

to_json <- function(x) {
  enc2utf8(as.character(jsonlite::toJSON(x, auto_unbox = TRUE, null = "null", na = "null", digits = NA, force = TRUE)))
}

respond <- function(status, body) {
  list(status = status, headers = list("Content-Type" = "application/json"), body = to_json(body))
}

# `Authorization: Bearer <token>`, compared without stopping at the first difference.
authorized <- function(req) {
  given <- charToRaw(req$HTTP_AUTHORIZATION %||% "")
  expected <- charToRaw(paste0("Bearer ", TOKEN))
  if (length(given) != length(expected)) return(FALSE)
  all(xor(given, expected) == as.raw(0))
}

handle <- function(req) {
  if (!nzchar(TOKEN) || !authorized(req)) return(respond(401L, list(error = "unauthorized")))
  method <- req$REQUEST_METHOD
  path <- req$PATH_INFO
  if (method == "POST" && path == "/adapter") {
    body <- rawToChar(req$rook.input$read())
    Encoding(body) <- "UTF-8"
    msg <- tryCatch(jsonlite::fromJSON(body, simplifyVector = FALSE), error = function(e) NULL)
    if (!is.list(msg)) return(respond(400L, list(error = "Invalid JSON")))
    out <- reply_to(msg)
    if (is.promising(out)) return(promises::then(out, function(r) respond(200L, r)))
    return(respond(200L, out))
  }
  if (method == "GET" && path == "/notifications") {
    after <- suppressWarnings(as.numeric(sub(".*(^|[?&])after=([0-9]+).*", "\\2", req$QUERY_STRING %||% "")))
    if (length(after) != 1 || is.na(after)) after <- 0
    out <- notifications(after)
    if (is.promising(out)) return(promises::then(out, function(r) respond(200L, unname(r))))
    return(respond(200L, unname(out)))
  }
  if (method == "GET" && path == "/health") {
    return(list(status = 200L, headers = list("Content-Type" = "text/plain"), body = "ok"))
  }
  list(status = 404L, headers = list("Content-Type" = "text/plain"), body = "")
}

start_bridge <- function() {
  for (attempt in 1:20) {
    port <- httpuv::randomPort(host = "127.0.0.1")
    bridge <- tryCatch(httpuv::startServer("127.0.0.1", port, list(call = handle)), error = function(e) NULL)
    if (!is.null(bridge)) {
      A$bridge <- bridge
      A$bridge_port <- port
      return(port)
    }
  }
  stop("couldn't start the bridge on 127.0.0.1")
}

# Written whole or not at all: the caller polls for the file.
write_state <- function() {
  if (!nzchar(STATE_FILE)) return(invisible(NULL))
  tmp <- paste0(STATE_FILE, ".tmp", Sys.getpid())
  writeLines(to_json(list(pid = Sys.getpid(), bridge_port = A$bridge_port,
                          ember_port = A$server$port)), tmp)
  if (!file.rename(tmp, STATE_FILE)) stop("couldn't write ", STATE_FILE)
  invisible(NULL)
}

main <- function() {
  if (!nzchar(TOKEN)) stop("ENDEAVOR_TOKEN is not set")
  if (!nzchar(EMBER_SECRET)) stop("ENDEAVOR_EMBER_SECRET is not set")
  serve(port = 0L, secret = EMBER_SECRET, on_ready = function(server) {
    A$server <- server
    start_bridge()
    write_state()
  })
}

if (!interactive()) main()
