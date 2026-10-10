using EndeavorRuntime
using Pluto
using Test
using UUIDs
using JSON
using HTTP
using Sockets
using SHA
using Base64

# Notebooks made without a path would otherwise pile up in the app's own depot.
ENV["JULIA_PLUTO_NEW_NOTEBOOKS_DIR"] = mktempdir()

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

# No retries, so a port must answer as soon as start returns. The long read
# timeout is for the first request, which compiles its handler.
answers(url) = try
    HTTP.get(url, ["Connection" => "close"]; retry=false, readtimeout=60, status_exception=false).status == 200
catch
    false
end

# ---------------------------------------------------------------------------
# Unit tests — no Pluto web server required
# ---------------------------------------------------------------------------

@testset "EndeavorRuntime.jl" begin

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

    @testset "snapshot: structured errors, and the cells the tools hide" begin
        session, nb, cells = make_session_with_notebook(
            "using Plots\nplot(sin, 0, 2pi)", "md\"# heading\"", "mdl = 1", "macro bind(def, element)\nend",
        )
        Pluto.update_save_run!(session, nb, cells[1:1]; run_async=false, save=true)
        snap = EndeavorRuntime.snapshot(nb)
        errored = snap["cells"][1]
        @test errored["errored"] && errored["error"]["kind"] == "pluto_multi_expression"
        @test occursin("begin ... end", errored["output"])
        @test [(c["markdown"], c["hidden"]) for c in snap["cells"]] == [(false, false), (true, false), (false, false), (false, true)]
        @test !haskey(snap["cells"][2], "error")
        manifest = Pluto.Cell(; cell_id=UUID("00000000-0000-0000-0000-000000000001"), code="PLUTO_PROJECT_TOML_CONTENTS = \"\"")
        @test EndeavorRuntime._snapshot_cell(manifest)["hidden"]
    end

    @testset "package_step: Pluto's package step as it goes, cleaned for the tools" begin
        session, nb, _ = make_session_with_notebook("using DataFrames")
        @test EndeavorRuntime.package_step(nb) === nothing
        @test EndeavorRuntime.snapshot(nb)["packages"] === nothing

        pkg = Pluto.Status.report_business_started!(nb.status_tree, :pkg)
        pkg.started_at = time() - 75
        Pluto.Status.report_business!(() -> nothing, pkg, :resolve)
        Pluto.Status.report_business_started!(pkg, :precompile)
        nb.nbpkg_busy_packages = ["nbpkg_sync", "DataFrames"]
        nb.nbpkg_terminal_outputs["nbpkg_sync"] = "\e[1mUpdating\e[22m git-repo `https://lab:s3cret@git.example.org/Registry.git`\n===\n" *
            "Precompiling...\r\e[32m  ◐ \e[39mDataFrames\r  ✓ https://ghp_token123@github.com/x/Y.jl  \n\n"
        step = EndeavorRuntime.package_step(nb)
        @test step["step"] == "precompiling"
        @test step["packages"] == ["DataFrames"]
        @test 74 <= step["seconds"] <= 80
        @test step["last_line"] == "✓ https://github.com/x/Y.jl"
        @test !occursin("s3cret", JSON.json(step)) && !occursin("ghp_token123", JSON.json(step))
        # The same, but for the clock.
        same(other) = delete!(copy(other), "seconds") == delete!(copy(step), "seconds")
        @test same(EndeavorRuntime.snapshot(nb)["packages"])
        @test same(only(EndeavorRuntime._notebook_summaries(session))["packages"])

        # No log yet, and a step Pluto names that the runtime doesn't know.
        empty!(nb.nbpkg_terminal_outputs)
        Pluto.Status.report_business_finished!(pkg, :precompile)
        Pluto.Status.report_business_started!(pkg, :something_new)
        @test EndeavorRuntime.package_step(nb)["step"] == "something_new"
        @test EndeavorRuntime.package_step(nb)["last_line"] === nothing

        Pluto.Status.report_business_finished!(nb.status_tree, :pkg)
        @test EndeavorRuntime.package_step(nb) === nothing

        # A Pluto whose internals changed shape: no package step, not an error.
        @test EndeavorRuntime.package_step((; notebook_id = nb.notebook_id)) === nothing
    end

    @testset "graph: the packages each cell loads, as the cells are now" begin
        session, nb, cells = make_session_with_notebook("using Statistics, Dates", "import LinearAlgebra: norm", "using Dates", "x = 1")
        graph = EndeavorRuntime.graph(nb; fresh=true, packages=true)
        packages = Dict(c["cell_id"] => c["packages"] for c in graph["cells"])
        @test [packages[string(c.cell_id)] for c in cells] == [["Dates", "Statistics"], ["LinearAlgebra"], ["Dates"], String[]]
        @test !haskey(only(EndeavorRuntime.graph(make_session_with_notebook("x = 1")[2])["cells"]), "packages")
    end

    @testset "render_png renders the cell's value as PNG" begin
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
        @test cells[1].output.mime == MIME("image/svg+xml")      # Pluto shows the SVG...
        rendered = EndeavorRuntime.render_png(session, nb, cells[1])
        @test (base64decode(rendered["png"]), rendered["mime"]) == (UInt8[0x89, 0x50, 0x4e, 0x47], "image/svg+xml")  # ...the PNG comes back
        @test occursin("view_cell_output", EndeavorRuntime._serialize_output(cells[1]))
        @test EndeavorRuntime.render_png(session, nb, cells[2]) == Dict("png" => nothing, "mime" => "text/plain")
        # Markdown is text/html with no PNG form: its output must not point at the tool.
        @test cells[3].output.mime == MIME("text/html")
        @test !occursin("view_cell_output", EndeavorRuntime._serialize_output(cells[3]))
        @test EndeavorRuntime.render_png(session, nb, cells[3])["png"] === nothing
    end

    @testset "render_png without a worker" begin
        session, nb, cells = make_session_with_notebook("plot", "fig")   # never run: no workspace
        png = UInt8[0x89, 0x50, 0x4e, 0x47]
        cells[1].output = Pluto.CellOutput(; body=png, mime=MIME("image/png"))
        cells[2].output = Pluto.CellOutput(; body="<svg/>", mime=MIME("image/svg+xml"))
        @test base64decode(EndeavorRuntime.render_png(session, nb, cells[1])["png"]) == png   # a PNG passes through
        err = try EndeavorRuntime.render_png(session, nb, cells[2]); "" catch e; e.msg end
        @test startswith(err, "no_image::") && occursin("safe preview", err)                   # an SVG needs the worker
    end

    @testset "render_text shows rich outputs as text" begin
        session, nb, cells = make_session_with_notebook(
            "Dict(\"mean\" => 2.5)",
            "md\"# Rates\"",
            "HTML(\"<table><tr><th>n</th><th>mean</th></tr><tr><td>3</td><td>2.5 &amp; up</td></tr></table>\")",
            "1 + 1",
        )
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        text(c) = EndeavorRuntime.render_text(session, nb, c)["text"]
        @test cells[1].output.mime != MIME("text/plain")
        @test text(cells[1]) == "Dict{String, Float64} with 1 entry:\n  \"mean\" => 2.5"
        @test strip(text(cells[2])) == "Rates\n  ≡≡≡≡≡"
        @test text(cells[3]) == "n\tmean\n3\t2.5 & up"      # HTML(...) has no text form of its own
        @test text(cells[4]) === nothing                       # already text
    end

    @testset "render_text without a worker strips stored HTML" begin
        session, nb, cells = make_session_with_notebook("t", "x")
        cells[1].output = Pluto.CellOutput(; body="<p>a<br>b</p><script>x()</script>", mime=MIME("text/html"))
        cells[2].output = Pluto.CellOutput(; body="<svg/>", mime=MIME("image/svg+xml"))
        @test EndeavorRuntime.render_text(session, nb, cells[1])["text"] == "a\nb"
        @test EndeavorRuntime.render_text(session, nb, cells[2])["text"] === nothing
        cells[2].running = true
        @test startswith(EndeavorRuntime.render_text(session, nb, cells[2])["text"], "(cells are running")
        @test length(EndeavorRuntime._cap_text("é" ^ 20_000)) < 9_000
    end

    @testset "validation parses as Pluto does" begin
        session, nb, cells = make_session_with_notebook("x = 1")
        errors = EndeavorRuntime._parse_validation_errors(nb, cells[1], "a = 1\nb = 2")
        @test [e["type"] for e in errors] == ["pluto_multi_expression", "syntax_error"]
        @test EndeavorRuntime._parse_validation_errors(nb, cells[1], "x = 42") == []
    end

    @testset "session status when Pluto is stopped" begin
        EndeavorRuntime.stop_pluto_stack!()
        status = EndeavorRuntime.session_status_dict()
        @test (status["pluto"], status["pluto_port"], status["notebooks"]) == ("stopped", 1234, [])
        @test_throws ArgumentError EndeavorRuntime.adapter_call(nothing, "snapshot", Dict{String,Any}())
    end

    @testset "lifecycle: start serves both ports, stop frees them for a restart" begin
        EndeavorRuntime.stop_pluto_stack!()
        pluto_port = 1250 + rand(0:99)
        mcp_port = 2450 + rand(0:99)
        try
            for _ in 1:2
                EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
                @test EndeavorRuntime.session_status_dict()["pluto"] == "running"
                @test answers("http://127.0.0.1:$mcp_port/health")
                @test answers("http://127.0.0.1:$pluto_port/ping")
                EndeavorRuntime.stop_pluto_stack!()
                @test !answers("http://127.0.0.1:$mcp_port/health")
                @test !answers("http://127.0.0.1:$pluto_port/ping")
            end
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "lifecycle: a start whose bridge port is taken fails and leaves nothing running" begin
        EndeavorRuntime.stop_pluto_stack!()
        pluto_port = 1550 + rand(0:99)
        mcp_port = 2750 + rand(0:99)
        taken = listen(IPv4("127.0.0.1"), mcp_port)
        try
            err = try
                EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
                ""
            catch e
                sprint(showerror, e)
            end
            @test startswith(err, "Couldn't start the bridge on port $mcp_port: ")
            @test EndeavorRuntime.session_status_dict()["pluto"] == "stopped"
            @test !answers("http://127.0.0.1:$pluto_port/ping")
            @test (close(listen(IPv4("127.0.0.1"), pluto_port)); true)
        finally
            close(taken)
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
            # The core answers the tools; Julia only what the core passes on.
            @test JSON.parse(String(ok.body))["error"] == Dict("code" => -32601, "message" => "Method not found: tools/list")
            @test HTTP.get("http://127.0.0.1:$mcp_port/health"; status_exception=false, readtimeout=2).status == 200
            # The adapter's routes, which only the core calls.
            snapshot = JSON.json(Dict("method" => "snapshot", "params" => Dict()))
            @test HTTP.post("http://127.0.0.1:$mcp_port/adapter", [], snapshot; status_exception=false, readtimeout=5).status == 401
            @test HTTP.get("http://127.0.0.1:$mcp_port/notifications"; status_exception=false, readtimeout=5).status == 401
            adapter = HTTP.post("http://127.0.0.1:$mcp_port/adapter", ["Authorization" => "Bearer s3cret-token"], snapshot; status_exception=false, readtimeout=5)
            @test JSON.parse(String(adapter.body))["result"]["notebooks"] == []

            app_call(method, params) = HTTP.post("http://127.0.0.1:$mcp_port/call",
                ["Content-Type" => "application/json", "Authorization" => "Bearer s3cret-token"],
                JSON.json(Dict("jsonrpc" => "2.0", "id" => 5, "method" => method, "params" => params));
                status_exception=false, readtimeout=5)
            folder = realpath(mktempdir())
            @test app_call("endeavor/set_folder", Dict("path" => folder)).status == 200
            @test EndeavorRuntime.standalone_session().options.server.notebook_path_suggestion == joinpath(folder, "")

            # Shutdown answers first, then ends the process.
            shut_down = Channel{Bool}(1)
            EndeavorRuntime._SHUTDOWN[] = () -> put!(shut_down, true)
            reply = app_call("endeavor/shutdown", Dict{String,Any}())
            @test reply.status == 200
            @test JSON.parse(String(reply.body))["result"] == Dict()
            @test timedwait(() -> isready(shut_down), 5.0) == :ok
        finally
            EndeavorRuntime._SHUTDOWN[] = () -> exit(0)
            EndeavorRuntime.stop_pluto_stack!()
            EndeavorRuntime.configure_standalone!(; token="")
        end
    end

    @testset "adapter: snapshot, graph and shutdown, and notifications of Pluto's changes" begin
        EndeavorRuntime.stop_pluto_stack!()
        pluto_port = 1450 + rand(0:99)
        mcp_port = 2650 + rand(0:99)
        EndeavorRuntime.configure_standalone!(; pluto_port, mcp_port)
        fixture = fresh_fixture()
        X, Y = "11111111-1111-1111-1111-111111111111", "22222222-2222-2222-2222-222222222222"
        try
            EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
            sock = Sockets.connect("127.0.0.1", mcp_port)
            write(sock, "GET /notifications HTTP/1.0\r\nHost: 127.0.0.1:$mcp_port\r\n\r\n")
            notes = Channel{Dict{String,Any}}(Inf)
            last_seq = Ref(0)
            @async for line in eachline(sock)
                if startswith(line, "data: ")
                    note = JSON.parse(line[7:end])
                    @assert note["seq"] > last_seq[]
                    last_seq[] = note["seq"]
                    put!(notes, note)
                end
            end
            # The next notification `method` names, skipping others.
            function next_note(method; nid = nothing)
                deadline = time() + 60
                while time() < deadline
                    timedwait(() -> isready(notes), max(0.0, deadline - time())) == :ok || break
                    note = take!(notes)
                    note["method"] == method && (nid === nothing || note["params"]["notebook_id"] == nid) && return note["params"]
                end
                error("no $method notification")
            end
            adapter(method, params) = JSON.parse(String(HTTP.post("http://127.0.0.1:$mcp_port/adapter", [],
                JSON.json(Dict("method" => method, "params" => params)); readtimeout=30).body))
            sleep(0.5)   # the stream is open before anything happens

            nid = adapter("open", Dict("path" => fixture, "run" => true))["result"]["notebook_id"]
            @test next_note("notebook_opened") == Dict("notebook_id" => nid, "path" => abspath(fixture))
            @test next_note("execution_done") == Dict("notebook_id" => nid)
            seq_now = last_seq[]
            all = adapter("snapshot", Dict())["result"]
            snap = adapter("snapshot", Dict("notebook_id" => nid))["result"]
            # Numbered like the notifications.
            @test seq_now <= all["seq"] <= snap["seq"]
            @test all["notebooks"] == [delete!(copy(snap), "seq")]
            @test (snap["path"], snap["cell_order"], snap["execution_allowed"], snap["safe_preview"]) ==
                  (abspath(fixture), [X, Y], true, false)
            y = snap["cells"][2]
            @test (y["cell_id"], y["code"], y["folded"], y["running"], y["queued"], y["errored"], y["output"], y["hidden"], y["markdown"]) ==
                  (Y, "y = x * 7", false, false, false, false, "42", false, false)
            @test y["last_run"] > 0 && y["runtime"] isa Integer && !haskey(y, "error")
            graph = adapter("graph", Dict("notebook_id" => nid))["result"]
            @test graph["cells"] == [
                Dict("cell_id" => X, "definitions" => ["x"], "functions" => [], "references" => []),
                Dict("cell_id" => Y, "definitions" => ["y"], "functions" => [], "references" => ["*", "x"]),
            ]
            @test (graph["order"], graph["errable"]) == ([X, Y], [])
            edges = adapter("graph", Dict("notebook_id" => nid, "edges" => true))["result"]["cells"]
            @test [(c["upstream"], c["downstream"]) for c in edges] == [([], [Y]), ([X], [])]

            # A change through the adapter: its code in the next cell_state, the graph changed.
            applied = adapter("apply", Dict("notebook_id" => nid, "ops" => [Dict("op" => "set_code", "cell_id" => Y, "code" => "y = x * 8 + z")]))
            @test applied["result"]["seq"] > snap["seq"]
            state = next_note("cell_state")
            while only(filter(c -> c["cell_id"] == Y, state["cells"]))["code"] != "y = x * 8 + z"
                state = next_note("cell_state")
            end
            @test only(filter(c -> c["cell_id"] == Y, state["cells"])) ==
                  Dict("cell_id" => Y, "code" => "y = x * 8 + z", "running" => false, "queued" => false, "errored" => false)
            @test next_note("topology_changed") == Dict("notebook_id" => nid)
            @test adapter("graph", Dict("notebook_id" => nid))["result"]["cells"][2]["references"] == ["*", "+", "x", "z"]

            # A change the tools didn't make (Pluto's editor submitting code) reaches the core too.
            sess = EndeavorRuntime.standalone_session()
            nb = sess.notebooks[UUID(nid)]
            nb.cells_dict[UUID(X)].code = "x = 5"
            EndeavorRuntime._notify_browser(sess, nb)
            state = next_note("cell_state")
            while state["cells"][1]["code"] != "x = 5"
                state = next_note("cell_state")
            end
            @test state["notebook_id"] == nid

            # An error names the notebook the core asked about.
            missing = string(uuid4())
            @test adapter("snapshot", Dict("notebook_id" => missing)) ==
                  Dict("error" => "KeyError: key \"notebook_not_found::No notebook with id '$missing' in the current session\" not found")
            @test adapter("graph", Dict("notebook_id" => "nope")) == Dict("error" => "ArgumentError: invalid_notebook_id::Invalid notebook ID: 'nope'")
            @test adapter("render", Dict("notebook_id" => nid)) == Dict("error" => "ArgumentError: unknown_method::Unknown adapter method: 'render'")
            # Analysing the notebook as it is now, without keeping the result.
            nb.cells_dict[UUID(X)].code = "x = 5; w = 1"
            fresh = adapter("graph", Dict("notebook_id" => nid, "fresh" => true))["result"]
            @test fresh["cells"][1]["definitions"] == ["w", "x"]
            @test adapter("graph", Dict("notebook_id" => nid))["result"]["cells"][1]["definitions"] == ["x"]
            nb.cells_dict[UUID(X)].code = "x = 5"
            @test HTTP.post("http://127.0.0.1:$mcp_port/adapter", [], "{nope"; status_exception=false).status == 400

            # Shutting it down: whether it was in safe preview.
            @test adapter("shutdown", Dict("notebook_id" => nid)) == Dict("result" => Dict("safe_preview" => false))
            @test next_note("notebook_shut_down") == Dict("notebook_id" => nid)
            @test adapter("snapshot", Dict())["result"]["notebooks"] == []

            previewed = adapter("open", Dict("path" => fresh_fixture(), "run" => false))["result"]["notebook_id"]
            @test adapter("snapshot", Dict("notebook_id" => previewed))["result"]["safe_preview"]
            @test adapter("shutdown", Dict("notebook_id" => previewed)) == Dict("result" => Dict("safe_preview" => true))
            close(sock)
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "adapter: changing, running, opening and making notebooks" begin
        EndeavorRuntime.stop_pluto_stack!()
        pluto_port = 1650 + rand(0:99)
        mcp_port = 2850 + rand(0:99)
        EndeavorRuntime.configure_standalone!(; pluto_port, mcp_port)
        X, Y = "11111111-1111-1111-1111-111111111111", "22222222-2222-2222-2222-222222222222"
        try
            EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
            sock = Sockets.connect("127.0.0.1", mcp_port)
            write(sock, "GET /notifications HTTP/1.0\r\nHost: 127.0.0.1:$mcp_port\r\n\r\n")
            notes = Channel{Dict{String,Any}}(Inf)
            @async for line in eachline(sock)
                startswith(line, "data: ") && put!(notes, JSON.parse(line[7:end]))
            end
            function next_note(method; nid = nothing)
                deadline = time() + 60
                while time() < deadline
                    timedwait(() -> isready(notes), max(0.0, deadline - time())) == :ok || break
                    note = take!(notes)
                    note["method"] == method && (nid === nothing || note["params"]["notebook_id"] == nid) && return note["params"]
                end
                error("no $method notification")
            end
            adapter(method, params) = JSON.parse(String(HTTP.post("http://127.0.0.1:$mcp_port/adapter", [],
                JSON.json(Dict("method" => method, "params" => params)); readtimeout=120).body))
            function result(method, params)
                reply = adapter(method, params)
                haskey(reply, "error") && error(reply["error"])
                return reply["result"]
            end
            sleep(0.5)

            fixture = fresh_fixture()
            opened = result("open", Dict("path" => fixture, "run" => true))
            nid = opened["notebook_id"]
            @test (opened["path"], opened["process_status"]) == (abspath(fixture), "starting")
            @test next_note("execution_done"; nid) == Dict("notebook_id" => nid)
            @test adapter("open", Dict("path" => fixture, "run" => false)) == Dict("error" =>
                "ArgumentError: notebook_already_open::'$fixture' is already open as notebook_id $nid; use that id")
            nb = EndeavorRuntime.standalone_session().notebooks[UUID(nid)]
            ops(list...) = Dict("notebook_id" => nid, "ops" => collect(list))

            # A cell whose code isn't what the core expects refuses the whole change.
            @test adapter("apply", ops(
                Dict("op" => "fold", "cell_id" => X, "folded" => true),
                Dict("op" => "set_code", "cell_id" => Y, "code" => "y = 0", "expected" => "y = 1"),
            )) == Dict("error" => "ArgumentError: stale_read::Cell $Y changed since last read; call read_cell again")
            @test !nb.cells_dict[UUID(X)].code_folded

            # Set code, insert, fold: saved, analysed, and a new cell_order vector.
            order_before = nb.cell_order
            applied = result("apply", ops(
                Dict("op" => "set_code", "cell_id" => Y, "code" => "y = x * 8", "expected" => "y = x * 7"),
                Dict("op" => "insert", "code" => "z = y + 1", "folded" => true, "index" => 2),
                Dict("op" => "fold", "cell_id" => X, "folded" => true),
            ))
            z = only(applied["inserted"])
            @test nb.cell_order !== order_before
            @test string.(nb.cell_order) == [X, Y, z]
            @test nb.cells_dict[UUID(z)].code_folded && nb.cells_dict[UUID(X)].code_folded
            saved = read(fixture, String)
            @test occursin("y = x * 8", saved) && occursin(Pluto._order_delimiter_folded * X, saved)
            graph = result("graph", Dict("notebook_id" => nid, "edges" => true))
            @test [(c["cell_id"], c["upstream"], c["downstream"]) for c in graph["cells"]] == [(X, [], [Y]), (Y, [X], [z]), (z, [Y], [])]

            # Runs: waited for, then not (a notification says when it's done).
            @test result("run", Dict("notebook_id" => nid, "cells" => [Y, z], "wait" => true, "timeout" => 60)) ==
                  Dict("accepted" => true, "process_status" => "ready", "completed" => [Y, z], "timed_out" => [])
            @test nb.cells_dict[UUID(z)].output.body == "49"
            @test result("run", Dict("notebook_id" => nid, "cells" => [X], "wait" => false, "timeout" => 60)) ==
                  Dict("accepted" => true, "process_status" => "ready")
            @test next_note("run_finished"; nid) == Dict("notebook_id" => nid, "cells" => [X])

            # A waited run that outlasts `timeout` returns when it passes, with the cell that
            # finished and the dependent still running. The run goes on, watched: a
            # notification says when it ends. The dependent waits for a file the test makes.
            gate = joinpath(mktempdir(), "release")
            insert(code, index) = only(result("apply", ops(Dict("op" => "insert", "code" => code, "folded" => false, "index" => index)))["inserted"])
            w, slow = insert("w = 5", 3), insert("slow = (while !isfile($(repr(gate))); sleep(0.05); end; w)", 4)
            try
                capped = result("run", Dict("notebook_id" => nid, "cells" => [w], "wait" => true, "timeout" => 8))
                @test capped == Dict("accepted" => true, "process_status" => "ready", "completed" => [w], "timed_out" => [slow])
                @test nb.cells_dict[UUID(w)].output.body == "5"
                @test nb.cells_dict[UUID(slow)].running || nb.cells_dict[UUID(slow)].queued
            finally
                touch(gate)
            end
            @test next_note("run_finished"; nid) == Dict("notebook_id" => nid, "cells" => [w])
            @test nb.cells_dict[UUID(slow)].output.body == "5"
            @test !any(c -> c["running"] || c["queued"], result("snapshot", Dict("notebook_id" => nid))["cells"])
            result("apply", ops(Dict("op" => "delete", "cell_id" => slow), Dict("op" => "delete", "cell_id" => w)))

            # A run whose task fails with nobody waiting: the cells marked queued by hand are released
            # and the run is said to have finished.
            failing = @task error("boom")
            schedule(failing)
            try wait(failing) catch end
            marked = nb.cells_dict[UUID(X)]
            marked.queued = true
            EndeavorRuntime._watch_run!(EndeavorRuntime.standalone_session(), nb, [marked], failing)
            @test !marked.queued
            @test next_note("run_finished"; nid) == Dict("notebook_id" => nid, "cells" => [X])

            # Move and delete, then the cleanup run that follows a delete.
            result("apply", ops(Dict("op" => "move", "cell_id" => z, "index" => 0)))
            @test string.(nb.cell_order) == [z, X, Y]
            result("apply", ops(Dict("op" => "delete", "cell_id" => z)))
            @test result("run", Dict("notebook_id" => nid, "cells" => [], "wait" => false, "timeout" => 60))["accepted"]
            @test string.(nb.cell_order) == [X, Y] && !haskey(nb.cells_dict, UUID(z))

            @test result("validate", Dict("notebook_id" => nid, "cell_id" => X, "code" => "a = 1\nb = 2"))["errors"][1]["type"] == "pluto_multi_expression"
            @test result("validate", Dict("notebook_id" => nid, "cell_id" => X, "code" => "a = 1"))["errors"] == []
            @test result("render_png", Dict("notebook_id" => nid, "cell_id" => X)) == Dict("png" => nothing, "mime" => "text/plain")
            @test result("interrupt", Dict("notebook_id" => nid))["interrupted"] isa Bool
            @test result("status", Dict())["pluto"] == "running"

            # Pluto's own Restart: a new process, and every cell runs.
            ran_before = nb.cells_dict[UUID(Y)].output.last_run_timestamp
            @test result("restart", Dict("notebook_id" => nid, "timeout" => 60)) == Dict("restarted" => true)
            @test Set(next_note("run_finished"; nid)["cells"]) == Set([X, Y])
            @test nb.process_status == Pluto.ProcessStatus.ready
            @test nb.cells_dict[UUID(Y)].output.last_run_timestamp > ran_before && nb.cells_dict[UUID(Y)].output.body == "48"

            # Moving the file, as Pluto's file box does.
            renamed = joinpath(dirname(fixture), "renamed.jl")
            @test result("move", Dict("notebook_id" => nid, "path" => renamed)) == Dict("path" => renamed)
            @test isfile(renamed) && !isfile(fixture) && nb.path == renamed

            # Safe preview: runs aren't accepted until execution is allowed.
            previewed = result("open", Dict("path" => fresh_fixture(), "run" => false))["notebook_id"]
            @test adapter("restart", Dict("notebook_id" => previewed, "timeout" => 60)) ==
                  Dict("error" => "ArgumentError: execution_blocked::The notebook is in safe preview; Run notebook starts it")
            @test result("run", Dict("notebook_id" => previewed, "cells" => [X], "wait" => true, "timeout" => 60)) ==
                  Dict("accepted" => false, "process_status" => "waiting_for_permission")
            @test result("allow_execution", Dict("notebook_id" => previewed, "run" => true, "timeout" => 60)) ==
                  Dict("already_allowed" => false, "ran" => true, "process_status" => "starting")
            @test X in next_note("run_finished"; nid=previewed)["cells"]
            @test result("allow_execution", Dict("notebook_id" => previewed, "run" => false, "timeout" => 60)) ==
                  Dict("already_allowed" => true, "ran" => false, "process_status" => "ready")

            # New notebooks: at a path, or named by Pluto in a folder.
            dir = realpath(mktempdir())
            made = result("new", Dict("path" => joinpath(dir, "made.jl")))
            @test made["path"] == joinpath(dir, "made.jl") && isfile(made["path"])
            @test [c["code"] for c in made["cells"]] == [""]
            named = result("new", Dict("folder" => dir))
            @test dirname(named["path"]) == dir && endswith(named["path"], ".jl")
            close(sock)
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "adapter: a notebook's own process ending by itself" begin
        EndeavorRuntime.stop_pluto_stack!()
        pluto_port = 1850 + rand(0:99)
        mcp_port = 3050 + rand(0:99)
        EndeavorRuntime.configure_standalone!(; pluto_port, mcp_port)
        X, Y = "11111111-1111-1111-1111-111111111111", "22222222-2222-2222-2222-222222222222"
        try
            EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
            sock = Sockets.connect("127.0.0.1", mcp_port)
            write(sock, "GET /notifications HTTP/1.0\r\nHost: 127.0.0.1:$mcp_port\r\n\r\n")
            notes = Channel{Dict{String,Any}}(Inf)
            @async for line in eachline(sock)
                startswith(line, "data: ") && put!(notes, JSON.parse(line[7:end]))
            end
            exits() = [n["params"] for n in collect_notes() if n["method"] == "process_exited"]
            function collect_notes()
                got = Dict{String,Any}[]
                while isready(notes)
                    push!(got, take!(notes))
                end
                got
            end
            function next_note(method; nid = nothing)
                deadline = time() + 60
                while time() < deadline
                    timedwait(() -> isready(notes), max(0.0, deadline - time())) == :ok || break
                    note = take!(notes)
                    note["method"] == method && (nid === nothing || note["params"]["notebook_id"] == nid) && return note["params"]
                end
                error("no $method notification")
            end
            adapter(method, params) = JSON.parse(String(HTTP.post("http://127.0.0.1:$mcp_port/adapter", [],
                JSON.json(Dict("method" => method, "params" => params)); readtimeout=120).body))
            result(method, params) = (reply = adapter(method, params); haskey(reply, "error") ? error(reply["error"]) : reply["result"])
            worker_pid(nid) = Pluto.WorkspaceManager.get_workspace((EndeavorRuntime.standalone_session(), EndeavorRuntime.standalone_session().notebooks[UUID(nid)])).worker.proc_pid
            sleep(0.5)

            nid = result("open", Dict("path" => fresh_fixture(), "run" => true))["notebook_id"]
            next_note("execution_done"; nid)
            nb = EndeavorRuntime.standalone_session().notebooks[UUID(nid)]
            result("apply", Dict("notebook_id" => nid, "ops" => [Dict("op" => "set_code", "cell_id" => Y, "code" => "y = (sleep(600); x)")]))

            # Killed during a run the caller waits for: the run ends, saying which cell was running.
            waited = @async result("run", Dict("notebook_id" => nid, "cells" => [Y], "wait" => true, "timeout" => 600))
            @test timedwait(() -> nb.cells_dict[UUID(Y)].running, 60) == :ok
            collect_notes()
            run(`kill -9 $(worker_pid(nid))`)
            @test timedwait(() -> istaskdone(waited), 10) == :ok
            @test fetch(waited) == Dict("accepted" => true, "process_status" => "no_process", "completed" => [Y], "timed_out" => [], "exited" => [Y])
            @test next_note("process_exited"; nid) == Dict("notebook_id" => nid, "running" => [Y])
            snap = result("snapshot", Dict("notebook_id" => nid))
            @test (snap["process_status"], snap["execution_allowed"], snap["exited"]) == ("no_process", false, [Y])
            @test !any(c -> c["running"] || c["queued"], snap["cells"])
            @test exits() == []   # said once

            # Restart is Endeavor's own stop: a new process, and nothing said to have ended by itself.
            result("apply", Dict("notebook_id" => nid, "ops" => [Dict("op" => "set_code", "cell_id" => Y, "code" => "y = x * 7")]))
            result("restart", Dict("notebook_id" => nid, "timeout" => 60))
            next_note("run_finished"; nid)
            @test nb.process_status == Pluto.ProcessStatus.ready
            @test result("snapshot", Dict("notebook_id" => nid))["exited"] === nothing
            @test exits() == []

            # Killed while idle: noticed too, with nothing running.
            run(`kill -9 $(worker_pid(nid))`)
            @test next_note("process_exited"; nid) == Dict("notebook_id" => nid, "running" => [])
            @test result("snapshot", Dict("notebook_id" => nid))["exited"] == []
            @test result("run", Dict("notebook_id" => nid, "cells" => [X], "wait" => true, "timeout" => 60)) ==
                  Dict("accepted" => false, "process_status" => "no_process")

            # A shutdown stops the process on purpose.
            result("restart", Dict("notebook_id" => nid, "timeout" => 60))
            next_note("run_finished"; nid)
            result("shutdown", Dict("notebook_id" => nid))
            next_note("notebook_shut_down"; nid)
            sleep(2)
            @test exits() == []
            close(sock)
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

end
