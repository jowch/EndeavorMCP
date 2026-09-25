# Pushes the notebook list (the `list_notebooks` summary: paths, run state,
# pending cells) to the app over `GET /events` (server-sent events) whenever it
# changes, so the app doesn't poll. Triggered by Pluto's events and after each
# tool call (pending-run changes don't always reach Pluto's state).

const _EVENT_LOCK = ReentrantLock()
const _EVENT_SUBSCRIBERS = Set{Channel{String}}()
const _LAST_EVENT = Ref("")

function _notebooks_json()
    sess = standalone_session()
    sess === nothing && return "[]"
    return JSON.json(tool_list_notebooks(sess, Dict{String,Any}()))
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
