# Structured cell output for MCP agents (mirrors Pluto frontend ErrorMessage.js where useful).

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

"""
    _structure_error(body) -> Union{Dict{String,Any}, Nothing}

Turn Pluto runner error bodies into agent-friendly structured errors.
"""
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
    see = _is_visual(mime) ? "; call view_cell_output to see it" : ""
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

# ---------------------------------------------------------------------------
# Visual outputs (view_cell_output)
# ---------------------------------------------------------------------------

# Outputs view_cell_output can usually turn into a PNG: PNGs as-is, and SVGs (plots),
# whose values typically also `show` as PNG. HTML (`md""`, `HTML()`) and other image
# formats rarely have a PNG form, so read_cell doesn't point agents at the tool for them.
_is_visual(mime) = mime == MIME("image/png") || mime == MIME("image/svg+xml")

"""
    _cell_png(session, nb, cell) -> Union{Vector{UInt8}, Nothing}

PNG rendering of a cell's output. Pluto displays the richest format it can (SVG
before PNG for plots), so unless the output already is a PNG, ask the notebook's
worker to re-render the cell's value as `image/png`. `nothing` if the value has
no PNG form; throws `no_image::` if the notebook has no running worker (safe preview).
"""
function _cell_png(session, nb, cell)
    body = cell.output.body
    cell.output.mime == MIME("image/png") && body isa Vector{UInt8} && return body
    workspace = Pluto.WorkspaceManager.get_workspace((session, nb); allow_creation=false)
    workspace === nothing && throw(ArgumentError("no_image::Cell $(cell.cell_id): the notebook " *
        "has no running worker (safe preview or not yet run), so its value can't be rendered"))
    cell_id = cell.cell_id
    # ponytail: no timeout (documented in the tool description); a worker busy with a
    # long run delays this call until it yields.
    Pluto.WorkspaceManager.Malt.remote_eval_fetch(workspace.worker, quote
        let value = get(PlutoRunner.cell_results, $cell_id, nothing)
            value !== nothing && showable(MIME"image/png"(), value) ?
                repr(MIME"image/png"(), value) : nothing
        end
    end)
end

# ---------------------------------------------------------------------------
# Text form of rich outputs (read_cell)
# ---------------------------------------------------------------------------

const _TEXT_MAX = 16_000

function _cap_text(text::AbstractString)
    sizeof(text) <= _TEXT_MAX && return String(text)
    cut = prevind(text, _TEXT_MAX + 1)
    return text[1:cut] * "\n… (cut at $(_TEXT_MAX) bytes of $(sizeof(text)))"
end

const _HTML_ENTITIES = ["&nbsp;" => " ", "&lt;" => "<", "&gt;" => ">", "&quot;" => "\"", "&#39;" => "'", "&amp;" => "&"]

"Readable text from an HTML string: rows and blocks on their own lines, table cells tab-separated."
function _html_text(html::AbstractString)
    s = replace(html, r"<(script|style)\b.*?</\1>"is => "")
    s = replace(s, r"</t[dh]>\s*"i => "\t", r"<br\s*/?>"i => "\n", r"</(tr|p|div|li|h[1-6]|pre|table|thead|tbody)>"i => "\n")
    s = replace(s, r"<[^>]*>"s => "")
    s = replace(s, _HTML_ENTITIES...)
    s = replace(s, r"[ \t]+\n" => "\n", r"\n{3,}" => "\n\n")
    return strip(s)
end

"""
    _cell_text(session, nb, cell) -> Union{String, Nothing}

Text form of a cell's output when Pluto shows it as something other than text
(HTML, tables, trees, images): the value's `text/plain` display, rendered by the
notebook's worker. `nothing` when the output already is text, or is empty or an error.
While cells are running the worker isn't asked, since it would wait for them; stored
HTML is stripped to text instead.
"""
function _cell_text(session, nb, cell)
    cell.errored && return nothing
    body = cell.output.body
    (body === nothing || cell.output.mime == MIME("text/plain")) && return nothing
    busy = any(c -> c.running || c.queued, values(nb.cells_dict))
    workspace = busy ? nothing : Pluto.WorkspaceManager.get_workspace((session, nb); allow_creation=false)
    if workspace !== nothing
        cell_id = cell.cell_id
        html = cell.output.mime == MIME("text/html")
        # A value that shows as HTML but has only the generic text/plain `show`
        # (`HTML(...)`, most widgets) prints as a struct dump; its stripped HTML reads better.
        text = Pluto.WorkspaceManager.Malt.remote_eval_fetch(workspace.worker, quote
            let value = get(PlutoRunner.cell_results, $cell_id, nothing)
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
