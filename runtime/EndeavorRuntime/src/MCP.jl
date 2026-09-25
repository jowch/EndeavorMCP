const MCP_PROTOCOL_VERSION = "2024-11-05"
const MCP_SERVER_NAME      = "endeavor-runtime"
# Derived from Project.toml so the version reported to MCP clients cannot
# drift away from the released package version.
const MCP_SERVER_VERSION   = let v = pkgversion(@__MODULE__)
    v === nothing ? "unknown" : string(v)
end

# ---------------------------------------------------------------------------
# Tool schema definitions
# ---------------------------------------------------------------------------

const MCP_TOOLS = [
    Dict{String,Any}(
        "name"        => "list_notebooks",
        "description" => "List all notebooks currently open in the Pluto session, with each one's run state: pending_run (ids of edited cells not yet run), running (ids of cells queued or running, in notebook order), and execution_allowed (whether Pluto will run code now: false in safe preview, and while the notebook process is stopped, restarting, or crashed). Does not count as reading any cell.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(),
            "required"   => String[],
        ),
    ),
    Dict{String,Any}(
        "name"        => "read_cell",
        "description" => "Return the code, output, stale flag, and structured error (when errored) of a single cell. Errored cells include an error object with kind, hint, and fixes when Pluto recognizes the failure (e.g. pluto_multi_expression).",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "cell_id"     => Dict("type" => "string", "description" => "The cell UUID."),
            ),
            "required" => ["notebook_id", "cell_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "view_cell_output",
        "description" => "See a cell's output as an image (PNG): plots, figures, rendered images. Use to check what a visual output actually looks like; read_cell only describes non-text outputs. Needs the notebook to be running code (not safe preview) unless the output already is a PNG, and waits while the notebook's worker is busy running cells. Outputs with no PNG form (Markdown, HTML, text) fail with no_image; use read_cell for those.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "cell_id"     => Dict("type" => "string", "description" => "The cell UUID."),
            ),
            "required" => ["notebook_id", "cell_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "edit_cell",
        "description" => "Replace the code in a cell. Requires a prior read_cell or read_notebook_code on this cell. Stages the edit by default (run_after=false); call submit_changes to run staged cells. After a successful edit, the server records a read receipt for the new code (satisfies read guard for your own staged edits only).",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "cell_id"     => Dict("type" => "string", "description" => "The cell UUID."),
                "code"        => Dict("type" => "string", "description" => "New cell code."),
                "run_after"   => Dict("type" => "boolean", "description" => "Run the cell after updating (non-blocking). Default: false."),
            ),
            "required" => ["notebook_id", "cell_id", "code"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "edit_cells",
        "description" => "Batch stage cell code edits. Each cell must have been read first. All-or-nothing on read guard failure. Never runs cells; call submit_changes to execute staged edits. After success, read receipts are updated for each edited cell.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "cells"       => Dict(
                    "type"        => "array",
                    "description" => "Array of {cell_id, code} objects to stage.",
                    "items"       => Dict{String,Any}(
                        "type"       => "object",
                        "properties" => Dict{String,Any}(
                            "cell_id" => Dict("type" => "string", "description" => "The cell UUID."),
                            "code"    => Dict("type" => "string", "description" => "New cell code."),
                        ),
                        "required" => ["cell_id", "code"],
                    ),
                ),
            ),
            "required" => ["notebook_id", "cells"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "add_cell",
        "description" => "Insert a new cell into the notebook. after_cell_id is required when the notebook is not empty; that anchor cell must have been read first. Records a read receipt for the new cell (same as edit_cell).",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id"   => Dict("type" => "string", "description" => "The notebook UUID."),
                "code"          => Dict("type" => "string", "description" => "Initial cell code."),
                "after_cell_id" => Dict("type" => "string", "description" => "Insert after this cell UUID; required when notebook is non-empty."),
                "run_after"     => Dict("type" => "boolean", "description" => "Run the new cell after inserting (non-blocking). Default: false."),
                "folded"        => Dict("type" => "boolean", "description" => "Hide the new cell's code, showing only its output (Pluto fold). Use for markdown/prose cells. Default: false."),
            ),
            "required" => ["notebook_id", "code"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "delete_cell",
        "description" => "Delete a cell from the notebook. This is irreversible within the session.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "cell_id"     => Dict("type" => "string", "description" => "The cell UUID to delete."),
            ),
            "required" => ["notebook_id", "cell_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "execute_cell",
        "description" => "Run a specific cell (Shift+Enter). Default wait_for_completion=false queues the run and returns immediately (poll read_cell); pass true only when a synchronous result is required on a durable session.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id"         => Dict("type" => "string", "description" => "The notebook UUID."),
                "cell_id"             => Dict("type" => "string", "description" => "The cell UUID."),
                "wait_for_completion" => Dict("type" => "boolean", "description" => "Block until the cell finishes. Default: false (non-blocking; preferred for stdio-bound sessions)."),
            ),
            "required" => ["notebook_id", "cell_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "submit_changes",
        "description" => "Run all staged (dirty) cells, like Cmd+S in Pluto. Runs reactive dependents automatically. Explicit cell_ids must be in pending_run unless force=true. Default wait_for_completion=false queues the run and clears pending_run in the background; poll read_cell / read_notebook_code for completion. Pass true only when a synchronous result is required on a durable session.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id"         => Dict("type" => "string", "description" => "The notebook UUID."),
                "cell_ids"            => Dict(
                    "type"        => "array",
                    "items"       => Dict("type" => "string"),
                    "description" => "Optional subset of cell IDs to run; defaults to all pending staged cells.",
                ),
                "force"               => Dict("type" => "boolean", "description" => "Allow running cell_ids not in pending_run. Default: false."),
                "wait_for_completion" => Dict("type" => "boolean", "description" => "Block until cells finish. Default: false (non-blocking; preferred for stdio-bound sessions)."),
            ),
            "required" => ["notebook_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "run_all_cells",
        "description" => "Re-run all cells in the notebook in dependency order. Clears pending_run when execution finishes. Default wait_for_completion=false queues the run; pass true to block until done. Prefer submit_changes for staged agent edits.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id"         => Dict("type" => "string", "description" => "The notebook UUID."),
                "wait_for_completion" => Dict("type" => "boolean", "description" => "Block until all cells finish. Default: false."),
            ),
            "required" => ["notebook_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "move_cell",
        "description" => "Reorder a cell relative to another. Pass an empty string for after_cell_id to move to the top.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id"   => Dict("type" => "string", "description" => "The notebook UUID."),
                "cell_id"       => Dict("type" => "string", "description" => "The cell UUID to move."),
                "after_cell_id" => Dict("type" => "string", "description" => "Move after this cell UUID; pass \"\" to move to top."),
            ),
            "required" => ["notebook_id", "cell_id", "after_cell_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "fold_cell",
        "description" => "Fold or unfold a cell: folded hides the code editor and shows only the output (Pluto's eye toggle). Metadata only; nothing runs. Persisted in the notebook file.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "cell_id"     => Dict("type" => "string", "description" => "The cell UUID."),
                "folded"      => Dict("type" => "boolean", "description" => "true to hide the code, false to show it."),
            ),
            "required" => ["notebook_id", "cell_id", "folded"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "read_notebook_code",
        "description" => "Return the notebook as a single code string with cell markers. Default order is execution (dependency) order; use order=visual for UI layout order.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id"      => Dict("type" => "string", "description" => "The notebook UUID."),
                "order"            => Dict(
                    "type"        => "string",
                    "enum"        => ["execution", "visual"],
                    "description" => "Cell ordering for projection. Default: execution.",
                ),
                "include_markdown" => Dict(
                    "type"        => "boolean",
                    "description" => "Include markdown cells as # md: prefixed blocks. Default: false.",
                ),
            ),
            "required" => ["notebook_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "get_cell_order",
        "description" => "Return cell IDs in visual (UI) order.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
            ),
            "required" => ["notebook_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "get_execution_order",
        "description" => "Return cell IDs in execution (dependency) order.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
            ),
            "required" => ["notebook_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "get_cell_dependencies",
        "description" => "Return upstream cell IDs and referenced symbols for a cell. For reactivity debugging, not the default edit workflow.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "cell_id"     => Dict("type" => "string", "description" => "The cell UUID."),
            ),
            "required" => ["notebook_id", "cell_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "get_cell_dependents",
        "description" => "Return downstream cell IDs that would re-run if this cell changes (transitive). For reactivity debugging.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "cell_id"     => Dict("type" => "string", "description" => "The cell UUID."),
            ),
            "required" => ["notebook_id", "cell_id"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "find_symbol_definitions",
        "description" => "Find cells where a symbol is defined (semantic analysis). For debugging, not default workflow.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "symbol"      => Dict("type" => "string", "description" => "Symbol name to look up."),
            ),
            "required" => ["notebook_id", "symbol"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "find_symbol_references",
        "description" => "Find cells that reference a symbol (semantic analysis). Differs from search_code text search.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "symbol"      => Dict("type" => "string", "description" => "Symbol name to look up."),
            ),
            "required" => ["notebook_id", "symbol"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "validate_cell",
        "description" => "Validate proposed cell code (parse + single-expression rules) before edit_cell. Does not mutate the notebook.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "cell_id"     => Dict("type" => "string", "description" => "The cell UUID (used for parse context)."),
                "code"        => Dict("type" => "string", "description" => "Proposed cell code to validate."),
            ),
            "required" => ["notebook_id", "cell_id", "code"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "search_code",
        "description" => "Text search across all cell codes. Returns snippets; unlike find_symbol_references, matches comments and shadowed names.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id" => Dict("type" => "string", "description" => "The notebook UUID."),
                "query"       => Dict("type" => "string", "description" => "Substring to search for."),
            ),
            "required" => ["notebook_id", "query"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "pluto_session_status",
        "description" => "Return whether Pluto is running, open notebooks, and (when managed) session_id plus mcp/pluto URLs.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(),
            "required"   => String[],
        ),
    ),
    Dict{String,Any}(
        "name"        => "open_notebook",
        "description" => "Load a .jl notebook file into the live Pluto session (user-confirmed path). Default safe preview (no auto-run); set run_notebook=true to queue a single non-blocking full run via SessionActions.open (no second run_all).",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "path"          => Dict("type" => "string", "description" => "Filesystem path to the notebook .jl file."),
                "run_notebook"  => Dict("type" => "boolean", "description" => "Queue a non-blocking full run after open. Default: false (safe preview)."),
            ),
            "required" => ["path"],
        ),
    ),
    Dict{String,Any}(
        "name"        => "new_notebook",
        "description" => "Create a new empty notebook file (written by Pluto itself) and load it into the live Pluto session, ready to run (no safe preview: it has no code yet). It starts with one empty cell, whose id is in cell_ids: edit it for the first cell. Use this instead of writing a .jl file by hand. Omit path to use Pluto's default new-notebook location; an existing file is never overwritten (use open_notebook for that).",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "path" => Dict("type" => "string", "description" => "Optional filesystem path for the new .jl file. Must not exist yet; its directory must."),
            ),
            "required"   => String[],
        ),
    ),
    Dict{String,Any}(
        "name"        => "allow_execution",
        "description" => "Exit safe preview on an open notebook (Glass Run notebook code equivalent). Use when the user explicitly asks to run the notebook. Default run_notebook=true queues a single non-blocking full run (poll read_cell for completion). run_notebook=false exits the gate without queuing a run.",
        "inputSchema" => Dict{String,Any}(
            "type"       => "object",
            "properties" => Dict{String,Any}(
                "notebook_id"  => Dict("type" => "string", "description" => "Notebook UUID."),
                "run_notebook" => Dict("type" => "boolean", "description" => "Queue a non-blocking full run after allowing execution. Default: true. false exits safe preview only."),
            ),
            "required" => ["notebook_id"],
        ),
    ),
]

# MCP's read-only hint (what Claude Code's plan mode checks before prompting):
# everything except the tools that write or run (Policy.jl).
for tool in MCP_TOOLS
    tool["annotations"] = Dict{String,Any}("readOnlyHint" => !(tool["name"] in _WRITE_TOOLS))
end

# ---------------------------------------------------------------------------
# JSON-RPC / MCP framing
# ---------------------------------------------------------------------------

function _read_message(io::IO)
    while !eof(io)
        line = readline(io; keep=false)
        isempty(strip(line)) && continue
        return JSON.parse(line, Dict{String,Any})
    end
    return nothing
end

function _write_message(io::IO, msg)
    write(io, JSON.json(msg))
    write(io, '\n')
    flush(io)
end

# ---------------------------------------------------------------------------
# Response builders
# ---------------------------------------------------------------------------

_ok(id, result) = Dict{String,Any}("jsonrpc" => "2.0", "id" => id, "result" => result)

_err(id, code, message) = Dict{String,Any}(
    "jsonrpc" => "2.0",
    "id"      => id,
    "error"   => Dict{String,Any}("code" => code, "message" => message),
)

# ---------------------------------------------------------------------------
# Tool call dispatch
# ---------------------------------------------------------------------------

function _handle_tool_call(session, name, arguments)
    result = call_tool_with_session(session, name, arguments)
    content = if result isa CellImage
        [
            Dict{String,Any}("type" => "text", "text" => JSON.json(result.meta)),
            Dict{String,Any}("type" => "image", "data" => base64encode(result.png), "mimeType" => "image/png"),
        ]
    else
        [Dict{String,Any}("type" => "text", "text" => JSON.json(result))]
    end
    Dict{String,Any}("content" => content, "isError" => false)
end

function _safe_handle_tool_call(session, name, arguments; owner::AbstractString="")
    try
        refusal = policy_refusal(owner, name)
        refusal === nothing || throw(refusal)
        _handle_tool_call(session, name, arguments)
    catch e
        raw = sprint(showerror, e)
        # Error format: "error_type::human message" (ArgumentError may prefix the type name)
        error_type, error_msg = if contains(raw, "::")
            parts = split(raw, "::", limit=2)
            et = string(strip(parts[1]))
            if occursin(':', et)
                et = strip(last(split(et, ':')))
            end
            et, string(parts[2])
        else
            "tool_error", raw
        end
        Dict{String,Any}(
            "content" => [Dict{String,Any}("type" => "text", "text" => JSON.json(
                Dict{String,Any}("error" => error_type, "message" => error_msg)
            ))],
            "isError" => true,
        )
    end
end

# ---------------------------------------------------------------------------
# JSON-RPC dispatch (shared by stdio and HTTP/SSE transports)
# ---------------------------------------------------------------------------

# `owner` = the calling agent session (its X-Endeavor-Session header), for policy.
function _dispatch_mcp(session, msg::Dict{String,Any}; owner::AbstractString="")
    method = get(msg, "method", "")
    id     = get(msg, "id", nothing)

    # Notifications have no id and require no response
    id === nothing && return nothing

    if method == "initialize"
        _ok(id, Dict{String,Any}(
            "protocolVersion" => MCP_PROTOCOL_VERSION,
            "capabilities"    => Dict{String,Any}("tools" => Dict{String,Any}()),
            "serverInfo"      => Dict{String,Any}("name" => MCP_SERVER_NAME, "version" => MCP_SERVER_VERSION),
        ))

    elseif method == "tools/list"
        _ok(id, Dict{String,Any}("tools" => MCP_TOOLS))

    elseif method == "tools/call"
        params    = get(msg, "params", Dict{String,Any}())
        name      = get(params, "name", "")
        arguments = get(params, "arguments", Dict{String,Any}())
        result    = _safe_handle_tool_call(session, name, arguments; owner)
        publish_notebooks!()
        _ok(id, result)

    elseif method == "ping"
        _ok(id, Dict{String,Any}())

    else
        _err(id, -32601, "Method not found: $method")
    end
end

# ---------------------------------------------------------------------------
# stdio loop (used directly by tests and by connect() proxy)
# ---------------------------------------------------------------------------

function run_mcp_server(session, io_in::IO=stdin, io_out::IO=stdout)
    while !eof(io_in)
        msg = _read_message(io_in)
        msg === nothing && break
        resp = _dispatch_mcp(session, msg)
        resp === nothing && continue
        _write_message(io_out, resp)
    end
end
