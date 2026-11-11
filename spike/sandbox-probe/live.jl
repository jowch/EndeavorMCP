# Start a live Pluto (loopback, with its secret) on a demo notebook and write
# the page and connection details the probe's live view needs.
using Pluto
dir = get(ENV, "SPIKE_DIR", @__DIR__)
nbpath = joinpath(dir, "demo.jl")
codes = [
 "md\"# Demo notebook\"",
 "@bind n html\"<input type=range min=1 max=20 value=5>\"",
 "squares = [i^2 for i in 1:something(n, 5)]",
 "m = 7",
 """HTML(\"\"\"<canvas width=200 height=60 style="border:1px solid #888"></canvas>
<script>
const c = currentScript.parentElement.querySelector("canvas");
const g = c.getContext("2d"); g.fillStyle = "orange"; g.fillRect(10,10,\$(10*m),40);
</script>\"\"\")""",
]
if !isfile(nbpath)
    Pluto.save_notebook(Pluto.Notebook([Pluto.Cell(c) for c in codes], nbpath))
end
s = Pluto.ServerSession()
s.options.server.port = parse(Int, get(ENV, "PLUTO_PORT", "1235"))
s.options.server.launch_browser = false
nb = Pluto.SessionActions.open(s, nbpath; run_async=false)
write(joinpath(dir, "live.html"), Pluto.generate_html(; offline_bundle=true, notebook_id_js=repr(string(nb.notebook_id)), disable_ui=false))
write(joinpath(dir, "live.json"), """{"port":$(s.options.server.port),"secret":"$(s.secret)","notebook_id":"$(nb.notebook_id)"}""")
println("ready")
Pluto.run(s)
