# Pushes notebook state to the app over `GET /events` (server-sent events)
# whenever it changes, so the app doesn't poll. Each event is
#   {"notebooks": [the list_notebooks summary],
#    "cells": {notebook_id: [{cell_id, running, errored, unrun, author, before, version, name}, ...]}}
# in notebook order. `unrun`: edited by the agent and not run since. `author`:
# who last changed the cell's code ("agent", "user", or null if unchanged since
# the runtime first saw it). `before`: an unrun cell's code before the agent's
# first edit since it last ran ("" for a cell the agent added), for the
# in-editor diff; absent otherwise. `version`: a hash of the code, so the app sees
# each edit. `name`: what the cell defines, as of its last run (null if nothing).
# Triggered by Pluto's events and after each tool call
# (pending-run changes don't always reach Pluto's state).

const _EVENT_LOCK = ReentrantLock()
const _EVENT_SUBSCRIBERS = Set{Channel{String}}()
const _LAST_EVENT = Ref("")

# Who last changed each cell's code: (hash of the code, "agent" | "user" | "").
const _AUTHOR_LOCK = ReentrantLock()
const _AUTHORS = Dict{UUID, Dict{UUID, Tuple{UInt64, String}}}()

_authors_for(notebook_id::UUID) = get!(() -> Dict{UUID, Tuple{UInt64, String}}(), _AUTHORS, notebook_id)

# Each unrun cell's code before the agent's first edit since it last ran.
const _BEFORES = Dict{UUID, Dict{UUID, String}}()
_befores_for(notebook_id::UUID) = get!(() -> Dict{UUID, String}(), _BEFORES, notebook_id)

"The agent's tools just wrote this cell's code; `before` is what it replaced."
function note_agent_edit!(notebook_id::UUID, cell, before::AbstractString)::Nothing
    lock(_AUTHOR_LOCK) do
        _authors_for(notebook_id)[cell.cell_id] = (hash(cell.code), "agent")
        get!(_befores_for(notebook_id), cell.cell_id, String(before))
    end
    return nothing
end

# The before-text while the cell is unrun and differs; forgotten once it runs.
function _before!(notebook_id::UUID, cell, unrun::Bool)
    lock(_AUTHOR_LOCK) do
        befores = _befores_for(notebook_id)
        unrun || (delete!(befores, cell.cell_id); return nothing)
        before = get(befores, cell.cell_id, nothing)
        before == cell.code ? nothing : before
    end
end

# Code that changed without our tools writing it was changed by the user.
function _author!(notebook_id::UUID, cell)
    lock(_AUTHOR_LOCK) do
        authors = _authors_for(notebook_id)
        h = hash(cell.code)
        known = get(authors, cell.cell_id, nothing)
        if known === nothing
            authors[cell.cell_id] = (h, "")
        elseif known[1] != h
            authors[cell.cell_id] = (h, "user")
        end
        author = authors[cell.cell_id][2]
        isempty(author) ? nothing : author
    end
end

function _cell_states(nb)
    pending = Set(pending_run_ids(nb.notebook_id, nb))
    [
        Dict{String,Any}(
            "cell_id" => string(id),
            "running" => is_running(nb, c),
            "errored" => c.errored,
            "unrun"   => id in pending,
            "author"  => _author!(nb.notebook_id, c),
            "before"  => _before!(nb.notebook_id, c, id in pending),
            "version" => string(hash(c.code); base=16),
            "name"    => _cell_name(nb, c),
        )
        for id in nb.cell_order for c in (nb.cells_dict[id],)
    ]
end

# Pluto's dependency data as of the last run: no reanalysis on every event.
function _cell_name(nb, cell)
    node = nb.topology.nodes[cell]   # a default dict: an unanalysed cell is an empty node
    defs = sort!(string.(collect(union(node.definitions, node.funcdefs_without_signatures))))
    isempty(defs) ? nothing : join(first(defs, 3), ", ")
end

function _notebooks_json()
    sess = standalone_session()
    sess === nothing && return JSON.json(Dict("notebooks" => [], "cells" => Dict()))
    return JSON.json(Dict(
        "notebooks" => tool_list_notebooks(sess, Dict{String,Any}()),
        "cells"     => Dict(string(nb.notebook_id) => _cell_states(nb) for nb in values(sess.notebooks)),
    ))
end

"Send the notebook list to subscribers if it changed since the last send."
function publish_notebooks!()::Nothing
    json = try
        _notebooks_json()
    catch e
        @warn "Couldn't summarize notebooks for the app" exception = e
        return nothing
    end
    lock(_EVENT_LOCK) do
        json == _LAST_EVENT[] && return
        _LAST_EVENT[] = json
        foreach(ch -> put!(ch, json), _EVENT_SUBSCRIBERS)
    end
    return nothing
end

"End every open event stream (e.g. when the bridge stops)."
function close_event_streams!()::Nothing
    lock(_EVENT_LOCK) do
        foreach(close, _EVENT_SUBSCRIBERS)
        empty!(_EVENT_SUBSCRIBERS)
        _LAST_EVENT[] = ""
    end
    return nothing
end

function _handle_events(http::HTTP.Stream)
    read(http)
    ch = Channel{String}(Inf)
    try
        # Register and snapshot together, so no change falls between them; inside
        # the try, so a failed snapshot doesn't leave a dead subscriber behind.
        first = lock(_EVENT_LOCK) do
            push!(_EVENT_SUBSCRIBERS, ch)
            _notebooks_json()
        end
        HTTP.setstatus(http, 200)
        HTTP.setheader(http, "Content-Type" => "text/event-stream")
        HTTP.setheader(http, "Cache-Control" => "no-cache")
        HTTP.startwrite(http)
        write(http, "data: $first\n\n")
        for json in ch
            write(http, "data: $json\n\n")
        end
    catch e
        e isa Base.IOError || e isa EOFError || e isa InvalidStateException || rethrow()
    finally
        lock(() -> delete!(_EVENT_SUBSCRIBERS, ch), _EVENT_LOCK)
        close(ch)
    end
end
