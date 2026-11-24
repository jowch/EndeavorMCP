
function _dict_get(d::AbstractDict, key::AbstractString)
    if haskey(d, key)
        return d[key]
    end
    sym = Symbol(key)
    return haskey(d, sym) ? d[sym] : nothing
end

function _error_msg(body)
    body === nothing && return ""
    body isa AbstractString && return body
    body isa AbstractDict || return sprint(show, body)
    msg = _dict_get(body, "msg")
    msg === nothing && (msg = _dict_get(body, "plain_error"))
    msg === nothing && return sprint(show, body)
    msg isa AbstractString ? msg : string(msg)
end

function _parse_boundaries(msg::AbstractString)
    m = match(r"Boundaries:\s*(\[[^\]]*\])", msg)
    m === nothing && return Int[]
    try
        b = Meta.parse(strip(m.captures[1]))
        b isa Vector{Int} && return b
        b isa Vector && return Int[b...]
        if b isa Expr && b.head == :vect
            return Int[b.args...]
        end
    catch
    end
    return Int[]
end

function _structure_error(body)
    msg = _error_msg(body)
    isempty(msg) && return nothing

    if occursin("extra token after end of expression", msg)
        boundaries = _parse_boundaries(msg)
        n = max(length(boundaries), 1)
        hint = if isempty(boundaries)
            "Multiple expressions in one cell. Wrap all code in a begin ... end block."
        else
            "Multiple expressions in one cell. Wrap all code in a begin ... end block (preferred), or split into $n cells."
        end
        core = strip(split(msg, "\n\nBoundaries:")[1])
        d = Dict{String,Any}(
            "kind"        => "pluto_multi_expression",
            "msg"         => core,
            "hint"        => hint,
            "fixes"       => ["wrap_begin_end", "split_cells"],
        )
        isempty(boundaries) || (d["boundaries"] = boundaries)
        isempty(boundaries) || (d["split_count"] = n)
        return d
    end

    d = Dict{String,Any}("kind" => "runtime", "msg" => msg)
    if body isa AbstractDict
        plain = _dict_get(body, "plain_error")
        plain !== nothing && plain != msg && (d["plain_error"] = plain)
    end
    return d
end

function _serialize_output(cell)
    try
        __serialize_output(cell)
    catch e
        "[$(string(cell.output.mime)) output; it couldn't be summarized: $(first(sprint(showerror, e), 200))]"
    end
end

function __serialize_output(cell)
    if cell.errored
        body = cell.output.body
        structured = _structure_error(body)
        structured !== nothing && haskey(structured, "hint") && return structured["hint"]
        msg = _error_msg(body)
        return isempty(msg) ? "" : msg
    end
    body = cell.output.body
    body === nothing && return ""
    mime = cell.output.mime
    see = _is_visual(mime) || _embedded_png_b64(cell.output) !== nothing ? "; call view_cell_output to see it" : ""
    if mime == MIME("text/plain") && body isa AbstractString
        return body
    elseif body isa AbstractString
        return "[$(string(mime)) output, $(sizeof(body)) bytes$see]"
    elseif body isa Vector{UInt8}
        return "[$(string(mime)) output, $(length(body)) bytes$see]"
    else
        return "[$(string(mime)) output]"
    end
end

_is_visual(mime) = mime == MIME("image/png") || mime == MIME("image/svg+xml")

const _EMBEDDED_PNG_MIN = 4_096

const _PNG_SRC = r"\bsrc\s*=\s*[\"']data:image/png;base64,([A-Za-z0-9+/=]+)[\"']"i

function _img_tags(html::AbstractString)
    s = String(html)
    tags = SubString{String}[]
    i = 1
    while (at = findnext(r"<img\b"i, s, i)) !== nothing
        gt = findnext('>', s, last(at))
        stop = gt === nothing ? lastindex(s) : gt
        push!(tags, SubString(s, first(at), stop))
        stop == lastindex(s) && break
        i = nextind(s, stop)
    end
    return tags
end

function _embedded_png_b64(output)
    output.mime == MIME("text/html") && output.body isa AbstractString || return nothing
    best = nothing
    for tag in _img_tags(output.body)
        m = match(_PNG_SRC, tag)
        m === nothing && continue
        b64 = m.captures[1]
        (best === nothing || sizeof(b64) > sizeof(best)) && (best = b64)
    end
    best === nothing || sizeof(best) * 3 ÷ 4 < _EMBEDDED_PNG_MIN || !startswith(best, "iVBORw0KGgo") ? nothing : best
end

function _embedded_png(output)
    b64 = _embedded_png_b64(output)
    b64 === nothing && return nothing
    png = try base64decode(b64) catch; return nothing end
    return length(png) >= 8 && png[1:8] == UInt8[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a] ? png : nothing
end

const _INTERACTIVE = r"<(script|canvas|input|select|button|textarea|iframe|bond)\b"i

_interactive_html(output) = output.mime == MIME("text/html") && output.body isa AbstractString && occursin(_INTERACTIVE, output.body)

const _BUSY_RENDER_WAIT = 10.0

const _BUSY = "is busy running cells"

const _RENDERS = Dict{Tuple{Any, String}, Tuple{Any, Task}}()
const _RENDERS_LOCK = ReentrantLock()

function _render_png(worker, cell_id, key)
    @async try
        Pluto.WorkspaceManager.Malt.remote_eval_fetch(worker, quote
            let value = get(PlutoRunner.cell_results, $cell_id, nothing)
                value isa PlutoRunner.Bond && (value = value.element)
                value !== nothing && showable(MIME"image/png"(), value) ?
                    repr(MIME"image/png"(), value) : nothing
            end
        end)
    finally
        lock(_RENDERS_LOCK) do
            last(get(_RENDERS, key, (nothing, nothing))) === current_task() && delete!(_RENDERS, key)
        end
    end
end

function _cell_png(session, nb, cell)
    body = cell.output.body
    cell.output.mime == MIME("image/png") && body isa Vector{UInt8} && return (body, "value")
    workspace = Pluto.WorkspaceManager.get_workspace((session, nb); allow_creation=false)
    if workspace === nothing
        embedded = _embedded_png(cell.output)
        embedded === nothing || return (embedded, "embedded image")
        throw(ArgumentError("no_image::Cell $(cell.cell_id): the notebook " *
            "has no running worker (safe preview or not yet run), so its value can't be rendered"))
    end
    cell_id = cell.cell_id
    key = (nb.notebook_id, string(cell_id))
    asked = lock(_RENDERS_LOCK) do
        worker, pending = get(_RENDERS, key, (nothing, nothing))
        worker === workspace.worker && !istaskdone(pending) && return pending
        asking = _render_png(workspace.worker, cell_id, key)
        _RENDERS[key] = (workspace.worker, asking)
        asking
    end
    if _cells_running(nb) && timedwait(() -> istaskdone(asked) || !_cells_running(nb), _BUSY_RENDER_WAIT) === :timed_out
        embedded = _embedded_png(cell.output)
        embedded === nothing || return (embedded, "embedded image")
        throw(ArgumentError("no_image::Cell $(cell.cell_id): the notebook's worker $(_BUSY) " *
            "and didn't render its value within $(round(Int, _BUSY_RENDER_WAIT)) seconds; " *
            "view it again when read_cell says they have finished"))
    end
    png = try
        fetch(asked)
    catch e
        e isa TaskFailedException ? throw(e.task.exception) : rethrow()
    end
    png === nothing || return (png, "value")
    return (_embedded_png(cell.output), "embedded image")
end

const _TEXT_MAX = 16_000

function _cap_text(text::AbstractString)
    sizeof(text) <= _TEXT_MAX && return String(text)
    cut = prevind(text, _TEXT_MAX + 1)
    return text[1:cut] * "\n… (cut at $(_TEXT_MAX) bytes of $(sizeof(text)))"
end

const _HTML_ENTITIES = ["&nbsp;" => " ", "&lt;" => "<", "&gt;" => ">", "&quot;" => "\"", "&#39;" => "'", "&amp;" => "&"]

function _numeric_entity(entity::AbstractString)
    hex = entity[3] in ('x', 'X')
    code = tryparse(UInt32, entity[(hex ? 4 : 3):end-1]; base = hex ? 16 : 10)
    code === nothing || code == 0 || !isvalid(Char, code) ? String(entity) : string(Char(code))
end

const _SCRIPT_END = Dict("script" => r"</script\s*>"i, "style" => r"</style\s*>"i)

function _without_scripts(html::AbstractString)
    s = String(html)
    out = IOBuffer()
    i = 1
    while (m = match(r"<(script|style)\b"i, s, i)) !== nothing
        write(out, SubString(s, i, prevind(s, m.offset)))
        ending = findnext(_SCRIPT_END[lowercase(m.captures[1])], s, m.offset)
        ending === nothing && return String(take!(out))
        i = nextind(s, last(ending))
    end
    write(out, SubString(s, i))
    return String(take!(out))
end

function _html_text(html::AbstractString)
    s = _without_scripts(html)
    s = replace(s, r"</t[dh]>\s*"i => "\t", r"<br\s*/?>"i => "\n", r"</(tr|p|div|li|h[1-6]|pre|table|thead|tbody)>"i => "\n")
    s = replace(s, r"<[^<>]*>" => "")
    s = replace(s, _HTML_ENTITIES..., r"&#[0-9]{1,7};|&#[xX][0-9a-fA-F]{1,6};" => _numeric_entity)
    s = join((rstrip(line, (' ', '\t')) for line in split(s, '\n')), '\n')
    s = replace(s, r"\n{3,}" => "\n\n")
    return strip(s)
end

function _cells_running(nb)
    held = _asking(nb)
    any(c -> c.running || (c.queued && (held === nothing || c ∉ held.cells)), values(nb.cells_dict))
end

function _cell_text(session, nb, cell)
    cell.errored && return nothing
    body = cell.output.body
    (body === nothing || cell.output.mime == MIME("text/plain")) && return nothing
    busy = _cells_running(nb)
    workspace = busy ? nothing : Pluto.WorkspaceManager.get_workspace((session, nb); allow_creation=false)
    if workspace !== nothing
        cell_id = cell.cell_id
        html = cell.output.mime == MIME("text/html")
        text = Pluto.WorkspaceManager.Malt.remote_eval_fetch(workspace.worker, quote
            let value = get(PlutoRunner.cell_results, $cell_id, nothing)
                value isa PlutoRunner.Bond && (value = value.element)
                generic = $html && which(show, Tuple{IO, MIME"text/plain", typeof(value)}).module === Base.Multimedia
                value === nothing || generic ? nothing : sprint(show, MIME"text/plain"(), value;
                    context = (:limit => true, :displaysize => (60, 160)))
            end
        end)
        text !== nothing && return _cap_text(text)
    end
    cell.output.mime == MIME("text/html") && body isa AbstractString && return _cap_text(_html_text(body))
    busy && return "(cells are running; read this cell again when they finish to see its output as text)"
    return nothing
end

function _cell_output_error(cell)
    cell.errored || return nothing
    return _structure_error(cell.output.body)
end

const _LOG_KINDS = ("Error" => "logged_error", "Warn" => "logged_warning", "Info" => "logged_info")
const _PLUTO_STDOUT_LOG_LEVEL = "LogLevel(-555)"
const _LOG_NOTE_MAX_BYTES = 300
const _LOG_NOTES_MAX = 5
const _PRINTED_TAIL_BYTES = 1_000

function _no_ansi(s)
    s = replace(s, r"\e\[[0-9;]*[A-Za-z]" => "", "\r\n" => "\n")
    shown(line) = (parts = filter(!isempty, split(line, '\r')); isempty(parts) ? "" : String(last(parts)))
    join((shown(line) for line in split(s, '\n')), '\n')
end

function _cut_note(s::AbstractString, max::Int)
    s = strip(s)
    sizeof(s) <= max && return String(s)
    return s[1:prevind(s, max + 1)] * " … (cut at $max bytes of $(sizeof(s)))"
end

function _log_text(m)
    m isa Tuple && length(m) == 2 || return nothing
    body, mime = m
    body isa AbstractString || return nothing
    string(mime) == "text/plain" && return _no_ansi(body)
    string(mime) == "text/html" && return _html_text(body)
    return nothing
end

function _log_message(log)
    text = something(_log_text(get(log, "msg", nothing)), "(not text)")
    kwargs = String[]
    for kv in get(log, "kwargs", ())
        kv isa Union{Tuple,AbstractVector} && length(kv) == 2 || continue
        name, value = kv
        value = _log_text(value)
        push!(kwargs, value === nothing ? "$name = (not text)" : "$name = $(strip(value))")
    end
    isempty(kwargs) ? text : "$(strip(text)) ($(join(kwargs, ", ")))"
end

function _cell_notes(cell)
    try
        __cell_notes(cell)
    catch
        String[]
    end
end

function __cell_notes(cell)
    logs = cell.logs
    notes = String[]
    isempty(logs) && return notes
    counted = Pair{String,Int}[]
    for (level, kind) in _LOG_KINDS, log in logs
        get(log, "level", nothing) == level || continue
        note = "$kind::" * _cut_note(_log_message(log), _LOG_NOTE_MAX_BYTES)
        i = findfirst(p -> p.first == note, counted)
        i === nothing ? push!(counted, note => 1) : (counted[i] = note => counted[i].second + 1)
    end
    for (note, n) in counted[1:min(end, _LOG_NOTES_MAX)]
        push!(notes, n == 1 ? note : "$note (logged $n times)")
    end
    length(counted) > _LOG_NOTES_MAX && (notes[end] *= " ($(length(counted) - _LOG_NOTES_MAX) more messages not shown)")
    printed = join((something(_log_text(get(log, "msg", nothing)), "") for log in logs if get(log, "level", nothing) == _PLUTO_STDOUT_LOG_LEVEL))
    printed = strip(_no_ansi(printed))
    if !isempty(printed)
        if sizeof(printed) > _PRINTED_TAIL_BYTES
            start = nextind(printed, sizeof(printed) - _PRINTED_TAIL_BYTES)
            printed = "(the last $(_PRINTED_TAIL_BYTES) bytes of $(sizeof(printed))) …" * printed[start:end]
        end
        push!(notes, "printed::" * printed)
    end
    return notes
end
