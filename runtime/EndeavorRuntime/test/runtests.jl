using EndeavorRuntime
using Pluto
using Test
using UUIDs
using JSON
using HTTP
using Sockets
using SHA
using Base64

# Pluto saves (and backs up) notebooks it opens, so tests work on a temp copy.
fresh_fixture() = cp(joinpath(@__DIR__, "fixtures", "test_notebook.jl"), joinpath(mktempdir(), "test_notebook.jl"))

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

function make_session_with_notebook(cells...)
    session  = Pluto.ServerSession()
    nb_cells = [Pluto.Cell(; code=c) for c in cells]
    nb       = Pluto.Notebook(collect(nb_cells), tempname() * ".jl")
    session.notebooks[nb.notebook_id] = nb
    session, nb, nb_cells
end

function read_cells!(session, nb, cells...)
    for cell in cells
        EndeavorRuntime.tool_read_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cell.cell_id),
        ))
    end
end

# ---------------------------------------------------------------------------
# Unit tests — no Pluto web server required
# ---------------------------------------------------------------------------

@testset "EndeavorRuntime.jl" begin

    EndeavorRuntime.reset_staging_state!()

    @testset "list_notebooks" begin
        session, nb, _ = make_session_with_notebook("x = 1")
        result = EndeavorRuntime.tool_list_notebooks(session, Dict())
        @test length(result) == 1
        @test result[1]["notebook_id"] == string(nb.notebook_id)
        @test result[1]["cell_count"] == 1
        @test result[1]["pending_run"] == String[]
        @test result[1]["running"] == String[]
        @test result[1]["execution_allowed"] isa Bool
    end

    @testset "list_notebooks reports pending_run without read receipts" begin
        session, nb, cells = make_session_with_notebook("y = 2")
        EndeavorRuntime.mark_pending!(nb.notebook_id, cells[1].cell_id)
        result = EndeavorRuntime.tool_list_notebooks(session, Dict())
        @test result[1]["pending_run"] == [string(cells[1].cell_id)]
        # Listing must not satisfy read-before-edit.
        @test_throws ArgumentError EndeavorRuntime.require_fresh_read!(nb.notebook_id, cells[1])
    end

    @testset "list_notebooks reports running cells" begin
        session, nb, cells = make_session_with_notebook("a = 1", "b = 2")
        # A queued flag with nothing running is Pluto's leftover (e.g. safe preview).
        cells[2].queued = true
        result = EndeavorRuntime.tool_list_notebooks(session, Dict())
        @test result[1]["running"] == String[]
        # While a cell runs, queued ones count too.
        cells[1].running = true
        result = EndeavorRuntime.tool_list_notebooks(session, Dict())
        @test result[1]["running"] == [string(cells[1].cell_id), string(cells[2].cell_id)]
        cells[2].queued = false
        result = EndeavorRuntime.tool_list_notebooks(session, Dict())
        @test result[1]["running"] == [string(cells[1].cell_id)]
    end

    @testset "list_notebooks reports execution_allowed=false in safe preview" begin
        session, nb, cells = make_session_with_notebook("z = 3")
        nb.process_status = Pluto.ProcessStatus.ready
        @test EndeavorRuntime.tool_list_notebooks(session, Dict())[1]["execution_allowed"] == true
        nb.process_status = Pluto.ProcessStatus.waiting_for_permission
        @test EndeavorRuntime.tool_list_notebooks(session, Dict())[1]["execution_allowed"] == false
    end

    @testset "read_cell" begin
        session, nb, cells = make_session_with_notebook("z = 99")
        args   = Dict("notebook_id" => string(nb.notebook_id), "cell_id" => string(cells[1].cell_id))
        result = EndeavorRuntime.tool_read_cell(session, args)
        @test result["cell_id"] == string(cells[1].cell_id)
        @test result["code"] == "z = 99"
        @test result["stale"] == false
    end

    @testset "error on unknown notebook_id" begin
        session, _, _ = make_session_with_notebook("x = 1")
        fake_id = string(uuid4())
        @test_throws Exception EndeavorRuntime.tool_read_cell(session,
            Dict("notebook_id" => fake_id, "cell_id" => string(uuid4())))
    end

    @testset "error on unknown cell_id" begin
        session, nb, _ = make_session_with_notebook("x = 1")
        fake_cell_id = string(uuid4())
        @test_throws Exception EndeavorRuntime.tool_read_cell(session,
            Dict("notebook_id" => string(nb.notebook_id), "cell_id" => fake_cell_id))
    end

    @testset "add_cell appended on empty notebook" begin
        session  = Pluto.ServerSession()
        nb       = Pluto.Notebook(Pluto.Cell[], tempname() * ".jl")
        session.notebooks[nb.notebook_id] = nb
        args = Dict(
            "notebook_id" => string(nb.notebook_id),
            "code"        => "new_var = 42",
            "run_after"   => false,
        )
        result = EndeavorRuntime.tool_add_cell(session, args)
        @test haskey(result, "cell_id")
        @test result["code"] == "new_var = 42"
        @test length(nb.cell_order) == 1
        @test nb.cell_order[1] == UUID(result["cell_id"])
    end

    @testset "add_cell rejects missing placement on non-empty notebook" begin
        session, nb, _ = make_session_with_notebook("x = 1")
        args = Dict(
            "notebook_id" => string(nb.notebook_id),
            "code"        => "new_var = 42",
        )
        @test_throws Exception EndeavorRuntime.tool_add_cell(session, args)
    end

    @testset "add_cell after_cell_id" begin
        session, nb, cells = make_session_with_notebook("first", "last")
        read_cells!(session, nb, cells[1])
        args = Dict(
            "notebook_id"   => string(nb.notebook_id),
            "code"          => "middle",
            "after_cell_id" => string(cells[1].cell_id),
            "run_after"     => false,
        )
        result = EndeavorRuntime.tool_add_cell(session, args)
        @test length(nb.cell_order) == 3
        @test nb.cell_order[2] == UUID(result["cell_id"])
    end

    @testset "add_cell assigns new cell_order vector" begin
        session, nb, cells = make_session_with_notebook("first", "last")
        read_cells!(session, nb, cells[2])
        order_before = nb.cell_order
        args = Dict(
            "notebook_id"   => string(nb.notebook_id),
            "code"          => "tail",
            "after_cell_id" => string(cells[2].cell_id),
            "run_after"     => false,
        )
        EndeavorRuntime.tool_add_cell(session, args)
        @test nb.cell_order !== order_before
    end

    @testset "edit_cell default does not execute" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)

        cell_y = cells[2]
        @test EndeavorRuntime._serialize_output(cell_y) == "1"

        read_cells!(session, nb, cells[1])
        result = EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 10",
        ))
        @test result["stale"] == true
        @test EndeavorRuntime._serialize_output(cell_y) == "1"
        @test result["pending_run"] == [string(cells[1].cell_id)]
    end

    @testset "submit_changes runs staged cells" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)

        read_cells!(session, nb, cells[1])
        EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 10",
        ))

        receipt = EndeavorRuntime.tool_submit_changes(session, Dict(
            "notebook_id"         => string(nb.notebook_id),
            "wait_for_completion" => true,
        ))
        @test receipt["applied"] == true
        @test string(cells[1].cell_id) ∈ receipt["affected_cells"]
        @test isempty(receipt["pending_run"])

        cell_y = cells[2]
        @test EndeavorRuntime._serialize_output(cell_y) == "10"
    end

    @testset "submit_changes noop when nothing pending" begin
        session, nb, _ = make_session_with_notebook("x = 1")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        receipt = EndeavorRuntime.tool_submit_changes(session, Dict(
            "notebook_id" => string(nb.notebook_id),
        ))
        @test receipt["execution"]["status"] == "completed"
        @test isempty(receipt["pending_run"])
    end

    @testset "submit_changes not_staged and force" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        read_cells!(session, nb, cells[1])
        @test_throws Exception EndeavorRuntime.tool_submit_changes(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_ids"    => [string(cells[2].cell_id)],
        ))
        receipt = EndeavorRuntime.tool_submit_changes(session, Dict(
            "notebook_id"         => string(nb.notebook_id),
            "cell_ids"            => [string(cells[2].cell_id)],
            "force"               => true,
            "wait_for_completion" => true,
        ))
        @test receipt["applied"] == true
        @test string(cells[2].cell_id) ∈ receipt["affected_cells"]
    end

    @testset "read-before-edit guard" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x")

        @test_throws Exception EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 2",
        ))

        read_cells!(session, nb, cells[1])
        result = EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 2",
        ))
        @test result["code"] == "x = 2"

        cells[1].code = "x = 99"
        @test_throws Exception EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 3",
        ))

        EndeavorRuntime.tool_read_notebook_code(session,
            Dict("notebook_id" => string(nb.notebook_id)))
        result2 = EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 3",
        ))
        @test result2["code"] == "x = 3"

        session2, nb2, cells2 = make_session_with_notebook("a = 1")
        @test_throws Exception EndeavorRuntime.tool_add_cell(session2, Dict(
            "notebook_id"   => string(nb2.notebook_id),
            "code"          => "b = 2",
            "after_cell_id" => string(cells2[1].cell_id),
        ))
        read_cells!(session2, nb2, cells2[1])
        add_result = EndeavorRuntime.tool_add_cell(session2, Dict(
            "notebook_id"   => string(nb2.notebook_id),
            "code"          => "b = 2",
            "after_cell_id" => string(cells2[1].cell_id),
        ))
        @test add_result["code"] == "b = 2"
    end

    @testset "delete_cell" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = 2")
        args = Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
        )
        result = EndeavorRuntime.tool_delete_cell(session, args)
        @test result["applied"] == true
        @test length(nb.cell_order) == 1
        @test !haskey(nb.cells_dict, cells[1].cell_id)
    end

    @testset "move_cell to top" begin
        session, nb, cells = make_session_with_notebook("first", "second", "third")
        args = Dict(
            "notebook_id"   => string(nb.notebook_id),
            "cell_id"       => string(cells[3].cell_id),
            "after_cell_id" => "",
        )
        EndeavorRuntime.tool_move_cell(session, args)
        @test nb.cell_order[1] == cells[3].cell_id
        @test nb.cell_order[2] == cells[1].cell_id
        @test nb.cell_order[3] == cells[2].cell_id
    end

    @testset "move_cell after target" begin
        session, nb, cells = make_session_with_notebook("first", "second", "third")
        args = Dict(
            "notebook_id"   => string(nb.notebook_id),
            "cell_id"       => string(cells[1].cell_id),
            "after_cell_id" => string(cells[3].cell_id),
        )
        EndeavorRuntime.tool_move_cell(session, args)
        @test nb.cell_order[1] == cells[2].cell_id
        @test nb.cell_order[2] == cells[3].cell_id
        @test nb.cell_order[3] == cells[1].cell_id
    end

    @testset "_serialize_output plain text" begin
        cell = Pluto.Cell(; code="1 + 1")
        cell.output = Pluto.CellOutput(body="2", mime=MIME("text/plain"))
        @test EndeavorRuntime._serialize_output(cell) == "2"
    end

    @testset "_serialize_output errored" begin
        cell = Pluto.Cell(; code="error(\"boom\")")
        cell.errored = true
        cell.output  = Pluto.CellOutput(body="boom", mime=MIME("text/plain"))
        @test EndeavorRuntime._serialize_output(cell) == "boom"
    end

    @testset "_structure_error multi_expression" begin
        body = Dict{Symbol,Any}(
            :msg => "syntax: extra token after end of expression\n\nBoundaries: [13, 30]",
        )
        err = EndeavorRuntime._structure_error(body)
        @test err["kind"] == "pluto_multi_expression"
        @test err["boundaries"] == [13, 30]
        @test err["split_count"] == 2
        @test err["fixes"] == ["wrap_begin_end", "split_cells"]
        @test occursin("begin ... end block (preferred)", err["hint"])
    end

    @testset "read_cell structured error" begin
        session, nb, cells = make_session_with_notebook("using Plots\nplot(sin, 0, 2pi)")
        cell = cells[1]
        cell.code = "using Plots\nplot(sin, 0, 2pi)"
        Pluto.update_save_run!(session, nb, [cell]; run_async=false, save=true)
        result = EndeavorRuntime.tool_read_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cell.cell_id),
        ))
        @test result["errored"] == true
        @test haskey(result, "error")
        @test result["error"]["kind"] == "pluto_multi_expression"
        @test occursin("begin ... end", result["output"])
    end

    @testset "_serialize_output HTML" begin
        cell = Pluto.Cell(; code="html\"<b>hi</b>\"")
        cell.output = Pluto.CellOutput(body="<b>hi</b>", mime=MIME("text/html"))
        out = EndeavorRuntime._serialize_output(cell)
        @test startswith(out, "[text/html output,")
    end

    # ---------------------------------------------------------------------------
    # MCP protocol round-trip tests (no network, no Pluto web server)
    # ---------------------------------------------------------------------------

    # Helper: write a newline-delimited JSON message to a buffer
    function write_msg(buf, msg)
        write(buf, EndeavorRuntime.JSON.json(msg))
        write(buf, '\n')
    end

    # Helper: read one newline-delimited JSON response from a buffer
    function read_resp(buf)
        seekstart(buf)
        EndeavorRuntime.JSON.parse(readline(buf; keep=false), Dict{String,Any})
    end

    @testset "MCP protocol: initialize" begin
        session, nb, _ = make_session_with_notebook("x = 7")

        buf_in  = IOBuffer()
        buf_out = IOBuffer()

        write_msg(buf_in, Dict("jsonrpc" => "2.0", "id" => 1, "method" => "initialize", "params" => Dict()))
        seekstart(buf_in)

        EndeavorRuntime.run_mcp_server(session, buf_in, buf_out)

        resp = read_resp(buf_out)
        @test resp["result"]["protocolVersion"] == EndeavorRuntime.MCP_PROTOCOL_VERSION
        @test resp["result"]["serverInfo"]["name"] == "endeavor-runtime"
        @test resp["result"]["serverInfo"]["version"] == EndeavorRuntime.MCP_SERVER_VERSION
    end

    @testset "serverInfo.version tracks Project.toml" begin
        # Regression: MCP_SERVER_VERSION was hardcoded to "1.0.0" and silently
        # drifted from the released version for several releases.
        toml = read(joinpath(pkgdir(EndeavorRuntime), "Project.toml"), String)
        m    = match(r"(?m)^version\s*=\s*\"([^\"]+)\"", toml)
        @test m !== nothing
        @test EndeavorRuntime.MCP_SERVER_VERSION == m.captures[1]
    end

    @testset "MCP protocol: tools/list" begin
        session, _, _ = make_session_with_notebook("x = 1")

        buf_in  = IOBuffer()
        buf_out = IOBuffer()

        write_msg(buf_in, Dict("jsonrpc" => "2.0", "id" => 2, "method" => "tools/list", "params" => Dict()))
        seekstart(buf_in)

        EndeavorRuntime.run_mcp_server(session, buf_in, buf_out)

        resp  = read_resp(buf_out)
        names = [t["name"] for t in resp["result"]["tools"]]

        @test "list_notebooks"  ∈ names
        @test "read_cell"       ∈ names
        @test "edit_cell"       ∈ names
        @test "edit_cells"      ∈ names
        @test "submit_changes"  ∈ names
        @test "view_cell_output" ∈ names
        @test "execute_cell"    ∈ names
        @test "add_cell"        ∈ names
        @test "delete_cell"     ∈ names
        @test "run_all_cells"   ∈ names
        @test "move_cell"       ∈ names
        @test "fold_cell"       ∈ names
        @test !("get_notebook_state" ∈ names)
        @test !("get_cell" ∈ names)
        @test !("set_cell_code" ∈ names)
        @test !("run_cell" ∈ names)

        fold_tool = only(t for t in EndeavorRuntime.MCP_TOOLS if t["name"] == "fold_cell")
        @test "folded" in fold_tool["inputSchema"]["required"]
        add_tool = only(t for t in EndeavorRuntime.MCP_TOOLS if t["name"] == "add_cell")
        @test haskey(add_tool["inputSchema"]["properties"], "folded")
    end

    @testset "MCP protocol: tools/call list_notebooks" begin
        session, nb, _ = make_session_with_notebook("x = 1")

        buf_in  = IOBuffer()
        buf_out = IOBuffer()

        write_msg(buf_in, Dict("jsonrpc" => "2.0", "id" => 3, "method" => "tools/call",
            "params" => Dict("name" => "list_notebooks", "arguments" => Dict{String,Any}())))
        seekstart(buf_in)

        EndeavorRuntime.run_mcp_server(session, buf_in, buf_out)

        resp = read_resp(buf_out)
        @test resp["result"]["isError"] == false
        data = EndeavorRuntime.JSON.parse(resp["result"]["content"][1]["text"])
        @test length(data) == 1
        @test data[1]["notebook_id"] == string(nb.notebook_id)
    end

    @testset "MCP protocol: unknown method returns error" begin
        session, _, _ = make_session_with_notebook("x = 1")

        buf_in  = IOBuffer()
        buf_out = IOBuffer()

        write_msg(buf_in, Dict("jsonrpc" => "2.0", "id" => 4, "method" => "nonexistent", "params" => Dict()))
        seekstart(buf_in)

        EndeavorRuntime.run_mcp_server(session, buf_in, buf_out)

        resp = read_resp(buf_out)
        @test haskey(resp, "error")
        @test resp["error"]["code"] == -32601
    end

    # ---------------------------------------------------------------------------
    # Integration test — real Pluto session, Julia API only (no MCP stdio)
    # ---------------------------------------------------------------------------

    @testset "Integration: edit_cell run_after triggers reactivity" begin
        fixture = fresh_fixture()
        @test isfile(fixture)

        tmp = tempname() * ".jl"
        cp(fixture, tmp)

        session = Pluto.ServerSession(;
            options = Pluto.Configuration.from_flat_kwargs(launch_browser = false),
        )
        pluto_task = @async Pluto.run!(session)
        try
            deadline = time() + 30.0
            while time() < deadline && isempty(session.notebooks)
                sleep(0.1)
            end

            nb = Pluto.SessionActions.open(session, tmp; run_async=false)

            cell_x_id = "11111111-1111-1111-1111-111111111111"
            cell_y_id = "22222222-2222-2222-2222-222222222222"

            result_y = EndeavorRuntime.tool_read_cell(session,
                Dict("notebook_id" => string(nb.notebook_id), "cell_id" => cell_y_id))
            @test result_y["output"] == "42"

            EndeavorRuntime.tool_read_cell(session,
                Dict("notebook_id" => string(nb.notebook_id), "cell_id" => cell_x_id))
            EndeavorRuntime.tool_edit_cell(session, Dict(
                "notebook_id" => string(nb.notebook_id),
                "cell_id"     => cell_x_id,
                "code"        => "x = 10",
                "run_after"   => true,
            ))

            # run_after is non-blocking; poll until reactive output updates.
            result_y2 = nothing
            deadline = time() + 30.0
            while time() < deadline
                result_y2 = EndeavorRuntime.tool_read_cell(session,
                    Dict("notebook_id" => string(nb.notebook_id), "cell_id" => cell_y_id))
                result_y2["output"] == "70" && !result_y2["running"] && !result_y2["queued"] && break
                sleep(0.05)
            end
            @test result_y2["output"] == "70"

            Pluto.SessionActions.shutdown(session, nb; async=false, verbose=false)
            sleep(1.0)
        finally
            rm(tmp; force=true)
            try; schedule(pluto_task, InterruptException(); error=true); catch; end
            sleep(0.5)
        end
    end

    @testset "edit_cells stages multiple cells without executing" begin
        session, nb, cells = make_session_with_notebook("a = 1", "b = 2", "c = a + b")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)

        cell_c = cells[3]
        @test EndeavorRuntime._serialize_output(cell_c) == "3"

        read_cells!(session, nb, cells[1], cells[2])
        receipt = EndeavorRuntime.tool_edit_cells(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cells"       => [
                Dict("cell_id" => string(cells[1].cell_id), "code" => "a = 10"),
                Dict("cell_id" => string(cells[2].cell_id), "code" => "b = 20"),
            ],
        ))
        @test receipt["applied"] == true
        @test receipt["mutation"]["type"] == "edit_cells"
        @test length(receipt["pending_run"]) == 2
        @test EndeavorRuntime._serialize_output(cell_c) == "3"
        @test receipt["execution"]["status"] == "staged"
    end

    @testset "delete_cell returns mutation receipt" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = 2")
        result = EndeavorRuntime.tool_delete_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
        ))
        @test result["applied"] == true
        @test result["mutation"]["type"] == "delete_cell"
        @test haskey(result, "cell_order")
        @test any(startswith(w, "async_execution::") for w in result["warnings"])
    end

    @testset "move_cell receipt includes cell_order" begin
        session, nb, cells = make_session_with_notebook("first", "second", "third")
        receipt = EndeavorRuntime.tool_move_cell(session, Dict(
            "notebook_id"   => string(nb.notebook_id),
            "cell_id"       => string(cells[3].cell_id),
            "after_cell_id" => "",
        ))
        @test receipt["applied"] == true
        @test receipt["cell_order"] == [string(id) for id in nb.cell_order]
        @test receipt["mutation"]["old_index"] == 3
        @test receipt["mutation"]["new_index"] == 1
    end

    @testset "fold_cell sets code_folded and persists the file marker" begin
        session, nb, cells = make_session_with_notebook("md\"# Title\"", "x = 1")
        @test cells[1].code_folded == false
        receipt = EndeavorRuntime.call_tool(session, "fold_cell", Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "folded"      => true,
        ))
        @test receipt["applied"] == true
        @test receipt["mutation"]["type"] == "fold_cell"
        @test receipt["mutation"]["folded"] == true
        @test receipt["execution"]["status"] == "completed"
        @test cells[1].code_folded == true
        saved = read(nb.path, String)
        @test occursin(Pluto._order_delimiter_folded * string(cells[1].cell_id), saved)
        @test occursin(Pluto._order_delimiter * string(cells[2].cell_id), saved)

        EndeavorRuntime.tool_fold_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "folded"      => false,
        ))
        @test cells[1].code_folded == false
        @test occursin(Pluto._order_delimiter * string(cells[1].cell_id), read(nb.path, String))
    end

    @testset "fold_cell rejects unknown cell_id and non-boolean folded" begin
        session, nb, cells = make_session_with_notebook("md\"hi\"")
        @test_throws Exception EndeavorRuntime.tool_fold_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => "00000000-0000-0000-0000-000000000000",
            "folded"      => true,
        ))
        @test_throws ArgumentError EndeavorRuntime.tool_fold_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "folded"      => "true",
        ))
        @test_throws ArgumentError EndeavorRuntime.tool_fold_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "folded"      => 1,
        ))
        @test_throws ArgumentError EndeavorRuntime.tool_fold_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "folded"      => nothing,
        ))
        @test cells[1].code_folded == false
    end

    @testset "read_cell reports code_folded" begin
        session, nb, cells = make_session_with_notebook("md\"hi\"")
        r = EndeavorRuntime.tool_read_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id), "cell_id" => string(cells[1].cell_id)))
        @test r["code_folded"] == false
        cells[1].code_folded = true
        r = EndeavorRuntime.tool_read_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id), "cell_id" => string(cells[1].cell_id)))
        @test r["code_folded"] == true
    end

    @testset "add_cell folded=true hides the new cell's code" begin
        session, nb, cells = make_session_with_notebook("x = 1")
        read_cells!(session, nb, cells[1])
        receipt = EndeavorRuntime.tool_add_cell(session, Dict(
            "notebook_id"   => string(nb.notebook_id),
            "code"          => "md\"## Section\"",
            "after_cell_id" => string(cells[1].cell_id),
            "folded"        => true,
        ))
        new_cell = nb.cells_dict[UUID(receipt["cell_id"])]
        @test new_cell.code_folded == true
        @test receipt["code_folded"] == true
        @test occursin(Pluto._order_delimiter_folded * string(new_cell.cell_id), read(nb.path, String))
    end

    @testset "add_cell default leaves code_folded false" begin
        session, nb, cells = make_session_with_notebook("x = 1")
        read_cells!(session, nb, cells[1])
        receipt = EndeavorRuntime.tool_add_cell(session, Dict(
            "notebook_id"   => string(nb.notebook_id),
            "code"          => "md\"## Section\"",
            "after_cell_id" => string(cells[1].cell_id),
        ))
        new_cell = nb.cells_dict[UUID(receipt["cell_id"])]
        @test new_cell.code_folded == false
        @test receipt["code_folded"] == false
        @test occursin(Pluto._order_delimiter * string(new_cell.cell_id), read(nb.path, String))

        # A non-boolean is rejected before any cell is created.
        n_before = length(nb.cell_order)
        @test_throws ArgumentError EndeavorRuntime.tool_add_cell(session, Dict(
            "notebook_id"   => string(nb.notebook_id),
            "code"          => "z = 3",
            "after_cell_id" => string(cells[1].cell_id),
            "folded"        => "true",
        ))
        @test length(nb.cell_order) == n_before
    end

    @testset "folded non-markdown cell stays in read_notebook_code" begin
        session, nb, cells = make_session_with_notebook("mdl = 1", "md\"# heading\"")
        Pluto.update_dependency_cache!(nb)
        cells[1].code_folded = true
        cells[2].code_folded = true
        result = EndeavorRuntime.tool_read_notebook_code(session,
            Dict("notebook_id" => string(nb.notebook_id)))
        @test string(cells[1].cell_id) ∈ result["cell_ids"]
        @test occursin("mdl = 1", result["code"])
        @test !(string(cells[2].cell_id) ∈ result["cell_ids"])
    end

    @testset "view_cell_output renders the cell's value as PNG" begin
        two_formats = """
        begin
            struct TwoFormats end
            Base.show(io::IO, ::MIME"image/svg+xml", ::TwoFormats) = print(io, "<svg xmlns='http://www.w3.org/2000/svg'/>")
            Base.show(io::IO, ::MIME"image/png", ::TwoFormats) = write(io, UInt8[0x89, 0x50, 0x4e, 0x47])
            TwoFormats()
        end
        """
        session, nb, cells = make_session_with_notebook(two_formats, "1 + 1", "md\"# hi\"")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        args(c) = Dict("notebook_id" => string(nb.notebook_id), "cell_id" => string(c.cell_id))

        @test cells[1].output.mime == MIME("image/svg+xml")      # Pluto shows the SVG...
        img = EndeavorRuntime.tool_view_cell_output(session, args(cells[1]))
        @test img.png == UInt8[0x89, 0x50, 0x4e, 0x47]            # ...the tool returns the PNG
        @test occursin("view_cell_output", EndeavorRuntime.tool_read_cell(session, args(cells[1]))["output"])
        @test_throws ArgumentError EndeavorRuntime.tool_view_cell_output(session, args(cells[2]))  # no PNG form

        # Markdown is text/html with no PNG form: read_cell must not point at the tool.
        @test cells[3].output.mime == MIME("text/html")
        @test !occursin("view_cell_output", EndeavorRuntime.tool_read_cell(session, args(cells[3]))["output"])
        err = try EndeavorRuntime.tool_view_cell_output(session, args(cells[3])); "" catch e; e.msg end
        @test startswith(err, "no_image::") && occursin("no PNG rendering", err)

        result = EndeavorRuntime._handle_tool_call(session, "view_cell_output", args(cells[1]))
        @test result["content"][2]["type"] == "image"
        @test result["content"][2]["mimeType"] == "image/png"
        @test base64decode(result["content"][2]["data"]) == img.png
    end

    @testset "view_cell_output without a worker" begin
        session, nb, cells = make_session_with_notebook("plot", "fig")   # never run: no workspace
        args(c) = Dict("notebook_id" => string(nb.notebook_id), "cell_id" => string(c.cell_id))
        png = UInt8[0x89, 0x50, 0x4e, 0x47]
        cells[1].output = Pluto.CellOutput(; body=png, mime=MIME("image/png"))
        cells[2].output = Pluto.CellOutput(; body="<svg/>", mime=MIME("image/svg+xml"))

        @test EndeavorRuntime.tool_view_cell_output(session, args(cells[1])).png == png  # PNG passes through
        err = try EndeavorRuntime.tool_view_cell_output(session, args(cells[2])); "" catch e; e.msg end
        @test startswith(err, "no_image::") && occursin("safe preview", err)     # SVG needs the worker
    end

    @testset "execute_cell receipt has execution status" begin
        session, nb, cells = make_session_with_notebook("x = 1")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)

        receipt = EndeavorRuntime.tool_execute_cell(session, Dict(
            "notebook_id"         => string(nb.notebook_id),
            "cell_id"             => string(cells[1].cell_id),
            "wait_for_completion" => true,
        ))
        @test receipt["applied"] == true
        @test receipt["execution"]["status"] == "completed"
        @test string(cells[1].cell_id) ∈ receipt["affected_cells"]
    end

    @testset "execute_cell default is non-blocking" begin
        session, nb, cells = make_session_with_notebook("x = 1")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)

        receipt = EndeavorRuntime.tool_execute_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
        ))
        @test receipt["applied"] == true
        @test receipt["execution"]["status"] == "running"
        @test any(startswith(w, "async_execution::") for w in receipt["warnings"])
    end

    @testset "submit_changes default is non-blocking" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)

        read_cells!(session, nb, cells[1])
        EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 10",
        ))

        receipt = EndeavorRuntime.tool_submit_changes(session, Dict(
            "notebook_id" => string(nb.notebook_id),
        ))
        @test receipt["applied"] == true
        @test receipt["execution"]["status"] == "running"
        @test any(startswith(w, "async_execution::") for w in receipt["warnings"])
    end

    @testset "read_notebook_code execution order" begin
        fixture = fresh_fixture()
        session = Pluto.ServerSession()
        nb      = Pluto.load_notebook_nobackup(fixture)
        session.notebooks[nb.notebook_id] = nb
        Pluto.update_dependency_cache!(nb)

        result = EndeavorRuntime.tool_read_notebook_code(session,
            Dict("notebook_id" => string(nb.notebook_id)))

        @test result["order"] == "execution"
        @test result["cell_ids"] == [
            "11111111-1111-1111-1111-111111111111",
            "22222222-2222-2222-2222-222222222222",
        ]
        @test occursin("# ╔═╡ 11111111-1111-1111-1111-111111111111", result["code"])
        @test occursin("x = 6", result["code"])
        @test occursin("y = x * 7", result["code"])
    end

    @testset "read_notebook_code empty cell" begin
        session, nb, cells = make_session_with_notebook("x = 1", "")
        Pluto.update_dependency_cache!(nb)

        result = EndeavorRuntime.tool_read_notebook_code(session,
            Dict("notebook_id" => string(nb.notebook_id)))

        @test string(cells[2].cell_id) ∈ result["cell_ids"]
        @test occursin("# ╔═╡ $(cells[2].cell_id)", result["code"])
        @test occursin("# (empty)", result["code"])
    end

    @testset "get_cell_order vs get_execution_order" begin
        cell_z = Pluto.Cell(; cell_id=UUID("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"), code="z = 10")
        cell_w = Pluto.Cell(; cell_id=UUID("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"), code="w = z + 1")
        session = Pluto.ServerSession()
        nb      = Pluto.Notebook([cell_z, cell_w], tempname() * ".jl")
        nb.cell_order = [cell_w.cell_id, cell_z.cell_id]
        session.notebooks[nb.notebook_id] = nb
        Pluto.update_dependency_cache!(nb)

        visual = EndeavorRuntime.tool_get_cell_order(session,
            Dict("notebook_id" => string(nb.notebook_id)))
        exec = EndeavorRuntime.tool_get_execution_order(session,
            Dict("notebook_id" => string(nb.notebook_id)))

        @test visual["cell_ids"] == [
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        ]
        @test exec["cell_ids"] == [
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
        ]
    end

    @testset "MCP protocol: tools/list projection tools" begin
        session, _, _ = make_session_with_notebook("x = 1")

        buf_in  = IOBuffer()
        buf_out = IOBuffer()

        write_msg(buf_in, Dict("jsonrpc" => "2.0", "id" => 5, "method" => "tools/list", "params" => Dict()))
        seekstart(buf_in)

        EndeavorRuntime.run_mcp_server(session, buf_in, buf_out)

        resp  = read_resp(buf_out)
        names = [t["name"] for t in resp["result"]["tools"]]

        @test "read_notebook_code"  ∈ names
        @test "get_cell_order"      ∈ names
        @test "get_execution_order" ∈ names
    end

    @testset "graph tools on reactive chain" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x * 7")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)

        cell_x, cell_y = cells[1], cells[2]

        deps = EndeavorRuntime.tool_get_cell_dependencies(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cell_y.cell_id),
        ))
        @test string(cell_x.cell_id) ∈ deps["upstream"]
        @test "x" ∈ deps["symbols"]

        upstream_x = EndeavorRuntime.tool_get_cell_dependencies(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cell_x.cell_id),
        ))
        @test isempty(upstream_x["upstream"])

        dependents = EndeavorRuntime.tool_get_cell_dependents(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cell_x.cell_id),
        ))
        @test dependents["downstream"] == [string(cell_y.cell_id)]

        leaf_dependents = EndeavorRuntime.tool_get_cell_dependents(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cell_y.cell_id),
        ))
        @test isempty(leaf_dependents["downstream"])
    end

    @testset "run_preview names targets and counts dependents" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x * 7", "z = y + 1")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        id = string(nb.notebook_id)
        p = EndeavorRuntime.run_preview(session, "execute_cell", Dict("notebook_id" => id, "cell_id" => string(cells[1].cell_id)))
        @test p["cells"][1]["name"] == "x"
        @test (p["count"], p["dependents"], p["all"]) == (1, 2, false)
        p = EndeavorRuntime.run_preview(session, "submit_changes", Dict("notebook_id" => id, "cell_ids" => [string(cells[2].cell_id)]))
        @test (p["cells"][1]["name"], p["dependents"]) == ("y", 1)
        p = EndeavorRuntime.run_preview(session, "run_all_cells", Dict("notebook_id" => id))
        @test (p["all"], p["count"], p["dependents"]) == (true, 3, 0)
        p = EndeavorRuntime.run_preview(session, "allow_execution", Dict("notebook_id" => id, "run_notebook" => false))
        @test (p["all"], p["count"]) == (false, 0)
    end

    @testset "find_symbol_definitions and references" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x * 7")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)

        defs_x = EndeavorRuntime.tool_find_symbol_definitions(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "symbol"      => "x",
        ))
        @test length(defs_x) == 1
        @test defs_x[1]["cell_id"] == string(cells[1].cell_id)
        @test defs_x[1]["line_hint"] == 1

        refs_x = EndeavorRuntime.tool_find_symbol_references(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "symbol"      => "x",
        ))
        ref_ids = [r["cell_id"] for r in refs_x]
        @test string(cells[2].cell_id) ∈ ref_ids
        @test string(cells[1].cell_id) ∉ ref_ids

        defs_y = EndeavorRuntime.tool_find_symbol_definitions(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "symbol"      => "y",
        ))
        @test length(defs_y) == 1
        @test defs_y[1]["cell_id"] == string(cells[2].cell_id)
    end

    @testset "validate_cell rejects multi-expression" begin
        session, nb, cells = make_session_with_notebook("x = 1")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)

        result = EndeavorRuntime.tool_validate_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "a = 1\nb = 2",
        ))
        @test result["valid"] == false
        @test any(e -> e["type"] == "pluto_multi_expression", result["errors"])

        ok = EndeavorRuntime.tool_validate_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 42",
        ))
        @test ok["valid"] == true
        @test isempty(ok["errors"])
    end

    @testset "search_code finds text symbol tools miss" begin
        session, nb, cells = make_session_with_notebook(
            "x = 1",
            "# comment mentions x but does not reference it",
            "y = x * 7",
        )
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)

        hits = EndeavorRuntime.tool_search_code(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "query"       => "mentions x",
        ))
        @test length(hits) == 1
        @test hits[1]["cell_id"] == string(cells[2].cell_id)

        refs_x = EndeavorRuntime.tool_find_symbol_references(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "symbol"      => "x",
        ))
        ref_ids = Set(r["cell_id"] for r in refs_x)
        @test string(cells[2].cell_id) ∉ ref_ids
    end

    @testset "MCP protocol: tools/list graph tools" begin
        session, _, _ = make_session_with_notebook("x = 1")

        buf_in  = IOBuffer()
        buf_out = IOBuffer()

        write_msg(buf_in, Dict("jsonrpc" => "2.0", "id" => 6, "method" => "tools/list", "params" => Dict()))
        seekstart(buf_in)

        EndeavorRuntime.run_mcp_server(session, buf_in, buf_out)

        resp  = read_resp(buf_out)
        names = [t["name"] for t in resp["result"]["tools"]]

        @test "get_cell_dependencies"    ∈ names
        @test "get_cell_dependents"      ∈ names
        @test "find_symbol_definitions"  ∈ names
        @test "find_symbol_references"   ∈ names
        @test "validate_cell"            ∈ names
        @test "search_code"              ∈ names
    end

    @testset "read_notebook_code excludes manifest cells" begin
        fixture = fresh_fixture()
        session = Pluto.ServerSession()
        nb      = Pluto.load_notebook_nobackup(fixture)
        session.notebooks[nb.notebook_id] = nb
        Pluto.update_dependency_cache!(nb)

        result = EndeavorRuntime.tool_read_notebook_code(session,
            Dict("notebook_id" => string(nb.notebook_id)))

        @test !occursin("PLUTO_PROJECT_TOML_CONTENTS", result["code"])
        @test !occursin("PLUTO_MANIFEST_TOML_CONTENTS", result["code"])
        @test !("00000000-0000-0000-0000-000000000001" in result["cell_ids"])
    end

    @testset "add_cell records read receipt for immediate edit" begin
        session, nb, cells = make_session_with_notebook("x = 1")
        read_cells!(session, nb, cells[1])
        added = EndeavorRuntime.tool_add_cell(session, Dict(
            "notebook_id"   => string(nb.notebook_id),
            "code"          => "y = 2",
            "after_cell_id" => string(cells[1].cell_id),
        ))
        receipt = EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => added["cell_id"],
            "code"        => "y = 3",
        ))
        @test receipt["applied"] == true
    end

    @testset "run_all_cells clears pending_run" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x + 1")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        read_cells!(session, nb, cells[1])
        EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 10",
        ))
        @test !isempty(EndeavorRuntime.pending_run_ids(nb.notebook_id))

        receipt = EndeavorRuntime.tool_run_all_cells(session, Dict(
            "notebook_id"       => string(nb.notebook_id),
            "wait_for_completion" => true,
        ))
        @test receipt["mutation"]["type"] == "run_all_cells"
        @test isempty(receipt["pending_run"])
    end

    @testset "run_all_cells async reports running when cells dispatched" begin
        session, nb, _ = make_session_with_notebook("x = 1", "y = x + 1")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        receipt = EndeavorRuntime.tool_run_all_cells(session, Dict(
            "notebook_id"         => string(nb.notebook_id),
            "wait_for_completion" => false,
        ))
        @test length(receipt["affected_cells"]) == 2
        @test receipt["execution"]["status"] == "running"
    end

    @testset "async run marks cells queued before returning" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x + 1")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        read_cells!(session, nb, cells[1])
        EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 10",
        ))
        receipt = EndeavorRuntime.tool_submit_changes(session, Dict(
            "notebook_id"         => string(nb.notebook_id),
            "wait_for_completion" => false,
        ))
        # Pluto flips `queued` only inside its async run task, so the tool must
        # pre-mark the cell or the pending_run waiter can clear it before any run.
        @test cells[1].queued || cells[1].running
        @test string(cells[1].cell_id) ∈ receipt["pending_run"]
        EndeavorRuntime._wait_cells!(cells)
        sleep(0.2)
        @test isempty(EndeavorRuntime.pending_run_ids(nb.notebook_id))
        @test EndeavorRuntime._serialize_output(cells[2]) == "11"
    end

    @testset "safe preview run keeps pending_run" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x + 1")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        nb.process_status = Pluto.ProcessStatus.waiting_for_permission
        read_cells!(session, nb, cells[1])
        EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 10",
        ))
        receipt = EndeavorRuntime.tool_submit_changes(session, Dict(
            "notebook_id"         => string(nb.notebook_id),
            "wait_for_completion" => true,
        ))
        @test string(cells[1].cell_id) ∈ receipt["pending_run"]
        @test any(startswith(w, "execution_blocked::") for w in receipt["warnings"])
        @test any(occursin("allow_execution", w) for w in receipt["warnings"])
        # Nothing ran: the receipt must not report completion or pre-edit outputs.
        @test receipt["execution"]["status"] == "blocked"
        @test isempty(receipt["outputs"]["changed"])
        @test EndeavorRuntime._serialize_output(cells[2]) == "2"
    end

    @testset "run_all_cells in safe preview keeps pending_run" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = x + 1")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        nb.process_status = Pluto.ProcessStatus.waiting_for_permission
        read_cells!(session, nb, cells[1])
        EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 10",
        ))
        receipt = EndeavorRuntime.tool_run_all_cells(session, Dict(
            "notebook_id"         => string(nb.notebook_id),
            "wait_for_completion" => true,
        ))
        @test receipt["mutation"]["type"] == "run_all_cells"
        @test string(cells[1].cell_id) ∈ receipt["pending_run"]
        @test any(startswith(w, "execution_blocked::") for w in receipt["warnings"])
        @test receipt["execution"]["status"] == "blocked"
        @test isempty(receipt["outputs"]["changed"])
        @test !cells[1].queued && !cells[2].queued
        @test EndeavorRuntime._serialize_output(cells[2]) == "2"
    end

    @testset "run_all_cells clears orphan pending ids" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = 2")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        read_cells!(session, nb, cells[1])
        EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 10",
        ))
        @test cells[1].cell_id ∈ EndeavorRuntime.pending_run_ids(nb.notebook_id)
        # Remove the staged cell the way Pluto's file hot-reload and the browser
        # delete do: straight out of cells_dict / cell_order, bypassing delete_cell.
        delete!(nb.cells_dict, cells[1].cell_id)
        nb.cell_order = filter(!=(cells[1].cell_id), nb.cell_order)

        receipt = EndeavorRuntime.tool_run_all_cells(session, Dict(
            "notebook_id"         => string(nb.notebook_id),
            "wait_for_completion" => true,
        ))
        @test isempty(receipt["pending_run"])
        @test isempty(EndeavorRuntime.pending_run_ids(nb.notebook_id))
        receipt = EndeavorRuntime.tool_submit_changes(session, Dict(
            "notebook_id" => string(nb.notebook_id),
        ))
        @test receipt["applied"] == true
        @test isempty(receipt["pending_run"])
    end

    @testset "submit_changes prunes orphan pending ids instead of throwing" begin
        session, nb, cells = make_session_with_notebook("x = 1", "y = 2")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        read_cells!(session, nb, cells[1], cells[2])
        EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[1].cell_id),
            "code"        => "x = 10",
        ))
        EndeavorRuntime.tool_edit_cell(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_id"     => string(cells[2].cell_id),
            "code"        => "y = 20",
        ))
        delete!(nb.cells_dict, cells[1].cell_id)
        nb.cell_order = filter(!=(cells[1].cell_id), nb.cell_order)

        receipt = EndeavorRuntime.tool_submit_changes(session, Dict(
            "notebook_id"         => string(nb.notebook_id),
            "wait_for_completion" => true,
        ))
        @test receipt["applied"] == true
        # The surviving staged cell still runs; the ghost id is dropped, not run.
        @test receipt["affected_cells"] == [string(cells[2].cell_id)]
        @test isempty(receipt["pending_run"])
        @test EndeavorRuntime._serialize_output(cells[2]) == "20"
        # An explicitly named ghost id is still an error, not silently ignored.
        @test_throws ArgumentError EndeavorRuntime.tool_submit_changes(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cell_ids"    => [string(cells[1].cell_id)],
        ))
    end

    @testset "edit_cells is atomic on read guard failure" begin
        session, nb, cells = make_session_with_notebook("a = 1", "b = 2")
        read_cells!(session, nb, cells[1])
        @test_throws Exception EndeavorRuntime.tool_edit_cells(session, Dict(
            "notebook_id" => string(nb.notebook_id),
            "cells"       => [
                Dict("cell_id" => string(cells[1].cell_id), "code" => "a = 10"),
                Dict("cell_id" => string(cells[2].cell_id), "code" => "b = 20"),
            ],
        ))
        @test cells[1].code == "a = 1"
        @test isempty(EndeavorRuntime.pending_run_ids(nb.notebook_id))
    end

    # ---------------------------------------------------------------------------
    # D15 lifecycle — deferred standalone session
    # ---------------------------------------------------------------------------

    @testset "lifecycle: pluto_session_status when stopped" begin
        EndeavorRuntime.stop_pluto_stack!()
        status = EndeavorRuntime.tool_pluto_session_status(Dict{String,Any}())
        @test status["pluto"] == "stopped"
        @test status["pluto_port"] == 1234
        @test isempty(status["notebooks"])
    end

    @testset "lifecycle: open_notebook loads file without run" begin
        EndeavorRuntime.stop_pluto_stack!()
        fixture = fresh_fixture()
        session = Pluto.ServerSession()
        EndeavorRuntime.bind_standalone_session!(session)
        try
            result = EndeavorRuntime.tool_open_notebook(Dict(
                "path"         => fixture,
                "run_notebook" => false,
            ))
            @test isfile(fixture)
            @test haskey(result, "notebook_id")
            @test result["path"] == abspath(fixture)
            @test result["execution_allowed"] == false
            @test result["ran"] == false
            @test haskey(session.notebooks, UUID(result["notebook_id"]))
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "lifecycle: open_notebook file_not_found" begin
        EndeavorRuntime.stop_pluto_stack!()
        session = Pluto.ServerSession()
        EndeavorRuntime.bind_standalone_session!(session)
        try
            @test_throws Exception EndeavorRuntime.tool_open_notebook(Dict("path" => "/no/such/notebook.jl"))
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "lifecycle: new_notebook creates and loads a Pluto-written file" begin
        EndeavorRuntime.stop_pluto_stack!()
        session = Pluto.ServerSession()
        EndeavorRuntime.bind_standalone_session!(session)
        dir = mktempdir()
        try
            path = joinpath(dir, "fresh.jl")
            result = EndeavorRuntime.tool_new_notebook(Dict{String,Any}("path" => path))
            @test result["created"] == true
            @test result["path"] == path
            # Nothing to distrust in a notebook we just made: no safe preview.
            @test result["execution_allowed"] == true
            @test length(result["cell_ids"]) == 1
            @test isfile(path)
            @test startswith(read(path, String), "### A Pluto.jl notebook ###")
            @test haskey(session.notebooks, UUID(result["notebook_id"]))

            # Never clobber, and reject non-notebook paths.
            @test_throws ArgumentError EndeavorRuntime.tool_new_notebook(Dict{String,Any}("path" => path))
            @test_throws ArgumentError EndeavorRuntime.tool_new_notebook(Dict{String,Any}("path" => joinpath(dir, "x.txt")))
            @test_throws ArgumentError EndeavorRuntime.tool_new_notebook(Dict{String,Any}("path" => joinpath(dir, "missing", "y.jl")))

            default = EndeavorRuntime.tool_new_notebook(Dict{String,Any}())
            @test isfile(default["path"])
            @test haskey(session.notebooks, UUID(default["notebook_id"]))
            rm(default["path"]; force=true)
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "pending clears when the cell runs outside our tools" begin
        EndeavorRuntime.stop_pluto_stack!()
        fixture = fresh_fixture()
        pluto_port = 1550 + rand(0:99)
        mcp_port = 2750 + rand(0:99)
        EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
        try
            nid = EndeavorRuntime.tool_open_notebook(Dict("path" => fixture, "run_notebook" => true))["notebook_id"]
            sess = EndeavorRuntime.standalone_session()
            nb = sess.notebooks[UUID(nid)]
            ycell = nb.cells_dict[UUID("22222222-2222-2222-2222-222222222222")]
            deadline = time() + 60
            while (ycell.queued || ycell.running || ycell.output.last_run_timestamp == 0) && time() < deadline
                sleep(0.25)
            end
            read_cells!(sess, nb, ycell)
            EndeavorRuntime.tool_edit_cell(sess, Dict("notebook_id" => nid, "cell_id" => string(ycell.cell_id), "code" => "y = x * 8"))
            @test ycell.cell_id in EndeavorRuntime.pending_run_ids(nb.notebook_id)
            @test EndeavorRuntime.is_stale(nb.notebook_id, ycell.cell_id)

            # Run it the way Pluto's own run button does, not through our tools.
            Pluto.update_save_run!(sess, nb, [ycell]; run_async=false)
            @test isempty(EndeavorRuntime.pending_run_ids(nb.notebook_id))
            @test !EndeavorRuntime.is_stale(nb.notebook_id, ycell.cell_id)
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "lifecycle: allow_execution exits safe preview" begin
        EndeavorRuntime.stop_pluto_stack!()
        fixture = fresh_fixture()
        pluto_port = 1250 + rand(0:99)
        mcp_port = 2450 + rand(0:99)
        EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
        try
            open_result = EndeavorRuntime.tool_open_notebook(Dict(
                "path"         => fixture,
                "run_notebook" => false,
            ))
            nid = open_result["notebook_id"]
            @test open_result["execution_allowed"] == false

            # An edit staged during safe preview.
            sess = EndeavorRuntime.standalone_session()
            nb = sess.notebooks[UUID(nid)]
            ycell = nb.cells_dict[UUID("22222222-2222-2222-2222-222222222222")]
            read_cells!(sess, nb, ycell)
            EndeavorRuntime.tool_edit_cell(sess, Dict(
                "notebook_id" => nid,
                "cell_id"     => string(ycell.cell_id),
                "code"        => "y = x * 8",
            ))
            @test ycell.cell_id in EndeavorRuntime.pending_run_ids(nb.notebook_id)

            allow_result = EndeavorRuntime.tool_allow_execution(Dict(
                "notebook_id"  => nid,
                "run_notebook" => true,
            ))
            @test allow_result["execution_allowed"] == true
            @test allow_result["ran"] == true
            @test allow_result["already_allowed"] == false
            @test any(startswith(w, "async_execution::") for w in get(allow_result, "run_warnings", String[]))
            @test Pluto.will_run_code(nb)

            # The run clears it, like run_all_cells does.
            deadline = time() + 60
            while !isempty(EndeavorRuntime.pending_run_ids(nb.notebook_id)) && time() < deadline
                sleep(0.25)
            end
            @test isempty(EndeavorRuntime.pending_run_ids(nb.notebook_id))
            # Cleared after the edited cell ran, not before.
            @test !ycell.queued && !ycell.running
            @test occursin("48", repr(ycell.output.body))
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "lifecycle: allow_execution run_notebook=false exits gate without full run" begin
        EndeavorRuntime.stop_pluto_stack!()
        fixture = fresh_fixture()
        pluto_port = 1250 + rand(0:99)
        mcp_port = 2450 + rand(0:99)
        EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
        try
            open_result = EndeavorRuntime.tool_open_notebook(Dict(
                "path"         => fixture,
                "run_notebook" => false,
            ))
            nid = open_result["notebook_id"]
            allow_result = EndeavorRuntime.tool_allow_execution(Dict(
                "notebook_id"  => nid,
                "run_notebook" => false,
            ))
            @test allow_result["execution_allowed"] == true
            @test allow_result["ran"] == false
            @test allow_result["already_allowed"] == false
            sess = EndeavorRuntime.standalone_session()
            nb = sess.notebooks[UUID(nid)]
            @test nb.process_status === Pluto.ProcessStatus.ready
            @test Pluto.will_run_code(nb)
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "lifecycle: allow_execution idempotent when already allowed" begin
        EndeavorRuntime.stop_pluto_stack!()
        fixture = fresh_fixture()
        pluto_port = 1250 + rand(0:99)
        mcp_port = 2450 + rand(0:99)
        EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
        try
            open_result = EndeavorRuntime.tool_open_notebook(Dict(
                "path"         => fixture,
                "run_notebook" => true,
            ))
            nid = open_result["notebook_id"]
            sess = EndeavorRuntime.standalone_session()
            nb = sess.notebooks[UUID(nid)]
            # open_notebook(run_notebook=true) queues a non-blocking run; wait until ready.
            deadline = time() + 60.0
            while time() < deadline && nb.process_status !== Pluto.ProcessStatus.ready
                sleep(0.05)
            end
            @test nb.process_status === Pluto.ProcessStatus.ready

            again = EndeavorRuntime.tool_allow_execution(Dict(
                "notebook_id"  => nid,
                "run_notebook" => false,
            ))
            @test again["already_allowed"] == true
            @test again["execution_allowed"] == true
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "lifecycle: call_tool_with_session without Pluto" begin
        EndeavorRuntime.stop_pluto_stack!()
        @test_throws Exception EndeavorRuntime.call_tool_with_session(
            nothing, "read_cell", Dict("notebook_id" => "x", "cell_id" => "y"),
        )
    end

    @testset "MCP protocol: deferred read_cell returns pluto_not_running" begin
        EndeavorRuntime.stop_pluto_stack!()

        buf_in  = IOBuffer()
        buf_out = IOBuffer()

        write_msg(buf_in, Dict("jsonrpc" => "2.0", "id" => 7, "method" => "tools/call",
            "params" => Dict(
                "name" => "read_cell",
                "arguments" => Dict(
                    "notebook_id" => string(uuid4()),
                    "cell_id"     => string(uuid4()),
                ),
            )))
        seekstart(buf_in)

        EndeavorRuntime.run_mcp_server(nothing, buf_in, buf_out)

        resp = read_resp(buf_out)
        @test resp["result"]["isError"] == true
        err = JSON.parse(resp["result"]["content"][1]["text"])
        @test err["error"] == "pluto_not_running"
    end

    @testset "lifecycle: stop releases HTTP and Pluto ports" begin
        EndeavorRuntime.stop_pluto_stack!()
        pluto_port = 1250 + rand(0:99)
        mcp_port = 2450 + rand(0:99)
        port_up(url) = try
            HTTP.get(url; readtimeout=1, connect_timeout=1, status_exception=false).status == 200
        catch
            false
        end
        try
            EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
            @test EndeavorRuntime.tool_pluto_session_status(Dict{String,Any}())["pluto"] == "running"
            @test port_up("http://127.0.0.1:$mcp_port/health")
            @test port_up("http://127.0.0.1:$pluto_port/ping")
            from_page = HTTP.get("http://127.0.0.1:$mcp_port/sse";
                headers = ["Origin" => "https://evil.example"],
                status_exception = false, readtimeout = 2)
            @test from_page.status == 403
            EndeavorRuntime.stop_pluto_stack!()
            sleep(0.5)
            @test !port_up("http://127.0.0.1:$mcp_port/health")
            @test !port_up("http://127.0.0.1:$pluto_port/ping")
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "bridge requires the bearer token when one is configured" begin
        EndeavorRuntime.stop_pluto_stack!()
        pluto_port = 1350 + rand(0:99)
        mcp_port = 2550 + rand(0:99)
        EndeavorRuntime.configure_standalone!(; pluto_port, mcp_port, token="s3cret-token")
        list = JSON.json(Dict("jsonrpc" => "2.0", "id" => 1, "method" => "tools/list", "params" => Dict()))
        call(headers) = HTTP.post("http://127.0.0.1:$mcp_port/call", ["Content-Type" => "application/json", headers...], list;
            status_exception=false, readtimeout=5)
        try
            EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
            @test call([]).status == 401
            @test call(["Authorization" => "Bearer wrong-token!"]).status == 401
            @test call(["Authorization" => "Bearer s3cret-token-and-more"]).status == 401
            ok = call(["Authorization" => "Bearer s3cret-token"])
            @test ok.status == 200
            @test !isempty(JSON.parse(String(ok.body))["result"]["tools"])
            sse = HTTP.get("http://127.0.0.1:$mcp_port/sse"; status_exception=false, readtimeout=2)
            @test sse.status == 401
            @test HTTP.get("http://127.0.0.1:$mcp_port/health"; status_exception=false, readtimeout=2).status == 200
        finally
            EndeavorRuntime.stop_pluto_stack!()
            EndeavorRuntime.configure_standalone!(; token="")
        end
    end

    @testset "events stream pushes notebook changes" begin
        EndeavorRuntime.stop_pluto_stack!()
        pluto_port = 1450 + rand(0:99)
        mcp_port = 2650 + rand(0:99)
        EndeavorRuntime.configure_standalone!(; pluto_port, mcp_port)
        fixture = fresh_fixture()
        try
            EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
            sock = Sockets.connect("127.0.0.1", mcp_port)
            write(sock, "GET /events HTTP/1.0\r\nHost: 127.0.0.1:$mcp_port\r\n\r\n")
            events = Channel{String}(Inf)
            reader = @async for line in eachline(sock)
                startswith(line, "data: ") && put!(events, line[7:end])
            end
            next_event() = (timedwait(() -> isready(events), 30.0) == :ok ? take!(events) : error("no event"))
            @test JSON.parse(next_event())["notebooks"] == []   # the current state, on connect
            call = JSON.json(Dict("jsonrpc" => "2.0", "id" => 1, "method" => "tools/call",
                "params" => Dict("name" => "open_notebook", "arguments" => Dict("path" => fixture, "run_notebook" => false))))
            HTTP.post("http://127.0.0.1:$mcp_port/call", ["Content-Type" => "application/json"], call; readtimeout=30)
            event = JSON.parse(next_event())
            opened = event["notebooks"]
            @test length(opened) == 1 && opened[1]["path"] == abspath(fixture)
            nid = opened[1]["notebook_id"]
            cells = event["cells"][nid]
            @test [c["cell_id"] for c in cells] == [string(id) for id in EndeavorRuntime.standalone_session().notebooks[UUID(nid)].cell_order]
            @test all(c -> c["author"] === nothing && !c["unrun"], cells)

            # An agent edit: unrun, authored by the agent.
            sess = EndeavorRuntime.standalone_session()
            nb = sess.notebooks[UUID(nid)]
            ycell = nb.cells_dict[UUID("22222222-2222-2222-2222-222222222222")]
            read_cells!(sess, nb, ycell)
            original = ycell.code
            EndeavorRuntime.tool_edit_cell(sess, Dict("notebook_id" => nid, "cell_id" => string(ycell.cell_id), "code" => "y = x * 8"))
            state(ev) = only(filter(c -> c["cell_id"] == string(ycell.cell_id), ev["cells"][nid]))
            ev = JSON.parse(next_event())
            while !(state(ev)["unrun"]) ; ev = JSON.parse(next_event()) ; end
            @test state(ev)["author"] == "agent"
            # The code it replaced, for the in-editor diff; a later edit keeps the first before-text.
            @test state(ev)["before"] == original
            @test state(ev)["version"] == string(hash("y = x * 8"); base=16)
            EndeavorRuntime.note_agent_edit!(nb.notebook_id, ycell, "y = x * 8")
            @test EndeavorRuntime._before!(nb.notebook_id, ycell, true) == original
            # Once the cell runs it's forgotten.
            @test EndeavorRuntime._before!(nb.notebook_id, ycell, false) === nothing
            @test EndeavorRuntime._before!(nb.notebook_id, ycell, true) === nothing

            # An edit that runs straight away is the agent's too.
            read_cells!(sess, nb, ycell)
            EndeavorRuntime.tool_edit_cell(sess, Dict("notebook_id" => nid, "cell_id" => string(ycell.cell_id), "code" => "y = x * 10", "run_after" => true))
            EndeavorRuntime.publish_notebooks!()
            @test EndeavorRuntime._author!(nb.notebook_id, ycell) == "agent"

            # A change the tools didn't make (Pluto's editor submitting code): the user's.
            ycell.code = "y = x * 9"
            EndeavorRuntime.publish_notebooks!()
            # Earlier states (e.g. the before-text clearing after the run) may still be queued.
            ev = JSON.parse(next_event())
            while state(ev)["author"] != "user" ; ev = JSON.parse(next_event()) ; end
            @test state(ev)["author"] == "user"
            close(sock)
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "plan policy refuses writes and runs for that session only" begin
        call(name; owner) = EndeavorRuntime._dispatch_mcp(nothing, Dict{String,Any}(
            "jsonrpc" => "2.0", "id" => 1, "method" => "tools/call",
            "params" => Dict{String,Any}("name" => name, "arguments" => Dict{String,Any}())); owner)
        err(resp) = JSON.parse(resp["result"]["content"][1]["text"])["error"]
        try
            EndeavorRuntime.set_policy!("7", "plan")
            @test err(call("edit_cell"; owner="7")) == "plan_mode"
            @test err(call("run_all_cells"; owner="7")) == "plan_mode"
            @test err(call("new_notebook"; owner="7")) == "plan_mode"
            # Reads still work in plan (they fail here only because Pluto isn't running).
            @test err(call("list_notebooks"; owner="7")) != "plan_mode"
            # Another session, and the app's own calls, aren't affected.
            @test err(call("edit_cell"; owner="8")) != "plan_mode"
            @test err(call("edit_cell"; owner="")) != "plan_mode"
            EndeavorRuntime.set_policy!("7", "ask")
            @test err(call("edit_cell"; owner="7")) != "plan_mode"
        finally
            EndeavorRuntime.set_policy!("7", "ask")
        end
    end

    @testset "MCP protocol: deferred pluto_session_status" begin
        EndeavorRuntime.stop_pluto_stack!()

        buf_in  = IOBuffer()
        buf_out = IOBuffer()

        write_msg(buf_in, Dict("jsonrpc" => "2.0", "id" => 8, "method" => "tools/call",
            "params" => Dict("name" => "pluto_session_status", "arguments" => Dict{String,Any}())))
        seekstart(buf_in)

        EndeavorRuntime.run_mcp_server(nothing, buf_in, buf_out)

        resp = read_resp(buf_out)
        @test resp["result"]["isError"] == false
        status = JSON.parse(resp["result"]["content"][1]["text"])
        @test status["pluto"] == "stopped"
    end

    @testset "MCP protocol: tools/list lifecycle tools" begin
        session, _, _ = make_session_with_notebook("x = 1")

        buf_in  = IOBuffer()
        buf_out = IOBuffer()

        write_msg(buf_in, Dict("jsonrpc" => "2.0", "id" => 9, "method" => "tools/list", "params" => Dict()))
        seekstart(buf_in)

        EndeavorRuntime.run_mcp_server(session, buf_in, buf_out)

        resp  = read_resp(buf_out)
        names = [t["name"] for t in resp["result"]["tools"]]

        @test "pluto_session_status" ∈ names
        @test "open_notebook"       ∈ names
        @test "new_notebook"        ∈ names
        @test "allow_execution"     ∈ names
    end

end
