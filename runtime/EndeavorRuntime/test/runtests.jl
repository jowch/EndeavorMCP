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

    @testset "_html_text: numeric entities, as Pluto's Markdown writes math" begin
        html = "<p>\$e^&#123;i\\pi&#125; &#43; 1 &#61; 0\$ &#x2014; &amp;#123; &lt;b&gt; &#0; &#99999999;</p>"
        @test EndeavorRuntime._html_text(html) == "\$e^{i\\pi} + 1 = 0\$ — &#123; <b> &#0; &#99999999;"
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

    @testset "controls: how read_cell words a widget, from its hooks" begin
        # Stand-ins for AbstractPlutoDingetjes' answers, matched by name as the runtime matches them.
        struct NotGiven end
        struct InfinitePossibilities end
        struct Slide; values::AbstractRange; end
        struct Pick; options::Vector{Pair}; end
        struct Box end
        Base.show(io::IO, ::MIME"text/plain", ::Box) = print(io, "A box over the plot")
        index = (el, i) -> el isa Slide ? el.values[i] : el isa Pick ? el.options[i].first : i
        line(el, value, default, possible) = EndeavorRuntime.__endeavor_control(:x, el, value, _ -> default, possible, index)

        slide = Slide(0.01:0.01:1.0)
        @test line(slide, Some(0.5), 0.5, 1:100)["line"] == "x: Slide 0.01 to 1.0, step 0.01, value 0.5"
        moved = line(slide, Some(0.3), 0.5, 1:100)
        @test moved["line"] == "x: Slide 0.01 to 1.0, step 0.01, value 0.3 (default 0.5)"
        @test (moved["moved"], moved["value"], moved["default"]) == (true, "0.3", "0.5")
        # Choices show their label when it differs, as the agent would otherwise guess the label.
        pick = Pick(["a" => "Apple", "b" => "b"])
        @test line(pick, Some("b"), "b", 1:2)["line"] == "x: Pick of Apple = \"a\", \"b\", value \"b\""
        many = Pick(["v$i" => "v$i" for i in 1:40])
        @test line(many, Some("v1"), "v1", 1:40)["line"] == "x: Pick 40 choices, from \"v1\" to \"v40\", value \"v1\""
        @test line(:w, Some(true), true, [false, true])["line"] == "x: Symbol, value true"
        @test line(:w, Some("hi"), "hi", InfinitePossibilities())["line"] == "x: Symbol, any text, value \"hi\""
        @test line(:w, Some(2), 2, :NotGiven)["line"] == "x: Symbol, value 2"
        @test line(:w, Some(2), 2, NotGiven())["line"] == "x: Symbol, value 2"
        # A widget with its own text form (Masque's) says what it is in its own words.
        @test line(Box(), Some(1), 1, NotGiven())["line"] == "x: Box, value 1. A box over the plot"
        # No default (a home-made `html"<input>"`): never moved. Not yet defined: not set.
        @test !line(:w, Some(3), missing, NotGiven())["moved"]
        @test line(:w, nothing, 1, NotGiven())["line"] == "x: Symbol, value not set"
        # Long values are cut.
        long = repeat("a", 200)
        @test endswith(line(:w, Some(long), long, InfinitePossibilities())["line"], "…")
    end

    @testset "controls: read_cell lists a cell's controls, and the cells that used a moved one say so" begin
        knob = """
        begin
            struct Knob; start::Int; end
            Base.get(k::Knob) = k.start
            Base.show(io::IO, ::MIME"text/html", ::Knob) = print(io, "<input type=range>")
        end
        """
        session, nb, cells = make_session_with_notebook(knob, "@bind w Knob(4)", "y = w * 2", "z = y + 1", "other = 1")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        snap() = Dict(c["cell_id"] => c for c in EndeavorRuntime.adapter_call(session, "snapshot", Dict("notebook_id" => string(nb.notebook_id)))["cells"])
        id(i) = string(cells[i].cell_id)
        moved_notes(s, i) = filter(n -> startswith(n, "uses_moved_input::"), s[id(i)]["notes"])

        s = snap()
        @test s[id(2)]["inputs"] == ["w: Knob, value 4"]
        @test !haskey(s[id(3)], "inputs") && !haskey(s[id(5)], "inputs")
        @test all(i -> isempty(moved_notes(s, i)), 1:5)

        # The user moves it on the page: the cells that read it, and theirs, say which value they used.
        nb.bonds[:w] = Pluto.BondValue(9)
        Pluto.set_bond_values_reactive(; session, notebook=nb, bound_sym_names=[:w], run_async=false)
        s = snap()
        @test s[id(2)]["inputs"] == ["w: Knob, value 9 (default 4)"]
        note = "uses_moved_input::This result used w = 9, set on the page. The file's default is 4, so a rerun of the file gives other results."
        @test moved_notes(s, 3) == [note] && moved_notes(s, 4) == [note]
        @test isempty(moved_notes(s, 2)) && isempty(moved_notes(s, 5))

        # The control's own cell running again puts it back to its default, and the notes go.
        Pluto.update_save_run!(session, nb, cells[2:2]; run_async=false, save=true)
        s = snap()
        @test s[id(2)]["inputs"] == ["w: Knob, value 4"]
        @test isempty(moved_notes(s, 3))

        # A process that doesn't answer (busy in a task that never yields, no cell running) holds
        # up only the snapshot that asked it, and that one for at most the wait.
        spin = Pluto.Cell(; code="spin = @async (sleep(1); while true end)")
        push!(nb.cells, spin)
        nb.cells_dict[spin.cell_id] = spin
        nb.cell_order = [nb.cell_order; spin.cell_id]
        Pluto.update_save_run!(session, nb, [spin]; run_async=false, save=true)
        sleep(2)
        nb.bonds[:w] = Pluto.BondValue(5)
        first_wait = @elapsed EndeavorRuntime.controls(session, nb)
        later = [@elapsed(EndeavorRuntime.controls(session, nb)) for _ in 1:3]
        @test first_wait < EndeavorRuntime._CONTROLS_WAIT + 1
        @test all(<(0.5), later)

        # A notebook with no control asks its process nothing.
        session2, nb2, _ = make_session_with_notebook("a = 1")
        Pluto.update_save_run!(session2, nb2, nb2.cells; run_async=false, save=true)
        @test EndeavorRuntime.controls(session2, nb2) == Dict()
        @test !haskey(EndeavorRuntime._CONTROLS, nb2.notebook_id)
        Pluto.SessionActions.shutdown(session, nb; async=false, verbose=false)
        Pluto.SessionActions.shutdown(session2, nb2; async=false, verbose=false)
    end

    @testset "render_png waits a while, not for the run, on a worker running cells (#62)" begin
        picture = """
        begin
            struct Picture end
            Base.show(io::IO, ::MIME"image/svg+xml", ::Picture) = print(io, "<svg xmlns='http://www.w3.org/2000/svg'/>")
            Base.show(io::IO, ::MIME"image/png", ::Picture) = write(io, UInt8[0x89, 0x50, 0x4e, 0x47])
            Picture()
        end
        """
        # A computation that never yields, so the worker answers nothing until it ends.
        long = "let t = time(); s = 0.0; while time() - t < 35; s += 1; end; s end"
        session, nb, cells = make_session_with_notebook(picture, long)
        Pluto.update_save_run!(session, nb, [cells[1]]; run_async=false, save=true)
        Pluto.update_save_run!(session, nb, [cells[2]]; run_async=true, save=true)
        timedwait(() -> cells[2].running, 30.0)
        sleep(1)   # `running` is set before the worker is inside the loop
        @test cells[2].running
        took = @elapsed err = try EndeavorRuntime.render_png(session, nb, cells[1]); "" catch e; e.msg end
        @test startswith(err, "no_image::") && occursin(EndeavorRuntime._BUSY, err)
        @test EndeavorRuntime._BUSY_RENDER_WAIT <= took < 20
        # The render that timed out still waits on the worker; asking again joins it.
        key = (nb.notebook_id, string(cells[1].cell_id))
        first = last(EndeavorRuntime._RENDERS[key])
        err = try EndeavorRuntime.render_png(session, nb, cells[1]); "" catch e; e.msg end
        @test occursin(EndeavorRuntime._BUSY, err)
        @test last(EndeavorRuntime._RENDERS[key]) === first && !istaskdone(first)
        # Once the run ends, the worker renders it again.
        timedwait(() -> !cells[2].running, 60.0)
        @test base64decode(EndeavorRuntime.render_png(session, nb, cells[1])["png"]) == UInt8[0x89, 0x50, 0x4e, 0x47]
        @test !haskey(EndeavorRuntime._RENDERS, key)   # a finished render leaves
        Pluto.SessionActions.shutdown(session, nb; async=false)
    end

    @testset "render_png without a worker" begin
        session, nb, cells = make_session_with_notebook("plot", "fig")   # never run: no workspace
        png = UInt8[0x89, 0x50, 0x4e, 0x47]
        cells[1].output = Pluto.CellOutput(; body=png, mime=MIME("image/png"))
        cells[2].output = Pluto.CellOutput(; body="<svg/>", mime=MIME("image/svg+xml"))
        @test base64decode(EndeavorRuntime.render_png(session, nb, cells[1])["png"]) == png   # a PNG passes through
        err = try EndeavorRuntime.render_png(session, nb, cells[2]); "" catch e; e.msg end
        @test startswith(err, "no_image::") && occursin("safe preview", err)                   # an SVG needs the worker
        @test occursin("has no running worker", err)    # the app's picture stands in for it (`NO_WORKER`, notebooks/tools.rs)
    end

    @testset "render_png reads a picture embedded in HTML" begin
        # A widget's static render (as Masque.jl shows a figure): HTML holding a PNG.
        png = vcat(UInt8[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], zeros(UInt8, 6_000))
        icon = vcat(UInt8[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], zeros(UInt8, 100))
        widget = "<div><img src='data:image/png;base64,$(base64encode(icon))'><img draggable=\"false\" src=\"data:image/png;base64,$(base64encode(png))\" style=\"width:100%\"><script>mount()</script></div>"
        session, nb, cells = make_session_with_notebook("HTML(widget)", "HTML(icon)", "md\"# hi\"")
        cells[1].output = Pluto.CellOutput(; body=widget, mime=MIME("text/html"))
        cells[2].output = Pluto.CellOutput(; body="<img src='data:image/png;base64,$(base64encode(icon))'> n = 3", mime=MIME("text/html"))
        cells[3].output = Pluto.CellOutput(; body="<h1>hi</h1>", mime=MIME("text/html"))
        # Without a worker (safe preview) the stored HTML is enough.
        rendered = EndeavorRuntime.render_png(session, nb, cells[1])
        @test base64decode(rendered["png"]) == png && rendered["source"] == "embedded image"
        @test occursin("view_cell_output", EndeavorRuntime._serialize_output(cells[1]))
        # An icon isn't the output's picture.
        @test !occursin("view_cell_output", EndeavorRuntime._serialize_output(cells[2]))
        @test_throws ArgumentError EndeavorRuntime.render_png(session, nb, cells[2])
        @test !occursin("view_cell_output", EndeavorRuntime._serialize_output(cells[3]))
        # A large data URI that isn't a PNG gets neither the pointer nor an image.
        cells[3].output = Pluto.CellOutput(; body="<img src='data:image/png;base64,$(base64encode(zeros(UInt8, 6_000)))'>", mime=MIME("text/html"))
        @test !occursin("view_cell_output", EndeavorRuntime._serialize_output(cells[3]))
        @test_throws ArgumentError EndeavorRuntime.render_png(session, nb, cells[3])
        # With a worker: a value with no PNG form falls back to the embedded one.
        session, nb, cells = make_session_with_notebook("HTML(\"$(escape_string(widget))\")", "md\"# hi\"")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        @test cells[1].output.mime == MIME("text/html")
        rendered = EndeavorRuntime.render_png(session, nb, cells[1])
        @test base64decode(rendered["png"]) == png && rendered["source"] == "embedded image"
        @test EndeavorRuntime.render_png(session, nb, cells[2]) == Dict("png" => nothing, "mime" => "text/html")
    end

    @testset "render_png and render_text read the widget inside an @bind" begin
        widget = """
        begin
            struct Pictured end
            Base.show(io::IO, ::MIME"text/html", ::Pictured) = print(io, "<input type=range>")
            Base.show(io::IO, ::MIME"image/png", ::Pictured) = write(io, UInt8[0x89, 0x50, 0x4e, 0x47])
            Base.show(io::IO, ::MIME"text/plain", ::Pictured) = print(io, "Pictured widget, value 3")
            struct HtmlOnly end
            Base.show(io::IO, ::MIME"text/html", ::HtmlOnly) = print(io, "<b>plain</b>")
        end
        """
        session, nb, cells = make_session_with_notebook(widget, "@bind x Pictured()", "@bind y HtmlOnly()")
        Pluto.update_save_run!(session, nb, nb.cells; run_async=false, save=true)
        @test cells[2].output.mime == MIME("text/html")
        rendered = EndeavorRuntime.render_png(session, nb, cells[2])
        @test (base64decode(rendered["png"]), rendered["mime"]) == (UInt8[0x89, 0x50, 0x4e, 0x47], "text/html")
        @test EndeavorRuntime.render_text(session, nb, cells[2])["text"] == "Pictured widget, value 3"
        # A widget with no picture or text form of its own reads as before.
        @test EndeavorRuntime.render_png(session, nb, cells[3])["png"] === nothing
        @test EndeavorRuntime.render_text(session, nb, cells[3])["text"] == "plain"
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

    # Past about 10 MB, a lazy pattern that had to cross the whole output failed every
    # snapshot, and with it every read tool, with "PCRE.exec error: match limit exceeded" (app #193).
    @testset "outputs of many megabytes: snapshot and render_text still read them" begin
        big = 12_000_000
        png = base64encode(UInt8[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, zeros(UInt8, 6_000)...])
        session, nb, cells = make_session_with_notebook("a", "b", "c", "d")
        cells[1].output = Pluto.CellOutput(; body="<img alt='x' src='data:image/jpeg;base64,$("A" ^ big)'><p>after</p>", mime=MIME("text/html"))
        cells[2].output = Pluto.CellOutput(; body="<script>$("x" ^ big)</script><STYLE>p{}</STYLE ><p>shown</p>", mime=MIME("text/html"))
        cells[3].output = Pluto.CellOutput(; body="<img src='data:image/jpeg;base64,$("A" ^ big)'><img src=\"data:image/png;base64,$png\">", mime=MIME("text/html"))
        cells[4].output = Pluto.CellOutput(; body="<p>a</p>" * "<td>1 < 2</td>" ^ 500_000 * "<script>never closed" * "y" ^ big, mime=MIME("text/html"))
        snap = EndeavorRuntime.snapshot(nb)
        @test startswith(snap["cells"][1]["output"], "[text/html output, ")
        @test !occursin("view_cell_output", snap["cells"][1]["output"])
        @test occursin("view_cell_output", snap["cells"][3]["output"])
        @test EndeavorRuntime._embedded_png_b64(cells[3].output) == png
        @test EndeavorRuntime.render_text(session, nb, cells[1])["text"] == "after"
        @test EndeavorRuntime.render_text(session, nb, cells[2])["text"] == "shown"
        text4 = EndeavorRuntime.render_text(session, nb, cells[4])["text"]
        @test startswith(text4, "a\n1 < 2\t1 < 2") && !occursin("never closed", text4)
        # A plot's PNG of megabytes whose src comes after a long attribute: the old search failed on it.
        plot = base64encode(UInt8[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, zeros(UInt8, 6_800_000)...])
        figure = Pluto.CellOutput(; body="<img data-x='$("A" ^ 10_000_000)' src=\"data:image/png;base64,$plot\">", mime=MIME("text/html"))
        @test EndeavorRuntime._embedded_png_b64(figure) == plot
    end

    # view_cell_output tells the agent it hasn't seen an interactive output work (#393).
    @testset "interactive HTML: scripts and controls, not Markdown" begin
        html(body) = Pluto.CellOutput(; body, mime=MIME("text/html"))
        @test EndeavorRuntime._interactive_html(html("<div id='p'></div><SCRIPT src='plotly.js'></SCRIPT>"))
        @test EndeavorRuntime._interactive_html(html("<bond def=\"n\"><input type=range></bond>"))
        @test EndeavorRuntime._interactive_html(html("<canvas width=300></canvas>"))
        @test !EndeavorRuntime._interactive_html(html("<div class=\"markdown\"><h1>Plot</h1><p>A <code>script</code> tag: &lt;script&gt;</p></div>"))
        @test !EndeavorRuntime._interactive_html(html("<table><tr><td>1</td></tr></table>"))
        @test !EndeavorRuntime._interactive_html(Pluto.CellOutput(; body="<script>", mime=MIME("text/plain")))
        @test EndeavorRuntime._interactive_html(html("<p>" ^ 3_000_000 * "<script>"))
    end

    @testset "saving: a notebook is written beside its file and renamed over it (#102)" begin
        @test parentmodule(which(Pluto.write_buffered, (Function, String))) === EndeavorRuntime
        dir = mktempdir()
        real = joinpath(dir, "real.jl")
        nb = Pluto.Notebook([Pluto.Cell(; code="x = 1")], real)
        Pluto.save_notebook(nb, real)
        chmod(real, 0o640)
        link = joinpath(dir, "link.jl")
        symlink(real, link)
        nb.cells[1].code = "x = 2"
        before = stat(real).inode
        Pluto.save_notebook(nb, link)
        @test stat(real).inode != before
        @test islink(link)
        @test occursin("x = 2", read(real, String))
        @test filemode(real) & 0o777 == 0o640
        @test sort(readdir(dir)) == ["link.jl", "real.jl"]
        @test Pluto.load_notebook_nobackup(real).cells[1].code == "x = 2"
        # A file with a hard link is written in place, so both names still show the saved notebook.
        hard = joinpath(dir, "hard.jl")
        hardlink(real, hard)
        nb.cells[1].code = "x = 3"
        Pluto.save_notebook(nb, real)
        @test stat(real).inode == stat(hard).inode
        @test occursin("x = 3", read(hard, String))
    end

    @testset "saving: a staged copy a killed save left is removed once it is old (#469)" begin
        dir = mktempdir()
        path = joinpath(dir, "nb.jl")
        nb = Pluto.Notebook([Pluto.Cell(; code="x = 1")], path)
        Pluto.save_notebook(nb, path)
        old = joinpath(dir, ".nb.jl.saving.999999.0")
        fresh = joinpath(dir, ".nb.jl.saving.999999.1")
        other = joinpath(dir, ".other.jl.saving.999999.0")
        for f in (old, fresh, other)
            write(f, "cut short")
        end
        hour_ago = time() - 3600
        for f in (old, other)
            ccall(:utime, Cint, (Cstring, Ptr{NTuple{2,Int}}), f, Ref((round(Int, hour_ago), round(Int, hour_ago))))
        end
        EndeavorRuntime._remove_left_staged(path)
        @test !isfile(old)
        @test isfile(fresh)  # it may be another save under way
        @test isfile(other)  # another notebook's is left to that notebook's save
        # A save does it, once per notebook in this process.
        nb2 = joinpath(mktempdir(), "nb2.jl")
        notebook2 = Pluto.Notebook([Pluto.Cell(; code="y = 1")], nb2)
        left = joinpath(dirname(nb2), ".nb2.jl.saving.999999.0")
        write(left, "cut short")
        ccall(:utime, Cint, (Cstring, Ptr{NTuple{2,Int}}), left, Ref((round(Int, hour_ago), round(Int, hour_ago))))
        Pluto.save_notebook(notebook2, nb2)
        @test !isfile(left)
        @test readdir(dirname(nb2)) == ["nb2.jl"]
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

    @testset "warm-up: waits for quiet, then precompiles in a process of its own" begin
        @test !EndeavorRuntime._runtime_busy(nothing)
        session, nb, cells = make_session_with_notebook("x = 1")
        nb.process_status = Pluto.ProcessStatus.ready
        @test !EndeavorRuntime._runtime_busy(session)
        cells[1].queued = true
        @test EndeavorRuntime._runtime_busy(session)
        cells[1].queued = false
        nb.process_status = Pluto.ProcessStatus.starting
        @test EndeavorRuntime._runtime_busy(session)
        nb.process_status = Pluto.ProcessStatus.ready
        @test !EndeavorRuntime._runtime_busy(session)

        # A small package in a project of its own stands in for EndeavorRuntimeWarm.
        project = mktempdir()
        uuid = UUIDs.uuid4()
        mkpath(joinpath(project, "Tiny", "src"))
        write(joinpath(project, "Tiny", "Project.toml"), "name = \"Tiny\"\nuuid = \"$uuid\"\nversion = \"0.1.0\"\n")
        write(joinpath(project, "Tiny", "src", "Tiny.jl"), "module Tiny\nf() = 1\nend\n")
        write(joinpath(project, "Project.toml"), "[deps]\nTiny = \"$uuid\"\n\n[sources]\nTiny = {path = \"Tiny\"}\n")
        # Written whole, so nothing needs resolving (or a registry).
        write(joinpath(project, "Manifest.toml"), "julia_version = \"$VERSION\"\nmanifest_format = \"2.0\"\n\n[[deps.Tiny]]\npath = \"Tiny\"\nuuid = \"$uuid\"\nversion = \"0.1.0\"\n")
        julia = `$(Base.julia_cmd()) --startup-file=no --project=$project`
        precompiled() = success(`$julia -e 'exit(Base.isprecompiled(Base.identify_package("Tiny")) ? 0 : 1)'`)
        @test !precompiled()
        pkg = Base.PkgId(uuid, "Tiny")
        @test occursin("Pkg.precompile([\"Tiny\"])", string(EndeavorRuntime._warm_command(pkg, project)))
        @test Sys.iswindows() || occursin("nice", string(EndeavorRuntime._warm_command(pkg, project)))
        task = EndeavorRuntime.warm_up_later(pkg; project, quiet=0.2, every=0.1)
        @test timedwait(() -> istaskdone(task), 120.0) === :ok
        @test precompiled()
    end

    @testset "lifecycle: a native image of ours with no .ji is removed once it is old" begin
        include(joinpath(@__DIR__, "..", "..", "cache_guard.jl"))
        depot = mktempdir()
        dir = joinpath(depot, "compiled", "v1.12", "EndeavorRuntime")
        other = joinpath(depot, "compiled", "v1.12", "Other")
        mkpath(dir); mkpath(other)
        # A set in use, two images whose .ji is gone (one with a debug bundle, one whose name isn't
        # ASCII), a precompile's temporary file, and someone else's image.
        for name in ("a.ji", "a.so", "b.so", "é.dylib", "jl_t.so")
            write(joinpath(dir, name), "x")
        end
        mkpath(joinpath(dir, "b.dylib.dSYM")); mkpath(joinpath(dir, "é.dylib.dSYM"))
        write(joinpath(other, "z.so"), "x")
        @test endeavor_drop_unreadable_caches(depot; log=devnull) == String[]
        @test length(readdir(dir)) == 7
        removed = endeavor_drop_unreadable_caches(depot; log=devnull, now=time() + 2 * ENDEAVOR_ORPHAN_AGE)
        @test sort(basename.(removed)) == sort(["b.so", "b.dylib.dSYM", "é.dylib", "é.dylib.dSYM"])
        @test sort(readdir(dir)) == sort(["a.ji", "a.so", "jl_t.so"])
        @test isfile(joinpath(other, "z.so"))
    end

    @testset "lifecycle: a compiled file of ours that can't be read is removed, then compiled again" begin
        include(joinpath(@__DIR__, "..", "..", "cache_guard.jl"))
        # A small package by the warm-up's name, in a project and a depot of its own. Loading it
        # compiles it, with no registry needed.
        project, depot = mktempdir(), mktempdir()
        uuid = UUIDs.uuid4()
        mkpath(joinpath(project, "EndeavorRuntimeWarm", "src"))
        write(joinpath(project, "EndeavorRuntimeWarm", "Project.toml"), "name = \"EndeavorRuntimeWarm\"\nuuid = \"$uuid\"\nversion = \"0.1.0\"\n")
        write(joinpath(project, "EndeavorRuntimeWarm", "src", "EndeavorRuntimeWarm.jl"), "module EndeavorRuntimeWarm\nf() = 1\nend\n")
        write(joinpath(project, "Project.toml"), "[deps]\nEndeavorRuntimeWarm = \"$uuid\"\n\n[sources]\nEndeavorRuntimeWarm = {path = \"EndeavorRuntimeWarm\"}\n")
        write(joinpath(project, "Manifest.toml"), "julia_version = \"$VERSION\"\nmanifest_format = \"2.0\"\n\n[[deps.EndeavorRuntimeWarm]]\npath = \"EndeavorRuntimeWarm\"\nuuid = \"$uuid\"\nversion = \"0.1.0\"\n")
        sep = Sys.iswindows() ? ';' : ':'
        julia(script; at=project) = addenv(`$(Base.julia_cmd()) --startup-file=no --project=$at -e $script`,
            "JULIA_DEPOT_PATH" => depot * sep, "JULIA_LOAD_PATH" => join(["@", "@stdlib"], sep))
        check = "exit(Base.isprecompiled(Base.identify_package(\"EndeavorRuntimeWarm\")) ? 0 : 1)"
        @test success(julia("using EndeavorRuntimeWarm"))
        @test success(julia(check))
        dir = only(filter(isdir, [joinpath(v, "EndeavorRuntimeWarm") for v in readdir(joinpath(depot, "compiled"); join=true)]))
        ji = only(filter(endswith(".ji"), readdir(dir; join=true)))
        stem = splitext(ji)[1]
        # A file of someone else's, and a precompile's temporary file, are left alone.
        mkpath(joinpath(depot, "compiled", "v0.0", "Other"))
        other = joinpath(depot, "compiled", "v0.0", "Other", "x.ji")
        temp = joinpath(dir, "jl_abc.ji")
        write(other, "x"); write(temp, "x")
        # A native image's debug bundle (macOS makes one) goes with it; here one stands in everywhere.
        mkpath(stem * ".dylib.dSYM")
        unreadable = Set([ji, other, temp])
        @test endeavor_drop_unreadable_caches(depot; log=devnull) == String[]
        removed = endeavor_drop_unreadable_caches(depot; readable=p -> !(p in unreadable), log=devnull)
        @test ji in removed && stem * ".dylib.dSYM" in removed && all(p -> startswith(p, stem * "."), removed)
        @test !isfile(ji) && isfile(other) && isfile(temp)
        @test !any(p -> startswith(p, stem * "."), readdir(dir; join=true))
        @test !success(julia(check))
        @test success(julia("using EndeavorRuntimeWarm"))
        @test success(julia(check))
        # Never in the user's own depot.
        home = mktempdir()
        mine = joinpath(home, ".julia", "compiled", "v1.12", "EndeavorRuntimeWarm", "x.ji")
        mkpath(dirname(mine)); write(mine, "x")
        withenv((Sys.iswindows() ? "USERPROFILE" : "HOME") => home) do
            @test endeavor_drop_unreadable_caches(joinpath(home, ".julia"); readable=_ -> false, log=devnull) == String[]
        end
        @test isfile(mine)
        # The failure itself (#372). Julia calls one unreadable cache file stale and compiles over
        # it, but beside a second cache file of the package (another runtime folder compiled into
        # the same depot, as each update makes) every load opens it to sort them, and throws,
        # every time. Root reads any file, so not as root.
        if !Sys.iswindows() && ccall(:geteuid, Cuint, ()) != 0
            second = joinpath(mktempdir(), "second")
            cp(project, second)
            @test success(julia("using EndeavorRuntimeWarm"; at=second))
            jis = filter(p -> endswith(p, ".ji") && p != temp, readdir(dir; join=true))
            @test length(jis) == 2
            ji = first(jis)
            chmod(ji, 0o200)
            for _ in 1:2
                @test !success(pipeline(julia("using EndeavorRuntimeWarm"); stderr=devnull))
            end
            @test ji in endeavor_drop_unreadable_caches(depot; log=devnull)
            @test success(julia("using EndeavorRuntimeWarm"))
            @test success(julia("using EndeavorRuntimeWarm"; at=second))
            @test success(julia(check))
        end
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
        default_shutdown = EndeavorRuntime._SHUTDOWN[]
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
            EndeavorRuntime._SHUTDOWN[] = default_shutdown
            EndeavorRuntime.stop_pluto_stack!()
            EndeavorRuntime.configure_standalone!(; token="")
        end
    end

    @testset "shutdown stops a notebook process busy in a cell within seconds" begin
        # Malt's own stop at exit would wait 15 s for it to take its exit (#509).
        worker = Pluto.Malt.Worker()
        try
            Pluto.Malt.remote_do(worker) do
                t = time(); s = 0.0
                while time() - t < 120; s += sin(s); end
                s
            end
            sleep(1)
            @test Pluto.Malt.isrunning(worker)
            took = @elapsed EndeavorRuntime._stop_workers!([worker])
            @test !Pluto.Malt.isrunning(worker)
            @test took < 2 * EndeavorRuntime._WORKER_STOP_GRACE[] + 3
        finally
            Pluto.Malt.isrunning(worker) && kill(worker, Base.SIGKILL)
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
            started = result("run", Dict("notebook_id" => nid, "cells" => [X], "wait" => false, "timeout" => 60))
            @test started == Dict("accepted" => true, "process_status" => "ready", "run" => started["run"])
            @test next_note("run_finished"; nid) == Dict("notebook_id" => nid, "run" => started["run"], "cells" => [X])

            # A waited run that outlasts `timeout` returns when it passes, with the cell that
            # finished and the dependent still running. The run goes on, watched: a
            # notification says when it ends. The dependent waits for a file the test makes.
            gate = joinpath(mktempdir(), "release")
            insert(code, index) = only(result("apply", ops(Dict("op" => "insert", "code" => code, "folded" => false, "index" => index)))["inserted"])
            w, slow = insert("w = 5", 3), insert("slow = (while !isfile($(repr(gate))); sleep(0.05); end; w)", 4)
            local capped
            try
                capped = result("run", Dict("notebook_id" => nid, "cells" => [w], "wait" => true, "timeout" => 8))
                @test capped == Dict("accepted" => true, "process_status" => "ready", "completed" => [w], "timed_out" => [slow], "run" => capped["run"])
                @test nb.cells_dict[UUID(w)].output.body == "5"
                @test nb.cells_dict[UUID(slow)].running || nb.cells_dict[UUID(slow)].queued
            finally
                touch(gate)
            end
            @test next_note("run_finished"; nid) == Dict("notebook_id" => nid, "run" => capped["run"], "cells" => [w])
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
            EndeavorRuntime._watch_run!(EndeavorRuntime.standalone_session(), nb, [marked], failing, "failed-run")
            @test !marked.queued
            @test next_note("run_finished"; nid) == Dict("notebook_id" => nid, "run" => "failed-run", "cells" => [X])

            # Move and delete, then the cleanup run that follows a delete.
            result("apply", ops(Dict("op" => "move", "cell_id" => z, "index" => 0)))
            @test string.(nb.cell_order) == [z, X, Y]
            result("apply", ops(Dict("op" => "delete", "cell_id" => z)))
            @test result("run", Dict("notebook_id" => nid, "cells" => [], "wait" => false, "timeout" => 60))["accepted"]
            @test string.(nb.cell_order) == [X, Y] && !haskey(nb.cells_dict, UUID(z))

            # A package the notebook doesn't have yet waits for the user's OK where the caller asks
            # (#397): the cell stays queued, and the snapshot says what would be added.
            run_asking(cells) = result("run", Dict("notebook_id" => nid, "cells" => cells, "wait" => true, "timeout" => 60, "ask_new" => true))
            stdlib = insert("using Dates", 2)
            @test !haskey(run_asking([stdlib]), "asking")
            array = insert("[1.0, 2.5]", 3)
            @test result("run", Dict("notebook_id" => nid, "cells" => [array], "wait" => true, "timeout" => 60))["completed"] == [array]
            pkg = insert("using Example", 4)
            asked = run_asking([pkg])
            @test (asked["asking"], asked["completed"], asked["timed_out"]) == (["Example"], [], [pkg])
            @test nb.cells_dict[UUID(pkg)].queued
            # Nothing runs while it waits, so a finished cell still reads as text, not as "cells are running" (#506).
            @test occursin("2.5", result("render_text", Dict("notebook_id" => nid, "cell_id" => array))["text"])
            step = result("snapshot", Dict("notebook_id" => nid))["packages"]
            @test (step["step"], step["new"], [p["package"] for p in step["download"]["packages"]]) == ("asking", true, ["Example"])
            @test !haskey(Pluto.PkgCompat.project(nb.nbpkg_ctx).dependencies, "Example")
            # An edit that drops it asks nothing: the held cell runs.
            result("apply", ops(Dict("op" => "set_code", "cell_id" => pkg, "code" => "nothing")))
            @test !haskey(run_asking([pkg]), "asking")
            @test result("snapshot", Dict("notebook_id" => nid))["packages"] === nothing
            # A cell held with it, which the run after the edit doesn't name, runs with that run: the
            # snapshot that saw nothing new released it, and it was asked to run (#523).
            title = insert("md\"Title\"", 5)
            result("apply", ops(Dict("op" => "set_code", "cell_id" => pkg, "code" => "using Example")))
            @test run_asking([pkg, title])["asking"] == ["Example"]
            @test nb.cells_dict[UUID(title)].queued && nb.cells_dict[UUID(title)].output.last_run_timestamp == 0
            result("apply", ops(Dict("op" => "set_code", "cell_id" => pkg, "code" => "nothing")))
            @test result("snapshot", Dict("notebook_id" => nid))["packages"] === nothing
            @test !nb.cells_dict[UUID(title)].queued
            ran = run_asking([pkg])
            @test !haskey(ran, "asking") && title in ran["completed"]
            @test nb.cells_dict[UUID(title)].output.last_run_timestamp > 0
            result("apply", ops(Dict("op" => "delete", "cell_id" => title)))
            # Asked again, and stopped: the cell isn't queued any more.
            result("apply", ops(Dict("op" => "set_code", "cell_id" => pkg, "code" => "using Example")))
            @test run_asking([pkg])["asking"] == ["Example"]
            @test result("interrupt", Dict("notebook_id" => nid))["interrupted"] == true
            @test !nb.cells_dict[UUID(pkg)].queued && result("snapshot", Dict("notebook_id" => nid))["packages"] === nothing
            # The user's yes: it installs, and the cell runs.
            @test run_asking([pkg])["asking"] == ["Example"]
            approved = result("approve_install", Dict("notebook_id" => nid, "timeout" => 60))
            @test (approved["approved"], approved["packages"], approved["cells"]) == (true, ["Example"], [pkg])
            @test pkg in next_note("run_finished"; nid)["cells"]
            @test haskey(Pluto.PkgCompat.project(nb.nbpkg_ctx).dependencies, "Example") && !nb.cells_dict[UUID(pkg)].errored
            @test adapter("approve_install", Dict("notebook_id" => nid)) == Dict("error" => "ArgumentError: nothing_to_approve::Nothing in this notebook waits for the user's OK")
            # Once in the notebook, it asks nothing.
            @test !haskey(run_asking([pkg]), "asking")
            # A yes covers the packages the user was shown (#418): one that names none of what waits approves nothing.
            two = insert("using JSON, CSV", 4)
            @test run_asking([two])["asking"] == ["CSV", "JSON"]
            stale = result("approve_install", Dict("notebook_id" => nid, "timeout" => 60, "packages" => ["Example"]))
            @test (stale["approved"], stale["asking"]) == (false, ["CSV", "JSON"]) && nb.cells_dict[UUID(two)].queued
            # Asked about JSON when CSV came since: the yes covers JSON, and CSV asks on its own.
            partial = result("approve_install", Dict("notebook_id" => nid, "timeout" => 60, "packages" => ["JSON"]))
            @test (partial["approved"], partial["packages"], partial["asking"]) == (true, ["JSON"], ["CSV"])
            @test nb.cells_dict[UUID(two)].queued && !haskey(Pluto.PkgCompat.project(nb.nbpkg_ctx).dependencies, "JSON")
            @test [p["package"] for p in result("snapshot", Dict("notebook_id" => nid))["packages"]["download"]["packages"]] == ["CSV"]
            result("interrupt", Dict("notebook_id" => nid))
            result("apply", ops(Dict("op" => "delete", "cell_id" => two)))
            result("apply", ops(Dict("op" => "delete", "cell_id" => pkg), Dict("op" => "delete", "cell_id" => stdlib), Dict("op" => "delete", "cell_id" => array)))
            result("run", Dict("notebook_id" => nid, "cells" => [], "wait" => true, "timeout" => 60))

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
            # The reply names the run it starts, and that run's `run_finished` says when it ends (#170).
            allowed = result("allow_execution", Dict("notebook_id" => previewed, "run" => true, "timeout" => 60))
            @test delete!(copy(allowed), "run") == Dict("already_allowed" => false, "ran" => true, "process_status" => "starting")
            @test allowed["run"] isa String
            finished = next_note("run_finished"; nid=previewed)
            @test X in finished["cells"] && finished["run"] == allowed["run"]
            @test result("allow_execution", Dict("notebook_id" => previewed, "run" => false, "timeout" => 60)) ==
                  Dict("already_allowed" => true, "ran" => false, "process_status" => "ready")

            # A file with no environment of its own: every package it uses is new, and neither leaving
            # safe preview nor an open that runs it adds one unasked (#397).
            function envless()
                path = joinpath(mktempdir(), "envless.jl")
                write(path, "### A Pluto.jl notebook ###\n# v0.20.0\n\nusing Markdown\nusing InteractiveUtils\n\n" *
                    "# ╔═╡ 00000000-0000-0000-0000-000000000397\nusing Example\n\n" *
                    "# ╔═╡ Cell order:\n# ╠═00000000-0000-0000-0000-000000000397\n")
                return path
            end
            held = "00000000-0000-0000-0000-000000000397"
            gated = result("open", Dict("path" => envless(), "run" => false))["notebook_id"]
            allowed = result("allow_execution", Dict("notebook_id" => gated, "run" => true, "timeout" => 60, "ask_new" => true))
            @test (allowed["ran"], allowed["asking"]) == (true, ["Example"])
            gated_nb = EndeavorRuntime.standalone_session().notebooks[UUID(gated)]
            @test gated_nb.cells_dict[UUID(held)].queued && result("snapshot", Dict("notebook_id" => gated))["packages"]["step"] == "asking"
            @test gated_nb.nbpkg_ctx === nothing || !haskey(Pluto.PkgCompat.project(gated_nb.nbpkg_ctx).dependencies, "Example")
            @test adapter("shutdown", Dict("notebook_id" => gated)) == Dict("result" => Dict("safe_preview" => false))
            opened = result("open", Dict("path" => envless(), "run" => true, "timeout" => 60, "ask_new" => true))
            @test opened["asking"] == ["Example"]
            opened_nb = EndeavorRuntime.standalone_session().notebooks[UUID(opened["notebook_id"])]
            @test opened_nb.cells_dict[UUID(held)].queued && Pluto.will_run_code(opened_nb)
            @test opened_nb.nbpkg_ctx === nothing || !haskey(Pluto.PkgCompat.project(opened_nb.nbpkg_ctx).dependencies, "Example")
            adapter("shutdown", Dict("notebook_id" => opened["notebook_id"]))

            # Julia's own folder gone, as juliaup leaves the version it updated past: what would start a
            # process says why and what to do, before anything stops. The running process lives on.
            session = EndeavorRuntime.standalone_session()
            worker = Pluto.WorkspaceManager.get_workspace((session, nb)).worker
            removed(reply) = haskey(reply, "error") && startswith(reply["error"], "ArgumentError: julia_removed::Julia $VERSION, which this Endeavor runs on, was removed")
            gone_dir = mktempdir()
            EndeavorRuntime._JULIA_EXE[] = joinpath(gone_dir, "bin", "julia")
            try
                @test removed(adapter("restart", Dict("notebook_id" => nid, "timeout" => 60)))
                @test nb.process_status == Pluto.ProcessStatus.ready && Pluto.Malt.isrunning(worker)
                @test result("run", Dict("notebook_id" => nid, "cells" => [Y], "wait" => true, "timeout" => 60))["completed"] == [Y]
                @test removed(adapter("open", Dict("path" => fresh_fixture(), "run" => true)))
                gated = result("open", Dict("path" => fresh_fixture(), "run" => false))["notebook_id"]
                @test removed(adapter("allow_execution", Dict("notebook_id" => gated, "run" => true, "timeout" => 60)))
                @test session.notebooks[UUID(gated)].process_status == Pluto.ProcessStatus.waiting_for_permission
                unmade = joinpath(gone_dir, "unmade.jl")
                @test removed(adapter("new", Dict("path" => unmade))) && !isfile(unmade)
                @test occursin(gone_dir, adapter("restart", Dict("notebook_id" => nid, "timeout" => 60))["error"])
                # An open notebook says so, as it does with its Julia there.
                @test startswith(adapter("open", Dict("path" => nb.path, "run" => true))["error"], "ArgumentError: notebook_already_open::")
                # Pluto's own Restart, from the page, does nothing rather than stop the process.
                @test_logs (:warn, r"^Restart: Julia .* was removed") Pluto.responses[:restart_process](Pluto.ClientRequest(; session, notebook = nb))
                @test nb.process_status == Pluto.ProcessStatus.ready && Pluto.Malt.isrunning(worker)
                @test Pluto.WorkspaceManager.get_workspace((session, nb)).worker === worker
                result("shutdown", Dict("notebook_id" => gated))
            finally
                EndeavorRuntime._JULIA_EXE[] = ""
            end

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

            # A process that ended while it started: Pluto's own Restart, as the notebook's page
            # sends it, starts a new one, and every cell runs (#194).
            # The Malt workers running now, a notebook's own Julia (pgrep, as macOS has no /proc).
            workers() = Set(readlines(ignorestatus(`pgrep -f "Malt.*worker.jl"`)))
            crashing = result("open", Dict("path" => fresh_fixture(), "run" => false))["notebook_id"]
            cnb = EndeavorRuntime.standalone_session().notebooks[UUID(crashing)]
            before = workers()
            result("allow_execution", Dict("notebook_id" => crashing, "run" => true, "timeout" => 60))
            @test timedwait(() -> !isempty(setdiff(workers(), before)), 60; pollint=0.02) == :ok
            run(`kill -9 $(collect(setdiff(workers(), before)))`)
            # The start fails when Malt reads no port from it, at the latest after its own 30 s.
            @test timedwait(() -> istaskfailed(Pluto.WorkspaceManager.active_workspaces[cnb.notebook_id]), 60) == :ok
            Pluto.responses[:restart_process](Pluto.ClientRequest(session=EndeavorRuntime.standalone_session(), notebook=cnb); run_async=false)
            @test cnb.process_status == Pluto.ProcessStatus.ready
            @test [string(cnb.cells_dict[UUID(id)].output.body) for id in (X, Y)] == ["6", "42"]
            @test !any(c -> c.running || c.queued || c.errored, cnb.cells)
            result("shutdown", Dict("notebook_id" => crashing))
            close(sock)
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

    @testset "Stop: the page's and the agent's stop a waiting cell in its own task, and its process lives on (#139)" begin
        EndeavorRuntime.stop_pluto_stack!()
        pluto_port = 1850 + rand(0:99)
        mcp_port = 3050 + rand(0:99)
        EndeavorRuntime.configure_standalone!(; pluto_port, mcp_port)
        X, Y = "11111111-1111-1111-1111-111111111111", "22222222-2222-2222-2222-222222222222"
        try
            EndeavorRuntime.start_pluto_stack!(; pluto_port, mcp_port, launch_browser=false, http_async=true)
            adapter(method, params) = JSON.parse(String(HTTP.post("http://127.0.0.1:$mcp_port/adapter", [],
                JSON.json(Dict("method" => method, "params" => params)); readtimeout=120).body))
            result(method, params) = (reply = adapter(method, params); haskey(reply, "error") ? error(reply["error"]) : reply["result"])
            @test EndeavorRuntime._stop_hook_ok()   # the pinned Pluto; on another, Stop is Pluto's own
            nid = result("open", Dict("path" => fresh_fixture(), "run" => true))["notebook_id"]
            session = EndeavorRuntime.standalone_session()
            nb = session.notebooks[UUID(nid)]
            @test timedwait(() -> !any(c -> c.running || c.queued, nb.cells), 120) == :ok
            worker = Pluto.WorkspaceManager.get_workspace((session, nb)).worker
            @test timedwait(() -> Pluto.Malt.remote_eval_fetch(worker, :(isdefined(Main, :__endeavor_stop))), 30) == :ok
            stop_page() = Pluto.responses[:interrupt_all](Pluto.ClientRequest(session=session, notebook=nb))
            stop_agent() = result("interrupt", Dict("notebook_id" => nid))
            for (code, stop) in (("y = (sleep(600); x)", stop_page), ("y = (sleep(600); x)", stop_agent),
                                 ("y = let n = 0; for i in 1:10^12; n += i; i % 10^6 == 0 && yield(); end; n end", stop_page))
                result("apply", Dict("notebook_id" => nid, "ops" => [Dict("op" => "set_code", "cell_id" => Y, "code" => code)]))
                result("run", Dict("notebook_id" => nid, "cells" => [Y], "wait" => false, "timeout" => 60))
                @test timedwait(() -> nb.cells_dict[UUID(Y)].running, 60) == :ok
                sleep(1)
                took = @elapsed stop()
                @test took < 4
                @test timedwait(() -> !nb.cells_dict[UUID(Y)].running, 10) == :ok
                @test nb.cells_dict[UUID(Y)].errored && occursin("InterruptException", string(nb.cells_dict[UUID(Y)].output.body))
                @test Pluto.Malt.isrunning(worker) && nb.process_status == Pluto.ProcessStatus.ready
                @test result("snapshot", Dict("notebook_id" => nid))["exited"] === nothing
            end
            # A late stop doesn't reach the next run: only the cell it meant, and only right away.
            result("apply", Dict("notebook_id" => nid, "ops" => [Dict("op" => "set_code", "cell_id" => Y, "code" => "y = (sleep(3); x * 7)")]))
            result("run", Dict("notebook_id" => nid, "cells" => [Y], "wait" => false, "timeout" => 60))
            @test timedwait(() -> nb.cells_dict[UUID(Y)].running, 60) == :ok
            @test Pluto.Malt.remote_eval_fetch(worker, :(Main.__endeavor_stop(Base.UUID($X), time() + 2))) == false
            @test Pluto.Malt.remote_eval_fetch(worker, :(Main.__endeavor_stop(Base.UUID($Y), time() - 1))) == false
            @test timedwait(() -> !nb.cells_dict[UUID(Y)].running, 30) == :ok
            @test string(nb.cells_dict[UUID(Y)].output.body) == "42"
            result("shutdown", Dict("notebook_id" => nid))
        finally
            EndeavorRuntime.stop_pluto_stack!()
        end
    end

@testset "registries in the user's depot are updated before a notebook opens (#367)" begin
    git(args...) = run(pipeline(`git $args`; stdout = devnull, stderr = devnull))
    root = mktempdir()
    remote, work = joinpath(root, "Lab.git"), joinpath(root, "work")
    ours, theirs = joinpath(root, "ours"), joinpath(root, "theirs")
    mkpath(joinpath(ours, "registries"))
    git("init", "-q", "--bare", remote)
    git("init", "-q", work)
    uuid = "0d6a8e8b-3a83-4a77-9f36-5f6a4d8d1b2a"
    write(joinpath(work, "Registry.toml"), "name = \"Lab\"\nuuid = \"$uuid\"\nrepo = \"file://$remote\"\n\n[packages]\n")
    git("-C", work, "-c", "user.name=t", "-c", "user.email=t@t", "add", ".")
    git("-C", work, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "registry")
    git("-C", work, "push", "-q", remote, "HEAD:refs/heads/main")
    git("clone", "-q", "--branch", "main", remote, joinpath(theirs, "registries", "Lab"))
    # A version registered from another computer: a new commit on the registry's remote.
    write(joinpath(work, "Registry.toml"), read(joinpath(work, "Registry.toml"), String) * "# v2\n")
    git("-C", work, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qam", "new version")
    git("-C", work, "push", "-q", remote, "HEAD:refs/heads/main")
    head(dir) = readchomp(`git -C $dir rev-parse HEAD`)
    @test head(joinpath(theirs, "registries", "Lab")) != head(work)
    @test EndeavorRuntime.update_user_registries(; cooldown = 0, depots = [ours, theirs]) == 1
    @test head(joinpath(theirs, "registries", "Lab")) == head(work)
    # Once per cooldown: a second open soon after doesn't fetch again.
    @test EndeavorRuntime.update_user_registries(; cooldown = 3600, depots = [ours, theirs]) == 0
    # One we have too is left to Pkg's own update of ours.
    cp(joinpath(theirs, "registries", "Lab"), joinpath(ours, "registries", "Lab"))
    @test EndeavorRuntime.update_user_registries(; cooldown = 0, depots = [ours, theirs]) == 0
end

end
