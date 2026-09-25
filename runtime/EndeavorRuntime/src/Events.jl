# Pushes notebook state to the app over `GET /events` (server-sent events)
# whenever it changes, so the app doesn't poll. Each event is
#   {"notebooks": [the list_notebooks summary],
#    "cells": {notebook_id: [{cell_id, running, errored, unrun, author}, ...]}}
# in notebook order. `unrun`: edited by the agent and not run since. `author`:
# who last changed the cell's code ("agent", "user", or null if unchanged since
# the runtime first saw it). Triggered by Pluto's events and after each tool call
# (pending-run changes don't always reach Pluto's state).

const _EVENT_LOCK = ReentrantLock()
const _EVENT_SUBSCRIBERS = Set{Channel{String}}()
const _LAST_EVENT = Ref("")

# Who last changed each cell's code: (hash of the code, "agent" | "user" | "").
const _AUTHOR_LOCK = ReentrantLock()
const _AUTHORS = Dict{UUID, Dict{UUID, Tuple{UInt64, String}}}()

_authors_for(notebook_id::UUID) = get!(() -> Dict{UUID, Tuple{UInt64, String}}(), _AUTHORS, notebook_id)

"The agent's tools just wrote this cell's code."
function note_agent_edit!(notebook_id::UUID, cell)::Nothing
    lock(() -> (_authors_for(notebook_id)[cell.cell_id] = (hash(cell.code), "agent")), _AUTHOR_LOCK)
    return nothing
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
            "running" => c.running || c.queued,
            "errored" => c.errored,
            "unrun"   => id in pending,
            "author"  => _author!(nb.notebook_id, c),
        )
        for id in nb.cell_order for c in (nb.cells_dict[id],)
    ]
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
    first = lock(_EVENT_LOCK) do
        push!(_EVENT_SUBSCRIBERS, ch)
        _notebooks_json()
    end
    try
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
