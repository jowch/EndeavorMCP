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
# interrupt, answer, restart, allow_execution, move, render_text, render_png
# and validate. Errors read as Julia shows them, which the core parses:
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

# The engines' key the core gave this bridge (not the agent's MCP token), read once and taken out of
# the environment, so code a notebook runs can't pick it up (as Julia's boot.jl does for Pluto's processes).
TOKEN <- Sys.getenv("ENDEAVOR_TOKEN")
STATE_FILE <- Sys.getenv("ENDEAVOR_R_STATE")
Sys.unsetenv(c("ENDEAVOR_TOKEN", "ENDEAVOR_R_STATE"))
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
A$runs <- 0L             # runs named so far (`run_id`)

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
# A package the code doesn't name: Ember's fix is to add it to the header's
# [extra_packages], which no tool does and an agent mustn't do by editing the
# file. Naming it in the cell does the same through the tools, so that is the
# fix the agent hears; Ember's page keeps its own wording. Ember gives that fix
# for a missing package, and for a run error whose message names a package the
# code can't show (Ember #184, such as tidytext asking for textdata).
EXTRA_FIX <- "^Add ([A-Za-z][A-Za-z0-9.]*) to \\[extra_packages\\]$"
structure_error <- function(e) {
  d <- list(kind = if (identical(e$kind, "error")) "runtime" else e$kind, msg = e$message %||% "")
  fixes <- e$fixes
  if (identical(e$kind, "missing_package") && length(e$names) == 1 && any(grepl("[extra_packages]", fixes, fixed = TRUE))) {
    fixes <- sprintf("Add `requireNamespace(\"%s\")` as the first line of this cell and run it", e$names)
  } else if (length(fixes) > 0) {
    named <- grepl(EXTRA_FIX, fixes)
    fixes[named] <- sprintf("Add `requireNamespace(\"%s\")` as the first line of this cell and run it", sub(EXTRA_FIX, "\\1", fixes[named]))
  }
  if (length(fixes) > 0) d$fixes <- I(fixes)
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

# knitr's inline-code pattern, as Ember matches it (text-cells.R), one line at a time.
INLINE_SPAN <- "(?<!(^``))(?<!(\\n``))`r[ #]([^`]+)\\s*`"

# A text cell's output: its prose without the `#'` prefixes, each `r expr` span replaced by
# its value, as the page shows it. Spans stay as written while there are no values for this
# code (not run yet, or edited since), so the prose still reads (#270).
text_output <- function(v) {
  lines <- strsplit(v$code %||% "", "\n", fixed = TRUE)[[1]]
  out <- v$output
  values <- if (!is.null(out) && identical(out$mime, "application/vnd.ember.inline") && !isTRUE(v$code_differs)) {
    unlist(out$data$values)
  }
  k <- 0L
  body <- vapply(lines, function(line) {
    if (!grepl("^#+'", line)) return(line)
    line <- sub("^#+'[ ]?", "", line)
    m <- gregexpr(INLINE_SPAN, line, perl = TRUE)[[1]]
    if (is.null(values) || m[[1]] == -1) return(line)
    found <- regmatches(line, list(m))[[1]]
    shown <- vapply(seq_along(found), function(i) {
      k <<- k + 1L
      if (k <= length(values)) values[[k]] else found[[i]]
    }, character(1))
    regmatches(line, list(m)) <- list(shown)
    line
  }, character(1), USE.NAMES = FALSE)
  cap_text(paste(body, collapse = "\n"))
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
    not_run = identical(v$status, "not_run") && identical(v$kind, "code"),
    # A disabled cell, or one downstream of it, that Ember won't run (#232).
    off = isTRUE(v$disabled) || !is.na(v$disabled_by %||% NA_character_))
  if (errored) {
    e <- if (length(v$errors) > 0) v$errors[[1]] else list(kind = "interrupted", message = "The run was interrupted.")
    d$error <- structure_error(e)
    d$output <- d$error$msg
  } else if (identical(v$kind, "code")) {
    d$output <- output_text(v)
  } else {
    d$output <- text_output(v)
  }
  d$notes <- I(cell_notes(v))
  d$inputs <- I(vapply(v$inputs %||% list(), input_line, character(1)))
  # A running cell waiting on `readline()`, `menu()` and the like (Ember #186):
  # what the prompt printed and, for `menu()`, its choices. `answer` replies.
  # Its output is what this run printed so far, where a question's own text
  # (a license, a menu's title) usually is: Ember keeps the last run's result,
  # error and notes on a running cell, and they say nothing about the question.
  if (isTRUE(v$running) && !is.null(v$prompt)) {
    d$prompt <- list(id = v$prompt$id, text = v$prompt$text %||% "", choices = I(as.character(unlist(v$prompt$choices))))
    d$errored <- FALSE
    d$error <- NULL
    d$output <- output_text(list(console = v$console))
    d$notes <- I(character())
  }
  d
}

# One of a cell's inputs (emberui, Ember's docs/inputs.md) as read_cell
# shows it: "n: slider 10 to 500, step 10, value 250 (default 100)". An
# input that isn't bound says why, as the page's greyed widget does.
input_line <- function(i) {
  # A number as JSON writes it (5, not 5L), anything else as R prints it.
  shown <- function(x) {
    if (is.numeric(x) && length(x) == 1 && is.null(names(x))) format(x, digits = 15)
    else paste(deparse(x, width.cutoff = 500L), collapse = "")
  }
  spec <- i$spec %||% list()
  type <- spec$type %||% "input"
  range <- function(min, max) {
    min <- if (length(min) == 1 && !is.na(min)) min
    max <- if (length(max) == 1 && !is.na(max)) max
    if (!is.null(min) && !is.null(max)) sprintf(" %s to %s", shown(min), shown(max))
    else if (!is.null(min)) sprintf(" at least %s", shown(min))
    else if (!is.null(max)) sprintf(" at most %s", shown(max))
    else ""
  }
  what <- switch(type,
    slider = , number = paste0(type, range(spec$min, spec$max),
                               if (length(spec$step) == 1 && !is.na(spec$step)) sprintf(", step %s", shown(spec$step)) else ""),
    select = , radio = {
      values <- vapply(spec$choices %||% list(), shown, character(1), USE.NAMES = FALSE)
      labels <- unlist(spec$labels %||% values)
      choices <- ifelse(labels == values | labels == gsub('^"|"$', "", values), values, sprintf("%s = %s", labels, values))
      sprintf("%s%s of %s", if (isTRUE(spec$multiple)) "multiple " else "", type, paste(choices, collapse = ", "))
    },
    type)
  value <- sprintf(", value %s", shown(i$value))
  if (!identical(i$value, i$default)) value <- sprintf("%s (default %s)", value, shown(i$default))
  name <- if (is.null(i$name)) "" else sprintf("%s: ", i$name)
  why <- if (isTRUE(i$bound)) {
    # Only the input on the cell's last line is its output, the widget the user can move.
    # An Ember from before #446 doesn't report `output`: say nothing then.
    if (!isFALSE(i$output)) "" else " Not movable on the page: only the input on the cell's last line is. Give it its own cell, or group the cell's inputs with input_list()."
  } else switch(i$reason %||% "",
    read_elsewhere = sprintf(" Not bound: read `%s` in another cell.", i$name),
    " Not bound: assign this input to a name at the top of the cell to use it.")
  paste0(name, what, value, why)
}

# The notes Ember's page shows under a cell, from its last run or its code, worded as the
# page words them (Ember's lang/english.json), each "kind::text" like a tool
# warning (#207). `changed_in_place`: the run changed, by reference, a value
# another cell defines, so the cells that read it didn't rerun.
# `settings_found`: the run found a global setting, so the cell became a
# settings cell. `load_notes`: a package it loaded reset a setting a settings
# cell sets. `from_lock`: the cell's install calls didn't run, since the lock
# answers them (Ember #79); "" stands for a call that names no package.
# `untracked_reads`: the cell reads globals through `get()` on a
# computed name, `eval()` or a function that does, so it doesn't rerun when
# they change (Ember #357, #365). That one comes from the code, so it shows
# before the cell runs too.
cell_notes <- function(v) {
  names_of <- function(x) paste(sub("^\\.__C__", "class ", as.character(x)), collapse = ", ")
  notes <- character()
  if (length(v$changed_in_place) > 0) {
    notes <- c(notes, sprintf("changed_in_place::Changed %s in place, which another cell defines, so the cells that read it don't rerun. Make the change in the cell that defines it.", names_of(v$changed_in_place)))
  }
  if (length(v$settings_found) > 0) {
    notes <- c(notes, sprintf("settings_found::Found a global setting (%s) when this cell ran; the cells after it rerun.", names_of(v$settings_found)))
  }
  if (length(v$untracked_reads) > 0) {
    notes <- c(notes, sprintf("untracked_read::Ember can't tell which variables this cell reads through %s, so it doesn't rerun when they change. Name the variables in the code, or keep the values in one list.", paste0(v$untracked_reads, "()", collapse = ", ")))
  }
  if (length(v$from_lock) > 0) {
    named <- setdiff(as.character(v$from_lock), "")
    notes <- c(notes, sprintf("from_lock::Install calls don't run in Ember: it installs %s from the notebook's lock when a cell uses them. To get a newer version, call preview_package_update.",
                              if (length(named) > 0) paste(named, collapse = ", ") else "packages"))
  }
  for (n in v$load_notes %||% list()) {
    notes <- c(notes, sprintf("load_note::Loading %s resets %s, which a settings cell sets. Under Rscript the setting is lost, because settings cells come first in the file. Move it into this cell, after %s loads.", n$package, n$setting, n$package))
  }
  notes
}

# Ember's package work as it goes, in the shape Adapter.jl's `package_step`
# gives the core, or NULL when there is none: while queued cells wait for
# packages (`waiting_for`) or the library installs. The core then counts queued cells
# as running, and `read_cell` waits and tells the agent what is going on.
# `packages` are the ones the cells wait for, `seconds` how long since this
# adapter first saw the work, and `last_line` the installer's last line, or
# the package it is on. Step `"asking"`: the install stopped before a large
# download and waits for the user's OK (Ember #37, `approve_install()`);
# `download` then gives the total `bytes` and each `package`, `version` and
# `bytes` (NULL when its size isn't known). While installing, `source_builds`
# names the packages ("name version") that build from source for want of a
# binary for this R (Ember #19, the target's `no_binary`), when there are any.
package_step <- function(rec, snap, state) {
  # A cell that isn't queued may name a package that isn't installed; only a run makes it wait.
  waits <- unique(unlist(lapply(snap$cells, function(c) if (isTRUE(c$queued)) c$waiting_for), use.names = FALSE))
  lib <- state$packages$target
  installing <- identical(lib$status, "installing")
  asking <- identical(lib$status, "asking")
  if (length(waits) == 0 && !installing && !asking) {
    rec$packages_since <- NULL
    return(NULL)
  }
  rec$packages_since <- rec$packages_since %||% Sys.time()
  progress <- lib$progress
  step <- if (installing) {
    if (is.numeric(progress$total) && progress$total > 1) sprintf("installing, %d of %d", as.integer(progress$done %||% 0), as.integer(progress$total)) else "installing"
  } else if (asking) "asking" else if (identical(lib$status, "checking")) "checking" else "resolving"
  lines <- trimws(lib$log %||% character())
  lines <- lines[nzchar(lines)]
  last <- if (length(lines) > 0) utils::tail(lines, 1) else if (length(progress$current) == 1 && !is.na(progress$current)) paste("installing", progress$current) else NULL
  if (!is.null(last)) last <- substr(sub("(\\w+://)[^/[:space:]@]+@", "\\1", last), 1, 300)
  out <- list(step = step, packages = I(sort(as.character(waits))), seconds = round(as.numeric(difftime(Sys.time(), rec$packages_since, units = "secs"))),
              last_line = last)
  if (asking) out$download <- download_step(lib$download)
  source_builds <- lib$no_binary
  if (installing && length(source_builds) > 0) out$source_builds <- I(paste(names(source_builds), unname(source_builds)))
  out
}

# An "asking" library's large packages (`library$download`, largest first)
# as the step gives them.
download_step <- function(dl) {
  if (is.null(dl) || nrow(dl) == 0) return(list(bytes = 0, packages = I(list())))
  list(bytes = sum(dl$bytes, na.rm = TRUE),
       packages = I(lapply(seq_len(nrow(dl)), function(i) {
         list(package = dl$package[[i]], version = dl$version[[i]],
              bytes = if (is.na(dl$bytes[[i]])) NULL else dl$bytes[[i]])
       })))
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
    packages = package_step(rec, snap, state),
    lazy = identical(state$file$header$on_cell_change, "lazy"),
    package_date = state$file$header$snapshot,
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

# A name for a run that goes on, as Pluto's adapter gives one: the core
# counts the run, its dependents included, as going on until `run_finished`
# names it (#151).
run_id <- function() {
  A$runs <- A$runs + 1L
  sprintf("r-run-%d", A$runs)
}

# The cells a run of `ids` covers: in autorun, the cells downstream of them,
# which Ember queues as each result comes in (in the same step, so a run never
# looks over between a cell and its dependents), and `readers` (`old_readers`).
# In lazy mode they don't run.
run_cells_of <- function(rec, ids, readers = character()) {
  state <- notebook_state(rec$nb)
  if (!identical(state$file$header$on_cell_change, "autorun")) return(ids)
  g <- dependency_graph(rec$nb)
  unique(c(ids, unlist(lapply(intersect(ids, names(g$downstream)), function(id) downstream(g, id, transitive = TRUE)), use.names = FALSE), readers))
}

# The cells that read a name the last run of one of `ids` defined, and those
# downstream of them, in autorun. When a run drops a name (a rename), Ember
# reruns its readers too, and they fail as in plain R, but the graph no longer
# links them to the cell, so `run_cells_of` alone would call the run over
# before they ran (#228). Read before the run's results come in, while
# `results` still holds the old names. A cell that has never run, or a
# disabled one (`graph$off`), isn't rerun.
# `cells` (the run's own) are left out; a reader Ember already queued with
# them, as it does one in a definition clash, is kept, so the core reports it.
old_readers <- function(rec, ids, cells = ids) {
  state <- notebook_state(rec$nb)
  if (!identical(state$file$header$on_cell_change, "autorun")) return(character())
  g <- dependency_graph(rec$nb)
  out <- character()
  todo <- ids
  seen <- character()
  while (length(todo) > 0) {
    id <- todo[[1]]
    todo <- todo[-1]
    if (id %in% seen) next
    seen <- c(seen, id)
    names <- state$results[[id]]$defined %||% character()
    if (length(names) == 0) next
    readers <- Filter(function(cid) {
      c <- g$cells[[cid]]
      !identical(cid, id) && length(intersect(names, setdiff(c$references, c$masked_none))) > 0
    }, g$ids)
    for (r in readers) {
      reached <- c(r, downstream(g, r, transitive = TRUE))
      out <- union(out, reached)
      todo <- c(todo, setdiff(reached, seen))
    }
  }
  Filter(function(cid) !is.null(state$results[[cid]]) && !(cid %in% names(g$off)), setdiff(out, cells))
}

# The cell of a run of `ids` (`run_cells_of`) waiting on a prompt, or NULL.
# Nothing runs until it is answered or interrupted.
prompted <- function(rec, ids, readers = character()) {
  if (!registered(rec)) return(NULL)
  running <- notebook_state(rec$nb)$worker$running
  if (!is.null(running$prompt) && running$cell %in% run_cells_of(rec, ids, readers)) running$cell
}

# A run is over when none of its cells, or of the cells that depend on them
# (`run_cells_of`), is left, its R ended, or the notebook shut down.
run_over <- function(rec, ids, readers = character()) {
  if (!registered(rec)) return(TRUE)
  state <- notebook_state(rec$nb)
  identical(state$worker$status, "stopped") || length(unfinished(state, run_cells_of(rec, ids, readers))) == 0
}

# Whether the notebook's install stopped before a large download and waits
# for the user's OK (Ember #37), with nothing running: nothing moves until
# approve_install(), so a waited run returns rather than wait it out.
waits_for_ok <- function(rec) {
  if (!registered(rec)) return(FALSE)
  state <- notebook_state(rec$nb)
  identical(state$packages$target$status, "asking") && is.null(state$worker$running)
}

# Once a run nobody waits for ends, `run_finished` names it (`run`) and says
# which of its cells finished.
watch_run <- function(rec, cells, ids, readers = character(), run = run_id()) {
  check <- function() {
    if (!run_over(rec, ids, readers)) return(later::later(check, 0.2))
    done <- if (registered(rec)) setdiff(cells, unfinished(notebook_state(rec$nb), cells)) else cells
    notify("run_finished", list(notebook_id = rec$id, run = run, cells = I(done)))
  }
  later::later(check, 0.2)
  run
}

# Run cells (Ember runs their unrun ancestors first). Not accepted in safe
# preview or for a read-only notebook: Ember would allow execution on a run,
# Pluto (and the user's approval) wants allow_execution for that. With
# `wait`, a promise of the reply once the run is over, a cell of it waits on
# a prompt (then `prompted` names it), or `timeout` passes; the cells still to
# run are then `timed_out`. No cells is Pluto's cleanup after a delete, which
# Ember doesn't need.
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
  readers <- old_readers(rec, ids, cells)
  # The core adds these to the graph's dependents, for `will_run` and the cells
  # a waited run reports. Ember's queue holds them too, with the ancestors it
  # runs first, which the core leaves out. A cell Ember skips (one in a
  # definition clash) loses its names in run_cells() itself, which queues
  # their readers there, so `old_readers` can't find those.
  result$dependents <- I(union(readers, setdiff(r$queued, cells)))
  if (!isTRUE(wait)) {
    result$run <- watch_run(rec, cells, ids, readers)
    return(result)
  }
  deadline <- Sys.time() + as.numeric(timeout %||% 60)
  promise(function(resolve, reject) {
    check <- function() {
      over <- run_over(rec, ids, readers)
      asks <- if (!over) prompted(rec, ids, readers)
      if (!over && is.null(asks) && Sys.time() < deadline && !waits_for_ok(rec)) return(later::later(check, 0.05))
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
      if (!over) result$run <- watch_run(rec, cells, ids, readers)
      if (!is.null(asks)) result$prompted <- asks
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
  run <- if (isTRUE(run_all)) run_everything(rec)
  result <- list(already_allowed = FALSE, ran = isTRUE(run_all), process_status = process_status(notebook_state(rec$nb)))
  # The run it started, named as `run` names one, so the core can tell when it ends.
  if (!is.null(run)) result$run <- run
  result
}

# Run every cell; the run's name, if it goes on.
run_everything <- function(rec) {
  r <- run_cells(rec$nb, NULL, wait = FALSE)
  if (isTRUE(r$accepted) && length(r$queued) > 0) watch_run(rec, r$queued, r$queued, old_readers(rec, r$queued))
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

# ---- Moving the package date -------------------------------------------------

# A data frame's rows, or [] for none.
rows <- function(df) {
  if (is.null(df) || NROW(df) == 0) return(I(list()))
  rownames(df) <- NULL
  df
}

# The date a call names, or today: local time, as the Packages tab's Update
# button does (`ember_update_packages`).
date_param <- function(date) {
  if (is.null(date) || identical(date, "")) return(format(Sys.time(), "%Y-%m-%d"))
  if (!is.character(date) || length(date) != 1 || !grepl("^[0-9]{4}-[0-9]{2}-[0-9]{2}$", date) || is.na(as.Date(date, optional = TRUE))) {
    argument_error("invalid_argument", "date must be a day as YYYY-MM-DD")
  }
  date
}

# Ember's proposal for `date`, or NULL. A day Package Manager hasn't
# published yet (today, early in the day in UTC) is previewed at the newest
# day it has: the proposal's `date` is that day, and `asked` the day asked
# for (Ember #162, #221), so either one finds it (#247).
date_proposal <- function(prop, date) {
  if (!is.null(prop) && (identical(prop$date, date) || identical(prop$asked, date))) prop
}

# Ember's preview of moving the notebook's snapshot date (`preview_date()`):
# what would change, and nothing changes yet. It may fetch that date's index
# first, so it is waited for until it is ready or failed, or `timeout` passes
# (then `status` is still "fetching"). `date` is the day it would move to,
# which is earlier than `asked` when that day isn't published yet.
# With `package`, the date is Ember's for putting that package at `version`
# (CRAN's newest when NULL): `preview_update()`, the Packages tab's row
# Update (#266). The other packages move with the date, as far as they must.
preview_packages <- function(rec, date, timeout, package = NULL, version = NULL) {
  if (!is.null(package)) return(preview_package(rec, package, version, timeout))
  date <- date_param(date)
  preview_date(rec$nb, date, wait = FALSE)
  current <- function() {
    prop <- date_proposal(package_status(rec$nb)$proposal, date)
    if (identical(prop$kind %||% "date", "date") && is.null(prop$update)) prop
  }
  wait_preview(rec, current, date, timeout)
}

# The proposal `current()` finds, waited for until it is ready or failed or
# `timeout` passes, as the result preview_package_update returns.
wait_preview <- function(rec, current, asked, timeout) {
  deadline <- Sys.time() + as.numeric(timeout %||% 60)
  promise(function(resolve, reject) {
    check <- function() {
      prop <- if (registered(rec)) current()
      done <- !is.null(prop) && prop$status %in% c("ready", "failed")
      if (!done && registered(rec) && Sys.time() < deadline) return(later::later(check, 0.1))
      # A package's update waits for CRAN's release dates first ("dates").
      status <- if (is.null(prop) || !prop$status %in% c("ready", "failed")) "fetching" else prop$status
      to <- if (is.null(prop) || is.na(prop$date)) asked else prop$date
      # What the agent was shown, which the apply must match (set_package_date).
      rec$previewed <- if (identical(status, "ready")) list(date = to, changes = to_json(rows(prop$changes)))
      # How many packages the notebook has: with none, nothing changes but the date (#269). Counted
      # from what the code and header name and what the lock holds, not package_status, whose
      # table is empty until they resolve. An Ember that doesn't say gives no count.
      st <- notebook_state(rec$nb)
      packages <- if (!is.null(st$packages$wanted_cache)) length(unique(c(st$packages$wanted_cache, st$file$lock$entries$name)))
      resolve(list(
        date = to, asked = asked, current_date = st$file$header$snapshot,
        status = status, changes = rows(prop$changes), problems = rows(prop$problems), message = prop$message,
        packages = packages))
    }
    check()
  })
}

# The proposal `prop` if it is the preview this adapter asked for: `ask`,
# the package and version (NULL for CRAN's newest) of its last
# preview_update(). Ember replaces a newest's NULL with the version it
# resolves before the preview is ready, so only an explicit version is
# checked against the proposal's.
update_proposal <- function(prop, ask) {
  u <- prop$update
  if (is.null(u) || is.null(ask) || !identical(u$name, ask$name)) return(NULL)
  if (!is.null(ask$version) && !identical(u$version, ask$version)) return(NULL)
  prop
}

# The date for one package (`preview_update()`), previewed. A call again for
# the same package and version, while its preview is still being made or is
# ready, waits on that one rather than starting over; a different ask (a pin,
# then the newest) starts a new one.
preview_package <- function(rec, package, version, timeout) {
  if (!is.character(package) || length(package) != 1 || !nzchar(package)) {
    argument_error("invalid_argument", "package must be a package name")
  }
  if (!is.null(version) && (!is.character(version) || length(version) != 1 || !nzchar(version))) {
    argument_error("invalid_argument", "version must be a version, such as \"1.1.2\"")
  }
  ask <- list(name = package, version = version)
  current <- function() update_proposal(package_status(rec$nb)$proposal, ask)
  prop <- if (identical(rec$update_ask, ask)) current()
  if (!is.null(prop) && !identical(prop$status, "failed")) return(wait_preview(rec, current, NA_character_, timeout))
  # A package the code loads that the lock doesn't have yet (the notebook
  # has just opened, or a cell added it) isn't refused: Ember's preview
  # waits for the lock, and fails saying why if the lock never gets it.
  tryCatch(preview_update(rec$nb, package, version, wait = FALSE),
           ember_refused = function(e) argument_error("not_updatable", conditionMessage(e)))
  rec$update_ask <- ask
  wait_preview(rec, current, NA_character_, timeout)
}

# Ember's set_date(): apply the preview for `date`, refused unless it is
# ready and current. The library installs; a worker with a changed package
# loaded restarts once it is ready, and its cells are left not run.
# Ember redoes its proposal when the notebook's packages change, so the
# proposal can differ from what preview_packages showed the agent: then
# this refuses too, and the agent previews again (#201).
set_package_date <- function(rec, date) {
  date <- date_param(date)
  # The day asked for in the preview applies the day it moved to, the one
  # Ember's set_date() takes (#247).
  prop <- date_proposal(package_status(rec$nb)$proposal, date)
  date <- prop$date %||% date
  shown <- rec$previewed
  if (!is.null(prop) && identical(prop$status, "ready") &&
      !(identical(shown$date, date) && identical(shown$changes, to_json(rows(prop$changes))))) {
    argument_error("no_preview", "the notebook's packages changed since the preview, so the update would change more or less than it showed. Call preview_package_update again and tell the user what it shows")
  }
  changes <- tryCatch(set_date(rec$nb, date),
                      ember_refused = function(e) argument_error("no_preview", conditionMessage(e)))
  rec$previewed <- NULL
  state <- notebook_state(rec$nb)
  list(date = state$file$header$snapshot, changes = rows(changes), process_status = process_status(state))
}

# ---- Inputs ------------------------------------------------------------------

# Ember's set_input(): a new value for the input bound to `name`. The worker
# checks it against the input's spec; a value it refuses is an error that
# says what the input allows. In autorun the cells that read `name` rerun,
# as a run of them, waited for as `run` waits; in lazy mode they are only
# marked stale. Either way the input's own cell doesn't rerun. R reads the
# value only once a running cell finishes; if it hasn't by the deadline, the
# value is still on its way, so the reply says it is pending, not refused.
set_input <- function(rec, name, value, wait, timeout) {
  if (!is.character(name) || length(name) != 1 || is.na(name) || !nzchar(name)) {
    argument_error("invalid_argument", "name must be the name an input is assigned to")
  }
  state <- notebook_state(rec$nb)
  input <- bound_input(state, name)
  if (!is.null(input)) value <- json_value(input$spec, value)
  accepted <- tryCatch(ember::set_input(rec$nb, name, value, wait = FALSE),
                       ember_refused = function(e) argument_error("input_refused", conditionMessage(e)))
  # This call's value and outcome, by the token Ember gave the value (as its
  # own set_input() waits): the value now with the worker is this one, since
  # Ember refuses an R value while another is with it. Past this call, the
  # worker may hold a page value, and the outcome may be another caller's.
  token <- notebook_state(rec$nb)$worker$input$token
  mine <- function(state) !is.null(token) && identical(state$worker$input$token, token)
  deadline <- Sys.time() + as.numeric(timeout %||% 60)
  promise(function(resolve, reject) {
    answered <- function() {
      if (registered(rec) && mine(notebook_state(rec$nb)) && Sys.time() < deadline) {
        return(later::later(answered, 0.05))
      }
      state <- if (registered(rec)) notebook_state(rec$nb)
      if (mine(state)) {
        return(resolve(list(cell = accepted$cell, cells = I(character()), pending = TRUE,
                            lazy = !identical(state$file$header$on_cell_change, "autorun"),
                            accepted = TRUE, process_status = process_status(state))))
      }
      out <- state$input_outcome
      if (is.null(out) || is.null(token) || !identical(out$token, token)) {
        return(reject(tryCatch(argument_error("input_refused", "R didn't answer; the input's value is unchanged"), error = identity)))
      }
      if (!isTRUE(out$ok)) return(reject(tryCatch(argument_error("invalid_value", out$error), error = identity)))
      cells <- as.character(out$cells)
      result <- list(cell = accepted$cell, value = out$value,
                     cells = I(cells), lazy = !identical(state$file$header$on_cell_change, "autorun"),
                     accepted = TRUE, process_status = process_status(state))
      if (result$lazy || length(cells) == 0) return(resolve(result))
      if (!isTRUE(wait)) {
        result$run <- watch_run(rec, cells, cells)
        return(resolve(result))
      }
      over_by <- function() {
        over <- run_over(rec, cells)
        if (!over && Sys.time() < deadline) return(later::later(over_by, 0.05))
        if (!registered(rec)) {
          result[c("completed", "timed_out")] <- list(I(cells), I(character()))
          return(resolve(result))
        }
        state <- notebook_state(rec$nb)
        result$process_status <- process_status(state)
        exited <- exited_cells(state)
        if (!is.null(exited)) result$exited <- I(exited)
        result$completed <- I(setdiff(cells, unfinished(state, cells)))
        result$timed_out <- I(if (over) unfinished(state, cells) else unfinished(state, names(state$cells)))
        if (!over) result$run <- watch_run(rec, cells, cells)
        resolve(result)
      }
      over_by()
    }
    answered()
  })
}

# The input bound to `name`, as the cells' last results report it, or NULL.
bound_input <- function(state, name) {
  for (r in state$results) {
    for (i in r$inputs %||% list()) if (isTRUE(i$bound) && identical(i$name, name)) return(i)
  }
  NULL
}

# A value from JSON, which has no integers and no vectors, as the R value
# the input means: a dropdown's or radio's choice compared as a number when
# both are numbers (JSON's 2 is the choice 2L or 2), and a JSON array as a
# vector: a multiple dropdown's choices, or one choice that is itself a
# vector. emberui compares numbers by value too; this is a backstop for an
# older emberui, which compared with identical().
json_value <- function(spec, value) {
  choice <- spec$type %in% c("select", "radio")
  if (choice && !isTRUE(spec$multiple) && is.list(value) && length(value) > 0 &&
      all(vapply(value, function(v) is.atomic(v) && length(v) == 1, logical(1)))) {
    value <- json_choice(spec$choices, unlist(value))
  } else if (is.list(value) && length(value) > 0 && all(vapply(value, function(v) is.atomic(v) && length(v) == 1, logical(1)))) {
    value <- if (choice) lapply(value, json_choice, choices = spec$choices) else value
    if (!is.list(spec$choices)) value <- unlist(value)
  } else if (spec$type %in% c("select", "radio")) {
    value <- json_choice(spec$choices, value)
  }
  value
}

json_choice <- function(choices, v) {
  for (ch in as.list(choices)) {
    ch <- unname(ch)
    if (identical(ch, v)) return(ch)
    if (is.numeric(ch) && is.numeric(v) && length(ch) == length(v) && isTRUE(all(ch == v))) return(ch)
  }
  v
}

# Ember's approve_install(): the user's OK for an install that stopped
# before a large download (Ember #37). Refused when no install waits for one.
approve <- function(rec) {
  tryCatch(approve_install(rec$nb),
           ember_refused = function(e) argument_error("nothing_to_approve", conditionMessage(e)))
  state <- notebook_state(rec$nb)
  list(approved = TRUE, process_status = process_status(state))
}

interrupt <- function(rec) {
  state <- notebook_state(rec$nb)
  busy <- !is.null(state$worker$running) || length(state$pending) > 0
  interrupt_notebook(rec$nb)
  list(interrupted = busy)
}

# Answer the prompt `cell` waits on (Ember's answer_input()), which lets it go on.
# With `id`, only if it is still that prompt: not one asked since.
answer <- function(rec, cell, text, id = NULL) {
  if (!is.character(text) || length(text) != 1 || is.na(text)) argument_error("invalid_argument", "text must be a string")
  answer_input <- get0("answer_input", envir = asNamespace("ember"), mode = "function")
  if (is.null(answer_input)) argument_error("no_prompt", "This Ember can't answer prompts; update it")
  tryCatch(answer_input(rec$nb, text, cell = cell, id = id),
           error = function(e) argument_error("no_prompt", sprintf("Cell %s isn't waiting for input (%s)", cell, sub("^ember: ", "", conditionMessage(e)))))
  list(answered = TRUE)
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
  list(notebook_id = rec$id, path = state$path, process_status = process_status(state),
       lazy = identical(state$file$header$on_cell_change, "lazy"), package_date = state$file$header$snapshot)
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

# jsonlite's base64 breaks the line every 72 characters. The core counts
# every character as data (`base64_len()`), and a strict client may not
# decode it, so the breaks go (#208).
render_image <- function(rec, cell) {
  r <- render_png(rec$nb, cell)
  png <- if (is.raw(r$png)) gsub("\n", "", jsonlite::base64_enc(r$png), fixed = TRUE)
  list(png = png, mime = r$mime %||% "")
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
    answer = answer(rec, check_cell(rec, params$cell_id), params$text, params$prompt_id),
    restart = restart(rec),
    preview_packages = preview_packages(rec, params$date, params$timeout, params$package, params$version),
    set_package_date = set_package_date(rec, params$date),
    set_input = set_input(rec, params$name, params$value, isTRUE(params$wait), params$timeout),
    approve_install = approve(rec),
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
